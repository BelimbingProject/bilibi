//! Documents shared with portal customers (issue 524). Staff with the share
//! permission share a document that keeps a file with one customer Person, or
//! with a Company for every business customer who signs in through it; the
//! customer reads what was shared with their own record through the portal,
//! never through a staff session.
//!
//! A customer sees a document only while every one of these holds, rechecked
//! on every read and again by the Artifacts owner when the bytes are read:
//! the share is to the customer's own Person, or to the Company the customer
//! signs in for; it has not ended or been revoked; the document is in the
//! customer's tenant, keeps a file, is not being deleted and is not past its
//! expiry date. A download also needs the latest revision's scan to be clean.
//! Every download is an Artifacts read audited as the customer, and every
//! refused one a `core.document.download_refused` action naming the reason.
use crate::files::{self, FILES};
use crate::store::{self, On, Store};
use crate::{Document, File, Reference, ScanStatus, Sort, Source, WriteError, WriteResult, MANAGE};
use bilibi_artifacts::Artifacts;
use bilibi_audit::{record_action, Attribution};
use bilibi_authz::authorize_proof;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{PortalScope, ProofClient, ReadClient, Scope};
use jiff::{civil::Date, Timestamp};
use serde_json::{json, Value};
use std::time::SystemTime;
use tokio_postgres::{types::ToSql, Client, Row, Transaction};

/// A customer's own read of the documents shared with them, on the portal.
pub const PORTAL_VIEW: &str = "portal.document.view";
/// The audit kind of a share with a customer.
pub const CUSTOMER_SHARE_RECORD: &str = "core.document.customer_share";
/// The portal's list of shared documents.
pub const PORTAL_INDEX: &str = "/portal/documents";

/// Who a customer share reaches: one Person, or every business customer who
/// signs in for one Company.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipient {
    Person(i64),
    Company(i64),
}
impl Recipient {
    /// The record it names, as `base/records` spells it.
    pub fn reference(self) -> Reference {
        match self {
            Self::Person(id) => Reference::new("person", id.to_string()),
            Self::Company(id) => Reference::new("company", id.to_string()),
        }
    }
    /// A `person` or `company` reference with a positive id.
    pub fn from_reference(reference: &Reference) -> Option<Self> {
        let id = reference.id.parse::<i64>().ok().filter(|id| *id > 0)?;
        match reference.kind.as_str() {
            "person" => Some(Self::Person(id)),
            "company" => Some(Self::Company(id)),
            _ => None,
        }
    }
    fn ids(self) -> (Option<i64>, Option<i64>) {
        match self {
            Self::Person(id) => (Some(id), None),
            Self::Company(id) => (None, Some(id)),
        }
    }
}

/// One share of a document with a customer.
#[derive(Clone, Debug)]
pub struct CustomerShare {
    pub id: i64,
    pub recipient: Recipient,
    /// When the share ends; `None` for one that does not.
    pub expires_at: Option<Timestamp>,
    pub shared_by: i64,
    pub shared_at: Timestamp,
}

/// A share to make or replace. `until` is the last day (UTC) it works.
#[derive(Clone, Copy, Debug)]
pub struct CustomerShareInput {
    pub recipient: Recipient,
    pub until: Option<Date>,
}

/// One shared document as its customer sees it: no filing company, grants,
/// links or change log, only what the customer needs to find and fetch it.
#[derive(Clone, Debug)]
pub struct SharedDocument {
    pub id: i64,
    pub name: String,
    pub publish_date: Date,
    /// The latest revision's number; the customer gets that revision only.
    pub revision: i32,
    pub last_revision_at: Timestamp,
    pub file_name: Option<String>,
    pub byte_size: Option<i64>,
    pub scan_status: ScanStatus,
    /// The latest end among the shares that reach this customer; `None`
    /// when one of them does not end.
    pub shared_until: Option<Timestamp>,
}
impl SharedDocument {
    /// Whether the file can be downloaded now: only a clean scan can.
    pub fn downloadable(&self) -> bool {
        self.file_name.is_some() && self.scan_status == ScanStatus::Clean
    }
}

/// A page of the documents shared with the customer. `page` counts from 1.
#[derive(Clone, Debug)]
pub struct PortalFilter {
    /// Part of the name, ignoring case.
    pub search: String,
    /// Only documents related to this record: a Domain's portal page names
    /// its own record. It narrows the shared set and shows nothing else.
    pub related: Option<Reference>,
    pub sort: Sort,
    pub descending: bool,
    pub page: i64,
    pub page_size: i64,
}
impl Default for PortalFilter {
    fn default() -> Self {
        Self {
            search: String::new(),
            related: None,
            sort: Sort::LastRevision,
            descending: true,
            page: 1,
            page_size: 25,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SharedPage {
    pub entries: Vec<SharedDocument>,
    pub total: i64,
}

/// The documents a customer may see. `$1` tenant, `$2` the customer's Person,
/// `$3` the Company they sign in for (NULL for a private individual, which
/// matches no company share).
const SHARED: &str = "d.tenant_id=$1 AND d.deleting_at IS NULL AND d.source='internal'
    AND (d.expires_on IS NULL OR d.expires_on >= (now() AT TIME ZONE 'UTC')::date)
    AND EXISTS (SELECT 1 FROM core_document_customer_shares s WHERE s.tenant_id=d.tenant_id AND s.document_id=d.id
        AND (s.expires_at IS NULL OR s.expires_at > now()) AND (s.person_id=$2 OR s.company_id=$3))";
const SHARED_UNTIL: &str = "(SELECT CASE WHEN bool_or(s.expires_at IS NULL) THEN NULL ELSE max(s.expires_at) END FROM core_document_customer_shares s
    WHERE s.tenant_id=d.tenant_id AND s.document_id=d.id AND (s.expires_at IS NULL OR s.expires_at > now()) AND (s.person_id=$2 OR s.company_id=$3))";
const SHARE_COLUMNS: &str = "id,person_id,company_id,expires_at,shared_by,shared_at";

/// The customer's tenant, Person and Company, from the sealed scope only.
fn customer(scope: &Scope) -> Result<(i64, i64, Option<i64>)> {
    let person = scope.actor().person().ok_or(Error::Forbidden)?.get();
    Ok((
        scope.tenant(),
        person,
        scope.actor().organisation().map(OrganisationId::get),
    ))
}

fn time(value: SystemTime) -> Result<Timestamp> {
    Timestamp::try_from(value).map_err(|_| Error::Internal)
}

fn share_from_row(row: &Row) -> Result<CustomerShare> {
    let recipient = match (
        row.get::<_, Option<i64>>("person_id"),
        row.get::<_, Option<i64>>("company_id"),
    ) {
        (Some(person), None) => Recipient::Person(person),
        (None, Some(company)) => Recipient::Company(company),
        _ => return Err(Error::Internal),
    };
    Ok(CustomerShare {
        id: row.get("id"),
        recipient,
        expires_at: row
            .get::<_, Option<SystemTime>>("expires_at")
            .map(time)
            .transpose()?,
        shared_by: row.get("shared_by"),
        shared_at: time(row.get("shared_at"))?,
    })
}

/// What audit keeps of a share: ids and the end, never a name.
fn share_values(document: i64, share: &CustomerShare) -> Value {
    let (person, company) = share.recipient.ids();
    json!({
        "document_id": document,
        "person_id": person,
        "company_id": company,
        "expires_at": share.expires_at.map(|t| t.to_string()),
    })
}

/// The end of `until` in UTC: the share stops working after that day.
fn end_of(until: Option<Date>) -> WriteResult<Option<Timestamp>> {
    let Some(day) = until else {
        return Ok(None);
    };
    let end = day
        .checked_add(jiff::Span::new().days(1))
        .and_then(|next| next.at(0, 0, 0, 0).to_zoned(jiff::tz::TimeZone::UTC))
        .map(|zoned| zoned.timestamp())
        .map_err(|_| WriteError::field("until", "is not a usable date"))?;
    if end <= Timestamp::now() {
        return Err(WriteError::field("until", "must be today or later"));
    }
    Ok(Some(end))
}

impl Store<'_> {
    /// The document a customer share changes: one the actor may manage and
    /// share, that keeps a file.
    async fn customer_shareable(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
    ) -> WriteResult<Document> {
        let document = self.editable(on, scope, id, None, MANAGE).await?;
        if !store::permission(on.client(), scope, id, "share").await? {
            return Err(WriteError::Forbidden);
        }
        Ok(document)
    }

    /// Checks a share and returns the document and when the share ends.
    async fn check_customer_share(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        input: &CustomerShareInput,
    ) -> WriteResult<(Document, Option<Timestamp>)> {
        let document = self.customer_shareable(on, scope, id).await?;
        if document.source != Source::Internal {
            return Err(WriteError::field(
                "id",
                "is a link; only a document with a file can be shared with a customer",
            ));
        }
        let reference = input.recipient.reference();
        if let Err(refusal) = self
            .records
            .check(on.client(), scope, &reference.kind, &reference.id)
            .await?
        {
            return Err(WriteError::field(
                "recipient",
                format!("The customer {}.", refusal.message()),
            ));
        }
        Ok((document, end_of(input.until)?))
    }

    pub(crate) async fn preview_share_customer(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        input: &CustomerShareInput,
    ) -> WriteResult<CustomerShare> {
        let (_, expires_at) = self
            .check_customer_share(On::Preview(client), scope, id, input)
            .await?;
        Ok(CustomerShare {
            id: 0,
            recipient: input.recipient,
            expires_at,
            shared_by: scope.user(),
            shared_at: Timestamp::now(),
        })
    }

    /// Adds the share, or replaces the end of the one already there.
    pub(crate) async fn share_customer(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        input: &CustomerShareInput,
    ) -> WriteResult<CustomerShare> {
        let (_, expires_at) = self
            .check_customer_share(On::Commit(tx.sql()), scope, id, input)
            .await?;
        let (tenant, actor) = (scope.tenant(), scope.user());
        let (person, company) = input.recipient.ids();
        let ends = expires_at.map(SystemTime::from);
        tx.mutate(move |sql| Box::pin(async move {
            let before = sql.query_opt(
                &format!("SELECT {SHARE_COLUMNS} FROM core_document_customer_shares WHERE tenant_id=$1 AND document_id=$2 AND person_id IS NOT DISTINCT FROM $3 AND company_id IS NOT DISTINCT FROM $4 FOR UPDATE"),
                &[&tenant, &id, &person, &company],
            ).await?.map(|row| share_from_row(&row)).transpose()?;
            let row = match &before {
                Some(old) => sql.query_one(
                    &format!("UPDATE core_document_customer_shares SET expires_at=$3,shared_by=$4,shared_at=now() WHERE tenant_id=$1 AND id=$2 RETURNING {SHARE_COLUMNS}"),
                    &[&tenant, &old.id, &ends, &actor],
                ).await?,
                None => sql.query_one(
                    &format!("INSERT INTO core_document_customer_shares(tenant_id,document_id,person_id,company_id,expires_at,shared_by) VALUES($1,$2,$3,$4,$5,$6) RETURNING {SHARE_COLUMNS}"),
                    &[&tenant, &id, &person, &company, &ends, &actor],
                ).await?,
            };
            let share = share_from_row(&row)?;
            let change = Change {
                kind: CUSTOMER_SHARE_RECORD.into(),
                id: share.id,
                old: before.as_ref().map_or_else(|| json!({}), |old| share_values(id, old)),
                new: share_values(id, &share),
            };
            Ok((share, vec![change]))
        })).await.map_err(store::write_error)
    }

    /// The share `unshare_customer` would revoke.
    pub(crate) async fn customer_share(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        share: i64,
    ) -> WriteResult<CustomerShare> {
        self.customer_shareable(on, scope, id).await?;
        let row = on
            .client()
            .query_opt(
                &format!("SELECT {SHARE_COLUMNS} FROM core_document_customer_shares WHERE tenant_id=$1 AND document_id=$2 AND id=$3"),
                &[&scope.tenant(), &id, &share],
            )
            .await?
            .ok_or(WriteError::NotFound)?;
        Ok(share_from_row(&row)?)
    }

    /// Revokes one share: the customer stops seeing the document at once.
    pub(crate) async fn unshare_customer(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        share: i64,
    ) -> WriteResult<()> {
        self.customer_share(On::Commit(tx.sql()), scope, id, share)
            .await?;
        let tenant = scope.tenant();
        tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_opt(
                &format!("DELETE FROM core_document_customer_shares WHERE tenant_id=$1 AND document_id=$2 AND id=$3 RETURNING {SHARE_COLUMNS}"),
                &[&tenant, &id, &share],
            ).await?.ok_or(Error::NotFound)?;
            let old = share_from_row(&row)?;
            let change = Change { kind: CUSTOMER_SHARE_RECORD.into(), id: share, old: share_values(id, &old), new: json!({}) };
            Ok(((), vec![change]))
        })).await.map_err(store::write_error)
    }

    /// Every customer share of the document, ended ones too, for someone
    /// allowed to share it.
    pub(crate) async fn customer_shares(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
    ) -> WriteResult<Vec<CustomerShare>> {
        authorize_proof(client, scope, MANAGE).await?;
        self.get(client, scope, id).await?;
        if !store::permission(client, scope, id, "share").await? {
            return Err(WriteError::Forbidden);
        }
        Ok(client
            .query(
                &format!("SELECT {SHARE_COLUMNS} FROM core_document_customer_shares WHERE tenant_id=$1 AND document_id=$2 ORDER BY id"),
                &[&scope.tenant(), &id],
            )
            .await?
            .iter()
            .map(share_from_row)
            .collect::<Result<Vec<_>>>()?)
    }
}

/// A page of the documents shared with the customer, newest revision first
/// by default. The capability is the caller's check.
pub(crate) async fn list(
    client: &Client,
    portal: &PortalScope,
    filter: &PortalFilter,
) -> Result<SharedPage> {
    let (tenant, person, company) = customer(portal.scope())?;
    let page_size = filter.page_size.clamp(1, 100);
    let offset = (filter.page.max(1) - 1).saturating_mul(page_size);
    let search = filter.search.trim().chars().take(200).collect::<String>();
    let pattern = format!(
        "%{}%",
        search
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let related = filter.related.as_ref().filter(|r| {
        !r.kind.is_empty() && r.kind.len() <= 100 && !r.id.is_empty() && r.id.len() <= 64
    });
    let (related_kind, related_id) = related.map_or((None, None), |r| {
        (Some(r.kind.as_str()), Some(r.id.as_str()))
    });
    let related_condition = if related.is_some() {
        "EXISTS (SELECT 1 FROM core_document_links l WHERE l.tenant_id=d.tenant_id AND l.document_id=d.id AND l.record_kind=$4 AND l.record_id=$5)"
    } else {
        "$4::text IS NULL AND $5::text IS NULL"
    };
    let conditions =
        format!("{SHARED} AND {related_condition} AND ($6::text = '' OR d.name ILIKE $7::text)");
    let params: [&(dyn ToSql + Sync); 7] = [
        &tenant,
        &person,
        &company,
        &related_kind,
        &related_id,
        &search,
        &pattern,
    ];
    let total: i64 = client
        .query_one(
            &format!("SELECT count(*) FROM core_documents d WHERE {conditions}"),
            &params,
        )
        .await?
        .get(0);
    let order = match filter.sort {
        Sort::LastRevision => "d.last_revision_at",
        Sort::Name => "lower(d.name)",
        Sort::PublishDate => "d.publish_date",
        Sort::Created => "d.created_at",
    };
    let direction = if filter.descending { "DESC" } else { "ASC" };
    let rows = client
        .query(
            &format!(
                "SELECT d.id,d.name,d.publish_date::text AS publish_date,d.revision,d.last_revision_at,r.file_name,r.byte_size,r.scan_status,{SHARED_UNTIL} AS shared_until
                 FROM core_documents d
                 LEFT JOIN core_document_revisions r ON r.tenant_id=d.tenant_id AND r.document_id=d.id AND r.revision=d.revision
                 WHERE {conditions} ORDER BY {order} {direction}, d.id {direction} LIMIT {page_size} OFFSET {offset}"
            ),
            &params,
        )
        .await?;
    let entries = rows
        .iter()
        .map(|row| {
            Ok(SharedDocument {
                id: row.get("id"),
                name: row.get("name"),
                publish_date: row
                    .get::<_, String>("publish_date")
                    .parse::<Date>()
                    .map_err(|_| Error::Internal)?,
                revision: row.get("revision"),
                last_revision_at: time(row.get("last_revision_at"))?,
                file_name: row.get("file_name"),
                byte_size: row.get("byte_size"),
                scan_status: row
                    .get::<_, Option<String>>("scan_status")
                    .as_deref()
                    .and_then(ScanStatus::parse)
                    .unwrap_or_default(),
                shared_until: row
                    .get::<_, Option<SystemTime>>("shared_until")
                    .map(time)
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(SharedPage { entries, total })
}

/// Why a customer's download was refused, for the audit trail.
async fn refusal(client: &Client, portal: &PortalScope, id: i64) -> Result<&'static str> {
    let (tenant, person, company) = customer(portal.scope())?;
    let row = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM core_document_customer_shares s WHERE s.tenant_id=$1 AND s.document_id=$4 AND (s.person_id=$2 OR s.company_id=$3)),
                    EXISTS (SELECT 1 FROM core_document_customer_shares s WHERE s.tenant_id=$1 AND s.document_id=$4 AND (s.person_id=$2 OR s.company_id=$3) AND (s.expires_at IS NULL OR s.expires_at > now())),
                    EXISTS (SELECT 1 FROM core_documents d WHERE d.tenant_id=$1 AND d.id=$4 AND d.expires_on < (now() AT TIME ZONE 'UTC')::date)",
            &[&tenant, &person, &company, &id],
        )
        .await?;
    let (any, live, expired): (bool, bool, bool) = (row.get(0), row.get(1), row.get(2));
    Ok(if any && !live {
        "share_ended"
    } else if any && expired {
        "document_expired"
    } else {
        "not_shared"
    })
}

/// Records a refused download as the customer: ids and a reason code only.
async fn refused(
    client: &Client,
    portal: &PortalScope,
    id: i64,
    revision: Option<i32>,
    reason: &str,
) -> Result<()> {
    record_action(
        client,
        &Attribution::from_scope(portal.scope()),
        None,
        files::DOWNLOAD_REFUSED,
        json!({"document_id": id, "revision": revision, "reason": reason, "portal": true}),
        false,
    )
    .await?;
    Ok(())
}

/// The latest revision's file of a document shared with the customer, read
/// through Artifacts (audited as the customer) every time. Anything else is
/// refused and the refusal audited: `NotFound` for a document the customer
/// may not see (another customer's, another tenant's, an ended or revoked
/// share, an expired or deleting document, no file), `Quarantined` or
/// `ScanPending` for a file that is not clean.
pub(crate) async fn read(
    client: &mut Client,
    artifacts: &Artifacts,
    portal: &PortalScope,
    id: i64,
) -> WriteResult<File> {
    if let Err(error) = bilibi_authz::authorize_portal(&*client, portal, PORTAL_VIEW).await {
        if matches!(error, Error::Forbidden) {
            refused(client, portal, id, None, "no_portal_capability").await?;
        }
        return Err(error.into());
    }
    let (tenant, person, company) = customer(portal.scope())?;
    let row = client
        .query_opt(
            &format!(
                "SELECT d.company_id,d.revision,r.artifact_id,r.file_name,r.scan_status FROM core_documents d
                 LEFT JOIN core_document_revisions r ON r.tenant_id=d.tenant_id AND r.document_id=d.id AND r.revision=d.revision
                 WHERE {SHARED} AND d.id=$4"
            ),
            &[&tenant, &person, &company, &id],
        )
        .await
        .map_err(Error::from)?;
    let Some(row) = row else {
        let reason = refusal(client, portal, id).await?;
        refused(client, portal, id, None, reason).await?;
        return Err(WriteError::NotFound);
    };
    let (company_id, revision): (i64, i32) = (row.get(0), row.get(1));
    let (artifact_id, file_name, scan_status): (Option<String>, Option<String>, Option<String>) =
        (row.get(2), row.get(3), row.get(4));
    let (Some(artifact_id), Some(file_name)) = (artifact_id, file_name) else {
        refused(client, portal, id, Some(revision), "no_file").await?;
        return Err(WriteError::NotFound);
    };
    match scan_status.as_deref() {
        Some("clean") => {}
        Some("pending") => {
            refused(client, portal, id, Some(revision), "scan_pending").await?;
            return Err(WriteError::ScanPending);
        }
        _ => {
            refused(client, portal, id, Some(revision), "not_clean").await?;
            return Err(WriteError::Quarantined);
        }
    }
    let company = OrganisationId::new(company_id).map_err(|_| Error::Internal)?;
    match artifacts
        .for_portal(portal, company, &FILES)
        .read(client, &artifact_id)
        .await
        .map_err(files::artifact_error)
    {
        Ok(file) => Ok(File {
            file_name,
            content_type: file.metadata.content_type.clone(),
            bytes: file.bytes,
        }),
        Err(error @ (WriteError::Forbidden | WriteError::NotFound)) => {
            // The share ended or the file changed between the two reads.
            refused(client, portal, id, Some(revision), "not_shared").await?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// The Artifacts owner's rule for a customer's read: a clean latest
/// revision's file of a document shared with the customer now.
pub(crate) async fn serves_file(
    client: &dyn ProofClient,
    scope: &Scope,
    artifact_id: Option<&str>,
) -> Result<bool> {
    let Some(artifact_id) = artifact_id else {
        return Ok(false);
    };
    authorize_proof(client, scope, PORTAL_VIEW).await?;
    let (tenant, person, company) = customer(scope)?;
    let artifact_id = artifact_id.to_owned();
    Ok(client
        .query_opt(
            &format!(
                "SELECT 1 FROM core_document_revisions r JOIN core_documents d ON d.tenant_id=r.tenant_id AND d.id=r.document_id AND d.revision=r.revision
                 WHERE r.artifact_id=$4 AND r.scan_status='clean' AND {SHARED}"
            ),
            &[&tenant, &person, &company, &artifact_id],
        )
        .await?
        .is_some())
}

/// Removes every customer share of a document being finalized for deletion.
pub(crate) async fn remove_all(
    sql: &Transaction<'_>,
    tenant: i64,
    document: i64,
) -> Result<Vec<Change>> {
    sql.query(
        &format!("DELETE FROM core_document_customer_shares WHERE tenant_id=$1 AND document_id=$2 RETURNING {SHARE_COLUMNS}"),
        &[&tenant, &document],
    )
    .await?
    .iter()
    .map(|row| {
        let share = share_from_row(row)?;
        Ok(Change {
            kind: CUSTOMER_SHARE_RECORD.into(),
            id: share.id,
            old: share_values(document, &share),
            new: json!({}),
        })
    })
    .collect()
}

/// Moves a merge source's live customer shares to the survivor. A recipient
/// the survivor already shares with keeps the survivor's share; an ended
/// share goes with the source.
pub(crate) async fn carry(
    sql: &Transaction<'_>,
    tenant: i64,
    source: i64,
    survivor: i64,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for row in sql
        .query(
            &format!("DELETE FROM core_document_customer_shares WHERE tenant_id=$1 AND document_id=$2 RETURNING {SHARE_COLUMNS}"),
            &[&tenant, &source],
        )
        .await?
    {
        let share = share_from_row(&row)?;
        changes.push(Change {
            kind: CUSTOMER_SHARE_RECORD.into(),
            id: share.id,
            old: share_values(source, &share),
            new: json!({}),
        });
        if share.expires_at.is_some_and(|end| end <= Timestamp::now()) {
            continue;
        }
        let (person, company) = share.recipient.ids();
        let ends = share.expires_at.map(SystemTime::from);
        let shared_at = SystemTime::from(share.shared_at);
        if let Some(row) = sql
            .query_opt(
                &format!(
                    "INSERT INTO core_document_customer_shares(tenant_id,document_id,person_id,company_id,expires_at,shared_by,shared_at)
                     SELECT $1::bigint,$2::bigint,$3::bigint,$4::bigint,$5::timestamptz,$6::bigint,$7::timestamptz WHERE NOT EXISTS (SELECT 1 FROM core_document_customer_shares s WHERE s.tenant_id=$1 AND s.document_id=$2
                         AND s.person_id IS NOT DISTINCT FROM $3 AND s.company_id IS NOT DISTINCT FROM $4)
                     RETURNING {SHARE_COLUMNS}"
                ),
                &[&tenant, &survivor, &person, &company, &ends, &share.shared_by, &shared_at],
            )
            .await?
        {
            let moved = share_from_row(&row)?;
            changes.push(Change {
                kind: CUSTOMER_SHARE_RECORD.into(),
                id: moved.id,
                old: json!({}),
                new: share_values(survivor, &moved),
            });
        }
    }
    Ok(changes)
}
