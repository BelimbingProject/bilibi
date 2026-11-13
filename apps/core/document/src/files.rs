//! Document files: the Artifacts owner `core.document` and the ledger
//! (`core_document_files`) it answers from. A file's ledger row is committed
//! before `base/artifacts` reserves it, so every artifact of this owner has
//! a row naming who stored it and what became of it; the reference subject
//! is the row's random `subject`, never a document id, because a file is
//! stored before its document exists. Attached rows stay attached until the
//! complete Artifact batch succeeds, so retained documents remain readable.
//!
//! The owner's rule, on top of Artifacts' own (the actor's company is the
//! file's company):
//! - `Create`: `core.document.manage`, a row this actor reserved;
//! - `Read`: `core.document.download`, and a revision's file the reader may
//!   see (`core.document.view`), or a file this actor stored and has not
//!   attached yet; for a portal customer, `portal.document.view` and the
//!   clean latest file of a document shared with them (`customer.rs`);
//! - `Delete`: `core.document.manage`, either a file this actor stored and
//!   never attached or an attached file whose document is being deleted;
//! - `Retain`: `core.document.manage`, a revision's file;
//! - `Purge`: `core.document.manage` maintenance.
use crate::{
    model::{self, ScanStatus, Upload},
    store,
};
use crate::{
    Document, File, StoredFile, WriteError, WriteResult, ALLOWED_TYPES, DOWNLOAD, FILE_RECORD,
    MANAGE, MAX_UPLOAD_BYTES, VIEW,
};
use bilibi_artifacts::{Artifacts, Operation, Owner, Reference};
use bilibi_audit::{record_action, Attribution};
use bilibi_authz::{authorize_proof, holds_proof};
use bilibi_database::{Change, Error, MutationBoundary, MutationTransaction, Result};
use bilibi_media::{
    AttachmentSubject, Media, Rules as MediaRules, ScanState as MediaScanState, SubjectAction,
    SubjectOwner, Upload as MediaUpload,
};
use bilibi_party_ids::OrganisationId;
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{AccountKind, PartyProofs, ProofClient, ProofFuture, ReadClient, Scope};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use tokio_postgres::{Client, Row};

/// The stable Artifacts owner id; Base also binds the adapter type.
pub const ARTIFACT_OWNER: &str = "core.document";
/// The artifact kind of every document file.
pub const FILE_KIND: &str = "document-file";
/// The audit event of a refused download or preview.
pub(crate) const DOWNLOAD_REFUSED: &str = "core.document.download_refused";
/// Media's private staging subject. A staging file is never a document grant.
pub const MEDIA_SUBJECT: &str = "core.document.upload";

pub struct DocumentFiles;
/// The one adapter instance Base and the facade share.
pub static FILES: DocumentFiles = DocumentFiles;
static PARTIES: OnceLock<Arc<PartyProofs>> = OnceLock::new();

pub(crate) fn bind(parties: Arc<PartyProofs>) -> bilibi_module_registry::Result<()> {
    PARTIES.set(parties).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate document files binding".into())
    })
}

impl SubjectOwner for DocumentFiles {
    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        action: SubjectAction,
        subject_id: &'a str,
    ) -> ProofFuture<'a, ()> {
        Box::pin(async move {
            let scanner = scope.principal() == Some(crate::scan::PRINCIPAL);
            authorize_proof(
                client,
                scope,
                if scanner {
                    crate::contributions::SCAN_SETTLE
                } else {
                    MANAGE
                },
            )
            .await?;
            let subject = uuid::Uuid::parse_str(subject_id)
                .map_err(|_| Error::NotFound)?
                .to_string();
            let row = client
                .query_opt(
                    &format!("SELECT {COLUMNS} FROM core_document_files WHERE tenant_id=$1 AND subject=$2::text::uuid"),
                    &[&scope.tenant(), &subject],
                )
                .await?
                .map(|row| from_row(&row))
                .ok_or(Error::NotFound)?;
            let allowed = row.state == "reserved"
                && row.stored_by == scope.user()
                && scope.current_company().map(|company| company.get()) == Some(row.company_id)
                && matches!(
                    action,
                    SubjectAction::Upload
                        | SubjectAction::Download
                        | SubjectAction::Inspect
                        | SubjectAction::Discard
                );
            let scanner_allowed = scanner
                && row.state == "attached"
                && row.artifact_id.is_none()
                && row.media_public_id.is_some()
                && matches!(action, SubjectAction::Download | SubjectAction::Inspect);
            if allowed || scanner_allowed {
                Ok(())
            } else {
                Err(Error::Forbidden)
            }
        })
    }
}

pub(crate) const COLUMNS: &str = "id,company_id,subject::text AS subject,artifact_id,media_public_id,file_name,content_type,byte_size,sha256,scan_status,state,stored_by";

/// One ledger row.
pub(crate) struct Ledger {
    pub(crate) id: i64,
    pub(crate) company_id: i64,
    subject: String,
    pub(crate) artifact_id: Option<String>,
    media_public_id: Option<String>,
    file_name: String,
    content_type: String,
    byte_size: i64,
    sha256: String,
    scan_status: String,
    pub(crate) state: String,
    stored_by: i64,
}
pub(crate) fn from_row(row: &Row) -> Ledger {
    Ledger {
        id: row.get("id"),
        company_id: row.get("company_id"),
        subject: row.get("subject"),
        artifact_id: row.get("artifact_id"),
        media_public_id: row.get("media_public_id"),
        file_name: row.get("file_name"),
        content_type: row.get("content_type"),
        byte_size: row.get("byte_size"),
        sha256: row.get("sha256"),
        scan_status: row.get("scan_status"),
        state: row.get("state"),
        stored_by: row.get("stored_by"),
    }
}
/// The values audit keeps for a ledger row: no content, no storage path.
pub(crate) fn audit_values(row: &Ledger) -> Value {
    json!({
        "artifact_id": row.artifact_id,
        "media_public_id": row.media_public_id,
        "file_name": row.file_name,
        "content_type": row.content_type,
        "byte_size": row.byte_size,
        "sha256": row.sha256,
        "scan_status": row.scan_status,
        "state": row.state,
        "stored_by": row.stored_by,
    })
}

impl Owner for DocumentFiles {
    fn owner_id(&self) -> &'static str {
        ARTIFACT_OWNER
    }
    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        company: OrganisationId,
        operation: Operation,
        reference: Option<&'a Reference>,
    ) -> ProofFuture<'a, ()> {
        Box::pin(async move {
            let parties = PARTIES.get().ok_or(Error::Forbidden)?;
            match operation {
                Operation::Create | Operation::Retain
                    if scope.principal() == Some(crate::scan::PRINCIPAL) =>
                {
                    parties
                        .organisation_name(client, scope, company)
                        .await?
                        .ok_or(Error::NotFound)?;
                }
                Operation::Delete
                    if scope.principal() == Some(crate::discard::PRINCIPAL)
                        || scope.principal() == Some(crate::deletion::PRINCIPAL) =>
                {
                    parties
                        .organisation_name(client, scope, company)
                        .await?
                        .ok_or(Error::NotFound)?;
                }
                Operation::Create | Operation::Delete | Operation::Retain => {
                    parties.organisation(client, scope, company).await?;
                }
                Operation::Read | Operation::Purge => {
                    parties
                        .organisation_name(client, scope, company)
                        .await?
                        .ok_or(Error::NotFound)?;
                }
            }
            let Some(reference) = reference else {
                // Purge: maintenance of every document file in the tenant.
                return authorize_proof(client, scope, MANAGE).await;
            };
            if reference.kind != FILE_KIND {
                return Err(Error::NotFound);
            }
            let subject = uuid::Uuid::parse_str(&reference.subject)
                .map_err(|_| Error::NotFound)?
                .to_string();
            let row = client
                .query_opt(
                    &format!("SELECT {COLUMNS} FROM core_document_files WHERE tenant_id=$1 AND company_id=$2 AND subject=$3::text::uuid"),
                    &[&scope.tenant(), &company.get(), &subject],
                )
                .await?
                .map(|row| from_row(&row))
                .ok_or(Error::NotFound)?;
            let own = row.stored_by == scope.user();
            let allowed = match operation {
                Operation::Create => {
                    if scope.principal() == Some(crate::scan::PRINCIPAL)
                        && row.state == "attached"
                        && row.artifact_id.is_none()
                        && row.media_public_id.is_some()
                    {
                        authorize_proof(client, scope, crate::contributions::SCAN_SETTLE).await?;
                        true
                    } else {
                        authorize_proof(client, scope, MANAGE).await?;
                        row.state == "reserved" && own
                    }
                }
                // A portal customer (`Artifacts::for_portal`): a clean latest
                // file of a document shared with the customer now.
                Operation::Read if scope.actor().kind() == AccountKind::Customer => {
                    row.state == "attached"
                        && crate::customer::serves_file(client, scope, row.artifact_id.as_deref())
                            .await?
                }
                Operation::Read => {
                    if scope.principal() == Some(crate::scan::PRINCIPAL)
                        && row.state == "attached"
                        && row.artifact_id.is_none()
                        && row.media_public_id.is_some()
                    {
                        authorize_proof(client, scope, crate::contributions::SCAN_SETTLE).await?;
                        true
                    } else {
                        authorize_proof(client, scope, DOWNLOAD).await?;
                        match row.state.as_str() {
                            "stored" => own,
                            "attached" => {
                                authorize_proof(client, scope, VIEW).await?;
                                let document = client
                                .query_opt(
                                    "SELECT r.document_id FROM core_document_revisions r JOIN core_documents d ON d.tenant_id=r.tenant_id AND d.id=r.document_id WHERE r.tenant_id=$1 AND r.artifact_id=$2 AND d.deleting_at IS NULL",
                                    &[&scope.tenant(), &row.artifact_id],
                                )
                                .await?
                                .map(|record| record.get::<_, i64>(0));
                                match document {
                                    Some(document) => {
                                        store::permission(client, scope, document, "download")
                                            .await?
                                    }
                                    None => false,
                                }
                            }
                            _ => false,
                        }
                    }
                }
                // The cleaner deletes only a file its job already took out of
                // reach of every write (README, Files and scanning).
                Operation::Delete if scope.principal() == Some(crate::discard::PRINCIPAL) => {
                    authorize_proof(client, scope, crate::contributions::DISCARD_SETTLE).await?;
                    row.state == "discarding"
                }
                Operation::Delete => {
                    let deleter = scope.principal() == Some(crate::deletion::PRINCIPAL);
                    authorize_proof(
                        client,
                        scope,
                        if deleter {
                            crate::contributions::DELETION_SETTLE
                        } else {
                            MANAGE
                        },
                    )
                    .await?;
                    match row.state.as_str() {
                        // The deleter only ever finishes a delete a person
                        // confirmed: an attached file of a deleting document.
                        "reserved" | "stored" if deleter => false,
                        "reserved" | "stored" => own,
                        "attached" => client
                            .query_opt(
                                "SELECT 1 FROM core_document_revisions r JOIN core_documents d ON d.tenant_id=r.tenant_id AND d.id=r.document_id WHERE r.tenant_id=$1 AND r.artifact_id=$2 AND d.deleting_at IS NOT NULL",
                                &[&scope.tenant(), &row.artifact_id],
                            )
                            .await?
                            .is_some(),
                        _ => false,
                    }
                }
                Operation::Retain => {
                    if scope.principal() == Some(crate::scan::PRINCIPAL)
                        && row.state == "attached"
                        && row.artifact_id.is_none()
                        && row.media_public_id.is_some()
                    {
                        authorize_proof(client, scope, crate::contributions::SCAN_SETTLE).await?;
                        true
                    } else {
                        authorize_proof(client, scope, MANAGE).await?;
                        if row.state != "attached" {
                            false
                        } else {
                            let document = client
                            .query_opt(
                                "SELECT r.document_id FROM core_document_revisions r JOIN core_documents d ON d.tenant_id=r.tenant_id AND d.id=r.document_id WHERE r.tenant_id=$1 AND r.artifact_id=$2 AND d.deleting_at IS NULL",
                                &[&scope.tenant(), &row.artifact_id],
                            )
                            .await?
                            .map(|record| record.get::<_, i64>(0));
                            match document {
                                Some(document) => {
                                    store::permission(client, scope, document, "manage").await?
                                }
                                None => false,
                            }
                        }
                    }
                }
                Operation::Purge => false,
            };
            if allowed {
                Ok(())
            } else {
                Err(Error::Forbidden)
            }
        })
    }
}

/// Maps a refusal from `base/artifacts` to the facade's errors. Storage
/// faults are logged by code only, never with a path or content.
pub(crate) fn artifact_error(error: bilibi_artifacts::Error) -> WriteError {
    use bilibi_artifacts::Error as A;
    match error {
        A::Database(Error::NotFound) | A::NotFound => WriteError::NotFound,
        A::Database(Error::Forbidden) => WriteError::Forbidden,
        A::Database(error) => WriteError::Access(error),
        A::LegalHold => WriteError::LegalHold,
        A::Retained => WriteError::Retained,
        error => {
            eprintln!(
                "error: a document file could not be handled as a private document: {}",
                error.code()
            );
            WriteError::Access(Error::Refused(
                "Files cannot be kept or read: private documents are not available on this server. Ask your administrator.".into(),
            ))
        }
    }
}

fn scan_status(state: MediaScanState) -> ScanStatus {
    match state {
        MediaScanState::Pending => ScanStatus::Pending,
        MediaScanState::Clean => ScanStatus::Clean,
        MediaScanState::Infected => ScanStatus::Quarantined,
        MediaScanState::Failed => ScanStatus::Failed,
        MediaScanState::Unscanned => ScanStatus::Unscanned,
    }
}

fn media_error(error: bilibi_media::Error) -> WriteError {
    use bilibi_media::Error as M;
    match error {
        M::ScanningRequired | M::NoScanner | M::NotScanned => WriteError::field(
            "file",
            "could not be checked for malware; ask an administrator to check the scanner",
        ),
        M::ScanPending => WriteError::field("file", "is still being checked for malware"),
        M::Quarantined => WriteError::field("file", "failed the malware check"),
        M::ScanFailed => WriteError::field(
            "file",
            "could not be checked for malware; try again after the scanner is available",
        ),
        M::InvalidSize => WriteError::field("file", "is empty or too large"),
        M::DisallowedType | M::InvalidUpload | M::MimeDetectionUnavailable => {
            WriteError::field("file", "is not a file type Documents can accept")
        }
        M::Database(error) => WriteError::Access(error),
        _ => WriteError::field("file", "could not be stored"),
    }
}

/// The upload limit: the tenant's, never above the operator's.
async fn upload_limit(client: &Client, scope: &Scope, settings: &Settings) -> Result<u64> {
    let tenant = settings
        .get(client, scope, Target::Tenant, MAX_UPLOAD_BYTES)
        .await?
        .value
        .as_u64()
        .ok_or(Error::Internal)?;
    let operator = settings
        .get_global(client, "artifacts.max_bytes")
        .await?
        .as_u64()
        .unwrap_or(tenant);
    Ok(tenant.min(operator))
}

/// Checks the upload, commits its reserved ledger row, stores the bytes
/// through `base/artifacts` and marks the row stored.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store(
    client: &mut Client,
    scope: &Scope,
    boundary: &MutationBoundary,
    artifacts: &Artifacts,
    media: &Media,
    settings: &Settings,
    queue: &bilibi_queue::Queue,
    upload: Upload<'_>,
) -> WriteResult<StoredFile> {
    authorize_proof(&*client, scope, MANAGE).await?;
    let company = scope.current_company().ok_or(WriteError::field(
        "file",
        "can only be stored by someone signed in to a company",
    ))?;
    let mut errors = Vec::new();
    let file_name = model::file_name(&mut errors, upload.file_name);
    if upload.bytes.is_empty() {
        errors.push(model::FieldError::new("file", "is empty"));
    }
    if !errors.is_empty() {
        return Err(WriteError::Invalid(errors));
    }
    let max_bytes = upload_limit(client, scope, settings).await?;
    if upload.bytes.len() as u64 > max_bytes {
        return Err(WriteError::TooLarge { max_bytes });
    }
    let allowed = settings
        .get(&*client, scope, Target::Tenant, ALLOWED_TYPES)
        .await?
        .value;
    let content_type = model::type_of(&file_name)
        .filter(|t| {
            allowed
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(t)))
        })
        .ok_or_else(|| WriteError::field("file", "is not a type of file documents accept"))?;
    if !model::content_matches(content_type, upload.bytes) {
        return Err(WriteError::field(
            "file",
            "does not contain what its name says it is",
        ));
    }
    let sha256: String = Sha256::digest(upload.bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let byte_size = upload.bytes.len() as i64;

    let subject = uuid::Uuid::new_v4().to_string();
    let mut tx = boundary
        .begin(client, scope.mutation_context("core.document.store_file"))
        .await?;
    let (tenant, user, name, kind, hash) = (
        scope.tenant(),
        scope.user(),
        file_name.clone(),
        content_type.to_owned(),
        sha256.clone(),
    );
    let reserved_subject = subject.clone();
    let reserved = tx
        .mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_one(
                        &format!("INSERT INTO core_document_files(tenant_id,company_id,subject,file_name,content_type,byte_size,sha256,state,stored_by) VALUES($1,$2,$3::text::uuid,$4,$5,$6,$7,'reserved',$8) RETURNING {COLUMNS}"),
                        &[&tenant, &company.get(), &reserved_subject, &name, &kind, &byte_size, &hash, &user],
                    )
                    .await?;
                let row = from_row(&row);
                let change = Change { kind: FILE_RECORD.into(), id: row.id, old: json!({}), new: audit_values(&row) };
                Ok((row, vec![change]))
            })
        })
        .await?;
    crate::discard::schedule(queue, &mut tx, tenant, reserved.id).await?;
    tx.commit().await?;

    let staged = media
        .upload_attachment(
            client,
            scope,
            AttachmentSubject {
                kind: MEDIA_SUBJECT,
                id: &subject,
            },
            MediaUpload {
                filename: file_name.clone(),
                bytes: upload.bytes.to_vec(),
            },
            MediaRules {
                max_bytes: Some(max_bytes),
                allowed_mime_types: Some(vec![content_type.to_owned()]),
            },
        )
        .await
        .map_err(media_error)?;
    let mut verdict = staged.scan_state;
    // A quick verdict keeps the existing clean upload path. Slow scans still
    // produce a document: the later Queue settlement promotes only clean bytes.
    for _ in 0..120 {
        if verdict != MediaScanState::Pending {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        verdict = media
            .inspect_attachment(client, scope, &staged.public_id)
            .await
            .map_err(media_error)?
            .scan_state;
    }
    let metadata = if verdict == MediaScanState::Clean {
        let clean = media
            .download_attachment(client, scope, &staged.public_id)
            .await
            .map_err(media_error)?;
        let metadata = artifacts
            .for_owner(scope, company, &FILES)
            .put(
                client,
                Reference {
                    subject: subject.clone(),
                    kind: FILE_KIND.into(),
                },
                &clean.bytes,
                content_type,
            )
            .await
            .map_err(|error| match error {
                bilibi_artifacts::Error::Invalid => WriteError::TooLarge { max_bytes },
                error => artifact_error(error),
            })?;
        if let Err(error) = media.discard_draft(client, scope, &staged.public_id).await {
            eprintln!("warning: a scanned document staging file awaits Media cleanup: {error}");
        }
        Some(metadata)
    } else {
        None
    };
    let scan_status = scan_status(verdict);
    let file_token = metadata
        .as_ref()
        .map(|metadata| metadata.id.clone())
        .unwrap_or_else(|| format!("upload:{subject}"));
    let media_public_id = metadata.is_none().then(|| staged.public_id.clone());

    let mut tx = boundary
        .begin(client, scope.mutation_context("core.document.store_file"))
        .await?;
    let artifact_id = metadata.as_ref().map(|metadata| metadata.id.clone());
    let id = reserved.id;
    let status = scan_status.code();
    tx.mutate(move |sql| {
        Box::pin(async move {
            let row = sql
                .query_one(
                    &format!("UPDATE core_document_files SET artifact_id=$3,media_public_id=$4,scan_status=$5,state='stored' WHERE tenant_id=$1 AND id=$2 AND state='reserved' RETURNING {COLUMNS}"),
                    &[&tenant, &id, &artifact_id, &media_public_id, &status],
                )
                .await?;
            let row = from_row(&row);
            let change = Change { kind: FILE_RECORD.into(), id: row.id, old: audit_values(&reserved), new: audit_values(&row) };
            Ok(((), vec![change]))
        })
    })
    .await?;
    tx.commit().await?;
    Ok(StoredFile {
        artifact_id: file_token,
        scan_status,
        file_name,
        content_type: content_type.to_owned(),
        byte_size,
        sha256,
    })
}

/// A file a write is about to attach: stored by this actor under their
/// company and not attached yet, locked when `lock` (a commit). Refusals
/// name `field`.
pub(crate) async fn take(
    client: &dyn ReadClient,
    scope: &Scope,
    artifact_id: &str,
    field: &'static str,
    lock: bool,
) -> WriteResult<TakenFile> {
    let row = if let Some(upload) = artifact_id.strip_prefix("upload:") {
        let subject = uuid::Uuid::parse_str(upload)
            .map_err(|_| WriteError::field(field, "is not a file you stored"))?
            .to_string();
        client
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM core_document_files WHERE tenant_id=$1 AND subject=$2::text::uuid{}",
                    if lock { " FOR UPDATE" } else { "" }
                ),
                &[&scope.tenant(), &subject],
            )
            .await?
    } else {
        client
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM core_document_files WHERE tenant_id=$1 AND artifact_id=$2{}",
                    if lock { " FOR UPDATE" } else { "" }
                ),
                &[&scope.tenant(), &artifact_id],
            )
            .await?
    }
    .map(|row| from_row(&row));
    match row {
        Some(row)
            if row.state == "stored"
                && row.stored_by == scope.user()
                && scope.current_company().map(|o| o.get()) == Some(row.company_id) =>
        {
            Ok(TakenFile(row))
        }
        Some(row) if row.state == "attached" => {
            Err(WriteError::field(field, "is already a revision's file"))
        }
        _ => Err(WriteError::field(field, "is not a file you stored")),
    }
}
/// A locked, stored file a write attaches in its `mutate`.
pub(crate) struct TakenFile(Ledger);
impl TakenFile {
    pub(crate) fn id(&self) -> i64 {
        self.0.id
    }
    pub(crate) fn artifact_id(&self) -> Option<&str> {
        self.0.artifact_id.as_deref()
    }
    pub(crate) fn token(&self) -> String {
        self.0
            .artifact_id
            .clone()
            .unwrap_or_else(|| format!("upload:{}", self.0.subject))
    }
    pub(crate) fn scan_status(&self) -> ScanStatus {
        ScanStatus::parse(&self.0.scan_status).unwrap_or(ScanStatus::Failed)
    }
    pub(crate) fn file_name(&self) -> &str {
        &self.0.file_name
    }
    pub(crate) fn content_type(&self) -> &str {
        &self.0.content_type
    }
    pub(crate) fn byte_size(&self) -> i64 {
        self.0.byte_size
    }
    pub(crate) fn sha256(&self) -> &str {
        &self.0.sha256
    }
    /// Marks the file attached; for the write's `mutate`.
    pub(crate) async fn attach(
        &self,
        sql: &tokio_postgres::Transaction<'_>,
        tenant: i64,
    ) -> Result<Change> {
        let row = sql
            .query_one(
                &format!("UPDATE core_document_files SET state='attached',attached_at=now() WHERE tenant_id=$1 AND id=$2 AND state='stored' RETURNING {COLUMNS}"),
                &[&tenant, &self.0.id],
            )
            .await?;
        Ok(Change {
            kind: FILE_RECORD.into(),
            id: self.0.id,
            old: audit_values(&self.0),
            new: audit_values(&from_row(&row)),
        })
    }
}

/// The document whose revision holds `artifact_id`, for an attached file.
pub(crate) async fn attached_document(
    tx: &tokio_postgres::Transaction<'_>,
    scope: &Scope,
    artifact_id: &str,
) -> Result<Option<i64>> {
    let row = if let Some(upload) = artifact_id.strip_prefix("upload:") {
        let subject = uuid::Uuid::parse_str(upload)
            .map_err(|_| Error::NotFound)?
            .to_string();
        tx.query_opt(
            "SELECT r.document_id FROM core_document_files f JOIN core_document_revisions r ON r.tenant_id=f.tenant_id AND r.file_id=f.id WHERE f.tenant_id=$1 AND f.subject=$2::text::uuid AND f.state='attached'",
            &[&scope.tenant(), &subject],
        )
        .await?
    } else {
        tx.query_opt(
            "SELECT r.document_id FROM core_document_files f JOIN core_document_revisions r ON r.tenant_id=f.tenant_id AND r.file_id=f.id WHERE f.tenant_id=$1 AND f.artifact_id=$2 AND f.state='attached'",
            &[&scope.tenant(), &artifact_id],
        )
        .await?
    };
    Ok(row.map(|row| row.get(0)))
}

/// Removes metadata only after every Artifact has accepted deletion, in the
/// deletion job's transaction (`deletion.rs`). Until this commits, the
/// document and its full revision history remain available to retention and
/// legal-hold checks. A document no longer deleting (finished meanwhile) is
/// left alone.
pub(crate) async fn finalize_document_delete(
    tx: &mut MutationTransaction<'_>,
    tenant: i64,
    document: i64,
) -> Result<()> {
    tx.mutate(move |sql| Box::pin(async move {
        let Some(row) = sql.query_opt(
            "SELECT name,version FROM core_documents WHERE tenant_id=$1 AND id=$2 AND deleting_at IS NOT NULL FOR UPDATE",
            &[&tenant, &document],
        ).await? else { return Ok(((), Vec::new())); };
        let mut changes = Vec::new();
        for row in sql.query(
            "DELETE FROM core_document_marks WHERE tenant_id=$1 AND document_id=$2 RETURNING id,user_id,favourite,following",
            &[&tenant, &document],
        ).await? {
            changes.push(Change { kind: crate::MARK_RECORD.into(), id: row.get(0), old: json!({"document_id":document,"user_id":row.get::<_,i64>(1),"favourite":row.get::<_,bool>(2),"following":row.get::<_,bool>(3)}), new: json!({}) });
        }
        for row in sql.query(
            "DELETE FROM core_document_links WHERE tenant_id=$1 AND document_id=$2 RETURNING id,record_kind,record_id",
            &[&tenant, &document],
        ).await? {
            changes.push(Change { kind: crate::LINK_RECORD.into(), id: row.get(0), old: json!({"document_id":document,"record":{"kind":row.get::<_,String>(1),"id":row.get::<_,String>(2)}}), new: json!({}) });
        }
        for row in sql.query(
            "DELETE FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 RETURNING id,user_id,can_view,can_download,can_manage,can_share",
            &[&tenant, &document],
        ).await? {
            changes.push(Change { kind: crate::ACCESS_RECORD.into(), id: row.get(0), old: json!({"document_id":document,"user_id":row.get::<_,i64>(1),"view":row.get::<_,bool>(2),"download":row.get::<_,bool>(3),"manage":row.get::<_,bool>(4),"share":row.get::<_,bool>(5)}), new: json!({}) });
        }
        changes.extend(crate::customer::remove_all(sql, tenant, document).await?);
        let file_ids: Vec<i64> = sql.query(
            "SELECT file_id FROM core_document_revisions WHERE tenant_id=$1 AND document_id=$2 AND file_id IS NOT NULL",
            &[&tenant, &document],
        ).await?.into_iter().map(|row| row.get(0)).collect();
        for row in sql.query(
            &format!("DELETE FROM core_document_revisions AS r WHERE r.tenant_id=$1 AND r.document_id=$2 RETURNING {}", crate::store::REVISION_COLUMNS),
            &[&tenant, &document],
        ).await? {
            let revision = crate::store::revision_from_row(&row)?;
            changes.push(Change { kind: crate::REVISION_RECORD.into(), id: revision.id, old: crate::store::revision_values(&revision), new: json!({}) });
        }
        for row in sql.query(
            &format!("DELETE FROM core_document_files WHERE tenant_id=$1 AND id = ANY($2) RETURNING {COLUMNS}"),
            &[&tenant, &file_ids],
        ).await?.iter().map(from_row) {
            changes.push(Change { kind: FILE_RECORD.into(), id: row.id, old: audit_values(&row), new: json!({}) });
        }
        for row in sql.query(
            &format!("SELECT {} FROM core_documents d WHERE d.tenant_id=$1 AND d.related_document_id=$2 FOR UPDATE", crate::store::COLUMNS),
            &[&tenant, &document],
        ).await? {
            let old = crate::store::from_row(&row)?;
            let row = sql.query_one(
                &format!("UPDATE core_documents d SET related_document_id=NULL,relation=NULL,version=d.version+1,updated_at=now() WHERE d.tenant_id=$1 AND d.id=$2 RETURNING {}", crate::store::COLUMNS),
                &[&tenant, &old.id],
            ).await?;
            let new = crate::store::from_row(&row)?;
            changes.push(Change { kind: crate::RECORD.into(), id: old.id, old: crate::store::audit_values(&old), new: crate::store::audit_values(&new) });
        }
        sql.execute("DELETE FROM core_documents WHERE tenant_id=$1 AND id=$2", &[&tenant, &document]).await?;
        changes.push(Change { kind: crate::RECORD.into(), id: document, old: json!({"name":row.get::<_,String>(0),"version":row.get::<_,i64>(1),"deleting":true}), new: json!({}) });
        Ok(((), changes))
    })).await
}

/// Deletes unattached files from `base/artifacts`, then their ledger rows.
/// Released revisions use `discard_document` so history is checked together.
pub(crate) async fn discard(
    client: &mut Client,
    scope: &Scope,
    boundary: &MutationBoundary,
    artifacts: &Artifacts,
    artifact_ids: &[String],
) -> Vec<(String, WriteError)> {
    let mut failed = Vec::new();
    for artifact_id in artifact_ids {
        if let Err(error) = discard_one(client, scope, boundary, artifacts, artifact_id).await {
            failed.push((artifact_id.clone(), error));
        }
    }
    failed
}

/// Deletes one document's whole attached file set atomically at the Artifact
/// lifecycle boundary. Metadata and ledger rows remain until finalization.
pub(crate) async fn discard_document(
    client: &mut Client,
    scope: &Scope,
    artifacts: &Artifacts,
    artifact_ids: &[String],
) -> WriteResult<()> {
    if artifact_ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<Ledger> = client
        .query(
            &format!("SELECT {COLUMNS} FROM core_document_files WHERE tenant_id=$1 AND artifact_id = ANY($2) ORDER BY id"),
            &[&scope.tenant(), &artifact_ids],
        )
        .await
        .map_err(Error::from)?
        .iter()
        .map(from_row)
        .collect();
    if rows.len() != artifact_ids.len()
        || rows.iter().any(|row| row.state != "attached")
        || rows.iter().any(|row| row.company_id != rows[0].company_id)
    {
        return Err(WriteError::Forbidden);
    }
    let company = OrganisationId::new(rows[0].company_id).map_err(|_| Error::Internal)?;
    artifacts
        .for_owner(scope, company, &FILES)
        .delete_batch(client, artifact_ids)
        .await
        .map_err(artifact_error)
}

async fn discard_one(
    client: &mut Client,
    scope: &Scope,
    boundary: &MutationBoundary,
    artifacts: &Artifacts,
    artifact_id: &str,
) -> WriteResult<()> {
    let row = if let Some(upload) = artifact_id.strip_prefix("upload:") {
        let subject = uuid::Uuid::parse_str(upload)
            .map_err(|_| WriteError::NotFound)?
            .to_string();
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM core_document_files WHERE tenant_id=$1 AND subject=$2::text::uuid"),
                &[&scope.tenant(), &subject],
            )
            .await
    } else {
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM core_document_files WHERE tenant_id=$1 AND artifact_id=$2"),
                &[&scope.tenant(), &artifact_id],
            )
            .await
    }
    .map_err(Error::from)?
    .map(|row| from_row(&row))
    .ok_or(WriteError::NotFound)?;
    if !matches!(row.state.as_str(), "reserved" | "stored") {
        return Err(WriteError::Forbidden);
    }
    let company = OrganisationId::new(row.company_id).map_err(|_| Error::Internal)?;
    if let Some(artifact_id) = &row.artifact_id {
        artifacts
            .for_owner(scope, company, &FILES)
            .delete(client, artifact_id)
            .await
            .map_err(artifact_error)?;
    }
    let mut tx = boundary
        .begin(client, scope.mutation_context("core.document.discard_file"))
        .await?;
    let tenant = scope.tenant();
    tx.mutate(move |sql| {
        Box::pin(async move {
            sql.execute(
                "DELETE FROM core_document_files WHERE tenant_id=$1 AND id=$2 AND state<>'attached'",
                &[&tenant, &row.id],
            )
            .await?;
            let change = Change {
                kind: FILE_RECORD.into(),
                id: row.id,
                old: audit_values(&row),
                new: json!({}),
            };
            Ok(((), vec![change]))
        })
    })
    .await?;
    tx.commit().await?;
    Ok(())
}

/// One revision's file of a document the reader may see.
/// Records a refused download on the reader's own connection, after the
/// refused read has rolled back. Ids and a reason code only, so the trail
/// says why without naming the document to anyone reading the log.
pub(crate) async fn record_refused(
    client: &Client,
    scope: &Scope,
    document: i64,
    revision: i32,
) -> WriteResult<()> {
    let grant = client
        .query_opt(
            "SELECT can_download, expires_at IS NOT NULL AND expires_at <= now() FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND user_id=$3",
            &[&scope.tenant(), &document, &scope.user()],
        )
        .await
        .map_err(Error::from)?
        .map(|row| (row.get::<_, bool>(0), row.get::<_, bool>(1)));
    let reason = match grant {
        None => "no_grant",
        Some((_, true)) => "grant_ended",
        Some((false, false)) => "no_download_permission",
        Some((true, false)) => {
            if holds_proof(client, scope, DOWNLOAD).await? {
                "no_file_access"
            } else {
                "no_download_capability"
            }
        }
    };
    record_action(
        client,
        &Attribution::from_scope(scope),
        None,
        DOWNLOAD_REFUSED,
        json!({"document_id": document, "revision": revision, "reason": reason}),
        false,
    )
    .await?;
    Ok(())
}

pub(crate) async fn read(
    client: &mut Client,
    scope: &Scope,
    artifacts: &Artifacts,
    document: &Document,
    revision: i32,
) -> WriteResult<File> {
    authorize_proof(&*client, scope, DOWNLOAD).await?;
    if document.deleting_at.is_some() {
        return Err(WriteError::NotFound);
    }
    let row = client
        .query_opt(
            "SELECT artifact_id,file_name,scan_status FROM core_document_revisions WHERE tenant_id=$1 AND document_id=$2 AND revision=$3",
            &[&scope.tenant(), &document.id, &revision],
        )
        .await
        .map_err(Error::from)?
        .ok_or(WriteError::NotFound)?;
    let (artifact_id, file_name, scan_status): (Option<String>, Option<String>, String) =
        (row.get(0), row.get(1), row.get(2));
    if file_name.is_some() && scan_status != "clean" {
        return Err(WriteError::Quarantined);
    }
    // An external revision is a link, not a file.
    let (Some(artifact_id), Some(file_name)) = (artifact_id, file_name) else {
        return Err(WriteError::NotFound);
    };
    let company = OrganisationId::new(document.company_id).map_err(|_| Error::Internal)?;
    let file = artifacts
        .for_owner(scope, company, &FILES)
        .read(client, &artifact_id)
        .await
        .map_err(artifact_error)?;
    Ok(File {
        file_name,
        content_type: file.metadata.content_type.clone(),
        bytes: file.bytes,
    })
}
