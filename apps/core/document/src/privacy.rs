//! Documents' personal-data store (ADR 05, `base/personal_data`): the
//! documents related to a person (a `person` link, such as an identity scan
//! or a signed contract), with their revisions and files (#583).
//!
//! - **Locate** counts the person's documents that are not being deleted.
//! - **Export** lists each with its revisions, and every clean file, read
//!   through `Artifacts::for_disclosure` (the owner vouches the file's subject
//!   is a revision of a document related to the request's person).
//! - **Erase** keeps a document under a legal hold (on the person or the
//!   document) or whose file Base still retains or holds, with its reason. A
//!   document also related to another record is that record's too: only its
//!   link to the person is removed, and it is kept as shared. Every other
//!   document is anonymised in the erasing transaction (its name, link,
//!   custom fields, its revisions' names, file names, change logs and labels,
//!   and its ledger rows' file names) and marked as deleting with its
//!   durable delete job (`deletion.rs`), which then deletes its files and
//!   rows as for any delete.
use crate::files::FILES;
use crate::{FILE_RECORD, LINK_RECORD, RECORD, REVISION_RECORD};
use bilibi_artifacts::Artifacts;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_party_ids::OrganisationId;
use bilibi_personal_data::{
    Action, Disclosure, Erased, ErasedRecord, Erasure, Exported, File, FileRef, Handler,
    HandlerFuture, Holds, Kept, KeptReason, KindCount, Located, PersonalData, RecordKind, Subject,
    ERASED, PERSONAL_DATA, RECORD_KINDS,
};
use bilibi_tenancy::{ProofClient, Scope};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use tokio_postgres::Client;

const DOCUMENT_FIELDS: [&str; 3] = ["name", "external_url", "custom_fields"];
const REVISION_FIELDS: [&str; 5] = ["name", "file_name", "external_url", "change_log", "label"];
const FILE_FIELDS: [&str; 1] = ["file_name"];
/// What an erased link reads: the column must stay an https URL until the
/// delete job removes the row, and `.invalid` never resolves.
const ERASED_URL: &str = "https://erased.invalid/";
/// The person's documents, not being deleted.
const LINKED: &str = "SELECT d.id FROM core_documents d WHERE d.tenant_id=$1 AND d.deleting_at IS NULL AND EXISTS (SELECT 1 FROM core_document_links l WHERE l.tenant_id=d.tenant_id AND l.document_id=d.id AND l.record_kind='person' AND l.record_id=$2::bigint::text)";

struct Bound {
    artifacts: Arc<Artifacts>,
    queue: Arc<bilibi_queue::Queue>,
}
static BOUND: OnceLock<Bound> = OnceLock::new();
/// Bound once by the Documents service factory.
pub(crate) fn bind(
    artifacts: Arc<Artifacts>,
    queue: Arc<bilibi_queue::Queue>,
) -> bilibi_module_registry::Result<()> {
    BOUND
        .set(Bound { artifacts, queue })
        .map_err(|_| RegistryError::Service("duplicate document privacy binding".into()))
}
fn bound() -> Result<&'static Bound> {
    BOUND.get().ok_or(Error::Internal)
}

/// Whether the ledger `subject` is the file of a revision of a document
/// related to `person`, for an access export (`Owner::disclosure_subject`).
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
                         FROM core_documents d WHERE d.tenant_id=$1 AND d.id IN ({LINKED})"
                    ),
                    &[&tenant, &person],
                )
                .await?
                .get(0);
            let files = client
                .query(
                    &format!(
                        "SELECT r.artifact_id,r.document_id,r.revision,r.file_name,r.content_type FROM core_document_revisions r
                         WHERE r.tenant_id=$1 AND r.document_id IN ({LINKED}) AND r.artifact_id IS NOT NULL AND r.scan_status='clean'
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
            let held = holds.records(grant, RECORD, &ids).await?;
            let person_hold = holds.person_hold(grant).await?;
            let mut erased = Erased::default();
            let kept = |id: i64, reason, until, hold| Kept {
                kind: RECORD.into(),
                id: id.to_string(),
                reason,
                until,
                hold,
            };
            for row in rows {
                let (id, company, shared): (i64, i64, bool) = (row.get(0), row.get(1), row.get(2));
                if let Some(hold) = person_hold.or_else(|| held.get(&id).copied()) {
                    erased.kept.push(Kept::held(RECORD, id, hold));
                    continue;
                }
                if shared {
                    unlink(tx, tenant, id, person).await?;
                    erased.kept.push(kept(id, KeptReason::Shared, None, None));
                    continue;
                }
                let files: Vec<String> = tx
                    .sql()
                    .query(
                        "SELECT artifact_id FROM core_document_revisions WHERE tenant_id=$1 AND document_id=$2 AND artifact_id IS NOT NULL ORDER BY artifact_id",
                        &[&tenant, &id],
                    )
                    .await?
                    .iter()
                    .map(|row| row.get(0))
                    .collect();
                let company = OrganisationId::new(company).map_err(|_| Error::Internal)?;
                match bound()?
                    .artifacts
                    .for_owner(scope, company, &FILES)
                    .ensure_unprotected(tx.sql(), &files, true)
                    .await
                {
                    Ok(()) => {}
                    Err(bilibi_artifacts::Error::LegalHold) => {
                        erased
                            .kept
                            .push(kept(id, KeptReason::LegalHold, None, None));
                        continue;
                    }
                    Err(bilibi_artifacts::Error::Retained) => {
                        let until: Option<String> = tx.sql().query_one(
                            "SELECT max(retain_until)::text FROM base_artifacts WHERE tenant_id=$1 AND public_id = ANY($2)",
                            &[&tenant, &files],
                        ).await?.get(0);
                        erased
                            .kept
                            .push(kept(id, KeptReason::RetainedUntil, until, None));
                        continue;
                    }
                    Err(error) => return Err(artifact_error(error)),
                }
                erased.records.extend(anonymise(tx, tenant, id).await?);
                crate::deletion::enqueue(&bound()?.queue, tx, tenant, id, None).await?;
                erased.count(RECORD, Action::Deleted, 1);
            }
            Ok(erased)
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
/// ledger rows, and marks it as deleting, in the erasing transaction. The
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
            "SELECT name,external_url,custom_fields FROM core_documents WHERE tenant_id=$1 AND id=$2",
            &[&tenant, &document],
        ).await?;
        let (name, url, custom): (String, Option<String>, Value) = (row.get(0), row.get(1), row.get(2));
        sql.execute(
            "UPDATE core_documents SET name=$3,external_url=CASE WHEN external_url IS NULL THEN NULL ELSE $4 END,custom_fields='{}'::jsonb,deleting_at=now(),version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2",
            &[&tenant, &document, &ERASED, &ERASED_URL],
        ).await?;
        let custom_erased = if custom.as_object().is_some_and(|o| !o.is_empty()) { json!(ERASED) } else { json!({}) };
        changes.push(Change {
            kind: RECORD.into(), id: document,
            old: json!({"name": name, "external_url": url, "custom_fields": custom, "deleting": false}),
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
&PersonalData { label: "Documents related to the person", kinds: &[RECORD, REVISION_RECORD, FILE_RECORD], person_subjects: &[], handler: &DOCUMENTS }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/document.document", provenance!(),
&RecordKind { kind: RECORD, fields: &DOCUMENT_FIELDS, subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/document.revision", provenance!(),
&RecordKind { kind: REVISION_RECORD, fields: &REVISION_FIELDS, subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/document.file", provenance!(),
&RecordKind { kind: FILE_RECORD, fields: &FILE_FIELDS, subject_identifier: false, person: false }) }
