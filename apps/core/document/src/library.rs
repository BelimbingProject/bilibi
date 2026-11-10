//! Library actions on documents: the reader's favourite and follow marks,
//! finding duplicates, copying and merging. Access never changes except the
//! one thing a merge exists to do: the sources' explicit grants of active
//! accounts join the survivor's, which is why a merge needs the share
//! permission on every document it unites. Every statement binds the tenant;
//! every write goes through the caller's `MutationTransaction`.
use crate::model::{self, Draft};
use crate::store::{
    self, audit_values, from_row, insert_link, link_values, reader, revision_from_row,
    revision_values, write_error, On, Store, COLUMNS, REVISION_COLUMNS,
};
use crate::{
    Document, Duplicate, DuplicateReason, Link, MarkPatch, Marks, MergePlan, MergeSource, Merged,
    Reference, Revision, Source, WriteError, WriteResult, ACCESS_RECORD, LINK_RECORD, MANAGE,
    MARK_RECORD, MERGE, RECORD, REVISION_RECORD,
};
use bilibi_authz::authorize_proof;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_notifications::Notice;
use bilibi_principal_directory::{PrincipalKind, PrincipalRef, Query};
use bilibi_tenancy::{ReadClient, Scope};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// The most documents one merge removes.
const MERGE_MAX: usize = 20;
/// The most duplicates one document lists.
const DUPLICATE_ROWS: i64 = 50;

fn mark_values(document: i64, user: i64, marks: Marks) -> Value {
    json!({"document_id": document, "user_id": user, "favourite": marks.favourite, "following": marks.following})
}

/// The reader's own marks on `ids` they may see.
pub(crate) async fn marks(
    client: &dyn ReadClient,
    scope: &Scope,
    ids: &[i64],
) -> Result<HashMap<i64, Marks>> {
    let reader = reader(client, scope).await?;
    Ok(client
        .query(
            "SELECT m.document_id,m.favourite,m.following FROM core_document_marks m WHERE m.tenant_id=$1 AND m.user_id=$2 AND m.document_id = ANY($3)
             AND EXISTS (SELECT 1 FROM core_document_access a WHERE a.tenant_id=m.tenant_id AND a.document_id=m.document_id AND a.user_id=m.user_id AND a.can_view AND (a.expires_at IS NULL OR a.expires_at > now()))",
            &[&reader.tenant, &reader.user, &ids],
        )
        .await?
        .iter()
        .map(|row| {
            (
                row.get::<_, i64>(0),
                Marks {
                    favourite: row.get(1),
                    following: row.get(2),
                },
            )
        })
        .collect())
}

/// The documents the reader may see that hold a file with this hash.
pub(crate) async fn duplicates_of_file(
    client: &dyn ReadClient,
    scope: &Scope,
    sha256: &str,
) -> Result<Vec<Document>> {
    let reader = reader(client, scope).await?;
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Vec::new());
    }
    let sha256 = sha256.to_ascii_lowercase();
    client
        .query(
            &format!("SELECT {COLUMNS} FROM core_documents d WHERE d.tenant_id=$1 AND d.deleting_at IS NULL
                      AND EXISTS (SELECT 1 FROM core_document_access a WHERE a.tenant_id=d.tenant_id AND a.document_id=d.id AND a.can_view AND (a.expires_at IS NULL OR a.expires_at > now()) AND a.user_id=$2)
                      AND EXISTS (SELECT 1 FROM core_document_revisions r WHERE r.tenant_id=d.tenant_id AND r.document_id=d.id AND r.sha256=$3)
                      ORDER BY d.id LIMIT 10"),
            &[&reader.tenant, &reader.user, &sha256],
        )
        .await?
        .iter()
        .map(from_row)
        .collect()
}

/// The ids `users` that are active accounts, in one directory read.
async fn active_accounts(
    store: &Store<'_>,
    client: &dyn ReadClient,
    scope: &Scope,
    users: &[i64],
) -> Result<HashSet<i64>> {
    let principals: Vec<PrincipalRef> = users
        .iter()
        .filter_map(|id| PrincipalRef::new(PrincipalKind::User, *id).ok())
        .collect();
    if principals.is_empty() {
        return Ok(HashSet::new());
    }
    store.active_accounts(client, scope, &principals).await
}

/// A checked merge: the documents, locked on a commit, in id order.
struct MergeCheck {
    survivor: Document,
    sources: Vec<Document>,
}

impl Store<'_> {
    /// Active accounts among `principals`.
    async fn active_accounts(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        principals: &[PrincipalRef],
    ) -> Result<HashSet<i64>> {
        let ranked = self
            .directory
            .rank(
                &store::Directory(client),
                scope,
                principals,
                &Query::default(),
            )
            .await
            .map_err(|_| Error::Internal)?;
        Ok(ranked.named.iter().map(|n| n.principal.id()).collect())
    }

    // ---- Marks ----------------------------------------------------------

    async fn marks_row(&self, client: &dyn ReadClient, scope: &Scope, id: i64) -> Result<Marks> {
        Ok(client
            .query_opt(
                "SELECT favourite,following FROM core_document_marks WHERE tenant_id=$1 AND user_id=$2 AND document_id=$3",
                &[&scope.tenant(), &scope.user(), &id],
            )
            .await?
            .map(|row| Marks {
                favourite: row.get(0),
                following: row.get(1),
            })
            .unwrap_or_default())
    }

    pub(crate) async fn preview_mark(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        patch: MarkPatch,
    ) -> WriteResult<(Document, Marks)> {
        let document = self.get(client, scope, id).await?;
        let current = self.marks_row(client, scope, id).await?;
        Ok((
            document,
            Marks {
                favourite: patch.favourite.unwrap_or(current.favourite),
                following: patch.following.unwrap_or(current.following),
            },
        ))
    }

    pub(crate) async fn mark(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        patch: MarkPatch,
    ) -> WriteResult<Marks> {
        self.get(tx.sql(), scope, id).await?;
        let (tenant, user) = (scope.tenant(), scope.user());
        tx.mutate(move |sql| {
            Box::pin(async move {
                let before = sql
                    .query_opt(
                        "SELECT id,favourite,following FROM core_document_marks WHERE tenant_id=$1 AND user_id=$2 AND document_id=$3 FOR UPDATE",
                        &[&tenant, &user, &id],
                    )
                    .await?;
                let old = before
                    .as_ref()
                    .map(|row| Marks {
                        favourite: row.get(1),
                        following: row.get(2),
                    })
                    .unwrap_or_default();
                let new = Marks {
                    favourite: patch.favourite.unwrap_or(old.favourite),
                    following: patch.following.unwrap_or(old.following),
                };
                if new == old {
                    return Ok((new, Vec::new()));
                }
                let row = sql
                    .query_one(
                        "INSERT INTO core_document_marks(tenant_id,user_id,document_id,favourite,following) VALUES($1,$2,$3,$4,$5)
                         ON CONFLICT (tenant_id,user_id,document_id) DO UPDATE SET favourite=EXCLUDED.favourite,following=EXCLUDED.following,updated_at=now() RETURNING id",
                        &[&tenant, &user, &id, &new.favourite, &new.following],
                    )
                    .await?;
                let change = Change {
                    kind: MARK_RECORD.into(),
                    id: row.get(0),
                    old: before.map_or_else(|| json!({}), |_| mark_values(id, user, old)),
                    new: mark_values(id, user, new),
                };
                Ok((new, vec![change]))
            })
        })
        .await
        .map_err(write_error)
    }

    /// Tells the document's followers (never the actor) about a new revision,
    /// when they can still see the document.
    pub(crate) async fn notify_followers(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        revision: &Revision,
    ) -> WriteResult<()> {
        let (tenant, actor, id) = (scope.tenant(), scope.user(), revision.document_id);
        let followers: Vec<i64> = tx
            .sql()
            .query(
                "SELECT m.user_id FROM core_document_marks m WHERE m.tenant_id=$1 AND m.document_id=$2 AND m.following AND m.user_id <> $3
                 AND EXISTS (SELECT 1 FROM core_document_access a WHERE a.tenant_id=m.tenant_id AND a.document_id=m.document_id AND a.user_id=m.user_id AND a.can_view AND (a.expires_at IS NULL OR a.expires_at > now()))",
                &[&tenant, &id, &actor],
            )
            .await
            .map_err(Error::from)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let followers = self.notifications.eligible(tx, &followers).await?;
        if followers.is_empty() {
            return Ok(());
        }
        let name: String = revision.name.chars().take(140).collect();
        let notice = Notice::new(format!("New revision: {name}"))
            .kind("core.document.revision")
            .body(format!(
                "Revision {} of a document you follow was added: {}",
                revision.revision,
                revision.change_log.chars().take(200).collect::<String>()
            ))
            .url(crate::web::path(id))
            .icon("document-text");
        self.notifications
            .notify(tx, scope, &followers, &notice)
            .await?;
        Ok(())
    }

    // ---- Duplicates -----------------------------------------------------

    pub(crate) async fn duplicates(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
    ) -> Result<Vec<Duplicate>> {
        let document = self.get(client, scope, id).await?;
        let (tenant, user) = (scope.tenant(), scope.user());
        let link = document
            .external_url
            .as_deref()
            .filter(|_| document.source == Source::External);
        client
            .query(
                &format!("SELECT * FROM (
                    SELECT {COLUMNS},
                      EXISTS (SELECT 1 FROM core_document_revisions a JOIN core_document_revisions b ON b.tenant_id=a.tenant_id AND b.sha256=a.sha256
                              WHERE a.tenant_id=d.tenant_id AND a.document_id=$2 AND b.document_id=d.id) AS same_file,
                      (d.source='external' AND COALESCE(d.external_url = $4::text, false)) AS same_link,
                      lower(btrim(d.name)) = lower(btrim($3::text)) AS same_name
                    FROM core_documents d
                    WHERE d.tenant_id=$1 AND d.id <> $2 AND d.deleting_at IS NULL
                      AND EXISTS (SELECT 1 FROM core_document_access a WHERE a.tenant_id=d.tenant_id AND a.document_id=d.id AND a.can_view AND (a.expires_at IS NULL OR a.expires_at > now()) AND a.user_id=$5)
                  ) x WHERE x.same_file OR x.same_link OR x.same_name ORDER BY x.id LIMIT {DUPLICATE_ROWS}"),
                &[&tenant, &id, &document.name, &link, &user],
            )
            .await?
            .iter()
            .map(|row| {
                let mut reasons = Vec::new();
                for (column, reason) in [
                    ("same_file", DuplicateReason::SameFile),
                    ("same_link", DuplicateReason::SameLink),
                    ("same_name", DuplicateReason::SameName),
                ] {
                    if row.get::<_, bool>(column) {
                        reasons.push(reason);
                    }
                }
                Ok(Duplicate {
                    document: from_row(row)?,
                    reasons,
                })
            })
            .collect()
    }

    // ---- Copy -----------------------------------------------------------

    /// The source of a file copy: a document with a file the actor may
    /// download and the capability to create documents.
    pub(crate) async fn copyable(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
    ) -> WriteResult<Document> {
        authorize_proof(client, scope, MANAGE).await?;
        let document = self.get(client, scope, id).await?;
        if !store::permission(client, scope, id, "download").await? {
            return Err(WriteError::Forbidden);
        }
        if document.source != Source::Internal {
            return Err(WriteError::field("file", "a link has no file to copy"));
        }
        Ok(document)
    }

    async fn copy_draft(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        artifact_id: Option<&str>,
    ) -> WriteResult<Draft> {
        authorize_proof(client, scope, MANAGE).await?;
        let source = self.get(client, scope, id).await?;
        let name: String = format!("Copy of {}", source.name)
            .chars()
            .take(model::NAME_MAX)
            .collect();
        Ok(Draft {
            name: Some(name),
            publish_date: Some(source.publish_date),
            source: source.source,
            artifact_id: artifact_id.map(str::to_owned),
            external_url: source.external_url,
            category: source.category,
            sub_category: source.sub_category,
            document_type: source.document_type,
            status: None,
            expires_on: None,
            custom: source.custom,
            sync_id: None,
            related_document_id: None,
            relation: None,
            revision_label: None,
            change_log: None,
            related: Vec::new(),
        })
    }

    pub(crate) async fn preview_copy(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        artifact_id: Option<&str>,
    ) -> WriteResult<Draft> {
        let draft = self.copy_draft(client, scope, id, artifact_id).await?;
        Ok(self
            .prepare_create(On::Preview(client), scope, &draft)
            .await?
            .draft())
    }

    pub(crate) async fn copy(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        artifact_id: Option<&str>,
    ) -> WriteResult<Document> {
        let draft = self.copy_draft(tx.sql(), scope, id, artifact_id).await?;
        self.create(tx, scope, &draft).await
    }

    // ---- Merge ----------------------------------------------------------

    async fn check_merge(
        &self,
        on: On<'_>,
        scope: &Scope,
        survivor: MergeSource,
        sources: &[MergeSource],
    ) -> WriteResult<MergeCheck> {
        let client = on.client();
        authorize_proof(client, scope, MERGE).await?;
        authorize_proof(client, scope, MANAGE).await?;
        if sources.is_empty() || sources.len() > MERGE_MAX {
            return Err(WriteError::MergeRefused(
                "name between one and twenty documents to merge into the survivor",
            ));
        }
        let mut seen = HashSet::from([survivor.id]);
        if sources.iter().any(|s| !seen.insert(s.id)) {
            return Err(WriteError::MergeRefused(
                "a document may be named once, and not as its own duplicate",
            ));
        }
        // Lock in id order, so two merges never wait on each other.
        let mut all: Vec<MergeSource> = std::iter::once(survivor)
            .chain(sources.iter().copied())
            .collect();
        all.sort_by_key(|m| m.id);
        let mut found: HashMap<i64, Document> = HashMap::new();
        for named in all {
            let document = self
                .editable(on, scope, named.id, Some(named.version), MANAGE)
                .await?;
            if !store::permission(client, scope, named.id, "share").await? {
                return Err(WriteError::Forbidden);
            }
            found.insert(named.id, document);
        }
        let survivor_document = found.remove(&survivor.id).ok_or(WriteError::NotFound)?;
        let mut documents: Vec<Document> = found.into_values().collect();
        documents.sort_by_key(|d| d.id);
        if documents
            .iter()
            .any(|d| d.source != survivor_document.source)
        {
            return Err(WriteError::MergeRefused(
                "they must all keep files, or all keep links",
            ));
        }
        if documents
            .iter()
            .any(|d| d.company_id != survivor_document.company_id)
        {
            return Err(WriteError::MergeRefused(
                "they are filed under different companies",
            ));
        }
        // A retained or legally held revision keeps its document, number,
        // change log and file (ADR 23): merging would move and rewrite it.
        let source_ids: Vec<i64> = documents.iter().map(|d| d.id).collect();
        let artifacts: Vec<String> = client
            .query(
                "SELECT artifact_id FROM core_document_revisions WHERE tenant_id=$1 AND document_id = ANY($2) AND artifact_id IS NOT NULL ORDER BY artifact_id",
                &[&scope.tenant(), &source_ids],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let company = bilibi_party_ids::OrganisationId::new(survivor_document.company_id)
            .map_err(|_| Error::Internal)?;
        match self
            .artifacts
            .for_owner(scope, company, &crate::files::FILES)
            .ensure_unprotected(client, &artifacts, on.locks())
            .await
        {
            Ok(()) => {}
            Err(bilibi_artifacts::Error::Retained) => {
                return Err(WriteError::MergeRefused(
                    "a revision is retained and cannot be moved or changed yet",
                ))
            }
            Err(bilibi_artifacts::Error::LegalHold) => {
                return Err(WriteError::MergeRefused(
                    "a revision is under legal hold and cannot be moved or changed",
                ))
            }
            Err(bilibi_artifacts::Error::Database(error)) => return Err(error.into()),
            Err(_) => return Err(Error::Internal.into()),
        }
        if documents.iter().any(|d| d.sync_id.is_some()) {
            return Err(WriteError::MergeRefused(
                "clear each source's integration sync ID before merging",
            ));
        }
        Ok(MergeCheck {
            survivor: survivor_document,
            sources: documents,
        })
    }

    pub(crate) async fn preview_merge(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        survivor: MergeSource,
        sources: &[MergeSource],
    ) -> WriteResult<MergePlan> {
        let check = self
            .check_merge(On::Preview(client), scope, survivor, sources)
            .await?;
        let ids: Vec<i64> = check.sources.iter().map(|d| d.id).collect();
        let revisions: i64 = client
            .query_opt(
                "SELECT count(*) FROM core_document_revisions WHERE tenant_id=$1 AND document_id = ANY($2)",
                &[&scope.tenant(), &ids],
            )
            .await?
            .ok_or(Error::Internal)?
            .get(0);
        let merged: Vec<String> = std::iter::once(check.survivor.id)
            .chain(ids.iter().copied())
            .map(|id| id.to_string())
            .collect();
        let links: i64 = client
            .query_opt(
                "SELECT count(*) FROM (SELECT DISTINCT l.record_kind,l.record_id FROM core_document_links l
                   WHERE l.tenant_id=$1 AND l.document_id = ANY($2) AND NOT (l.record_kind=$3 AND l.record_id = ANY($4))) x
                 WHERE NOT EXISTS (SELECT 1 FROM core_document_links s WHERE s.tenant_id=$1 AND s.document_id=$5 AND s.record_kind=x.record_kind AND s.record_id=x.record_id)",
                &[&scope.tenant(), &ids, &RECORD, &merged, &check.survivor.id],
            )
            .await?
            .ok_or(Error::Internal)?
            .get(0);
        if i64::from(check.survivor.revision).saturating_add(revisions) > i64::from(i32::MAX) {
            return Err(WriteError::MergeRefused(
                "they have too many revisions together",
            ));
        }
        Ok(MergePlan {
            survivor: check.survivor,
            sources: check.sources,
            revisions,
            links,
        })
    }

    pub(crate) async fn merge(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        survivor: MergeSource,
        sources: &[MergeSource],
    ) -> WriteResult<Merged> {
        let check = self
            .check_merge(On::Commit(tx.sql()), scope, survivor, sources)
            .await?;
        let tenant = scope.tenant();
        let source_ids: Vec<i64> = check.sources.iter().map(|d| d.id).collect();
        // Only active accounts keep their grants; the rest go with the source.
        let grantees: Vec<i64> = tx
            .sql()
            .query(
                "SELECT DISTINCT user_id FROM core_document_access WHERE tenant_id=$1 AND document_id = ANY($2)",
                &[&tenant, &source_ids],
            )
            .await
            .map_err(Error::from)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let active = active_accounts(self, tx.sql(), scope, &grantees).await?;
        let merged: Vec<String> = std::iter::once(check.survivor.id)
            .chain(source_ids.iter().copied())
            .map(|id| id.to_string())
            .collect();
        for source in &check.sources {
            if let Some(task) = source.expiry_activity_id {
                self.remove_reminder(tx, scope, source.id, task).await?;
            }
        }
        let before = check.survivor.clone();
        let sources_in = check.sources.clone();
        let (document, revisions) = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let mut changes: Vec<Change> = Vec::new();
                    let mut next = before.revision;
                    let mut moved = 0_i64;
                    let mut latest_link = before.external_url.clone();
                    for source in &sources_in {
                        let rows = sql
                            .query(
                                &format!("SELECT {REVISION_COLUMNS} FROM core_document_revisions r WHERE r.tenant_id=$1 AND r.document_id=$2 ORDER BY r.revision FOR UPDATE"),
                                &[&tenant, &source.id],
                            )
                            .await?;
                        for row in rows {
                            let old = revision_from_row(&row)?;
                            next += 1;
                            moved += 1;
                            let log: String = format!(
                                "Merged from “{}” (document {}), revision {}: {}",
                                source.name, source.id, old.revision, old.change_log
                            )
                            .chars()
                            .take(model::CHANGE_LOG_MAX)
                            .collect();
                            let row = sql
                                .query_one(
                                    &format!("UPDATE core_document_revisions r SET document_id=$3,revision=$4,change_log=$5 WHERE r.tenant_id=$1 AND r.id=$2 RETURNING {REVISION_COLUMNS}"),
                                    &[&tenant, &old.id, &before.id, &next, &log],
                                )
                                .await?;
                            let new = revision_from_row(&row)?;
                            if new.external_url.is_some() {
                                latest_link = new.external_url.clone();
                            }
                            changes.push(Change {
                                kind: REVISION_RECORD.into(),
                                id: new.id,
                                old: revision_values(&old),
                                new: revision_values(&new),
                            });
                        }
                    }
                    let row = sql
                        .query_one(
                            &format!("UPDATE core_documents d SET revision=$4,last_revision_at=now(),external_url=$5,version=d.version+1,updated_at=now() WHERE d.tenant_id=$1 AND d.id=$2 AND d.version=$3 RETURNING {COLUMNS}"),
                            &[&tenant, &before.id, &before.version, &next, &latest_link],
                        )
                        .await?;
                    let mut document = from_row(&row)?;
                    changes.push(Change {
                        kind: RECORD.into(),
                        id: document.id,
                        old: audit_values(&before),
                        new: audit_values(&document),
                    });
                    for source in &sources_in {
                        // Links: the survivor gains each record once; a link to a
                        // merged document itself is dropped.
                        for row in sql
                            .query(
                                "DELETE FROM core_document_links WHERE tenant_id=$1 AND document_id=$2 RETURNING id,record_kind,record_id,linked_by,linked_at",
                                &[&tenant, &source.id],
                            )
                            .await?
                        {
                            let link = Link {
                                id: row.get(0),
                                record: Reference::new(row.get::<_, String>(1), row.get::<_, String>(2)),
                                linked_by: row.get(3),
                                linked_at: row
                                    .get::<_, std::time::SystemTime>(4)
                                    .try_into()
                                    .map_err(|_| Error::Internal)?,
                            };
                            changes.push(Change {
                                kind: LINK_RECORD.into(),
                                id: link.id,
                                old: link_values(source.id, &link),
                                new: json!({}),
                            });
                            if link.record.kind == RECORD && merged.contains(&link.record.id) {
                                continue;
                            }
                            if let Some(added) =
                                insert_link(sql, tenant, link.linked_by, before.id, &link.record).await?
                            {
                                changes.push(Change {
                                    kind: LINK_RECORD.into(),
                                    id: added.id,
                                    old: json!({}),
                                    new: link_values(before.id, &added),
                                });
                            }
                        }
                        // Access: explicit grants of active accounts, unioned.
                        for row in sql
                            .query(
                                "DELETE FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 RETURNING id,user_id,can_view,can_download,can_manage,can_share,granted_by,expires_at,(expires_at IS NOT NULL AND expires_at <= now()) AS lapsed",
                                &[&tenant, &source.id],
                            )
                            .await?
                        {
                            let (grant, user): (i64, i64) = (row.get(0), row.get(1));
                            let (download, manage, share): (bool, bool, bool) =
                                (row.get(3), row.get(4), row.get(5));
                            changes.push(Change {
                                kind: ACCESS_RECORD.into(),
                                id: grant,
                                old: json!({"document_id":source.id,"user_id":user,"view":row.get::<_,bool>(2),"download":download,"manage":manage,"share":share}),
                                new: json!({}),
                            });
                            // An ended grant, or one of an account that can no
                            // longer sign in, goes with the source.
                            if !active.contains(&user) || row.get::<_, bool>(8) {
                                continue;
                            }
                            let ends: Option<std::time::SystemTime> = row.get(7);
                            let existing = sql
                                .query_opt(
                                    "SELECT id,can_download,can_manage,can_share,expires_at::text FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND user_id=$3 FOR UPDATE",
                                    &[&tenant, &before.id, &user],
                                )
                                .await?;
                            let granted_by: i64 = row.get(6);
                            let new = sql
                                .query_one(
                                    "INSERT INTO core_document_access(tenant_id,document_id,user_id,can_view,can_download,can_manage,can_share,granted_by,expires_at) VALUES($1,$2,$3,true,$4,$5,$6,$7,$8)
                                     ON CONFLICT(tenant_id,document_id,user_id) DO UPDATE SET can_download=CASE WHEN (core_document_access.expires_at IS NOT NULL AND core_document_access.expires_at <= now()) THEN EXCLUDED.can_download ELSE core_document_access.can_download OR EXCLUDED.can_download END,can_manage=CASE WHEN (core_document_access.expires_at IS NOT NULL AND core_document_access.expires_at <= now()) THEN EXCLUDED.can_manage ELSE core_document_access.can_manage OR EXCLUDED.can_manage END,can_share=CASE WHEN (core_document_access.expires_at IS NOT NULL AND core_document_access.expires_at <= now()) THEN EXCLUDED.can_share ELSE core_document_access.can_share OR EXCLUDED.can_share END,expires_at=CASE WHEN (core_document_access.expires_at IS NOT NULL AND core_document_access.expires_at <= now()) THEN EXCLUDED.expires_at WHEN core_document_access.expires_at IS NULL OR EXCLUDED.expires_at IS NULL THEN NULL ELSE GREATEST(core_document_access.expires_at,EXCLUDED.expires_at) END
                                     RETURNING id,can_download,can_manage,can_share,expires_at::text",
                                    &[&tenant, &before.id, &user, &download, &manage, &share, &granted_by, &ends],
                                )
                                .await?;
                            let now = json!({"document_id":before.id,"user_id":user,"view":true,"download":new.get::<_,bool>(1),"manage":new.get::<_,bool>(2),"share":new.get::<_,bool>(3),"expires_at":new.get::<_,Option<String>>(4)});
                            let was = existing.map_or_else(|| json!({}), |old| json!({"document_id":before.id,"user_id":user,"view":true,"download":old.get::<_,bool>(1),"manage":old.get::<_,bool>(2),"share":old.get::<_,bool>(3),"expires_at":old.get::<_,Option<String>>(4)}));
                            if was != now {
                                changes.push(Change { kind: ACCESS_RECORD.into(), id: new.get(0), old: was, new: now });
                            }
                        }
                        // Marks: either mark carries over.
                        for row in sql
                            .query(
                                "DELETE FROM core_document_marks WHERE tenant_id=$1 AND document_id=$2 RETURNING id,user_id,favourite,following",
                                &[&tenant, &source.id],
                            )
                            .await?
                        {
                            let user: i64 = row.get(1);
                            let old = Marks { favourite: row.get(2), following: row.get(3) };
                            changes.push(Change { kind: MARK_RECORD.into(), id: row.get(0), old: mark_values(source.id, user, old), new: json!({}) });
                            let existing = sql
                                .query_opt(
                                    "SELECT favourite,following FROM core_document_marks WHERE tenant_id=$1 AND user_id=$2 AND document_id=$3 FOR UPDATE",
                                    &[&tenant, &user, &before.id],
                                )
                                .await?;
                            let current = existing.as_ref().map(|r| Marks { favourite: r.get(0), following: r.get(1) }).unwrap_or_default();
                            let joined = Marks { favourite: current.favourite || old.favourite, following: current.following || old.following };
                            if existing.is_none() || joined != current {
                                let row = sql
                                    .query_one(
                                        "INSERT INTO core_document_marks(tenant_id,user_id,document_id,favourite,following) VALUES($1,$2,$3,$4,$5)
                                         ON CONFLICT (tenant_id,user_id,document_id) DO UPDATE SET favourite=EXCLUDED.favourite,following=EXCLUDED.following,updated_at=now() RETURNING id",
                                        &[&tenant, &user, &before.id, &joined.favourite, &joined.following],
                                    )
                                    .await?;
                                changes.push(Change {
                                    kind: MARK_RECORD.into(),
                                    id: row.get(0),
                                    old: existing.map_or_else(|| json!({}), |_| mark_values(before.id, user, current)),
                                    new: mark_values(before.id, user, joined),
                                });
                            }
                        }
                        // Keep incoming document lineage pointed at the survivor.
                        // A survivor that pointed at this source cannot point to itself.
                        for row in sql.query(
                            &format!("SELECT {COLUMNS} FROM core_documents d WHERE d.tenant_id=$1 AND d.related_document_id=$2 FOR UPDATE"),
                            &[&tenant, &source.id],
                        ).await? {
                            let old = store::from_row(&row)?;
                            let target = (old.id != before.id).then_some(before.id);
                            let updated = sql.query_one(
                                &format!("UPDATE core_documents d SET related_document_id=$3,relation=CASE WHEN $3::bigint IS NULL THEN NULL ELSE relation END,version=d.version+1,updated_at=now() WHERE d.tenant_id=$1 AND d.id=$2 RETURNING {COLUMNS}"),
                                &[&tenant, &old.id, &target],
                            ).await?;
                            let new = store::from_row(&updated)?;
                            if old.id == before.id {
                                document = new.clone();
                            }
                            changes.push(Change { kind: RECORD.into(), id: old.id, old: audit_values(&old), new: audit_values(&new) });
                        }
                        let row = sql.query_one(
                            &format!("DELETE FROM core_documents AS d WHERE d.tenant_id=$1 AND d.id=$2 RETURNING {COLUMNS}"),
                            &[&tenant, &source.id],
                        ).await?;
                        let removed = from_row(&row)?;
                        changes.push(Change {
                            kind: RECORD.into(),
                            id: removed.id,
                            old: audit_values(&removed),
                            new: json!({}),
                        });
                    }
                    Ok(((document, moved), changes))
                })
            })
            .await
            .map_err(write_error)?;
        for id in &source_ids {
            self.records
                .redirect(tx, scope, RECORD, &id.to_string(), &document.id.to_string())
                .await?;
        }
        Ok(Merged {
            survivor: document,
            removed: source_ids,
            revisions,
        })
    }
}
