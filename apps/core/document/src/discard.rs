//! Files stored but never attached. Storing a file (Upload, Quick add, Copy)
//! queues, in the transaction that reserves its ledger row, a job that runs
//! `UNATTACHED_AFTER` later as the narrow `document-file-cleaner` principal.
//! A file attached by then is left alone. Otherwise the job first moves the
//! row to `discarding`, which no write can attach, then deletes its Artifact,
//! then the row. A held upload's Media draft is Media's draft cleanup's.
use crate::contributions::DISCARD_SETTLE;
use crate::files::{audit_values, from_row, COLUMNS, FILES};
use crate::FILE_RECORD;
use bilibi_authz::SystemScope;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_party_ids::OrganisationId;
use bilibi_queue::{Execution, HandlerBinding, Outcome, Worker, HANDLERS, WORKERS};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime};

pub(crate) const PRINCIPAL: &str = "document-file-cleaner";
pub(crate) const WORKER: &str = "core/document-file-discard";
pub(crate) const HANDLER: &str = "core/document.file_discard";
/// How long a stored file waits to be attached: a person may keep the New
/// document dialog open with the file chosen for a long while.
const UNATTACHED_AFTER: Duration = Duration::from_secs(24 * 3600);

fn arguments(args: Value) -> Result<Value> {
    let object = args.as_object().ok_or(Error::Invalid)?;
    if object.len() != 1
        || object
            .get("file")
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
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: HANDLER, capability: DISCARD_SETTLE }) }

/// Queues the discard of ledger row `file`, in the transaction that reserves
/// it: a stored file always has its way out.
pub(crate) async fn schedule(
    queue: &bilibi_queue::Queue,
    tx: &mut MutationTransaction<'_>,
    tenant: i64,
    file: i64,
) -> Result<()> {
    let system = SystemScope::rehydrate(tx.sql(), tenant, Some(PRINCIPAL)).await?;
    queue
        .enqueue_system_at_within(
            tx,
            &system,
            WORKER,
            json!({"file": file}),
            SystemTime::now() + UNATTACHED_AFTER,
        )
        .await?;
    Ok(())
}

pub(crate) struct DiscardJob {
    pub db: bilibi_database::Db,
    pub artifacts: std::sync::Arc<bilibi_artifacts::Artifacts>,
}
impl bilibi_queue::Handler for DiscardJob {
    fn perform<'a>(
        &'a self,
        execution: &'a Execution,
        args: Value,
    ) -> bilibi_queue::HandlerFuture<'a> {
        Box::pin(async move {
            match self.run(execution, &args).await {
                Ok(()) => Outcome::Completed,
                Err(error @ (Error::Forbidden | Error::Unauthorized)) => {
                    eprintln!("error: unattached document file discard refused: {error}");
                    Outcome::Cancel("discard_refused")
                }
                Err(error) => {
                    eprintln!("warning: unattached document file awaits discard retry: {error}");
                    Outcome::Retry("discard_unfinished")
                }
            }
        })
    }
}
impl DiscardJob {
    async fn run(&self, execution: &Execution, args: &Value) -> Result<()> {
        let scope = execution.scope().ok_or(Error::Unauthorized)?;
        if scope.principal() != Some(PRINCIPAL) {
            return Err(Error::Forbidden);
        }
        let file = args["file"].as_i64().ok_or(Error::Invalid)?;
        let tenant = scope.tenant();
        let mut client = self.db.get().await?;
        let mut tx = execution
            .begin(&mut client, "core.document.discard_file")
            .await?;
        // Decide with `sql()` first: a write must report a change.
        let before = tx
            .sql()
            .query_opt(
                &format!("SELECT {COLUMNS} FROM core_document_files WHERE tenant_id=$1 AND id=$2 FOR UPDATE"),
                &[&tenant, &file],
            )
            .await?
            .map(|row| from_row(&row));
        let row = match before {
            // Attached in time, or gone.
            None => None,
            Some(before) if before.state == "attached" => None,
            Some(before) if before.state == "discarding" => Some(before),
            Some(before) => Some(tx.mutate(move |sql| Box::pin(async move {
                let row = sql.query_one(
                    &format!("UPDATE core_document_files SET state='discarding' WHERE tenant_id=$1 AND id=$2 RETURNING {COLUMNS}"),
                    &[&tenant, &file],
                ).await?;
                let after = from_row(&row);
                let change = Change { kind: FILE_RECORD.into(), id: file, old: audit_values(&before), new: audit_values(&after) };
                Ok((after, vec![change]))
            })).await?),
        };
        tx.commit().await?;
        let Some(row) = row else {
            return Ok(());
        };
        if let Some(artifact) = &row.artifact_id {
            let company = OrganisationId::new(row.company_id).map_err(|_| Error::Internal)?;
            match self
                .artifacts
                .for_owner(scope, company, &FILES)
                .delete(&mut client, artifact)
                .await
            {
                Ok(()) | Err(bilibi_artifacts::Error::NotFound) => {}
                Err(bilibi_artifacts::Error::Database(error)) => return Err(error),
                Err(error) => return Err(Error::Refused(error.code().into())),
            }
        }
        let mut tx = execution
            .begin(&mut client, "core.document.discard_file")
            .await?;
        let id = row.id;
        let old = audit_values(&row);
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute(
                "DELETE FROM core_document_files WHERE tenant_id=$1 AND id=$2 AND state='discarding'",
                &[&tenant, &id],
            ).await?;
            Ok(((), vec![Change { kind: FILE_RECORD.into(), id, old, new: json!({}) }]))
        })).await?;
        tx.commit().await
    }
}
