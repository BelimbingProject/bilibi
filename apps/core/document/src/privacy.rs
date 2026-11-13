//! Documents' personal-data store (ADR 05, `base/personal_data`): the
//! documents related to a person (a `person` link, such as an identity scan
//! or a signed contract), with their revisions and files (#583, #659).
//!
//! - **Locate** counts the person's documents, including one being deleted.
//! - **Export** lists each one not being deleted with its revisions, and every clean file, read
//!   through `Artifacts::for_disclosure` (the owner vouches the file's subject
//!   is a revision of a document related to the request's person).
//! - **Erase** keeps a document under a legal hold (on the person, the
//!   document, one of its revisions or file ledger rows, or a held upload's
//!   Media file) or whose file Base still retains or holds, with its reason. A
//!   document also related to another record is that record's too: only its
//!   link to the person is removed, and it is kept as shared. Every other
//!   document is anonymised in the erasing transaction (its name, link,
//!   custom fields, its revisions' names, file names, change logs and labels,
//!   and its ledger rows' file names), the Media files of its held uploads
//!   become tombstones, and it is marked as erasing.
//! - **After the erasure**, `after_erase` removes those Media files' bytes,
//!   then deletes each erasing document's Artifacts under the erasure grant
//!   and its rows, and counts it deleted. A hold or retention placed since
//!   the erasure keeps the document, anonymised, and the certificate says
//!   kept with that reason. The person link stays until the bytes are gone,
//!   so Media and Artifacts can still ask whose files they are.
use crate::files::{FILES, MEDIA_SUBJECT};
use crate::{FILE_RECORD, LINK_RECORD, RECORD, REVISION_RECORD};
use bilibi_artifacts::Artifacts;
use bilibi_database::{Change, Error, MutationBoundary, MutationTransaction, Result};
use bilibi_media::{AttachmentSubject, Media, ASSET_KIND, ATTACHMENT_KIND};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_party_ids::OrganisationId;
use bilibi_personal_data::{
    Action, Disclosure, Erased, ErasedRecord, Erasure, Exported, File, FileRef, Handler,
    HandlerFuture, Holds, Kept, KeptReason, KindCount, Located, PersonalData, Progress, RecordKind,
    Subject, ERASED, PERSONAL_DATA, RECORD_KINDS,
};
use bilibi_tenancy::{ProofClient, Scope};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use tokio_postgres::{Client, GenericClient};

const DOCUMENT_FIELDS: [&str; 3] = ["name", "external_url", "custom_fields"];
const REVISION_FIELDS: [&str; 5] = ["name", "file_name", "external_url", "change_log", "label"];
const FILE_FIELDS: [&str; 1] = ["file_name"];
/// What an erased link reads: the column must stay an https URL until the
/// erasure removes the row, and `.invalid` never resolves.
const ERASED_URL: &str = "https://erased.invalid/";
/// The person's documents, including one being deleted or erased.
const LINKED: &str = "SELECT d.id FROM core_documents d WHERE d.tenant_id=$1 AND EXISTS (SELECT 1 FROM core_document_links l WHERE l.tenant_id=d.tenant_id AND l.document_id=d.id AND l.record_kind='person' AND l.record_id=$2::bigint::text)";

struct Bound {
    artifacts: Arc<Artifacts>,
    media: Arc<Media>,
    boundary: Arc<MutationBoundary>,
}
static BOUND: OnceLock<Bound> = OnceLock::new();
/// Bound once by the Documents service factory.
pub(crate) fn bind(
    artifacts: Arc<Artifacts>,
    media: Arc<Media>,
    boundary: Arc<MutationBoundary>,
) -> bilibi_module_registry::Result<()> {
    BOUND
        .set(Bound {
            artifacts,
            media,
            boundary,
        })
        .map_err(|_| RegistryError::Service("duplicate document privacy binding".into()))
}
fn bound() -> Result<&'static Bound> {
    BOUND.get().ok_or(Error::Internal)
}

/// Whether the ledger `subject` is the file of a revision of a document
/// related to `person`: the owner's answer to Artifacts (`disclosure_subject`,
/// `erasure_subject`) and to Media (`person_subject`, a held upload).
pub(crate) async fn person_file(
    client: &dyn ProofClient,
    tenant: i64,
    person: i64,
    subject: &str,
) -> Result<bool> {
    let Ok(subject) = uuid::Uuid::parse_str(subject) else {
        return Ok(false);
    };
    Ok(client
        .query_opt(
            "SELECT 1 FROM core_document_files f JOIN core_document_revisions r ON r.tenant_id=f.tenant_id AND r.file_id=f.id JOIN core_document_links l ON l.tenant_id=r.tenant_id AND l.document_id=r.document_id AND l.record_kind='person' AND l.record_id=$3::bigint::text WHERE f.tenant_id=$1 AND f.subject=$2::text::uuid AND f.state='attached' LIMIT 1",
            &[&tenant, &subject.to_string(), &person],
        )
        .await?
        .is_some())
}

fn artifact_error(error: bilibi_artifacts::Error) -> Error {
    match error {
        bilibi_artifacts::Error::Database(error) => error,
        bilibi_artifacts::Error::NotFound => Error::NotFound,
        _ => Error::Internal,
    }
}
fn media_error(error: bilibi_media::Error) -> Error {
    match error {
        bilibi_media::Error::Database(error) => error,
        bilibi_media::Error::NotFound => Error::NotFound,
        _ => Error::Internal,
    }
}

/// Why Artifacts keeps `document`'s `files`: a legal hold, or retention
/// with its latest date. `None` for any other refusal.
async fn protected(
    client: &(impl GenericClient + Sync),
    tenant: i64,
    document: i64,
    files: &[String],
    error: &bilibi_artifacts::Error,
) -> Result<Option<Kept>> {
    let (reason, until) = match error {
        bilibi_artifacts::Error::LegalHold => (KeptReason::LegalHold, None),
        bilibi_artifacts::Error::Retained => (
            KeptReason::RetainedUntil,
            client
                .query_one(
                    "SELECT max(retain_until)::text FROM base_artifacts WHERE tenant_id=$1 AND public_id = ANY($2)",
                    &[&tenant, &files],
                )
                .await?
                .get(0),
        ),
        _ => return Ok(None),
    };
    Ok(Some(Kept {
        kind: RECORD.into(),
        id: document.to_string(),
        reason,
        until,
        hold: None,
    }))
}

/// The revisions and files of some of the person's documents.
#[derive(Default)]
struct Files {
    revisions: Vec<i64>,
    /// File ledger rows.
    ledger: Vec<i64>,
    /// Each ledger row's subject: its Artifact's subject, and its Media
    /// upload's while the file is held.
    subjects: Vec<String>,
    /// (Artifact ID, ledger subject).
    artifacts: Vec<(String, String)>,
}
impl Files {
    fn artifact_ids(&self) -> Vec<String> {
        self.artifacts.iter().map(|(id, _)| id.clone()).collect()
    }
}
/// The revisions and files of `ids`, by document; every document has an entry.
async fn files_of(
    client: &(impl GenericClient + Sync),
    tenant: i64,
    ids: &[i64],
) -> Result<BTreeMap<i64, Files>> {
    let mut files: BTreeMap<i64, Files> = ids.iter().map(|id| (*id, Files::default())).collect();
    for row in client
        .query(
            "SELECT r.document_id,r.id,r.artifact_id,f.id,f.subject::text FROM core_document_revisions r LEFT JOIN core_document_files f ON f.tenant_id=r.tenant_id AND f.id=r.file_id WHERE r.tenant_id=$1 AND r.document_id = ANY($2) ORDER BY r.document_id,r.revision",
            &[&tenant, &ids],
        )
        .await?
    {
        let entry = files.entry(row.get(0)).or_default();
        entry.revisions.push(row.get(1));
        let (artifact, ledger, subject): (Option<String>, Option<i64>, Option<String>) =
            (row.get(2), row.get(3), row.get(4));
        entry.ledger.extend(ledger);
        if let Some(subject) = subject {
            if let Some(artifact) = artifact {
                entry.artifacts.push((artifact, subject.clone()));
            }
            entry.subjects.push(subject);
        }
    }
    Ok(files)
}
/// The privacy hold that keeps each of `files`' documents: on the person,
/// the document, or one of its revisions or file ledger rows (document →
/// hold).
async fn held(
    holds: &Holds,
    grant: &Erasure,
    files: &BTreeMap<i64, Files>,
) -> Result<BTreeMap<i64, i64>> {
    let person = holds.person_hold(grant).await?;
    let ids: Vec<i64> = files.keys().copied().collect();
    let revisions: Vec<i64> = files.values().flat_map(|f| f.revisions.clone()).collect();
    let ledger: Vec<i64> = files.values().flat_map(|f| f.ledger.clone()).collect();
    let documents = holds.records(grant, RECORD, &ids).await?;
    let revisions = holds.records(grant, REVISION_RECORD, &revisions).await?;
    let ledger = holds.records(grant, FILE_RECORD, &ledger).await?;
    Ok(files
        .iter()
        .filter_map(|(id, f)| {
            person
                .or_else(|| documents.get(id).copied())
                .or_else(|| f.revisions.iter().find_map(|r| revisions.get(r).copied()))
                .or_else(|| f.ledger.iter().find_map(|l| ledger.get(l).copied()))
                .map(|hold| (*id, hold))
        })
        .collect())
}
/// Removes the bytes of `file`'s erased Media files (a held upload's).
async fn remove_bytes(
    client: &mut Client,
    scope: &Scope,
    grant: &Erasure,
    file: &str,
) -> Result<()> {
    loop {
        let removal = bound()?
            .media
            .remove_erased_bytes(
                client,
                scope,
                grant,
                AttachmentSubject {
                    kind: MEDIA_SUBJECT,
                    id: file,
                },
            )
            .await
            .map_err(media_error)?;
        if !removal.more {
            return Ok(());
        }
        if removal.removed == 0 {
            // Another attempt is removing them now: try again later.
            return Err(Error::Conflict);
        }
    }
}

struct DocumentData;
static DOCUMENTS: DocumentData = DocumentData;
impl Handler for DocumentData {
    fn locate<'a>(
        &'a self,
        client: &'a Client,
        scope: &'a Scope,
        subject: &'a Subject,
    ) -> HandlerFuture<'a, Located> {
        Box::pin(async move {
            let count: i64 = client
                .query_one(
                    &format!("SELECT count(*) FROM ({LINKED}) x"),
                    &[&scope.tenant(), &subject.person.get()],
                )
                .await?
                .get(0);
            Ok(Located {
                kinds: vec![KindCount {
                    kind: RECORD.into(),
                    by_person: count,
                    by_identifier: 0,
                }],
                ..Located::default()
            })
        })
    }

    fn export<'a>(
        &'a self,
        client: &'a Client,
        scope: &'a Scope,
        subject: &'a Subject,
        grant: &'a Disclosure,
    ) -> HandlerFuture<'a, Exported> {
        Box::pin(async move {
            grant.check_subject(scope, subject)?;
            let (tenant, person) = (scope.tenant(), subject.person.get());
            let records: Value = client
                .query_one(
                    &format!(
                        "SELECT coalesce(json_agg(json_build_object(
                            'id',d.id,'name',d.name,'publish_date',d.publish_date,'source',d.source,'external_url',d.external_url,
                            'category',d.category,'sub_category',d.sub_category,'type',d.document_type,'status',d.status,
                            'expires_on',d.expires_on,'custom_fields',d.custom_fields,'created_at',d.created_at,'updated_at',d.updated_at,
                            'revisions',(SELECT coalesce(json_agg(json_build_object(
                                'revision',r.revision,'name',r.name,'file_name',r.file_name,'content_type',r.content_type,
                                'byte_size',r.byte_size,'external_url',r.external_url,'change_log',r.change_log,'label',r.label,
                                'scan_status',r.scan_status,'created_at',r.created_at) ORDER BY r.revision),'[]'::json)
                              FROM core_document_revisions r WHERE r.tenant_id=d.tenant_id AND r.document_id=d.id)
                         ) ORDER BY d.id),'[]'::json)::jsonb
                         FROM core_documents d WHERE d.tenant_id=$1 AND d.id IN ({LINKED} AND d.deleting_at IS NULL)"
                    ),
                    &[&tenant, &person],
                )
                .await?
                .get(0);
            let files = client
                .query(
                    &format!(
                        "SELECT r.artifact_id,r.document_id,r.revision,r.file_name,r.content_type FROM core_document_revisions r
                         WHERE r.tenant_id=$1 AND r.document_id IN ({LINKED} AND d.deleting_at IS NULL) AND r.artifact_id IS NOT NULL AND r.scan_status='clean'
                         ORDER BY r.document_id,r.revision"
                    ),
                    &[&tenant, &person],
                )
                .await?
                .iter()
                .map(|row| FileRef {
                    reference: row.get(0),
                    name: format!(
                        "document-{}-r{}-{}",
                        row.get::<_, i64>(1),
                        row.get::<_, i32>(2),
                        row.get::<_, Option<String>>(3).unwrap_or_default()
                    ),
                    content_type: row
                        .get::<_, Option<String>>(4)
                        .unwrap_or_else(|| "application/octet-stream".into()),
                })
                .collect();
            Ok(Exported {
                records: json!({ "documents": records }),
                files,
            })
        })
    }

    fn file<'a>(
        &'a self,
        client: &'a mut Client,
        scope: &'a Scope,
        grant: &'a Disclosure,
        reference: &'a str,
    ) -> HandlerFuture<'a, File> {
        Box::pin(async move {
            grant.check(scope)?;
            let row = client
                .query_opt(
                    "SELECT f.company_id,f.subject::text FROM core_document_files f WHERE f.tenant_id=$1 AND f.artifact_id=$2 AND f.state='attached'",
                    &[&scope.tenant(), &reference],
                )
                .await?
                .ok_or(Error::NotFound)?;
            let company = OrganisationId::new(row.get(0)).map_err(|_| Error::Internal)?;
            let subject: String = row.get(1);
            let document = bound()?
                .artifacts
                .for_disclosure(scope, company, &FILES, grant, &subject)
                .read(client, reference)
                .await
                .map_err(artifact_error)?;
            Ok(File {
                content_type: document.metadata.content_type.clone(),
                bytes: document.bytes,
            })
        })
    }

    fn erase<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        subject: &'a Subject,
        grant: &'a Erasure,
        holds: &'a Holds,
    ) -> HandlerFuture<'a, Erased> {
        Box::pin(async move {
            grant.check_subject(scope, subject)?;
            let (tenant, person) = (scope.tenant(), subject.person.get());
            let rows = tx
                .sql()
                .query(
                    &format!("SELECT d.id,d.company_id,EXISTS (SELECT 1 FROM core_document_links o WHERE o.tenant_id=d.tenant_id AND o.document_id=d.id AND NOT (o.record_kind='person' AND o.record_id=$2::bigint::text)) AS shared FROM core_documents d WHERE d.tenant_id=$1 AND d.id IN ({LINKED}) ORDER BY d.id FOR UPDATE OF d"),
                    &[&tenant, &person],
                )
                .await?;
            let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
            let files = files_of(tx.sql(), tenant, &ids).await?;
            let held = held(holds, grant, &files).await?;
            let mut erased = Erased::default();
            for row in rows {
                let (id, company, shared): (i64, i64, bool) = (row.get(0), row.get(1), row.get(2));
                if let Some(hold) = held.get(&id) {
                    erased.kept.push(Kept::held(RECORD, id, *hold));
                    continue;
                }
                if shared {
                    unlink(tx, tenant, id, person).await?;
                    erased.kept.push(Kept {
                        kind: RECORD.into(),
                        id: id.to_string(),
                        reason: KeptReason::Shared,
                        until: None,
                        hold: None,
                    });
                    continue;
                }
                let company = OrganisationId::new(company).map_err(|_| Error::Internal)?;
                let document = &files[&id];
                let artifacts = document.artifact_ids();
                if let Err(error) = bound()?
                    .artifacts
                    .for_owner(scope, company, &FILES)
                    .ensure_unprotected(tx.sql(), &artifacts, true)
                    .await
                {
                    match protected(tx.sql(), tenant, id, &artifacts, &error).await? {
                        Some(kept) => {
                            erased.kept.push(kept);
                            continue;
                        }
                        None => return Err(artifact_error(error)),
                    }
                }
                // A held upload's bytes are a Media file (no Artifact yet):
                // Media erases them, asking `person_subject` whose they are.
                let mut media_hold = None;
                for file in &document.subjects {
                    let done = bound()?
                        .media
                        .erase_attachments(
                            tx,
                            scope,
                            grant,
                            AttachmentSubject {
                                kind: MEDIA_SUBJECT,
                                id: file,
                            },
                            holds,
                        )
                        .await
                        .map_err(media_error)?;
                    for (attachment, hold) in done.held {
                        erased
                            .kept
                            .push(Kept::held(ATTACHMENT_KIND, attachment, hold));
                        media_hold.get_or_insert(hold);
                    }
                    erased.count(ATTACHMENT_KIND, Action::Deleted, done.erased.len() as i64);
                    for file in done.erased {
                        erased.records.push(ErasedRecord {
                            kind: ATTACHMENT_KIND.into(),
                            id: file.id,
                            person: None,
                        });
                        erased.records.push(ErasedRecord {
                            kind: ASSET_KIND.into(),
                            id: file.asset_id,
                            person: None,
                        });
                    }
                }
                if let Some(hold) = media_hold {
                    erased.kept.push(Kept::held(RECORD, id, hold));
                    continue;
                }
                erased.records.extend(anonymise(tx, tenant, id).await?);
            }
            Ok(erased)
        })
    }

    /// Finishes each document the erasure marked: removes its Media files'
    /// bytes, then deletes its Artifacts and rows and counts it deleted. A
    /// hold or retention placed since keeps it, anonymised, and shows it
    /// again: the certificate says kept, with the reason.
    fn after_erase<'a>(
        &'a self,
        client: &'a mut Client,
        scope: &'a Scope,
        subject: &'a Subject,
        grant: &'a Erasure,
        holds: &'a Holds,
        progress: &'a dyn Progress,
    ) -> HandlerFuture<'a, ()> {
        Box::pin(async move {
            grant.check_subject(scope, subject)?;
            let (tenant, person) = (scope.tenant(), subject.person.get());
            let rows = client
                .query(
                    &format!("SELECT d.id,d.company_id,d.erased_at IS NOT NULL FROM core_documents d WHERE d.tenant_id=$1 AND d.id IN ({LINKED}) ORDER BY d.id"),
                    &[&tenant, &person],
                )
                .await?;
            let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
            let files = files_of(&*client, tenant, &ids).await?;
            // First, while the person link still vouches for them.
            for document in files.values() {
                for file in &document.subjects {
                    remove_bytes(client, scope, grant, file).await?;
                }
            }
            let held = held(holds, grant, &files).await?;
            let bound = bound()?;
            for row in rows {
                let (id, company, marked): (i64, i64, bool) = (row.get(0), row.get(1), row.get(2));
                if !marked {
                    continue;
                }
                let company = OrganisationId::new(company).map_err(|_| Error::Internal)?;
                let document = &files[&id];
                let artifacts = document.artifact_ids();
                let mut kept = held.get(&id).map(|hold| Kept::held(RECORD, id, *hold));
                if kept.is_none() {
                    if let Err(error) = bound
                        .artifacts
                        .for_owner(scope, company, &FILES)
                        .ensure_unprotected(&*client, &artifacts, false)
                        .await
                    {
                        kept = protected(&*client, tenant, id, &artifacts, &error).await?;
                        if kept.is_none() {
                            return Err(artifact_error(error));
                        }
                    }
                }
                if kept.is_none() {
                    for (artifact, file) in &document.artifacts {
                        if let Err(error) = bound
                            .artifacts
                            .for_erasure(scope, company, &FILES, grant, file)
                            .delete(client, artifact)
                            .await
                        {
                            kept = protected(&*client, tenant, id, &artifacts, &error).await?;
                            if kept.is_none() {
                                return Err(artifact_error(error));
                            }
                            break;
                        }
                    }
                }
                let mut step = Erased::default();
                let mut tx = bound
                    .boundary
                    .begin(client, scope.mutation_context("core.document.erase_finish"))
                    .await?;
                match kept {
                    Some(kept) => {
                        crate::deletion::restore(&mut tx, tenant, id).await?;
                        step.kept.push(kept);
                    }
                    None => {
                        crate::files::finalize_document_delete(&mut tx, tenant, id).await?;
                        step.count(RECORD, Action::Deleted, 1);
                    }
                }
                tx.commit().await?;
                progress.record(step).await?;
            }
            Ok(())
        })
    }
}

/// Removes the person's link from a document another record shares.
async fn unlink(
    tx: &mut MutationTransaction<'_>,
    tenant: i64,
    document: i64,
    person: i64,
) -> Result<()> {
    tx.mutate(move |sql| Box::pin(async move {
        let changes = sql.query(
            "DELETE FROM core_document_links WHERE tenant_id=$1 AND document_id=$2 AND record_kind='person' AND record_id=$3::bigint::text RETURNING id,record_kind,record_id",
            &[&tenant, &document, &person],
        ).await?.iter().map(|row| Change {
            kind: LINK_RECORD.into(), id: row.get(0),
            old: json!({"document_id":document,"record":{"kind":row.get::<_,String>(1),"id":row.get::<_,String>(2)}}),
            new: json!({}),
        }).collect();
        Ok(((), changes))
    })).await
}

/// Replaces every personal value of the document, its revisions and their
/// ledger rows, and marks it as erased (deleting, finished by
/// `after_erase`, never by the delete job), in the erasing transaction. The
/// changes record `[erased]` where a value was.
async fn anonymise(
    tx: &mut MutationTransaction<'_>,
    tenant: i64,
    document: i64,
) -> Result<Vec<ErasedRecord>> {
    tx.mutate(move |sql| Box::pin(async move {
        let mut changes = Vec::new();
        let erased = |value: Option<String>| value.map(|_| json!(ERASED)).unwrap_or(Value::Null);
        for row in sql.query(
            "SELECT id,file_name FROM core_document_files WHERE tenant_id=$1 AND id IN (SELECT file_id FROM core_document_revisions WHERE tenant_id=$1 AND document_id=$2 AND file_id IS NOT NULL) ORDER BY id FOR UPDATE",
            &[&tenant, &document],
        ).await? {
            let (id, file_name): (i64, String) = (row.get(0), row.get(1));
            sql.execute("UPDATE core_document_files SET file_name=$3 WHERE tenant_id=$1 AND id=$2", &[&tenant, &id, &ERASED]).await?;
            changes.push(Change { kind: FILE_RECORD.into(), id, old: json!({"file_name": file_name}), new: json!({"file_name": ERASED}) });
        }
        for row in sql.query(
            "SELECT id,name,file_name,external_url,change_log,label FROM core_document_revisions WHERE tenant_id=$1 AND document_id=$2 ORDER BY id FOR UPDATE",
            &[&tenant, &document],
        ).await? {
            let id: i64 = row.get(0);
            let old: Vec<Option<String>> = (1..6).map(|i| row.get(i)).collect();
            sql.execute(
                "UPDATE core_document_revisions SET name=$3,file_name=CASE WHEN file_name IS NULL THEN NULL ELSE $3 END,external_url=CASE WHEN external_url IS NULL THEN NULL ELSE $4 END,change_log=$3,label=NULL WHERE tenant_id=$1 AND id=$2",
                &[&tenant, &id, &ERASED, &ERASED_URL],
            ).await?;
            let old_values = json!(REVISION_FIELDS.iter().zip(&old).map(|(f, v)| ((*f).to_owned(), json!(v))).collect::<serde_json::Map<_, _>>());
            let new_values = json!(REVISION_FIELDS.iter().zip(old).map(|(f, v)| ((*f).to_owned(), erased(v))).collect::<serde_json::Map<_, _>>());
            changes.push(Change { kind: REVISION_RECORD.into(), id, old: old_values, new: new_values });
        }
        let row = sql.query_one(
            "SELECT name,external_url,custom_fields,deleting_at IS NOT NULL FROM core_documents WHERE tenant_id=$1 AND id=$2",
            &[&tenant, &document],
        ).await?;
        let (name, url, custom, deleting): (String, Option<String>, Value, bool) = (row.get(0), row.get(1), row.get(2), row.get(3));
        sql.execute(
            "UPDATE core_documents SET name=$3,external_url=CASE WHEN external_url IS NULL THEN NULL ELSE $4 END,custom_fields='{}'::jsonb,deleting_at=coalesce(deleting_at,now()),erased_at=now(),version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2",
            &[&tenant, &document, &ERASED, &ERASED_URL],
        ).await?;
        let custom_erased = if custom.as_object().is_some_and(|o| !o.is_empty()) { json!(ERASED) } else { json!({}) };
        changes.push(Change {
            kind: RECORD.into(), id: document,
            old: json!({"name": name, "external_url": url, "custom_fields": custom, "deleting": deleting}),
            new: json!({"name": ERASED, "external_url": erased(url.clone()), "custom_fields": custom_erased, "deleting": true}),
        });
        let records = changes
            .iter()
            .filter(|c| c.kind != LINK_RECORD)
            .map(|c| ErasedRecord { kind: c.kind.clone(), id: c.id, person: None })
            .collect();
        Ok((records, changes))
    })).await
}

macro_rules! provenance {
    () => {
        Provenance {
            module: crate::MODULE,
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! { Contribution::new(PERSONAL_DATA, "core/document.documents", provenance!(),
&PersonalData { label: "Documents related to the person", kinds: &[RECORD, REVISION_RECORD, FILE_RECORD, ATTACHMENT_KIND, ASSET_KIND], person_subjects: &[], handler: &DOCUMENTS }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/document.document", provenance!(),
&RecordKind { kind: RECORD, fields: &DOCUMENT_FIELDS, subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/document.revision", provenance!(),
&RecordKind { kind: REVISION_RECORD, fields: &REVISION_FIELDS, subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/document.file", provenance!(),
&RecordKind { kind: FILE_RECORD, fields: &FILE_FIELDS, subject_identifier: false, person: false }) }
