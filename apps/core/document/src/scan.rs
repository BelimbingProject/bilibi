//! A held internal revision is a document record before its untrusted bytes
//! become an Artifact. Queue rechecks the Media verdict as a narrowly declared
//! system principal; its ending hook gives every pending revision a way out.
use crate::store::Store;
use crate::{Revision, ScanStatus, WriteResult, REVISION_RECORD};
use bilibi_authz::SystemScope;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_notifications::Notice;
use bilibi_queue::{EndingBinding, HandlerBinding, Worker, ENDINGS, HANDLERS, WORKERS};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};

pub(crate) const PRINCIPAL: &str = "document-scanner";
pub(crate) const WORKER: &str = "core/document-scan";
pub(crate) const HANDLER: &str = "core/document.scan";
pub(crate) const ENDED: &str = "core/document.scan_ended";

fn arguments(args: Value) -> Result<Value> {
    let object = args.as_object().ok_or(Error::Invalid)?;
    if object.len() != 1
        || object
            .get("revision")
            .and_then(Value::as_i64)
            .is_none_or(|id| id <= 0)
    {
        return Err(Error::Invalid);
    }
    Ok(args)
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
inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: 20, validate: arguments }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: HANDLER, capability: crate::contributions::SCAN_SETTLE }) }
inventory::submit! { Contribution::new(ENDINGS, WORKER, provenance!(), &EndingBinding { service: ENDED, principal: PRINCIPAL, capability: crate::contributions::SCAN_SETTLE }) }

impl Store<'_> {
    /// Queue a pending revision in the same mutation that attached it, or
    /// notify its uploader and managers that the first verdict holds it.
    pub(crate) async fn scan_attached(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        revision: &Revision,
    ) -> WriteResult<()> {
        if revision.scan_status == ScanStatus::Pending {
            let system = SystemScope::rehydrate(tx.sql(), scope.tenant(), Some(PRINCIPAL)).await?;
            let job = match self
                .queue
                .enqueue_system_within(tx, &system, WORKER, json!({"revision": revision.id}))
                .await
            {
                Ok(job) => job.id(),
                Err(error) => {
                    // A caller must not commit a pending revision without a job.
                    tx.mutate(move |_| Box::pin(async move { Err::<((), Vec<Change>), _>(error) }))
                        .await?;
                    return Err(Error::Internal.into());
                }
            };
            let tenant = scope.tenant();
            let id = revision.id;
            tx.mutate(move |sql| Box::pin(async move {
                sql.execute(
                    "UPDATE core_document_revisions SET scan_job_id=$3 WHERE tenant_id=$1 AND id=$2 AND scan_status='pending' AND scan_job_id IS NULL",
                    &[&tenant, &id, &job],
                ).await?;
                Ok(((), vec![Change {
                    kind: REVISION_RECORD.into(), id,
                    old: json!({"scan_job_id":null}), new: json!({"scan_job_id":job}),
                }]))
            })).await?;
        }
        if revision.scan_status != ScanStatus::Clean
            && revision.scan_status != ScanStatus::NotScanned
        {
            self.notify_scan(tx, scope, revision, revision.scan_status)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn notify_scan(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        revision: &Revision,
        state: ScanStatus,
    ) -> Result<()> {
        let tenant = scope.tenant();
        let mut recipients = vec![revision.created_by];
        recipients.extend(
            tx.sql().query(
                "SELECT user_id FROM core_document_access WHERE tenant_id=$1 AND document_id=$2 AND can_manage AND (expires_at IS NULL OR expires_at>now()) ORDER BY user_id LIMIT 499",
                &[&tenant, &revision.document_id],
            ).await?.iter().map(|row| row.get::<_, i64>(0))
        );
        let recipients = self.notifications.eligible(tx, &recipients).await?;
        if recipients.is_empty() {
            return Ok(());
        }
        let (title, body) = match state {
            ScanStatus::Pending => (
                "Document file is being checked",
                "The file is held while the malware check runs.",
            ),
            ScanStatus::Quarantined => (
                "Document file quarantined",
                "The malware check found an infected file. Its bytes are unavailable.",
            ),
            ScanStatus::Failed => (
                "Document file check failed",
                "The file could not be checked. Its bytes remain unavailable.",
            ),
            ScanStatus::Unscanned => (
                "Document file was not scanned",
                "A scanner was unavailable for this file. Its bytes remain unavailable.",
            ),
            ScanStatus::Clean => (
                "Document file is ready",
                "The file passed the malware check and is available.",
            ),
            ScanStatus::NotScanned => return Ok(()),
        };
        let notice = Notice::new(title)
            .kind("core.document.scan")
            .body(body)
            .url(crate::web::path(revision.document_id))
            .icon("document-text");
        self.notifications
            .notify(tx, scope, &recipients, &notice)
            .await
    }
}
pub(crate) struct ScanJob {
    pub db: bilibi_database::Db,
    pub documents: std::sync::Arc<crate::Documents>,
}
impl bilibi_queue::Handler for ScanJob {
    fn perform<'a>(
        &'a self,
        execution: &'a bilibi_queue::Execution,
        args: serde_json::Value,
    ) -> bilibi_queue::HandlerFuture<'a> {
        Box::pin(async move {
            match self.run(execution, &args).await {
                Ok(true) => bilibi_queue::Outcome::Completed,
                Ok(false) => bilibi_queue::Outcome::Retry("scan_pending"),
                Err(
                    bilibi_database::Error::Forbidden
                    | bilibi_database::Error::NotFound
                    | bilibi_database::Error::Unauthorized,
                ) => bilibi_queue::Outcome::Cancel("scan_refused"),
                Err(error) => {
                    eprintln!("error: document scan settlement: {error}");
                    bilibi_queue::Outcome::Retry("scan_settlement_failed")
                }
            }
        })
    }
}
impl ScanJob {
    async fn run(
        &self,
        execution: &bilibi_queue::Execution,
        args: &serde_json::Value,
    ) -> Result<bool> {
        let scope = execution.scope().ok_or(Error::Unauthorized)?;
        if scope.principal() != Some(PRINCIPAL) {
            return Err(Error::Forbidden);
        }
        let id = args["revision"].as_i64().ok_or(Error::Invalid)?;
        let tenant = scope.tenant();
        let mut client = self.db.get().await?;
        let Some(row) = client.query_opt(
            "SELECT f.id,f.company_id,f.subject::text,f.media_public_id,f.content_type FROM core_document_revisions r JOIN core_document_files f ON f.tenant_id=r.tenant_id AND f.id=r.file_id WHERE r.tenant_id=$1 AND r.id=$2 AND r.scan_job_id=$3 AND r.scan_status='pending' AND f.state='attached'",
            &[&tenant,&id,&execution.job_id],
        ).await? else { return Ok(true); };
        let file: i64 = row.get(0);
        let company = bilibi_party_ids::OrganisationId::new(row.get::<_, i64>(1))
            .map_err(|_| Error::Internal)?;
        let subject: String = row.get(2);
        let media_id: String = row.get::<_, Option<String>>(3).ok_or(Error::Internal)?;
        let content_type: String = row.get(4);
        let verdict = match self
            .documents
            .media
            .inspect_attachment(&**client, scope, &media_id)
            .await
        {
            Ok(attachment) => attachment.scan_state,
            Err(bilibi_media::Error::NotFound) => bilibi_media::ScanState::Failed,
            Err(error) => return Err(Error::Refused(format!("Media inspection failed: {error}"))),
        };
        let status = match verdict {
            bilibi_media::ScanState::Pending => return Ok(false),
            bilibi_media::ScanState::Clean => ScanStatus::Clean,
            bilibi_media::ScanState::Infected => ScanStatus::Quarantined,
            bilibi_media::ScanState::Failed => ScanStatus::Failed,
            bilibi_media::ScanState::Unscanned => ScanStatus::Unscanned,
        };
        let artifact = if status == ScanStatus::Clean {
            let prior = self
                .documents
                .artifacts
                .subject_documents(&client, scope, &crate::files::FILES, &subject)
                .await
                .map_err(|error| Error::Refused(format!("Artifact recovery failed: {error}")))?;
            if let Some((_, metadata)) = prior.into_iter().find(|(found, metadata)| {
                *found == company && metadata.reference.kind == crate::files::FILE_KIND
            }) {
                Some(metadata.id)
            } else {
                let clean = self
                    .documents
                    .media
                    .download_attachment(&**client, scope, &media_id)
                    .await
                    .map_err(|error| Error::Refused(format!("Media download failed: {error}")))?;
                let metadata = self
                    .documents
                    .artifacts
                    .for_owner(scope, company, &crate::files::FILES)
                    .put(
                        &mut client,
                        bilibi_artifacts::Reference {
                            subject: subject.clone(),
                            kind: crate::files::FILE_KIND.into(),
                        },
                        &clean.bytes,
                        &content_type,
                    )
                    .await
                    .map_err(|error| Error::Refused(format!("Artifact copy failed: {error}")))?;
                Some(metadata.id)
            }
        } else {
            None
        };
        let mut tx = execution
            .begin(&mut client, "core.document.scan_settle")
            .await?;
        if status == ScanStatus::Clean {
            self.documents
                .store()
                .retain_new_file(&mut tx, scope, company.get(), artifact.as_deref())
                .await
                .map_err(|error| Error::Refused(error.to_string()))?;
        }
        let revised = tx.mutate(move |sql| Box::pin(async move {
            let old = sql.query_opt("SELECT r.* FROM core_document_revisions r WHERE r.tenant_id=$1 AND r.id=$2 AND r.scan_job_id=$3 AND r.scan_status='pending' FOR UPDATE", &[&tenant,&id,&execution.job_id]).await?;
            let Some(old) = old else { return Ok((None, Vec::new())); };
            let before = crate::store::revision_from_row(&old)?;
            let previous: String = sql.query_one("SELECT scan_status FROM core_document_files WHERE tenant_id=$1 AND id=$2 FOR UPDATE", &[&tenant,&file]).await?.get(0);
            let code = status.code();
            // The revision's existing Artifact FK points to the ledger.
            // Publish that key on the file row first, within this transaction.
            sql.execute("UPDATE core_document_files SET scan_status=$3,artifact_id=$4 WHERE tenant_id=$1 AND id=$2", &[&tenant,&file,&code,&artifact]).await?;
            let updated = sql.query_one("UPDATE core_document_revisions SET scan_status=$4,artifact_id=$5 WHERE tenant_id=$1 AND id=$2 AND scan_job_id=$3 RETURNING *", &[&tenant,&id,&execution.job_id,&code,&artifact]).await?;
            let after = crate::store::revision_from_row(&updated)?;
            Ok((Some(after.clone()), vec![
                Change { kind: REVISION_RECORD.into(), id, old: crate::store::revision_values(&before), new: crate::store::revision_values(&after) },
                Change { kind: crate::FILE_RECORD.into(), id: file, old: json!({"scan_status":previous,"artifact_id":null}), new: json!({"scan_status":code,"artifact_id":artifact}) },
            ]))
        })).await?;
        if let Some(revised) = revised {
            self.documents
                .store()
                .notify_scan(&mut tx, scope, &revised, status)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }
}

pub(crate) struct ScanEnded {
    pub documents: std::sync::Arc<crate::Documents>,
}
impl bilibi_queue::Ending for ScanEnded {
    fn ended<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        job: &'a bilibi_queue::Ended,
    ) -> bilibi_queue::EndingFuture<'a> {
        Box::pin(async move {
            if scope.principal() != Some(PRINCIPAL) {
                return Err(Error::Forbidden);
            }
            let Some(id) = job.args["revision"].as_i64() else {
                return Ok(());
            };
            let tenant = scope.tenant();
            let job_id = job.job_id;
            let revised = tx.mutate(move |sql| Box::pin(async move {
                let old = sql.query_opt("SELECT r.* FROM core_document_revisions r WHERE r.tenant_id=$1 AND r.id=$2 AND r.scan_job_id=$3 AND r.scan_status='pending' FOR UPDATE", &[&tenant,&id,&job_id]).await?;
                let Some(old) = old else { return Ok((None, Vec::new())); };
                let before = crate::store::revision_from_row(&old)?;
                let after = sql.query_one("UPDATE core_document_revisions SET scan_status='failed' WHERE tenant_id=$1 AND id=$2 AND scan_job_id=$3 RETURNING *", &[&tenant,&id,&job_id]).await?;
                let after = crate::store::revision_from_row(&after)?;
                sql.execute("UPDATE core_document_files SET scan_status='failed' WHERE tenant_id=$1 AND id=(SELECT file_id FROM core_document_revisions WHERE tenant_id=$1 AND id=$2)", &[&tenant,&id]).await?;
                Ok((Some(after.clone()), vec![Change { kind: REVISION_RECORD.into(), id, old: crate::store::revision_values(&before), new: crate::store::revision_values(&after) }]))
            })).await?;
            if let Some(revised) = revised {
                self.documents
                    .store()
                    .notify_scan(tx, scope, &revised, ScanStatus::Failed)
                    .await?;
            }
            Ok(())
        })
    }
}
