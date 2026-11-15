//! Optional owner contract for removing records a finished import created.
use crate::{import, store, BulkData, Run, RunState, UndoOutcome, CREATE};
use bilibi_agent_api::Error as ContractError;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::{Consumer, Entry, RegistryError};
use bilibi_notifications::Notice;
use bilibi_queue::{Execution, Handler, HandlerFuture, Outcome as JobOutcome};
use bilibi_tenancy::Scope;
use jiff::{SignedDuration, Timestamp};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub const IMPORT_UNDO: &str = "base/bulk_data.import_undo";
pub(crate) const WORKER: &str = "base/bulk_data-import-undo";
pub(crate) const SERVICE: &str = "base/bulk_data.import-undo-job";
const SLICE: Duration = Duration::from_secs(120);

struct RecordedUndo {
    row_number: i32,
    outcome: UndoOutcome,
    message: Option<String>,
}

/// The owner's version-fenced removal operation for records created by an
/// import. The operation takes `run_id`, the configured id and version fields,
/// and must ask [`crate::BulkData::claim_undo`] in its mutation transaction
/// before removing the record. That proof keeps the operation safe through
/// every caller, not only Bulk Data's queued job.
#[derive(Debug)]
pub struct ImportUndo {
    pub delete: &'static str,
    pub delete_id: &'static str,
    pub delete_version: &'static str,
    /// The property holding the new record's version in the create result.
    pub created_version: &'static str,
}

fn shape(entry: &Entry) -> bilibi_module_registry::Result<()> {
    let undo = entry.get::<ImportUndo>()?;
    if [
        undo.delete,
        undo.delete_id,
        undo.delete_version,
        undo.created_version,
    ]
    .iter()
    .any(|value| value.trim().is_empty() || value.chars().count() > 100)
    {
        return Err(invalid(
            entry,
            "has a blank or overlong operation or property name",
        ));
    }
    Ok(())
}
inventory::submit! { Consumer::new::<ImportUndo>(IMPORT_UNDO, "base/bulk_data", |entries| entries.iter().try_for_each(shape)) }

pub(crate) fn invalid(entry: &Entry, reason: impl std::fmt::Display) -> RegistryError {
    RegistryError::Contribution(format!(
        "import undo {} at {}:{} {reason}",
        entry.id, entry.provenance.source, entry.provenance.line
    ))
}

pub(crate) fn check(entry: &Entry) -> bilibi_module_registry::Result<&'static ImportUndo> {
    shape(entry)?;
    entry.get::<ImportUndo>()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Availability {
    pub supported: bool,
    pub available: bool,
    pub deadline: Option<Timestamp>,
}

pub(crate) struct UndoJob(pub Arc<BulkData>);

impl Handler for UndoJob {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let Some(run) = args["run"].as_i64().filter(|id| *id > 0) else {
                return JobOutcome::Cancel("worker_args_refused");
            };
            match self.0.work_undo(execution, run).await {
                Ok(()) => JobOutcome::Completed,
                Err(Error::Unauthorized | Error::Forbidden) => JobOutcome::Cancel("actor_refused"),
                Err(Error::NotFound) => JobOutcome::Cancel("run_missing"),
                Err(Error::Conflict) => JobOutcome::Retry("lease_lost"),
                Err(_) => JobOutcome::Retry("undo_unavailable"),
            }
        })
    }
}

pub(crate) fn arguments(args: Value) -> Result<Value> {
    let object = args.as_object().ok_or(Error::Invalid)?;
    if object.len() == 1
        && object
            .get("run")
            .and_then(Value::as_i64)
            .is_some_and(|id| id > 0)
    {
        Ok(args)
    } else {
        Err(Error::Invalid)
    }
}

impl BulkData {
    pub(crate) async fn undo_availability(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        run: &Run,
    ) -> Result<Availability> {
        let Some(registered) = self.types.get(run.record_type.as_str()) else {
            return Ok(Availability {
                supported: false,
                available: false,
                deadline: None,
            });
        };
        let supported = registered.undo.is_some();
        let days = self
            .settings
            .get_global(client, crate::UNDO_DAYS)
            .await?
            .as_i64()
            .ok_or(Error::Internal)?;
        let deadline = run.finished_at.and_then(|at| {
            at.checked_add(SignedDuration::from_hours(days.saturating_mul(24)))
                .ok()
        });
        let has_created = client.query_opt(
                "SELECT 1 FROM base_bulk_data_rows WHERE tenant_id=$1 AND run_id=$2 AND pass='run' AND outcome='create' AND record_id IS NOT NULL AND record_version IS NOT NULL AND undo_outcome IS NULL LIMIT 1",
                &[&scope.tenant(), &run.id],
            ).await?.is_some();
        let available = supported
            && !run.erased
            && ((run.state == RunState::Completed
                && deadline.is_some_and(|at| at > Timestamp::now())
                && has_created)
                || (run.state == RunState::Undoing && run.stopped));
        Ok(Availability {
            supported,
            available,
            deadline,
        })
    }

    pub(crate) async fn ready_undo(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        run_id: i64,
        version: i64,
    ) -> std::result::Result<Run, crate::WriteError> {
        bilibi_authz::authorize_key(client, scope, CREATE).await?;
        let run = store::load(client, scope, run_id, true).await?;
        if run.version != version {
            return Err(crate::WriteError::Stale);
        }
        let registered = self.registered(&run.record_type)?;
        bilibi_authz::authorize_key(client, scope, registered.definition.import.capability).await?;
        let availability = self.undo_availability(client, scope, &run).await?;
        if !availability.supported {
            return Err(crate::refused("run_id", "has no owner removal operation"));
        }
        if run.erased {
            return Err(crate::refused("run_id", "was erased and cannot be undone"));
        }
        if run.state != RunState::Completed && !(run.state == RunState::Undoing && run.stopped) {
            return Err(crate::refused(
                "run_id",
                "is not a finished import waiting to be undone or a stopped undo",
            ));
        }
        if !availability.available {
            return Err(crate::refused(
                "run_id",
                "is past its undo period or created no records that remain to remove",
            ));
        }
        Ok(run)
    }

    /// Prove that one owner record is still an untouched create result of the
    /// person's currently undoing run. Owner removal operations call this in
    /// preview and repeat it with [`claim_undo`] in their mutation transaction.
    pub async fn preview_undo(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        record_type: &str,
        run_id: i64,
        record_id: &str,
        version: i64,
    ) -> Result<()> {
        bilibi_authz::authorize_key(client, scope, CREATE).await?;
        let registered = self.types.get(record_type).ok_or(Error::NotFound)?;
        bilibi_authz::authorize_key(client, scope, registered.definition.import.capability).await?;
        let run = store::load(client, scope, run_id, false).await?;
        if run.erased || run.state != RunState::Undoing || run.record_type != record_type {
            return Err(Error::Forbidden);
        }
        client.query_opt(
            "SELECT id FROM base_bulk_data_rows WHERE tenant_id=$1 AND run_id=$2 AND pass='run' AND outcome='create' AND record_id=$3 AND record_version=$4 AND undo_outcome IS NULL LIMIT 1",
            &[&scope.tenant(), &run_id, &record_id, &version],
        ).await?.ok_or(Error::Conflict)?;
        Ok(())
    }

    /// Claim one untouched create result inside its owner's removal
    /// transaction. The claim and owner deletion commit or roll back together,
    /// so no caller can use the operation on another record or outside a live
    /// import undo.
    pub async fn claim_undo(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        record_type: &str,
        run_id: i64,
        record_id: &str,
        version: i64,
    ) -> Result<i32> {
        bilibi_authz::authorize_key(tx.sql(), scope, CREATE).await?;
        let registered = self.types.get(record_type).ok_or(Error::NotFound)?;
        bilibi_authz::authorize_key(tx.sql(), scope, registered.definition.import.capability)
            .await?;
        // Hold the run row until the owner removal commits. Erasure changes
        // this same row before forgetting import metadata, so it cannot race
        // a successful claim and reintroduce a deletion after erasure starts.
        let run = store::lock(tx.sql(), scope, run_id).await?;
        if run.erased || run.state != RunState::Undoing || run.record_type != record_type {
            return Err(Error::Forbidden);
        }
        let tenant = scope.tenant();
        let record_id = record_id.to_owned();
        let row = tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_opt(
                "UPDATE base_bulk_data_rows SET undo_outcome='removed',undo_message=NULL,undone_at=clock_timestamp() WHERE tenant_id=$1 AND run_id=$2 AND pass='run' AND outcome='create' AND record_id=$3 AND record_version=$4 AND undo_outcome IS NULL RETURNING id,row_number",
                &[&tenant, &run_id, &record_id, &version],
            ).await?.ok_or(Error::Conflict)?;
            let (id, row_number): (i64, i32) = (row.get(0), row.get(1));
            sql.execute("UPDATE base_bulk_data_runs SET undo_next_row=greatest(undo_next_row,$3),updated_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND state='undoing'", &[&tenant, &run_id, &row_number]).await?;
            Ok((row_number, vec![Change { kind: "base/bulk_data.row".into(), id, old: json!({"undo":null}), new: json!({"undo":"removed"}) }]))
        })).await?;
        Ok(row)
    }

    async fn record_undo_result(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        run_id: i64,
        job_id: i64,
        result: RecordedUndo,
    ) -> Result<bool> {
        let run = store::lock(tx.sql(), scope, run_id).await?;
        if run.state != RunState::Undoing || run.job_id != Some(job_id) {
            return Ok(false);
        }
        let tenant = scope.tenant();
        let row_number = result.row_number;
        let outcome = result.outcome;
        let message = result.message.map(import::bounded);
        tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_opt(
                "UPDATE base_bulk_data_rows SET undo_outcome=$4,undo_message=$5,undone_at=CASE WHEN $4='removed' THEN clock_timestamp() END WHERE tenant_id=$1 AND run_id=$2 AND row_number=$3 AND pass='run' AND outcome='create' AND undo_outcome IS NULL RETURNING id",
                &[&tenant, &run_id, &row_number, &outcome.as_str(), &message],
            ).await?.ok_or(Error::Conflict)?;
            let id: i64 = row.get(0);
            sql.execute("UPDATE base_bulk_data_runs SET undo_next_row=greatest(undo_next_row,$3),updated_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2", &[&tenant, &run_id, &row_number]).await?;
            Ok(((), vec![Change { kind: "base/bulk_data.row".into(), id, old: json!({"undo":null}), new: json!({"undo":outcome.as_str()}) }]))
        })).await?;
        Ok(true)
    }

    async fn work_undo(&self, execution: &Execution, run_id: i64) -> Result<()> {
        let scope = execution
            .scope()
            .filter(|scope| scope.principal().is_none())
            .ok_or(Error::Forbidden)?
            .clone();
        let mut object = self.db.get().await?;
        let client: &mut tokio_postgres::Client = &mut object;
        let started = Instant::now();
        loop {
            let run = store::load(&*client, &scope, run_id, false).await?;
            if run.state != RunState::Undoing || run.job_id != Some(execution.job_id) {
                return Ok(());
            }
            let registered = self
                .types
                .get(run.record_type.as_str())
                .ok_or(Error::NotFound)?;
            let undo = registered.undo.ok_or(Error::NotFound)?;
            let next = client.query_opt(
                "SELECT row_number,record_id,record_version FROM base_bulk_data_rows WHERE tenant_id=$1 AND run_id=$2 AND pass='run' AND outcome='create' AND record_id IS NOT NULL AND record_version IS NOT NULL AND row_number>$3 AND undo_outcome IS NULL ORDER BY row_number LIMIT 1",
                &[&scope.tenant(), &run_id, &run.undo_next_row],
            ).await?;
            let Some(row) = next else {
                return self.finish_undo(execution, client, &scope, run_id).await;
            };
            let (row_number, record_id, version): (i32, String, i64) =
                (row.get(0), row.get(1), row.get(2));
            let mut input = serde_json::Map::new();
            input.insert("run_id".into(), json!(run_id));
            input.insert(undo.delete_id.into(), crate::typed(&record_id));
            input.insert(undo.delete_version.into(), json!(version));
            let mut tx = execution.begin(client, undo.delete).await?;
            let result = self
                .jobs
                .commit(&mut tx, execution, undo.delete, &Value::Object(input))
                .await;
            match result {
                Ok(_) => tx.commit().await?,
                Err(error) => {
                    drop(tx);
                    let (outcome, message) = match &error {
                        ContractError::Backend(Error::Conflict) => (
                            UndoOutcome::Edited,
                            "Changed after it was imported, so it was left alone.".to_owned(),
                        ),
                        ContractError::NotFound | ContractError::Backend(Error::NotFound) => (
                            UndoOutcome::Removed,
                            "It had already been removed.".to_owned(),
                        ),
                        _ => match import::refusal(&error, &[]) {
                            Some(message) => (UndoOutcome::Error, message),
                            None => return Err(import::error_of(error)),
                        },
                    };
                    let mut tx = execution
                        .begin(client, "base.bulk_data.import.undo-row")
                        .await?;
                    if !self
                        .record_undo_result(
                            &mut tx,
                            &scope,
                            run_id,
                            execution.job_id,
                            RecordedUndo {
                                row_number,
                                outcome,
                                message: Some(message),
                            },
                        )
                        .await?
                    {
                        return Ok(());
                    }
                    tx.commit().await?;
                }
            }
            self.wake(scope.tenant(), scope.user());
            if started.elapsed() >= SLICE {
                return self.continue_undo(execution, client, &scope, run_id).await;
            }
        }
    }

    async fn continue_undo(
        &self,
        execution: &Execution,
        client: &mut tokio_postgres::Client,
        scope: &Scope,
        run_id: i64,
    ) -> Result<()> {
        let mut tx = execution
            .begin(client, "base.bulk_data.import.undo-continue")
            .await?;
        let run = store::lock(tx.sql(), scope, run_id).await?;
        if run.state != RunState::Undoing || run.job_id != Some(execution.job_id) {
            return Ok(());
        }
        let job = self
            .queue
            .enqueue_for(&mut tx, scope, WORKER, json!({"run":run_id}))
            .await?;
        let tenant = scope.tenant();
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute("UPDATE base_bulk_data_runs SET job_id=$3,updated_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2", &[&tenant, &run_id, &job.id()]).await?;
            Ok(((), vec![Change { kind: "base/bulk_data.run".into(), id: run_id, old: Value::Null, new: json!({"undo_continued":true}) }]))
        })).await?;
        tx.commit().await
    }

    async fn finish_undo(
        &self,
        execution: &Execution,
        client: &mut tokio_postgres::Client,
        scope: &Scope,
        run_id: i64,
    ) -> Result<()> {
        let mut tx = execution
            .begin(client, "base.bulk_data.import.undo-finish")
            .await?;
        let old = store::lock(tx.sql(), scope, run_id).await?;
        if old.state != RunState::Undoing || old.job_id != Some(execution.job_id) {
            return Ok(());
        }
        let tenant = scope.tenant();
        let run = tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_one(&format!("UPDATE base_bulk_data_runs SET state='undone',undo_finished_at=clock_timestamp(),version=version+1,updated_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 RETURNING {}", store::COLUMNS), &[&tenant, &run_id]).await?;
            let run = Run::from_row(&row)?;
            Ok((run, vec![Change { kind: "base/bulk_data.run".into(), id: run_id, old: crate::evidence(&old), new: json!({"state":"undone"}) }]))
        })).await?;
        let mut counted = vec![run];
        store::count(tx.sql(), tenant, &mut counted).await?;
        let counts = counted[0].undo;
        let notice = Notice::new(format!("Undo of {} finished", counted[0].file_name))
            .kind("bulk_data.import.undo")
            .body(format!(
                "{} removed, {} left because they changed, {} could not be removed.",
                counts.removed, counts.edited, counts.error
            ))
            .url(format!("/imports/{run_id}?undo=left"));
        self.notifications
            .notify(&mut tx, scope, &[scope.user()], &notice)
            .await?;
        tx.commit().await?;
        self.wake(tenant, scope.user());
        Ok(())
    }
}
