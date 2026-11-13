//! Finishing a delete. `core.document.delete` marks the document as deleting
//! and, in that same transaction, queues a durable job that runs as the
//! narrow `document-deleter` principal: it deletes every revision's Artifact
//! as one protected batch, then removes the metadata. A failed attempt
//! retries with Queue's backoff, and a restart loses nothing. A job that ends
//! without finishing queues the next one an hour later while the document is
//! still deleting, so a deleting document always has its job. A hold or
//! retention placed after the delete committed makes the batch refuse: the
//! document is then shown again with its full history (README,
//! "Retention-safe deletion").
use crate::contributions::DELETION_SETTLE;
use crate::store::{audit_values, from_row, Store, COLUMNS};
use crate::{WriteError, WriteResult, RECORD};
use bilibi_authz::SystemScope;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_queue::{
    EndingBinding, Execution, HandlerBinding, Outcome, Worker, ENDINGS, HANDLERS, WORKERS,
};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime};

pub(crate) const PRINCIPAL: &str = "document-deleter";
pub(crate) const WORKER: &str = "core/document-delete";
pub(crate) const HANDLER: &str = "core/document.delete";
pub(crate) const ENDED: &str = "core/document.delete_ended";
/// About a day of attempts with Queue's backoff (doubling, at most an hour).
const ATTEMPTS: i32 = 30;
/// When a job ended without finishing, the next one starts this much later.
const AGAIN_AFTER: Duration = Duration::from_secs(3600);

fn arguments(args: Value) -> Result<Value> {
    let object = args.as_object().ok_or(Error::Invalid)?;
    if object.len() != 1
        || object
            .get("document")
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
inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: ATTEMPTS, validate: arguments }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: HANDLER, capability: DELETION_SETTLE }) }
inventory::submit! { Contribution::new(ENDINGS, WORKER, provenance!(), &EndingBinding { service: ENDED, principal: PRINCIPAL, capability: DELETION_SETTLE }) }

/// Queues the job that finishes deleting `document`, in `tx`, at `at`
/// (`None`: now).
async fn enqueue(
    queue: &bilibi_queue::Queue,
    tx: &mut MutationTransaction<'_>,
    tenant: i64,
    document: i64,
    at: Option<SystemTime>,
) -> Result<()> {
    let system = SystemScope::rehydrate(tx.sql(), tenant, Some(PRINCIPAL)).await?;
    let args = json!({"document": document});
    match at {
        Some(at) => {
            queue
                .enqueue_system_at_within(tx, &system, WORKER, args, at)
                .await?
        }
        None => {
            queue
                .enqueue_system_within(tx, &system, WORKER, args)
                .await?
        }
    };
    Ok(())
}

impl Store<'_> {
    /// Queues the job that finishes the delete, in the delete's own
    /// transaction: the mark and its job commit together or not at all.
    pub(crate) async fn finish_delete(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        document: i64,
    ) -> WriteResult<()> {
        if let Err(error) = enqueue(self.queue, tx, scope.tenant(), document, None).await {
            // The caller must not commit a deleting document without its job.
            tx.mutate(move |_| Box::pin(async move { Err::<((), Vec<Change>), _>(error) }))
                .await?;
            return Err(Error::Internal.into());
        }
        Ok(())
    }
}

pub(crate) struct DeleteJob {
    pub db: bilibi_database::Db,
    pub documents: std::sync::Arc<crate::Documents>,
}
impl bilibi_queue::Handler for DeleteJob {
    fn perform<'a>(
        &'a self,
        execution: &'a Execution,
        args: Value,
    ) -> bilibi_queue::HandlerFuture<'a> {
        Box::pin(async move {
            match self.run(execution, &args).await {
                Ok(()) => Outcome::Completed,
                Err(error @ (Error::Forbidden | Error::Unauthorized)) => {
                    eprintln!("error: document delete refused: {error}");
                    Outcome::Cancel("delete_refused")
                }
                Err(error) => {
                    eprintln!("warning: document delete awaits retry: {error}");
                    Outcome::Retry("delete_unfinished")
                }
            }
        })
    }
}
impl DeleteJob {
    async fn run(&self, execution: &Execution, args: &Value) -> Result<()> {
        let scope = execution.scope().ok_or(Error::Unauthorized)?;
        if scope.principal() != Some(PRINCIPAL) {
            return Err(Error::Forbidden);
        }
        let document = args["document"].as_i64().ok_or(Error::Invalid)?;
        let tenant = scope.tenant();
        let mut client = self.db.get().await?;
        if client
            .query_opt(
                "SELECT 1 FROM core_documents WHERE tenant_id=$1 AND id=$2 AND deleting_at IS NOT NULL",
                &[&tenant, &document],
            )
            .await?
            .is_none()
        {
            // Finished by an earlier job, or shown again.
            return Ok(());
        }
        let files: Vec<String> = client
            .query(
                "SELECT artifact_id FROM core_document_revisions WHERE tenant_id=$1 AND document_id=$2 AND artifact_id IS NOT NULL ORDER BY revision",
                &[&tenant, &document],
            )
            .await?
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        let protected = match crate::files::discard_document(
            &mut client,
            scope,
            &self.documents.artifacts,
            &files,
        )
        .await
        {
            Ok(()) => false,
            Err(WriteError::LegalHold | WriteError::Retained) => true,
            Err(WriteError::Access(error)) => return Err(error),
            Err(WriteError::Forbidden) => return Err(Error::Forbidden),
            Err(error) => return Err(Error::Refused(error.to_string())),
        };
        let mut tx = execution
            .begin(&mut client, "core.document.delete.finalize")
            .await?;
        // Decide with `sql()` first: a write must report a change, so a
        // document finished or shown again meanwhile writes nothing.
        if tx
            .sql()
            .query_opt(
                "SELECT 1 FROM core_documents WHERE tenant_id=$1 AND id=$2 AND deleting_at IS NOT NULL FOR UPDATE",
                &[&tenant, &document],
            )
            .await?
            .is_none()
        {
            return Ok(());
        }
        if protected {
            restore(&mut tx, tenant, document).await?;
        } else {
            crate::files::finalize_document_delete(&mut tx, tenant, document).await?;
        }
        tx.commit().await
    }
}

/// Shows a document again whose files a hold or retention placed after the
/// delete committed now keeps: every row is still there, so it reads as
/// before the delete.
async fn restore(tx: &mut MutationTransaction<'_>, tenant: i64, document: i64) -> Result<()> {
    tx.mutate(move |sql| Box::pin(async move {
        let Some(row) = sql.query_opt(
            &format!("SELECT {COLUMNS} FROM core_documents d WHERE d.tenant_id=$1 AND d.id=$2 AND d.deleting_at IS NOT NULL FOR UPDATE"),
            &[&tenant, &document],
        ).await? else { return Ok(((), Vec::new())); };
        let before = from_row(&row)?;
        let row = sql.query_one(
            &format!("UPDATE core_documents d SET deleting_at=NULL,version=d.version+1,updated_at=now() WHERE d.tenant_id=$1 AND d.id=$2 RETURNING {COLUMNS}"),
            &[&tenant, &document],
        ).await?;
        let after = from_row(&row)?;
        Ok(((), vec![Change { kind: RECORD.into(), id: document, old: audit_values(&before), new: audit_values(&after) }]))
    })).await
}

pub(crate) struct DeleteEnded {
    pub documents: std::sync::Arc<crate::Documents>,
}
impl bilibi_queue::Ending for DeleteEnded {
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
            let Some(document) = job.args["document"].as_i64() else {
                return Ok(());
            };
            let tenant = scope.tenant();
            let deleting = tx
                .sql()
                .query_opt(
                    "SELECT 1 FROM core_documents WHERE tenant_id=$1 AND id=$2 AND deleting_at IS NOT NULL",
                    &[&tenant, &document],
                )
                .await?
                .is_some();
            if deleting {
                eprintln!(
                    "warning: document {document} delete job {} ended ({}); trying again in an hour",
                    job.job_id,
                    job.failure_code.as_deref().unwrap_or("unknown")
                );
                let at = SystemTime::now() + AGAIN_AFTER;
                enqueue(&self.documents.queue, tx, tenant, document, Some(at)).await?;
            }
            Ok(())
        })
    }
}
