//! `core_documents`, `core_document_revisions`, `core_document_links` and
//! `core_document_marks` SQL. Every statement binds the tenant; every write
//! goes through the caller's `MutationTransaction` and returns its
//! `Change`s. Checks run before a write, so a refusal never poisons the
//! caller's transaction.
use crate::custom;
use crate::files::{self, TakenFile};
use crate::model::{self, Custom, Draft, FieldError, Patch, RevisionInput, ScanStatus, Source};
use crate::{
    contributions::ACTIVE, AccessGrant, Deleted, Document, Filter, Link, Page, Reference, Revision,
    ShareInput, Sort, WriteError, WriteResult, LINK_RECORD, LIST_CATEGORY, LIST_STATUS,
    LIST_SUB_CATEGORY, LIST_TYPE, MANAGE, RECORD, REVISION_RECORD, VIEW,
};
use bilibi_activity::Activities;
use bilibi_authz::authorize_proof;
use bilibi_custom_fields::CustomFields;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_datetime::DisplayPolicy;
use bilibi_notifications::NotificationBoundary;
use bilibi_picklists::PickLists;
use bilibi_principal_directory::{
    DirectoryClient, DirectoryFuture, PrincipalDirectory, PrincipalKind, PrincipalRef, Query,
};
use bilibi_records::Records;
use bilibi_settings::Settings;
use bilibi_tenancy::{ProofClient, ReadClient, Scope};
use jiff::{civil::Date, Timestamp};
use serde_json::{json, Value};
use std::time::SystemTime;
use tokio_postgres::{types::ToSql, Row, Transaction};

pub(crate) const COLUMNS: &str = "d.id,d.company_id,d.name,d.publish_date::text AS publish_date,d.source,d.external_url,d.category,d.sub_category,d.document_type,d.status,d.expires_on::text AS expires_on,d.expiry_activity_id,d.expiry_user_id,d.custom_fields,d.sync_id,d.related_document_id,d.relation,d.revision,d.last_revision_at,d.created_by,d.created_at,d.updated_at,d.deleting_at,d.version";
pub(crate) const REVISION_COLUMNS: &str = "r.id,r.document_id,r.revision,r.name,r.artifact_id,r.file_name,r.content_type,r.byte_size,r.sha256,r.external_url,r.change_log,r.label,r.replaces_revision_id,r.scan_status,r.created_by,r.created_at";
pub(crate) const LINK_COLUMNS: &str = "l.id,l.record_kind,l.record_id,l.linked_by,l.linked_at";

fn time(value: SystemTime) -> Result<Timestamp> {
    Timestamp::try_from(value).map_err(|_| Error::Internal)
}
fn date(value: Option<&str>) -> Result<Option<Date>> {
    value
        .map(|d| d.parse::<Date>().map_err(|_| Error::Internal))
        .transpose()
}

pub(crate) fn from_row(row: &Row) -> Result<Document> {
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
        expires_on: date(row.get("expires_on"))?,
        expiry_activity_id: row.get("expiry_activity_id"),
        expiry_user_id: row.get("expiry_user_id"),
        custom: row
            .get::<_, Value>("custom_fields")
            .as_object()
            .cloned()
            .ok_or(Error::Internal)?,
        sync_id: row.get("sync_id"),
        related_document_id: row.get("related_document_id"),
        relation: row.get("relation"),
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
pub(crate) fn revision_from_row(row: &Row) -> Result<Revision> {
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
        label: row.get("label"),
        replaces_revision_id: row.get("replaces_revision_id"),
        scan_status: ScanStatus::parse(row.get("scan_status")).ok_or(Error::Internal)?,
        created_by: row.get("created_by"),
        created_at: time(row.get("created_at"))?,
    })
}
fn grant(row: &Row) -> Result<AccessGrant> {
    Ok(AccessGrant {
        id: row.get(0),
        user_id: row.get(1),
        can_view: row.get(2),
        can_download: row.get(3),
        can_manage: row.get(4),
        can_share: row.get(5),
        expires_at: row.get::<_, Option<SystemTime>>(6).map(time).transpose()?,
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
pub(crate) fn audit_values(d: &Document) -> Value {
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
        "expires_on": d.expires_on.map(|d| d.to_string()),
        "expiry_activity_id": d.expiry_activity_id,
        "expiry_user_id": d.expiry_user_id,
        "custom_fields": d.custom,
        "sync_id": d.sync_id,
        "related_document_id": d.related_document_id,
        "relation": d.relation,
        "revision": d.revision,
        "deleting": d.deleting_at.is_some(),
        "version": d.version,
    })
}
pub(crate) fn revision_values(r: &Revision) -> Value {
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
        "label": r.label,
        "replaces_revision_id": r.replaces_revision_id,
        "scan_status": r.scan_status.code(),
    })
}
pub(crate) fn link_values(document: i64, l: &Link) -> Value {
    json!({"document_id": document, "record": {"kind": l.record.kind, "id": l.record.id}})
}

/// Who is reading, and how far their reach goes.
pub(crate) struct Reader {
    pub(crate) tenant: i64,
    pub(crate) user: i64,
}
pub(crate) async fn reader(client: &dyn ProofClient, scope: &Scope) -> Result<Reader> {
    authorize_proof(client, scope, VIEW).await?;
    Ok(Reader {
        tenant: scope.tenant(),
        user: scope.user(),
    })
}
impl Reader {
    pub(crate) async fn permission(
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
            "SELECT EXISTS (SELECT 1 FROM core_document_access a WHERE a.tenant_id=$1 AND a.document_id=$2 AND a.{column} AND (a.expires_at IS NULL OR a.expires_at > now()) AND a.user_id=$3)"
        );
        Ok(client
            .query_opt(&sql, &[&self.tenant, &document, &self.user])
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
pub(crate) async fn select(
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
pub(crate) struct Directory<'c>(pub(crate) &'c dyn ReadClient);
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
    pub(crate) fn locks(self) -> bool {
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
    pub revision_label: Option<String>,
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
            expires_on: self.values.expires_on,
            custom: self.values.custom.clone(),
            sync_id: self.values.sync_id.clone(),
            related_document_id: self.values.related_document_id,
            relation: self.values.relation.clone(),
            revision_label: self.revision_label.clone(),
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
    pub label: Option<String>,
    pub replaces_revision_id: Option<i64>,
}

/// The values a write stores, checked.
struct Values {
    name: String,
    publish_date: Date,
    category: Option<String>,
    sub_category: Option<String>,
    document_type: Option<String>,
    status: String,
    expires_on: Option<Date>,
    /// The custom values the document will hold after the write.
    custom: Custom,
    sync_id: Option<String>,
    related_document_id: Option<i64>,
    relation: Option<String>,
}

/// What a write checks against the database: records, pick-lists, people
/// and the reader's zone.
pub(crate) struct Store<'s> {
    pub records: &'s Records,
    pub picklists: &'s PickLists,
    pub display: &'s DisplayPolicy,
    pub directory: &'s PrincipalDirectory,
    pub notifications: &'s NotificationBoundary,
    pub settings: &'s Settings,
    pub custom: &'s CustomFields,
    pub activities: &'s Activities,
    pub artifacts: &'s bilibi_artifacts::Artifacts,
    pub queue: &'s bilibi_queue::Queue,
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
            "SELECT id,user_id,can_view,can_download,can_manage,can_share,expires_at FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 ORDER BY user_id",
            &[&scope.tenant(), &id],
        ).await?.into_iter().map(|row| grant(&row)).collect::<Result<Vec<_>>>()?)
    }

    /// Checks a share and returns when the grant ends (the end of `until`,
    /// UTC) and the document it is on.
    async fn check_share(
        &self,
        on: On<'_>,
        scope: &Scope,
        id: i64,
        input: &ShareInput,
    ) -> WriteResult<(Document, Option<Timestamp>)> {
        let document = self.editable(on, scope, id, None, MANAGE).await?;
        let client = on.client();
        if !permission(client, scope, id, "share").await? {
            return Err(WriteError::Forbidden);
        }
        if !(input.user_id > 0 && self.active_account(client, scope, input.user_id).await?) {
            return Err(WriteError::field(
                "user_id",
                "is not an active account you can share with",
            ));
        }
        let expires_at = match input.until {
            None => None,
            Some(_) if input.user_id == scope.user() => {
                return Err(WriteError::field(
                    "until",
                    "cannot end your own access to a document",
                ))
            }
            Some(day) => {
                let end = day
                    .checked_add(jiff::Span::new().days(1))
                    .and_then(|next| next.at(0, 0, 0, 0).to_zoned(jiff::tz::TimeZone::UTC))
                    .map(|zoned| zoned.timestamp())
                    .map_err(|_| WriteError::field("until", "is not a usable date"))?;
                if end <= Timestamp::now() {
                    return Err(WriteError::field("until", "must be today or later"));
                }
                Some(end)
            }
        };
        Ok((document, expires_at))
    }

    pub(crate) async fn preview_share(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        input: &ShareInput,
    ) -> WriteResult<AccessGrant> {
        let (_, expires_at) = self
            .check_share(On::Preview(client), scope, id, input)
            .await?;
        Ok(AccessGrant {
            id: 0,
            user_id: input.user_id,
            can_view: true,
            can_download: input.can_download,
            can_manage: input.can_manage,
            can_share: input.can_share,
            expires_at,
        })
    }

    /// Returns the grant and the document it is on.
    pub(crate) async fn share(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        input: &ShareInput,
    ) -> WriteResult<(AccessGrant, Document)> {
        let (document, expires_at) = self
            .check_share(On::Commit(tx.sql()), scope, id, input)
            .await?;
        let (tenant, actor) = (scope.tenant(), scope.user());
        let input = *input;
        let grant = tx.mutate(move |sql| Box::pin(async move {
            let before = sql.query_opt(
                "SELECT id,can_view,can_download,can_manage,can_share,expires_at FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND user_id=$3 FOR UPDATE",
                &[&tenant,&id,&input.user_id],
            ).await?;
            let row = sql.query_one(
                "INSERT INTO core_document_access(tenant_id,document_id,user_id,can_view,can_download,can_manage,can_share,expires_at,granted_by) VALUES($1,$2,$3,true,$4,$5,$6,$7,$8) ON CONFLICT(tenant_id,document_id,user_id) DO UPDATE SET can_view=true,can_download=EXCLUDED.can_download,can_manage=EXCLUDED.can_manage,can_share=EXCLUDED.can_share,expires_at=EXCLUDED.expires_at,granted_by=EXCLUDED.granted_by,granted_at=now() RETURNING id,user_id,can_view,can_download,can_manage,can_share,expires_at",
                &[&tenant,&id,&input.user_id,&input.can_download,&input.can_manage,&input.can_share,&expires_at.map(SystemTime::from),&actor],
            ).await?;
            let grant = grant(&row)?;
            let old = before.map(|row| json!({"document_id":id,"user_id":input.user_id,"view":row.get::<_,bool>(1),"download":row.get::<_,bool>(2),"manage":row.get::<_,bool>(3),"share":row.get::<_,bool>(4),"expires_at":row.get::<_,Option<SystemTime>>(5).and_then(|t| time(t).ok()).map(|t| t.to_string())})).unwrap_or_else(|| json!({}));
            let change = Change { kind: crate::ACCESS_RECORD.into(), id: grant.id, old, new: json!({"document_id":id,"user_id":grant.user_id,"view":grant.can_view,"download":grant.can_download,"manage":grant.can_manage,"share":grant.can_share,"expires_at":grant.expires_at.map(|t| t.to_string())}) };
            Ok((grant, vec![change]))
        })).await.map_err(write_error)?;
        Ok((grant, document))
    }

    pub(crate) async fn unshare(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        grant: i64,
    ) -> WriteResult<()> {
        let document = self
            .editable(On::Commit(tx.sql()), scope, id, None, MANAGE)
            .await?;
        if !permission(tx.sql(), scope, id, "share").await? {
            return Err(WriteError::Forbidden);
        }
        // The grantee's reminder task goes before their access does.
        let grantee: i64 = tx
            .sql()
            .query_opt(
                "SELECT user_id FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND id=$3",
                &[&scope.tenant(), &id, &grant],
            )
            .await
            .map_err(Error::from)?
            .ok_or(WriteError::NotFound)?
            .get(0);
        if grantee != scope.user() {
            self.drop_reminder_of(tx, scope, &document, grantee).await?;
        }
        let tenant = scope.tenant();
        let actor = scope.user();
        tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_opt(
                "SELECT user_id,can_view,can_download,can_manage,can_share FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND id=$3 FOR UPDATE",
                &[&tenant,&id,&grant],
            ).await?.ok_or(Error::NotFound)?;
            if row.get::<_,i64>(0) == actor {
                return Err(Error::Forbidden);
            }
            sql.execute("DELETE FROM core_document_access WHERE tenant_id=$1 AND id=$2", &[&tenant,&grant]).await?;
            let change = Change { kind: crate::ACCESS_RECORD.into(), id: grant, old: json!({"document_id":id,"user_id":row.get::<_,i64>(0),"view":row.get::<_,bool>(1),"download":row.get::<_,bool>(2),"manage":row.get::<_,bool>(3),"share":row.get::<_,bool>(4)}), new: json!({}) };
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
            "SELECT id,user_id,can_view,can_download,can_manage,can_share,expires_at FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND id=$3",
            &[&scope.tenant(),&id,&grant],
        ).await?.ok_or(WriteError::NotFound)?;
        let result = self::grant(&row)?;
        if result.user_id == scope.user() {
            return Err(WriteError::Forbidden);
        }
        Ok(result)
    }

    pub(crate) async fn list(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        filter: &Filter,
        today: Date,
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
             AND EXISTS (SELECT 1 FROM core_document_access a WHERE a.tenant_id=d.tenant_id AND a.document_id=d.id AND a.can_view AND (a.expires_at IS NULL OR a.expires_at > now()) AND a.user_id=$2)
             AND ($3::text IS NULL OR EXISTS (SELECT 1 FROM core_document_links l WHERE l.tenant_id=d.tenant_id AND l.document_id=d.id AND l.record_kind=$3 AND l.record_id=$4))
             AND ($5 = '' OR d.name ILIKE $6)
             AND ($7::text IS NULL OR d.category = $7)
             AND ($8::text IS NULL OR d.document_type = $8)
             AND ($9::text IS NULL OR d.status = $9)
             AND ($11::text IS NULL OR ($11 = 'expired' AND d.expires_on < $12::text::date) OR ($11 = 'soon' AND d.expires_on >= $12::text::date AND d.expires_on <= $12::text::date + 30))
             AND ($10::text IS NULL OR EXISTS (SELECT 1 FROM core_document_marks m WHERE m.tenant_id=d.tenant_id AND m.document_id=d.id AND m.user_id=$2 AND CASE WHEN $10 = 'favourites' THEN m.favourite ELSE m.following END))";
        let marked = filter.marked.map(|m| m.code());
        let expiry = filter.expiry.map(|e| e.code());
        let today = today.to_string();
        let params: [&(dyn ToSql + Sync); 12] = [
            &reader.tenant,
            &reader.user,
            &related_kind,
            &related_id,
            &search,
            &pattern,
            &filter.category,
            &filter.document_type,
            &filter.status,
            &marked,
            &expiry,
            &today,
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
    pub(crate) async fn editable(
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

    pub(crate) async fn check_record(
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

    pub(crate) async fn active_account(
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
        if let Some(sync_id) = &values.sync_id {
            let duplicate = on.client().query_opt(
                "SELECT id FROM core_documents WHERE tenant_id=$1 AND sync_id=$2 AND ($3::bigint IS NULL OR id<>$3)",
                &[&scope.tenant(), sync_id, &before.map(|d| d.id)],
            ).await?;
            if duplicate.is_some() {
                errors.push(FieldError::new(
                    "sync_id",
                    "is already used by another document",
                ));
            }
        }
        match (values.related_document_id, values.relation.as_deref()) {
            (None, None) => {}
            (Some(target), Some("supersedes" | "replaces")) => {
                if before.is_some_and(|d| d.id == target) {
                    errors.push(FieldError::new(
                        "related_document_id",
                        "cannot refer to this document",
                    ));
                } else if before.is_none_or(|d| d.related_document_id != Some(target)) {
                    match self.get(on.client(), scope, target).await {
                        Ok(_) => {}
                        Err(Error::NotFound | Error::Forbidden) => errors.push(FieldError::new(
                            "related_document_id",
                            "must name a document you may view",
                        )),
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            _ => errors.push(FieldError::new(
                "relation",
                "needs a related document and a relation of supersedes or replaces",
            )),
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
        let sync_id = model::text(
            &mut errors,
            "sync_id",
            draft.sync_id.as_deref(),
            model::SYNC_ID_MAX,
        );
        let relation = model::text(&mut errors, "relation", draft.relation.as_deref(), 10);
        let revision_label = model::text(
            &mut errors,
            "revision_label",
            draft.revision_label.as_deref(),
            model::REVISION_LABEL_MAX,
        );
        let change_log = model::long_text(
            &mut errors,
            "change_log",
            draft.change_log.as_deref(),
            model::CHANGE_LOG_MAX,
        )
        .unwrap_or_else(|| model::FIRST_CHANGE_LOG.to_owned());
        let file = match (
            draft.source,
            bilibi_ui::params::blank_to_none(draft.artifact_id.as_deref()),
        ) {
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
        let stored = self
            .check_custom(on, scope, &draft.custom, &Custom::new())
            .await;
        let values = Values {
            name: name.unwrap_or_default(),
            publish_date: publish_date.unwrap_or_default(),
            category,
            sub_category,
            document_type,
            status,
            expires_on: draft.expires_on,
            custom: match &stored {
                Ok(stored) => custom::merge(&Custom::new(), stored),
                Err(_) => Custom::new(),
            },
            sync_id,
            related_document_id: draft.related_document_id,
            relation,
        };
        if let Err(WriteError::Invalid(mut refused)) = stored {
            errors.append(&mut refused);
        } else {
            stored?;
        }
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
            revision_label,
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
        let document = tx.mutate(move |sql| {
            Box::pin(async move {
                let CreatePlan { company, values, source, external_url, change_log, file, related, revision_label, .. } = plan;
                let row = sql
                    .query_one(
                        &format!("INSERT INTO core_documents AS d (tenant_id,company_id,name,publish_date,source,external_url,category,sub_category,document_type,status,expires_on,custom_fields,sync_id,related_document_id,relation,revision,last_revision_at,created_by)
                                  VALUES ($1,$2,$3,$4::text::date,$5,$6,$7,$8,$9,$10,$12::text::date,$13,$14,$15,$16,1,now(),$11) RETURNING {COLUMNS}"),
                        &[&tenant, &company, &values.name, &values.publish_date.to_string(), &source.code(), &external_url,
                          &values.category, &values.sub_category, &values.document_type, &values.status, &user,
                          &values.expires_on.map(|d| d.to_string()), &Value::Object(values.custom.clone()), &values.sync_id, &values.related_document_id, &values.relation],
                    )
                    .await?;
                let document = from_row(&row)?;
                let mut changes = vec![Change { kind: RECORD.into(), id: document.id, old: json!({}), new: audit_values(&document) }];
                let access = sql
                    .query_one(
                        "INSERT INTO core_document_access(tenant_id,document_id,user_id,can_view,can_download,can_manage,can_share,granted_by) VALUES($1,$2,$3,true,true,true,true,$3) RETURNING id",
                        &[&tenant, &document.id, &user],
                    )
                    .await?;
                changes.push(Change { kind: crate::ACCESS_RECORD.into(), id: access.get(0), old: json!({}), new: json!({"document_id":document.id,"user_id":user,"view":true,"download":true,"manage":true,"share":true}) });
                let revision = insert_revision(sql, tenant, user, &document, 1, file.as_ref(), external_url.as_deref(), &change_log, revision_label.as_deref(), None).await?;
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
        .map_err(write_error)?;
        let document = if document.expires_on.is_some() {
            self.refresh_reminder(tx, scope, &document, None).await?
        } else {
            document
        };
        Ok(document)
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
        let stored = self
            .check_custom(on, scope, &patch.custom, &before.custom)
            .await;
        let values = Values {
            name,
            publish_date: patch.publish_date.unwrap_or(before.publish_date),
            category,
            sub_category,
            document_type,
            status,
            expires_on: patch.expires_on.unwrap_or(before.expires_on),
            custom: match &stored {
                Ok(stored) => custom::merge(&before.custom, stored),
                Err(_) => before.custom.clone(),
            },
            sync_id: match &patch.sync_id {
                None => before.sync_id.clone(),
                Some(v) => model::text(&mut errors, "sync_id", v.as_deref(), model::SYNC_ID_MAX),
            },
            related_document_id: patch
                .related_document_id
                .unwrap_or(before.related_document_id),
            relation: match &patch.relation {
                None => before.relation.clone(),
                Some(v) => model::text(&mut errors, "relation", v.as_deref(), 10),
            },
        };
        if let Err(WriteError::Invalid(mut refused)) = stored {
            errors.append(&mut refused);
        } else {
            stored?;
        }
        self.check_values(on, scope, errors, &values, Some(&before))
            .await?;
        let after = Document {
            name: values.name,
            publish_date: values.publish_date,
            category: values.category,
            sub_category: values.sub_category,
            document_type: values.document_type,
            status: values.status,
            expires_on: values.expires_on,
            custom: values.custom,
            sync_id: values.sync_id,
            related_document_id: values.related_document_id,
            relation: values.relation,
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
        let (before_expiry, before_name, before_activity) = (
            before.expires_on,
            before.name.clone(),
            before.expiry_activity_id,
        );
        let updated = tx.mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_opt(
                        &format!("UPDATE core_documents d SET name=$4,publish_date=$5::text::date,category=$6,sub_category=$7,document_type=$8,status=$9,expires_on=$10::text::date,custom_fields=$11,sync_id=$12,related_document_id=$13,relation=$14,version=d.version+1,updated_at=now()
                                  WHERE d.tenant_id=$1 AND d.id=$2 AND d.version=$3 RETURNING {COLUMNS}"),
                        &[&tenant, &before.id, &before.version, &after.name, &after.publish_date.to_string(), &after.category,
                          &after.sub_category, &after.document_type, &after.status,
                          &after.expires_on.map(|d| d.to_string()), &Value::Object(after.custom.clone()), &after.sync_id, &after.related_document_id, &after.relation],
                    )
                    .await?
                    .ok_or(Error::Conflict)?;
                let document = from_row(&row)?;
                let change = Change { kind: RECORD.into(), id: document.id, old: audit_values(&before), new: audit_values(&document) };
                Ok((document, vec![change]))
            })
        })
        .await
        .map_err(write_error)?;
        if updated.expires_on != before_expiry
            || (updated.expires_on.is_some() && updated.name != before_name)
        {
            return self
                .refresh_reminder(tx, scope, &updated, before_activity)
                .await;
        }
        Ok(updated)
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
                if bilibi_ui::params::blank_to_none(input.external_url.as_deref()).is_some() {
                    errors.push(FieldError::new(
                        "external_url",
                        "this document keeps files, not links",
                    ));
                }
                match bilibi_ui::params::blank_to_none(input.artifact_id.as_deref()) {
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
                if bilibi_ui::params::blank_to_none(input.artifact_id.as_deref()).is_some() {
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
        let label = model::text(
            &mut errors,
            "label",
            input.label.as_deref(),
            model::REVISION_LABEL_MAX,
        );
        if let Some(replaces) = input.replaces_revision_id {
            let found = on.client().query_opt(
                "SELECT 1 FROM core_document_revisions WHERE tenant_id=$1 AND document_id=$2 AND id=$3",
                &[&scope.tenant(), &id, &replaces],
            ).await?;
            if found.is_none() {
                errors.push(FieldError::new(
                    "replaces_revision_id",
                    "must name an earlier revision of this document",
                ));
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
            label,
            replaces_revision_id: input.replaces_revision_id,
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
        let revision = tx.mutate(move |sql| {
            Box::pin(async move {
                let RevisionPlan { document: before, revision: number, change_log, external_url, file, label, replaces_revision_id } = plan;
                let row = sql
                    .query_opt(
                        &format!("UPDATE core_documents d SET revision=$4,last_revision_at=now(),external_url=COALESCE($5,d.external_url),version=d.version+1,updated_at=now()
                                  WHERE d.tenant_id=$1 AND d.id=$2 AND d.version=$3 RETURNING {COLUMNS}"),
                        &[&tenant, &before.id, &before.version, &number, &external_url],
                    )
                    .await?
                    .ok_or(Error::Conflict)?;
                let document = from_row(&row)?;
                let revision = insert_revision(sql, tenant, user, &document, number, file.as_ref(), external_url.as_deref(), &change_log, label.as_deref(), replaces_revision_id).await?;
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
        .map_err(write_error)?;
        self.notify_followers(tx, scope, &revision).await?;
        Ok(revision)
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
        if let (None, Some(task)) = (before.deleting_at, before.expiry_activity_id) {
            self.remove_reminder(tx, scope, id, task).await?;
        }
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
pub(crate) fn write_error(error: Error) -> WriteError {
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
    label: Option<&str>,
    replaces_revision_id: Option<i64>,
) -> Result<Revision> {
    let row = sql
        .query_one(
            &format!("INSERT INTO core_document_revisions AS r (tenant_id,document_id,source,revision,name,artifact_id,file_name,content_type,byte_size,sha256,external_url,change_log,label,replaces_revision_id,scan_status,created_by)
                      VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) RETURNING {REVISION_COLUMNS}"),
            &[&tenant, &document.id, &document.source.code(), &number, &document.name,
              &file.map(TakenFile::artifact_id), &file.map(TakenFile::file_name), &file.map(TakenFile::content_type),
              &file.map(TakenFile::byte_size), &file.map(TakenFile::sha256), &external_url, &change_log, &label, &replaces_revision_id,
              &if file.is_some() { "clean" } else { "not_scanned" }, &user],
        )
        .await?;
    revision_from_row(&row)
}

/// Inserts a link; `None` when it already exists.
pub(crate) async fn insert_link(
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
