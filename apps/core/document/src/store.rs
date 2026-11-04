//! `core_documents`, `core_document_revisions`, `core_document_links` and
//! `core_document_marks` SQL. Every statement binds the tenant; every write
//! goes through the caller's `MutationTransaction` and returns its
//! `Change`s. Checks run before a write, so a refusal never poisons the
//! caller's transaction.
use crate::files::{self, TakenFile};
use crate::model::{self, Draft, FieldError, Patch, RevisionInput, ScanStatus, Source};
use crate::{
    contributions::ACTIVE, AccessGrant, Deleted, Document, Filter, Link, Page, Reference, Revision,
    Sort, WriteError, WriteResult, LINK_RECORD, LIST_CATEGORY, LIST_STATUS, LIST_SUB_CATEGORY,
    LIST_TYPE, MANAGE, RECORD, REVISION_RECORD, VIEW,
};
use bilibi_authz::authorize_proof;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_datetime::DisplayPolicy;
use bilibi_picklists::PickLists;
use bilibi_principal_directory::{
    DirectoryClient, DirectoryFuture, PrincipalDirectory, PrincipalKind, PrincipalRef, Query,
};
use bilibi_records::Records;
use bilibi_tenancy::{ProofClient, ReadClient, Scope};
use jiff::{civil::Date, Timestamp};
use serde_json::{json, Value};
use std::time::SystemTime;
use tokio_postgres::{types::ToSql, Row, Transaction};

const COLUMNS: &str = "d.id,d.company_id,d.name,d.publish_date::text AS publish_date,d.source,d.external_url,d.category,d.sub_category,d.document_type,d.status,d.revision,d.last_revision_at,d.created_by,d.created_at,d.updated_at,d.deleting_at,d.version";
const REVISION_COLUMNS: &str = "r.id,r.document_id,r.revision,r.name,r.artifact_id,r.file_name,r.content_type,r.byte_size,r.sha256,r.external_url,r.change_log,r.scan_status,r.created_by,r.created_at";
const LINK_COLUMNS: &str = "l.id,l.record_kind,l.record_id,l.linked_by,l.linked_at";

fn time(value: SystemTime) -> Result<Timestamp> {
    Timestamp::try_from(value).map_err(|_| Error::Internal)
}
fn date(value: Option<&str>) -> Result<Option<Date>> {
    value
        .map(|d| d.parse::<Date>().map_err(|_| Error::Internal))
        .transpose()
}

fn from_row(row: &Row) -> Result<Document> {
    Ok(Document {
        id: row.get("id"),
        company_id: row.get("company_id"),
        name: row.get("name"),
        publish_date: date(row.get("publish_date"))?.ok_or(Error::Internal)?,
        source: Source::parse(row.get("source")).ok_or(Error::Internal)?,
        external_url: row.get("external_url"),
        category: row.get("category"),
        sub_category: row.get("sub_category"),
        document_type: row.get("document_type"),
        status: row.get("status"),
        revision: row.get("revision"),
        last_revision_at: time(row.get("last_revision_at"))?,
        created_by: row.get("created_by"),
        created_at: time(row.get("created_at"))?,
        updated_at: time(row.get("updated_at"))?,
        deleting_at: row
            .get::<_, Option<SystemTime>>("deleting_at")
            .map(time)
            .transpose()?,
        version: row.get("version"),
    })
}
fn revision_from_row(row: &Row) -> Result<Revision> {
    Ok(Revision {
        id: row.get("id"),
        document_id: row.get("document_id"),
        revision: row.get("revision"),
        name: row.get("name"),
        artifact_id: row.get("artifact_id"),
        file_name: row.get("file_name"),
        content_type: row.get("content_type"),
        byte_size: row.get("byte_size"),
        sha256: row.get("sha256"),
        external_url: row.get("external_url"),
        change_log: row.get("change_log"),
        scan_status: ScanStatus::parse(row.get("scan_status")).ok_or(Error::Internal)?,
        created_by: row.get("created_by"),
        created_at: time(row.get("created_at"))?,
    })
}
fn link_from_row(row: &Row) -> Result<Link> {
    Ok(Link {
        id: row.get("id"),
        record: Reference::new(
            row.get::<_, String>("record_kind"),
            row.get::<_, String>("record_id"),
        ),
        linked_by: row.get("linked_by"),
        linked_at: time(row.get("linked_at"))?,
    })
}

/// The values audit keeps for a document.
fn audit_values(d: &Document) -> Value {
    json!({
        "company_id": d.company_id,
        "name": d.name,
        "publish_date": d.publish_date.to_string(),
        "source": d.source.code(),
        "external_url": d.external_url,
        "category": d.category,
        "sub_category": d.sub_category,
        "document_type": d.document_type,
        "status": d.status,
        "revision": d.revision,
        "deleting": d.deleting_at.is_some(),
        "version": d.version,
    })
}
fn revision_values(r: &Revision) -> Value {
    json!({
        "document_id": r.document_id,
        "revision": r.revision,
        "name": r.name,
        "artifact_id": r.artifact_id,
        "file_name": r.file_name,
        "content_type": r.content_type,
        "byte_size": r.byte_size,
        "sha256": r.sha256,
        "external_url": r.external_url,
        "change_log": r.change_log,
        "scan_status": r.scan_status.code(),
    })
}
fn link_values(document: i64, l: &Link) -> Value {
    json!({"document_id": document, "record": {"kind": l.record.kind, "id": l.record.id}})
}

/// Who is reading, and how far their reach goes.
struct Reader {
    tenant: i64,
    company: Option<i64>,
    user: i64,
}
async fn reader(client: &dyn ProofClient, scope: &Scope) -> Result<Reader> {
    authorize_proof(client, scope, VIEW).await?;
    Ok(Reader {
        tenant: scope.tenant(),
        company: scope.current_company().map(|o| o.get()),
        user: scope.user(),
    })
}
impl Reader {
    async fn permission(
        &self,
        client: &dyn ProofClient,
        document: i64,
        permission: &str,
    ) -> Result<bool> {
        let column = match permission {
            "view" => "can_view",
            "download" => "can_download",
            "manage" => "can_manage",
            "share" => "can_share",
            _ => return Err(Error::Internal),
        };
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM core_document_access a WHERE a.tenant_id=$1 AND a.document_id=$2 AND a.{column} AND ((a.principal_type='user' AND a.principal_id=$3) OR (a.principal_type='company' AND a.principal_id=$4)))"
        );
        Ok(client
            .query_opt(&sql, &[&self.tenant, &document, &self.user, &self.company])
            .await?
            .ok_or(Error::Internal)?
            .get(0))
    }
}

pub(crate) async fn permission(
    client: &dyn ProofClient,
    scope: &Scope,
    document: i64,
    permission: &str,
) -> Result<bool> {
    reader(client, scope)
        .await?
        .permission(client, document, permission)
        .await
}

/// The document, whoever may see it; `lock` holds it for a write.
async fn select(
    client: &dyn ProofClient,
    scope: &Scope,
    id: i64,
    lock: bool,
) -> Result<Option<Document>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM core_documents d WHERE d.tenant_id=$1 AND d.id=$2{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    client
        .query_opt(&sql, &[&scope.tenant(), &id])
        .await?
        .map(|row| from_row(&row))
        .transpose()
}

/// Whether the reader has explicit view access, for `base/records`.
pub(crate) async fn filed_proof(client: &dyn ProofClient, scope: &Scope, id: i64) -> Result<()> {
    let reader = reader(client, scope).await?;
    let document = select(client, scope, id, false)
        .await?
        .ok_or(Error::NotFound)?;
    if document.deleting_at.is_none() && reader.permission(client, id, "view").await? {
        Ok(())
    } else {
        Err(Error::Forbidden)
    }
}

pub(crate) async fn revisions(
    client: &dyn ReadClient,
    scope: &Scope,
    id: i64,
) -> Result<Vec<Revision>> {
    client
        .query(
            &format!("SELECT {REVISION_COLUMNS} FROM core_document_revisions r WHERE r.tenant_id=$1 AND r.document_id=$2 ORDER BY r.revision DESC"),
            &[&scope.tenant(), &id],
        )
        .await?
        .iter()
        .map(revision_from_row)
        .collect()
}

pub(crate) async fn latest_revisions(
    client: &dyn ReadClient,
    scope: &Scope,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, Revision>> {
    authorize_proof(client, scope, VIEW).await?;
    client
        .query(
            &format!("SELECT DISTINCT ON (r.document_id) {REVISION_COLUMNS} FROM core_document_revisions r WHERE r.tenant_id=$1 AND r.document_id = ANY($2) ORDER BY r.document_id, r.revision DESC"),
            &[&scope.tenant(), &ids],
        )
        .await?
        .iter()
        .map(|row| revision_from_row(row).map(|r| (r.document_id, r)))
        .collect()
}

pub(crate) async fn links(client: &dyn ReadClient, scope: &Scope, id: i64) -> Result<Vec<Link>> {
    client
        .query(
            &format!("SELECT {LINK_COLUMNS} FROM core_document_links l WHERE l.tenant_id=$1 AND l.document_id=$2 ORDER BY l.id"),
            &[&scope.tenant(), &id],
        )
        .await?
        .iter()
        .map(link_from_row)
        .collect()
}

/// Directory reads on a borrowed client, so a write checks an assignee on
/// its own transaction.
struct Directory<'c>(&'c dyn ReadClient);
impl DirectoryClient for Directory<'_> {
    fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> DirectoryFuture<'a, Vec<Row>> {
        self.0.query(sql, params)
    }
}

/// Where a write's checks run: a preview's read-only client, or the
/// commit's own transaction, which also locks what it reads.
#[derive(Clone, Copy)]
pub(crate) enum On<'c> {
    Preview(&'c dyn ReadClient),
    Commit(&'c Transaction<'c>),
}
impl<'c> On<'c> {
    pub(crate) fn client(self) -> &'c dyn ReadClient {
        match self {
            Self::Preview(client) => client,
            Self::Commit(tx) => tx,
        }
    }
    fn locks(self) -> bool {
        matches!(self, Self::Commit(_))
    }
}

/// A checked new document, ready to insert.
pub(crate) struct CreatePlan {
    company: i64,
    values: Values,
    /// `None` in a preview whose draft left it to today.
    pub publish_date: Option<Date>,
    pub source: Source,
    pub external_url: Option<String>,
    pub change_log: String,
    pub file: Option<TakenFile>,
    pub related: Vec<Reference>,
}
impl CreatePlan {
    /// The checked draft: defaults filled in, references in their one
    /// spelling.
    pub(crate) fn draft(&self) -> Draft {
        Draft {
            name: Some(self.values.name.clone()),
            publish_date: self.publish_date,
            source: self.source,
            artifact_id: self.file.as_ref().map(|f| f.artifact_id().to_owned()),
            external_url: self.external_url.clone(),
            category: self.values.category.clone(),
            sub_category: self.values.sub_category.clone(),
            document_type: self.values.document_type.clone(),
            status: Some(self.values.status.clone()).filter(|s| !s.is_empty()),
            change_log: Some(self.change_log.clone()),
            related: self.related.clone(),
        }
    }
}

/// A checked new revision, ready to insert.
pub(crate) struct RevisionPlan {
    pub document: Document,
    pub revision: i32,
    pub change_log: String,
    pub external_url: Option<String>,
    pub file: Option<TakenFile>,
}

/// The values a write stores, checked.
struct Values {
    name: String,
    publish_date: Date,
    category: Option<String>,
    sub_category: Option<String>,
    document_type: Option<String>,
    status: String,
}

/// What a write checks against the database: records, pick-lists, people
/// and the reader's zone.
pub(crate) struct Store<'s> {
    pub records: &'s Records,
    pub picklists: &'s PickLists,
    pub display: &'s DisplayPolicy,
    pub directory: &'s PrincipalDirectory,
}
impl Store<'_> {
    /// Links are filing aids only. They never grant document access.
    async fn visible(
        &self,
        client: &dyn ReadClient,
        _scope: &Scope,
        reader: &Reader,
        document: &Document,
    ) -> Result<bool> {
        if document.deleting_at.is_some() {
            return Ok(false);
        }
        reader.permission(client, document.id, "view").await
    }

    pub(crate) async fn get(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
    ) -> Result<Document> {
        let reader = reader(client, scope).await?;
        let document = select(client, scope, id, false)
            .await?
            .ok_or(Error::NotFound)?;
        if self.visible(client, scope, &reader, &document).await? {
            Ok(document)
        } else {
            Err(Error::Forbidden)
        }
    }

    pub(crate) async fn access(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
    ) -> WriteResult<Vec<AccessGrant>> {
        authorize_proof(client, scope, MANAGE).await?;
        self.get(client, scope, id).await?;
        if !permission(client, scope, id, "share").await? {
            return Err(WriteError::Forbidden);
        }
        Ok(client.query(
            "SELECT id,principal_type,principal_id,can_view,can_download,can_manage,can_share FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 ORDER BY principal_type,principal_id",
            &[&scope.tenant(), &id],
        ).await?.into_iter().map(|row| AccessGrant { id: row.get(0), principal_type: row.get(1), principal_id: row.get(2), can_view: row.get(3), can_download: row.get(4), can_manage: row.get(5), can_share: row.get(6) }).collect())
    }

    async fn check_share(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        principal_type: &str,
        principal_id: i64,
    ) -> WriteResult<()> {
        self.editable(on, scope, id, None, MANAGE).await?;
        let client = on.client();
        if !permission(client, scope, id, "share").await? {
            return Err(WriteError::Forbidden);
        }
        let valid = match principal_type {
            "user" => principal_id > 0 && self.active_account(client, scope, principal_id).await?,
            "company" => scope.current_company().map(|company| company.get()) == Some(principal_id),
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(WriteError::field(
                "principal",
                "is not an account or workplace you can share with",
            ))
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn preview_share(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        principal_type: &str,
        principal_id: i64,
        can_download: bool,
        can_manage: bool,
        can_share: bool,
    ) -> WriteResult<AccessGrant> {
        self.check_share(On::Preview(client), scope, id, principal_type, principal_id)
            .await?;
        Ok(AccessGrant {
            id: 0,
            principal_type: principal_type.to_owned(),
            principal_id,
            can_view: true,
            can_download,
            can_manage,
            can_share,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn share(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        principal_type: &str,
        principal_id: i64,
        can_download: bool,
        can_manage: bool,
        can_share: bool,
    ) -> WriteResult<AccessGrant> {
        self.check_share(
            On::Commit(tx.sql()),
            scope,
            id,
            principal_type,
            principal_id,
        )
        .await?;
        let (tenant, actor, kind) = (scope.tenant(), scope.user(), principal_type.to_owned());
        tx.mutate(move |sql| Box::pin(async move {
            let before = sql.query_opt(
                "SELECT id,can_view,can_download,can_manage,can_share FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND principal_type=$3 AND principal_id=$4 FOR UPDATE",
                &[&tenant,&id,&kind,&principal_id],
            ).await?;
            let row = sql.query_one(
                "INSERT INTO core_document_access(tenant_id,document_id,principal_type,principal_id,can_view,can_download,can_manage,can_share,granted_by) VALUES($1,$2,$3,$4,true,$5,$6,$7,$8) ON CONFLICT(tenant_id,document_id,principal_type,principal_id) DO UPDATE SET can_view=true,can_download=EXCLUDED.can_download,can_manage=EXCLUDED.can_manage,can_share=EXCLUDED.can_share,granted_by=EXCLUDED.granted_by,granted_at=now() RETURNING id,principal_type,principal_id,can_view,can_download,can_manage,can_share",
                &[&tenant,&id,&kind,&principal_id,&can_download,&can_manage,&can_share,&actor],
            ).await?;
            let grant = AccessGrant { id: row.get(0), principal_type: row.get(1), principal_id: row.get(2), can_view: row.get(3), can_download: row.get(4), can_manage: row.get(5), can_share: row.get(6) };
            let old = before.map(|row| json!({"document_id":id,"principal_type":kind,"principal_id":principal_id,"view":row.get::<_,bool>(1),"download":row.get::<_,bool>(2),"manage":row.get::<_,bool>(3),"share":row.get::<_,bool>(4)})).unwrap_or_else(|| json!({}));
            let change = Change { kind: crate::ACCESS_RECORD.into(), id: grant.id, old, new: json!({"document_id":id,"principal_type":grant.principal_type,"principal_id":grant.principal_id,"view":grant.can_view,"download":grant.can_download,"manage":grant.can_manage,"share":grant.can_share}) };
            Ok((grant, vec![change]))
        })).await.map_err(write_error)
    }

    pub(crate) async fn unshare(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        grant: i64,
    ) -> WriteResult<()> {
        self.editable(On::Commit(tx.sql()), scope, id, None, MANAGE)
            .await?;
        if !permission(tx.sql(), scope, id, "share").await? {
            return Err(WriteError::Forbidden);
        }
        let tenant = scope.tenant();
        let actor = scope.user();
        tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_opt(
                "SELECT principal_type,principal_id,can_view,can_download,can_manage,can_share FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND id=$3 FOR UPDATE",
                &[&tenant,&id,&grant],
            ).await?.ok_or(Error::NotFound)?;
            if row.get::<_,String>(0) == "user" && row.get::<_,i64>(1) == actor {
                return Err(Error::Forbidden);
            }
            sql.execute("DELETE FROM core_document_access WHERE tenant_id=$1 AND id=$2", &[&tenant,&grant]).await?;
            let change = Change { kind: crate::ACCESS_RECORD.into(), id: grant, old: json!({"document_id":id,"principal_type":row.get::<_,String>(0),"principal_id":row.get::<_,i64>(1),"view":row.get::<_,bool>(2),"download":row.get::<_,bool>(3),"manage":row.get::<_,bool>(4),"share":row.get::<_,bool>(5)}), new: json!({}) };
            Ok(((), vec![change]))
        })).await.map_err(write_error)
    }

    pub(crate) async fn preview_unshare(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        grant: i64,
    ) -> WriteResult<AccessGrant> {
        self.editable(On::Preview(client), scope, id, None, MANAGE)
            .await?;
        if !permission(client, scope, id, "share").await? {
            return Err(WriteError::Forbidden);
        }
        let row = client.query_opt(
            "SELECT id,principal_type,principal_id,can_view,can_download,can_manage,can_share FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND id=$3",
            &[&scope.tenant(),&id,&grant],
        ).await?.ok_or(WriteError::NotFound)?;
        let result = AccessGrant {
            id: row.get(0),
            principal_type: row.get(1),
            principal_id: row.get(2),
            can_view: row.get(3),
            can_download: row.get(4),
            can_manage: row.get(5),
            can_share: row.get(6),
        };
        if result.principal_type == "user" && result.principal_id == scope.user() {
            return Err(WriteError::Forbidden);
        }
        Ok(result)
    }

    pub(crate) async fn list(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        filter: &Filter,
    ) -> Result<Page> {
        let reader = reader(client, scope).await?;
        // A record filter narrows documents the reader already has access to.
        let related = match &filter.related {
            None => None,
            Some(record) => match self
                .records
                .check(client, scope, &record.kind, &record.id)
                .await?
            {
                Ok(record) => Some(record),
                Err(bilibi_records::Refusal::Forbidden) => return Err(Error::Forbidden),
                Err(_) => return Err(Error::NotFound),
            },
        };
        let page_size = filter.page_size.clamp(1, 300);
        let offset = (filter.page.max(1) - 1).saturating_mul(page_size);
        let search = filter.search.trim().chars().take(200).collect::<String>();
        let pattern = format!(
            "%{}%",
            search
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let (related_kind, related_id) = related.as_ref().map_or((None, None), |r| {
            (Some(r.kind.as_str()), Some(r.id.as_str()))
        });
        let conditions = "d.tenant_id=$1 AND d.deleting_at IS NULL
             AND EXISTS (SELECT 1 FROM core_document_access a WHERE a.tenant_id=d.tenant_id AND a.document_id=d.id AND a.can_view AND ((a.principal_type='user' AND a.principal_id=$2) OR (a.principal_type='company' AND a.principal_id=$4)))
             AND ($3::text IS NULL OR EXISTS (SELECT 1 FROM core_document_links l WHERE l.tenant_id=d.tenant_id AND l.document_id=d.id AND l.record_kind=$3 AND l.record_id=$5))
             AND ($6 = '' OR d.name ILIKE $7)
             AND ($8::text IS NULL OR d.category = $8)
             AND ($9::text IS NULL OR d.document_type = $9)
             AND ($10::text IS NULL OR d.status = $10)";
        let params: [&(dyn ToSql + Sync); 10] = [
            &reader.tenant,
            &reader.user,
            &related_kind,
            &reader.company,
            &related_id,
            &search,
            &pattern,
            &filter.category,
            &filter.document_type,
            &filter.status,
        ];
        let total: i64 = client
            .query_opt(
                &format!("SELECT count(*) FROM core_documents d WHERE {conditions}"),
                &params,
            )
            .await?
            .ok_or(Error::Internal)?
            .get(0);
        let order = match filter.sort {
            Sort::LastRevision => "d.last_revision_at",
            Sort::Name => "lower(d.name)",
            Sort::PublishDate => "d.publish_date",
            Sort::Created => "d.created_at",
        };
        let direction = if filter.descending { "DESC" } else { "ASC" };
        let sql = format!(
            "SELECT {COLUMNS} FROM core_documents d WHERE {conditions} ORDER BY {order} {direction}, d.id {direction} LIMIT {page_size} OFFSET {offset}"
        );
        let entries = client
            .query(&sql, &params)
            .await?
            .iter()
            .map(from_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(Page { entries, total })
    }

    /// The document a write acts on, with explicit manage access.
    async fn editable(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        version: Option<i64>,
        capability: &str,
    ) -> WriteResult<Document> {
        let document = self
            .editable_including_deleting(on, scope, id, version, capability)
            .await?;
        if document.deleting_at.is_some() {
            return Err(WriteError::Stale);
        }
        Ok(document)
    }

    /// The same authorization as `editable`, but used only to retry a delete
    /// whose byte or metadata finalization was interrupted.
    async fn editable_including_deleting(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        version: Option<i64>,
        capability: &str,
    ) -> WriteResult<Document> {
        let client = on.client();
        authorize_proof(client, scope, capability).await?;
        let reader = reader(client, scope).await?;
        let document = select(client, scope, id, on.locks())
            .await?
            .ok_or(WriteError::NotFound)?;
        if !reader.permission(client, document.id, "manage").await? {
            return Err(if self.visible(client, scope, &reader, &document).await? {
                WriteError::Forbidden
            } else {
                WriteError::NotFound
            });
        }
        if version.is_some_and(|v| v != document.version) {
            return Err(WriteError::Stale);
        }
        Ok(document)
    }

    async fn check_record(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        record: &Reference,
    ) -> WriteResult<Reference> {
        self.records
            .check(client, scope, &record.kind, &record.id)
            .await?
            .map_err(|refusal| WriteError::RelatedRefused(refusal.message()))
    }

    async fn active_account(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        user: i64,
    ) -> Result<bool> {
        let Ok(principal) = PrincipalRef::new(PrincipalKind::User, user) else {
            return Ok(false);
        };
        let ranked = self
            .directory
            .rank(&Directory(client), scope, &[principal], &Query::default())
            .await
            .map_err(|_| Error::Internal)?;
        Ok(!ranked.named.is_empty())
    }

    /// A pick-list code, when it is new to the record. A commit takes the
    /// value's share lock; a preview only asks whether it may be offered.
    async fn check_code(
        &self,
        on: On<'_>,
        scope: &Scope,
        errors: &mut Vec<FieldError>,
        (field, list): (&'static str, &str),
        code: Option<&str>,
        parent: Option<&str>,
    ) -> Result<()> {
        let Some(code) = code else { return Ok(()) };
        match on {
            On::Commit(tx) => {
                if let Err(refusal) = self
                    .picklists
                    .check_in(tx, scope, list, code, parent)
                    .await?
                {
                    errors.push(FieldError::new(field, refusal.message()));
                }
            }
            On::Preview(client) => {
                if !self
                    .picklists
                    .contains_choice(client, scope, list, code, parent)
                    .await?
                {
                    errors.push(FieldError::new(field, "is not one of the choices"));
                }
            }
        }
        Ok(())
    }

    /// Checks `values` against what is stored (`before`, none for a new
    /// document): field shapes, every changed pick-list code and the
    /// assignee.
    async fn check_values(
        &self,
        on: On<'_>,
        scope: &Scope,
        mut errors: Vec<FieldError>,
        values: &Values,
        before: Option<&Document>,
    ) -> WriteResult<()> {
        let changed = |new: &Option<String>, old: Option<&Option<String>>| {
            new.is_some() && old.is_none_or(|old| old != new)
        };
        if values.name.chars().count() > model::NAME_MAX {
            errors.push(FieldError::new(
                "name",
                format!("must be at most {} characters", model::NAME_MAX),
            ));
        }
        if values.sub_category.is_some() && values.category.is_none() {
            errors.push(FieldError::new("sub_category", "needs a category"));
        }
        let category_changed = changed(&values.category, before.map(|b| &b.category));
        if category_changed {
            self.check_code(
                on,
                scope,
                &mut errors,
                ("category", LIST_CATEGORY),
                values.category.as_deref(),
                None,
            )
            .await?;
        }
        if category_changed || changed(&values.sub_category, before.map(|b| &b.sub_category)) {
            self.check_code(
                on,
                scope,
                &mut errors,
                ("sub_category", LIST_SUB_CATEGORY),
                values.sub_category.as_deref(),
                values.category.as_deref(),
            )
            .await?;
        }
        if changed(&values.document_type, before.map(|b| &b.document_type)) {
            self.check_code(
                on,
                scope,
                &mut errors,
                ("document_type", LIST_TYPE),
                values.document_type.as_deref(),
                None,
            )
            .await?;
        }
        if !values.status.is_empty() && before.is_none_or(|b| b.status != values.status) {
            self.check_code(
                on,
                scope,
                &mut errors,
                ("status", LIST_STATUS),
                Some(&values.status),
                None,
            )
            .await?;
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(WriteError::Invalid(errors))
        }
    }

    /// Today in the actor's own zone.
    async fn today(&self, tx: &Transaction<'_>, scope: &Scope) -> Result<Date> {
        let display = self
            .display
            .display(tx, scope, scope.current_company(), "en")
            .await?;
        Timestamp::now()
            .in_tz(&display.timezone)
            .map(|now| now.date())
            .map_err(|_| Error::Internal)
    }

    pub(crate) async fn attach(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        related: &Reference,
        artifact_id: &str,
        name: Option<&str>,
        category: Option<&str>,
    ) -> WriteResult<i64> {
        authorize_proof(tx.sql(), scope, MANAGE).await?;
        // Attached already: the same document, with this link too.
        if let Some(id) = files::attached_document(tx.sql(), scope, artifact_id).await? {
            self.link(tx, scope, id, related).await?;
            return Ok(id);
        }
        let draft = Draft {
            name: name.map(str::to_owned),
            artifact_id: Some(artifact_id.to_owned()),
            category: category.map(str::to_owned),
            related: vec![related.clone()],
            ..Draft::default()
        };
        Ok(self.create(tx, scope, &draft).await?.id)
    }

    /// Everything `create` checks, with no write.
    pub(crate) async fn prepare_create(
        &self,
        on: On<'_>,
        scope: &Scope,
        draft: &Draft,
    ) -> WriteResult<CreatePlan> {
        let client = on.client();
        authorize_proof(client, scope, MANAGE).await?;
        let company = scope.current_company().ok_or(WriteError::field(
            "company",
            "Documents are filed under your company: sign in to a company to create one",
        ))?;
        let mut errors = Vec::new();
        let name = model::text(&mut errors, "name", draft.name.as_deref(), model::NAME_MAX);
        let external_url = match draft.source {
            Source::External => {
                let url = model::https_url(&mut errors, draft.external_url.as_deref());
                if url.is_none() {
                    errors.push(FieldError::new("external_url", "is required for a link"));
                }
                url
            }
            Source::Internal => None,
        };
        let category = model::text(
            &mut errors,
            "category",
            draft.category.as_deref(),
            model::CODE_MAX,
        );
        let sub_category = model::text(
            &mut errors,
            "sub_category",
            draft.sub_category.as_deref(),
            model::CODE_MAX,
        );
        let document_type = model::text(
            &mut errors,
            "document_type",
            draft.document_type.as_deref(),
            model::CODE_MAX,
        );
        let status = model::text(
            &mut errors,
            "status",
            draft.status.as_deref(),
            model::CODE_MAX,
        );
        let change_log = model::long_text(
            &mut errors,
            "change_log",
            draft.change_log.as_deref(),
            model::CHANGE_LOG_MAX,
        )
        .unwrap_or_else(|| model::FIRST_CHANGE_LOG.to_owned());
        let file = match (draft.source, model::clean(draft.artifact_id.as_deref())) {
            (Source::Internal, Some(artifact)) => {
                match files::take(client, scope, &artifact, "artifact_id", on.locks()).await {
                    Ok(file) => Some(file),
                    Err(WriteError::Invalid(mut refused)) => {
                        errors.append(&mut refused);
                        None
                    }
                    Err(error) => return Err(error),
                }
            }
            (Source::Internal, None) => {
                errors.push(FieldError::new("artifact_id", "needs a file"));
                None
            }
            (Source::External, Some(_)) => {
                errors.push(FieldError::new("artifact_id", "a link has no file"));
                None
            }
            (Source::External, None) => None,
        };
        // The name defaults to the first file's name.
        let name = name.or_else(|| file.as_ref().map(|f| f.file_name().to_owned()));
        if name.is_none() && !errors.iter().any(|e| e.field == "artifact_id") {
            errors.push(FieldError::new("name", "is required"));
        }
        // A preview leaves an omitted status to the commit: the list's
        // default then.
        let status = match (status, on) {
            (Some(status), _) => status,
            (None, On::Commit(tx)) => self
                .picklists
                .default(tx, scope, LIST_STATUS)
                .await?
                .unwrap_or_else(|| ACTIVE.to_owned()),
            (None, On::Preview(_)) => String::new(),
        };
        // A preview leaves an omitted date to the commit: today then.
        let publish_date = match (draft.publish_date, on) {
            (Some(date), _) => Some(date),
            (None, On::Commit(tx)) => Some(self.today(tx, scope).await?),
            (None, On::Preview(_)) => None,
        };
        let values = Values {
            name: name.unwrap_or_default(),
            publish_date: publish_date.unwrap_or_default(),
            category,
            sub_category,
            document_type,
            status,
        };
        self.check_values(on, scope, errors, &values, None).await?;
        let mut related = Vec::with_capacity(draft.related.len());
        for record in &draft.related {
            let record = self.check_record(client, scope, record).await?;
            if !related.contains(&record) {
                related.push(record);
            }
        }
        Ok(CreatePlan {
            company: company.get(),
            values,
            publish_date,
            source: draft.source,
            external_url,
            change_log,
            file,
            related,
        })
    }

    pub(crate) async fn create(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        draft: &Draft,
    ) -> WriteResult<Document> {
        let plan = self
            .prepare_create(On::Commit(tx.sql()), scope, draft)
            .await?;
        let (tenant, user) = (scope.tenant(), scope.user());
        tx.mutate(move |sql| {
            Box::pin(async move {
                let CreatePlan { company, values, source, external_url, change_log, file, related, .. } = plan;
                let row = sql
                    .query_one(
                        &format!("INSERT INTO core_documents AS d (tenant_id,company_id,name,publish_date,source,external_url,category,sub_category,document_type,status,revision,last_revision_at,created_by)
                                  VALUES ($1,$2,$3,$4::text::date,$5,$6,$7,$8,$9,$10,1,now(),$11) RETURNING {COLUMNS}"),
                        &[&tenant, &company, &values.name, &values.publish_date.to_string(), &source.code(), &external_url,
                          &values.category, &values.sub_category, &values.document_type, &values.status, &user],
                    )
                    .await?;
                let document = from_row(&row)?;
                let mut changes = vec![Change { kind: RECORD.into(), id: document.id, old: json!({}), new: audit_values(&document) }];
                let access = sql
                    .query_one(
                        "INSERT INTO core_document_access(tenant_id,document_id,principal_type,principal_id,can_view,can_download,can_manage,can_share,granted_by) VALUES($1,$2,'user',$3,true,true,true,true,$3) RETURNING id",
                        &[&tenant, &document.id, &user],
                    )
                    .await?;
                changes.push(Change { kind: crate::ACCESS_RECORD.into(), id: access.get(0), old: json!({}), new: json!({"document_id":document.id,"principal_type":"user","principal_id":user,"view":true,"download":true,"manage":true,"share":true}) });
                let revision = insert_revision(sql, tenant, user, &document, 1, file.as_ref(), external_url.as_deref(), &change_log).await?;
                changes.push(Change { kind: REVISION_RECORD.into(), id: revision.id, old: json!({}), new: revision_values(&revision) });
                if let Some(file) = &file {
                    changes.push(file.attach(sql, tenant).await?);
                }
                for record in &related {
                    let link = insert_link(sql, tenant, user, document.id, record).await?.ok_or(Error::Conflict)?;
                    changes.push(Change { kind: LINK_RECORD.into(), id: link.id, old: json!({}), new: link_values(document.id, &link) });
                }
                Ok((document, changes))
            })
        })
        .await
        .map_err(write_error)
    }

    /// Everything `update` checks, with no write: the stored document and
    /// the values it would hold.
    pub(crate) async fn prepare_update(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Patch,
    ) -> WriteResult<(Document, Document)> {
        let before = self.editable(on, scope, id, Some(version), MANAGE).await?;
        if patch.is_empty() {
            return Ok((before.clone(), before));
        }
        let mut errors = Vec::new();
        let optional = |errors: &mut Vec<FieldError>,
                        field,
                        value: &Option<Option<String>>,
                        old: &Option<String>,
                        max| match value {
            None => old.clone(),
            Some(value) => model::text(errors, field, value.as_deref(), max),
        };
        let name = match &patch.name {
            None => before.name.clone(),
            Some(name) => model::text(&mut errors, "name", Some(name), model::NAME_MAX)
                .unwrap_or_else(|| {
                    errors.push(FieldError::new("name", "is required"));
                    String::new()
                }),
        };
        let category = optional(
            &mut errors,
            "category",
            &patch.category,
            &before.category,
            model::CODE_MAX,
        );
        // A new category clears a sub-category the edit does not name.
        let sub_category = if patch.sub_category.is_none() && category != before.category {
            None
        } else {
            optional(
                &mut errors,
                "sub_category",
                &patch.sub_category,
                &before.sub_category,
                model::CODE_MAX,
            )
        };
        let document_type = optional(
            &mut errors,
            "document_type",
            &patch.document_type,
            &before.document_type,
            model::CODE_MAX,
        );
        let status = match &patch.status {
            None => before.status.clone(),
            Some(status) => model::text(&mut errors, "status", Some(status), model::CODE_MAX)
                .unwrap_or_else(|| {
                    errors.push(FieldError::new("status", "is required"));
                    String::new()
                }),
        };
        let values = Values {
            name,
            publish_date: patch.publish_date.unwrap_or(before.publish_date),
            category,
            sub_category,
            document_type,
            status,
        };
        self.check_values(on, scope, errors, &values, Some(&before))
            .await?;
        let after = Document {
            name: values.name,
            publish_date: values.publish_date,
            category: values.category,
            sub_category: values.sub_category,
            document_type: values.document_type,
            status: values.status,
            ..before.clone()
        };
        Ok((before, after))
    }

    pub(crate) async fn update(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Patch,
    ) -> WriteResult<Document> {
        let (before, after) = self
            .prepare_update(On::Commit(tx.sql()), scope, id, version, patch)
            .await?;
        if patch.is_empty() {
            return Ok(before);
        }
        let tenant = scope.tenant();
        tx.mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_opt(
                        &format!("UPDATE core_documents d SET name=$4,publish_date=$5::text::date,category=$6,sub_category=$7,document_type=$8,status=$9,version=d.version+1,updated_at=now()
                                  WHERE d.tenant_id=$1 AND d.id=$2 AND d.version=$3 RETURNING {COLUMNS}"),
                        &[&tenant, &before.id, &before.version, &after.name, &after.publish_date.to_string(), &after.category,
                          &after.sub_category, &after.document_type, &after.status],
                    )
                    .await?
                    .ok_or(Error::Conflict)?;
                let document = from_row(&row)?;
                let change = Change { kind: RECORD.into(), id: document.id, old: audit_values(&before), new: audit_values(&document) };
                Ok((document, vec![change]))
            })
        })
        .await
        .map_err(write_error)
    }

    /// Everything `add_revision` checks, with no write.
    pub(crate) async fn prepare_revision(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        input: &RevisionInput,
    ) -> WriteResult<RevisionPlan> {
        let before = self.editable(on, scope, id, Some(version), MANAGE).await?;
        let mut errors = Vec::new();
        let number = input.revision.unwrap_or(before.revision.saturating_add(1));
        if number <= before.revision {
            errors.push(FieldError::new(
                "revision",
                format!("must be above the latest revision, {}", before.revision),
            ));
        }
        let change_log = model::long_text(
            &mut errors,
            "change_log",
            Some(&input.change_log),
            model::CHANGE_LOG_MAX,
        );
        if change_log.is_none() {
            errors.push(FieldError::new("change_log", "is required"));
        }
        let mut external_url = None;
        let mut file = None;
        match before.source {
            Source::Internal => {
                if model::clean(input.external_url.as_deref()).is_some() {
                    errors.push(FieldError::new(
                        "external_url",
                        "this document keeps files, not links",
                    ));
                }
                match model::clean(input.artifact_id.as_deref()) {
                    None => errors.push(FieldError::new("artifact_id", "needs a file")),
                    Some(artifact) => {
                        match files::take(on.client(), scope, &artifact, "artifact_id", on.locks())
                            .await
                        {
                            Ok(taken) => file = Some(taken),
                            Err(WriteError::Invalid(mut refused)) => errors.append(&mut refused),
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
            Source::External => {
                if model::clean(input.artifact_id.as_deref()).is_some() {
                    errors.push(FieldError::new(
                        "artifact_id",
                        "this document keeps links, not files",
                    ));
                }
                external_url = model::https_url(&mut errors, input.external_url.as_deref());
                if external_url.is_none() {
                    errors.push(FieldError::new("external_url", "is required for a link"));
                }
            }
        }
        if !errors.is_empty() {
            return Err(WriteError::Invalid(errors));
        }
        Ok(RevisionPlan {
            document: before,
            revision: number,
            change_log: change_log.unwrap_or_default(),
            external_url,
            file,
        })
    }

    pub(crate) async fn add_revision(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        input: &RevisionInput,
    ) -> WriteResult<Revision> {
        let plan = self
            .prepare_revision(On::Commit(tx.sql()), scope, id, version, input)
            .await?;
        let (tenant, user) = (scope.tenant(), scope.user());
        tx.mutate(move |sql| {
            Box::pin(async move {
                let RevisionPlan { document: before, revision: number, change_log, external_url, file } = plan;
                let row = sql
                    .query_opt(
                        &format!("UPDATE core_documents d SET revision=$4,last_revision_at=now(),external_url=COALESCE($5,d.external_url),version=d.version+1,updated_at=now()
                                  WHERE d.tenant_id=$1 AND d.id=$2 AND d.version=$3 RETURNING {COLUMNS}"),
                        &[&tenant, &before.id, &before.version, &number, &external_url],
                    )
                    .await?
                    .ok_or(Error::Conflict)?;
                let document = from_row(&row)?;
                let revision = insert_revision(sql, tenant, user, &document, number, file.as_ref(), external_url.as_deref(), &change_log).await?;
                let mut changes = vec![
                    Change { kind: RECORD.into(), id: document.id, old: audit_values(&before), new: audit_values(&document) },
                    Change { kind: REVISION_RECORD.into(), id: revision.id, old: json!({}), new: revision_values(&revision) },
                ];
                if let Some(file) = &file {
                    changes.push(file.attach(sql, tenant).await?);
                }
                Ok((revision, changes))
            })
        })
        .await
        .map_err(write_error)
    }

    /// Everything `link` checks, with no write: the document, the record in
    /// its one spelling, and the link when it exists already.
    pub(crate) async fn prepare_link(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        record: &Reference,
    ) -> WriteResult<(Document, Reference, Option<Link>)> {
        let document = self.editable(on, scope, id, None, MANAGE).await?;
        let record = self.check_record(on.client(), scope, record).await?;
        if record.kind == RECORD && record.id == document.id.to_string() {
            return Err(WriteError::RelatedRefused("is this document itself"));
        }
        let existing = find_link(on.client(), scope.tenant(), id, &record).await?;
        Ok((document, record, existing))
    }

    pub(crate) async fn link(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        record: &Reference,
    ) -> WriteResult<Link> {
        let (_, record, existing) = self
            .prepare_link(On::Commit(tx.sql()), scope, id, record)
            .await?;
        if let Some(link) = existing {
            return Ok(link);
        }
        let (tenant, user) = (scope.tenant(), scope.user());
        tx.mutate(move |sql| {
            Box::pin(async move {
                let link = insert_link(sql, tenant, user, id, &record)
                    .await?
                    .ok_or(Error::Conflict)?;
                let change = Change {
                    kind: LINK_RECORD.into(),
                    id: link.id,
                    old: json!({}),
                    new: link_values(id, &link),
                };
                Ok((link, vec![change]))
            })
        })
        .await
        .map_err(write_error)
    }

    /// Everything `unlink` checks, with no write: the document and the link.
    pub(crate) async fn prepare_unlink(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        record: &Reference,
    ) -> WriteResult<(Document, Link)> {
        let document = self.editable(on, scope, id, None, MANAGE).await?;
        let link = find_link(on.client(), scope.tenant(), id, record)
            .await?
            .ok_or(WriteError::NotFound)?;
        Ok((document, link))
    }

    pub(crate) async fn unlink(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        record: &Reference,
    ) -> WriteResult<()> {
        let (_, link) = self
            .prepare_unlink(On::Commit(tx.sql()), scope, id, record)
            .await?;
        let tenant = scope.tenant();
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "DELETE FROM core_document_links WHERE tenant_id=$1 AND id=$2",
                    &[&tenant, &link.id],
                )
                .await?;
                let change = Change {
                    kind: LINK_RECORD.into(),
                    id: link.id,
                    old: link_values(id, &link),
                    new: json!({}),
                };
                Ok(((), vec![change]))
            })
        })
        .await
        .map_err(write_error)
    }

    /// Everything `delete` checks, with no write: the document.
    pub(crate) async fn prepare_delete(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<Document> {
        let document = self
            .editable_including_deleting(on, scope, id, Some(version), MANAGE)
            .await?;
        Ok(document)
    }

    pub(crate) async fn delete(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<Deleted> {
        let before = self
            .prepare_delete(On::Commit(tx.sql()), scope, id, version)
            .await?;
        let tenant = scope.tenant();
        tx.mutate(move |sql| {
            Box::pin(async move {
                let artifact_ids: Vec<String> = sql.query(
                    "SELECT artifact_id FROM core_document_revisions WHERE tenant_id=$1 AND document_id=$2 AND artifact_id IS NOT NULL ORDER BY revision",
                    &[&tenant, &id],
                ).await?.into_iter().map(|row| row.get(0)).collect();
                if before.deleting_at.is_some() {
                    return Ok((Deleted { files: artifact_ids }, Vec::new()));
                }
                let row = sql.query_one(
                    &format!("UPDATE core_documents d SET deleting_at=now(),version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 AND version=$3 RETURNING {COLUMNS}"),
                    &[&tenant, &id, &before.version],
                ).await?;
                let after = from_row(&row)?;
                let change = Change { kind: RECORD.into(), id, old: audit_values(&before), new: audit_values(&after) };
                Ok((Deleted { files: artifact_ids }, vec![change]))
            })
        })
        .await
        .map_err(write_error)
    }
}

/// A losing concurrent write is stale, never a database error.
fn write_error(error: Error) -> WriteError {
    match error {
        Error::Conflict => WriteError::Stale,
        error => WriteError::Access(error),
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert_revision(
    sql: &Transaction<'_>,
    tenant: i64,
    user: i64,
    document: &Document,
    number: i32,
    file: Option<&TakenFile>,
    external_url: Option<&str>,
    change_log: &str,
) -> Result<Revision> {
    let row = sql
        .query_one(
            &format!("INSERT INTO core_document_revisions AS r (tenant_id,document_id,source,revision,name,artifact_id,file_name,content_type,byte_size,sha256,external_url,change_log,scan_status,created_by)
                      VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) RETURNING {REVISION_COLUMNS}"),
            &[&tenant, &document.id, &document.source.code(), &number, &document.name,
              &file.map(TakenFile::artifact_id), &file.map(TakenFile::file_name), &file.map(TakenFile::content_type),
              &file.map(TakenFile::byte_size), &file.map(TakenFile::sha256), &external_url, &change_log,
              &if file.is_some() { "clean" } else { "not_scanned" }, &user],
        )
        .await?;
    revision_from_row(&row)
}

/// Inserts a link; `None` when it already exists.
async fn insert_link(
    sql: &Transaction<'_>,
    tenant: i64,
    user: i64,
    document: i64,
    record: &Reference,
) -> Result<Option<Link>> {
    sql.query_opt(
        &format!("INSERT INTO core_document_links AS l (tenant_id,document_id,record_kind,record_id,linked_by) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (tenant_id,document_id,record_kind,record_id) DO NOTHING RETURNING {LINK_COLUMNS}"),
        &[&tenant, &document, &record.kind, &record.id, &user],
    )
    .await?
    .map(|row| link_from_row(&row))
    .transpose()
}

async fn find_link(
    client: &dyn ReadClient,
    tenant: i64,
    document: i64,
    record: &Reference,
) -> Result<Option<Link>> {
    client
        .query_opt(
            &format!("SELECT {LINK_COLUMNS} FROM core_document_links l WHERE l.tenant_id=$1 AND l.document_id=$2 AND l.record_kind=$3 AND l.record_id=$4"),
            &[&tenant, &document, &record.kind, &record.id],
        )
        .await?
        .map(|row| link_from_row(&row))
        .transpose()
}
