//! Worker leases. A claim selects one due candidate without row locks, then
//! takes subject -> run -> items in the kernel lock order, reconciles expired
//! leases and timers on the saved graph, and leases the item only if it is
//! still available. Every later worker call carries the claim's token.
use super::*;
use coordination::{finish_item, reason_text, Finish};

/// Candidates refused under their lock are skipped; the claim gives up after
/// this many, so retained unsupported runs never starve supported work.
const CLAIM_ATTEMPTS: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimOptions {
    pub lease_seconds: i32,
    pub definition_key: Option<String>,
    /// `Some(empty)` claims nothing.
    pub executor_keys: Option<Vec<String>>,
    pub run_ids: Option<Vec<i64>>,
}
impl Default for ClaimOptions {
    fn default() -> Self {
        Self {
            lease_seconds: 300,
            definition_key: None,
            executor_keys: None,
            run_ids: None,
        }
    }
}
/// The worker's durable credential for one leased item. Keep it with the
/// in-flight work so a restarted worker can still finish or fail it; Workflow
/// never returns the token in run or work facts.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub run_id: i64,
    pub work_item_id: i64,
    pub lease_token: String,
}
impl std::fmt::Debug for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Claim")
            .field("run_id", &self.run_id)
            .field("work_item_id", &self.work_item_id)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claimed {
    pub run: Run,
    pub work_item: WorkItem,
    pub claim: Claim,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkView {
    pub run: Run,
    pub work_item: WorkItem,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkCommand {
    /// Extend a live lease to now plus `lease_seconds`.
    Heartbeat { lease_seconds: i32 },
    Complete {
        outcome: String,
        output: Value,
        result_ref: Option<String>,
    },
    /// Retryable work with attempts remaining returns to pending at
    /// `retry_at` (UTC whole seconds, the caller's backoff; `None` is now);
    /// otherwise the item fails.
    Fail {
        error: String,
        retryable: bool,
        category: String,
        retry_at: Option<i64>,
    },
    Block {
        reason: String,
        output: Value,
        result_ref: Option<String>,
    },
}
impl WorkCommand {
    fn operation(&self) -> Operation {
        match self {
            Self::Heartbeat { .. } => Operation::HeartbeatWork,
            Self::Complete { .. } => Operation::CompleteWork,
            Self::Fail { .. } => Operation::FailWork,
            Self::Block { .. } => Operation::BlockClaimedWork,
        }
    }
    fn action(&self) -> &'static str {
        match self {
            Self::Heartbeat { .. } => "workflow.work.heartbeat",
            Self::Complete { .. } => "workflow.work.completed",
            Self::Fail { .. } => "workflow.work.failed",
            Self::Block { .. } => "workflow.work.blocked",
        }
    }
    fn validate(&self) -> Result<()> {
        use crate::process::text;
        let valid = match self {
            Self::Heartbeat { lease_seconds } => *lease_seconds >= 1,
            Self::Complete {
                outcome,
                result_ref,
                ..
            } => text(outcome) && result_ref.as_deref().is_none_or(text),
            Self::Fail {
                error,
                category,
                retry_at,
                ..
            } => reason_text(error) && text(category) && retry_at.is_none_or(|t| t >= 0),
            Self::Block {
                reason, result_ref, ..
            } => reason_text(reason) && result_ref.as_deref().is_none_or(text),
        };
        if !valid || serde_json::to_vec(self).map_err(|_| Error::Invalid)?.len() > 65536 {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
impl Claim {
    fn valid(&self) -> bool {
        self.run_id > 0 && self.work_item_id > 0 && crate::process::text(&self.lease_token)
    }
}
/// Length is not secret (every token is a UUID); content is compared without
/// an early exit.
fn same_token(saved: &str, given: &str) -> bool {
    saved.len() == given.len()
        && saved
            .bytes()
            .zip(given.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}
async fn item_view(sql: &Transaction<'_>, run_id: i64, item_id: i64) -> Result<WorkView> {
    let run = sql
        .query_one(
            &format!("SELECT {RUN_COLUMNS} FROM base_workflow_process_runs WHERE id=$1"),
            &[&run_id],
        )
        .await
        .map(run_row)?;
    let item = sql
        .query_one(
            &format!("SELECT {ITEM_COLUMNS} FROM base_workflow_process_work_items WHERE id=$1"),
            &[&item_id],
        )
        .await
        .map(item_row)?;
    Ok(WorkView {
        run,
        work_item: item.fact,
    })
}
impl Engine {
    /// Leases one due item to `worker`, or returns `None` when nothing is
    /// claimable. Due pending work and expired leases are candidates too: the
    /// locked run is reconciled first, so a worker started after a crash
    /// recovers the expired leases it finds. Only runs saved under an
    /// installed definition fingerprint are considered.
    pub async fn claim_work(
        &self,
        scope: &Scope,
        worker: &str,
        options: ClaimOptions,
    ) -> Result<Option<Claimed>> {
        use crate::process::text;
        if !text(worker)
            || options.lease_seconds < 1
            || options.definition_key.as_deref().is_some_and(|k| !text(k))
            || options
                .executor_keys
                .as_ref()
                .is_some_and(|keys| keys.len() > 500 || !keys.iter().all(|k| text(k)))
            || options
                .run_ids
                .as_ref()
                .is_some_and(|ids| ids.len() > 500 || ids.iter().any(|id| *id <= 0))
        {
            return Err(Error::Invalid);
        }
        if options.executor_keys.as_ref().is_some_and(Vec::is_empty)
            || options.run_ids.as_ref().is_some_and(Vec::is_empty)
        {
            return Ok(None);
        }
        let (mut keys, mut versions, mut fingerprints) = (Vec::new(), Vec::new(), Vec::new());
        for definition in self.registry.processes.values() {
            keys.push(definition.key);
            versions.push(definition.version);
            fingerprints.push(definition.fingerprint.as_str());
        }
        let (mut skipped_items, mut skipped_runs): (Vec<i64>, Vec<i64>) = (Vec::new(), Vec::new());
        let mut client = self.db.get().await?;
        for _ in 0..CLAIM_ATTEMPTS {
            // Unlocked selection: the database never chooses a lock order.
            let candidate = client.query_opt("SELECT w.id,r.id FROM base_workflow_process_work_items w JOIN base_workflow_process_runs r ON r.id=w.run_id WHERE r.tenant_id=$1 AND w.tenant_id=$1 AND r.status='running' AND r.last_error IS NULL AND r.available_at<=now() AND NOT w.id=ANY($2) AND NOT r.id=ANY($3) AND ((w.status IN ('available','pending') AND w.available_at<=now()) OR (w.status='leased' AND w.lease_expires_at<=now())) AND (r.definition_key,r.definition_version,r.definition_fingerprint) IN (SELECT * FROM unnest($4::text[],$5::integer[],$6::text[])) AND ($7::text IS NULL OR r.definition_key=$7) AND ($8::text[] IS NULL OR w.executor_key=ANY($8)) AND ($9::bigint[] IS NULL OR r.id=ANY($9)) ORDER BY r.priority DESC,w.priority DESC,w.available_at,w.id LIMIT 1", &[&scope.tenant(),&skipped_items,&skipped_runs,&keys,&versions,&fingerprints,&options.definition_key,&options.executor_keys,&options.run_ids]).await?;
            let Some(candidate) = candidate else {
                return Ok(None);
            };
            let (item_id, run_id): (i64, i64) = (candidate.get(0), candidate.get(1));
            let mut tx = self
                .boundary
                .begin(&mut client, scope.mutation_context("workflow.work.claimed"))
                .await?;
            let context = tx.context().clone();
            let (engine, scope, worker) = (self.clone(), scope.clone(), worker.to_owned());
            let lease_seconds = options.lease_seconds;
            let result = tx
                .mutate(move |sql| {
                    Box::pin(async move {
                        engine.writer(&context, &scope, run_id)?;
                        let scope = engine.live(sql, &scope).await?;
                        let (run, _, items, dependencies) = engine
                            .locked_run(sql, &scope, run_id, Operation::ClaimWork)
                            .await?;
                        let mut changes = Vec::new();
                        settle(sql, &run, items, &dependencies, &mut changes).await?;
                        let eligible: bool = sql.query_opt("SELECT r.status='running' AND r.last_error IS NULL AND r.available_at<=now() AND w.status='available' AND w.available_at<=now() FROM base_workflow_process_runs r JOIN base_workflow_process_work_items w ON w.run_id=r.id WHERE r.id=$1 AND w.id=$2", &[&run_id, &item_id]).await?.and_then(|row| row.get::<_, Option<bool>>(0)).unwrap_or(false);
                        let claim = if eligible {
                            let token = uuid::Uuid::new_v4().to_string();
                            let row = sql.query_one("UPDATE base_workflow_process_work_items SET status='leased',attempts=attempts+1,lease_owner=$2,lease_token=$3,lease_expires_at=now()+make_interval(secs=>$4::integer),heartbeat_at=now(),updated_at=now() WHERE id=$1 RETURNING attempts,lease_expires_at::text,version", &[&item_id, &worker, &token, &lease_seconds]).await?;
                            let (attempt, expires, version): (i32, String, i64) =
                                (row.get(0), row.get(1), row.get(2));
                            sql.execute("UPDATE base_workflow_process_runs SET heartbeat_at=now(),updated_at=now() WHERE id=$1", &[&run_id]).await?;
                            // Audit receives the worker and attempt, never the token.
                            changes.push(Change {kind:"workflow.process_work".into(),id:item_id,old:json!({"status":"available"}),new:json!({"status":"leased","worker":worker,"attempt":attempt,"lease_expires_at":expires,"version":version})});
                            append_event(sql, &run, Some(item_id), "work.claimed", json!({"worker":worker,"attempt":attempt,"lease_expires_at":expires}), &mut changes).await?;
                            Some(Claim {
                                run_id,
                                work_item_id: item_id,
                                lease_token: token,
                            })
                        } else {
                            None
                        };
                        changes.push(Change {
                            kind: "workflow.work_claim".into(),
                            id: run_id,
                            old: json!({}),
                            new: json!({"work_item":item_id,"claimed":claim.is_some()}),
                        });
                        let claimed = match claim {
                            Some(claim) => {
                                let view = item_view(sql, run_id, item_id).await?;
                                Some(Claimed {
                                    run: view.run,
                                    work_item: view.work_item,
                                    claim,
                                })
                            }
                            None => None,
                        };
                        Ok((claimed, changes))
                    })
                })
                .await;
            match result {
                Ok(Some(claimed)) => {
                    tx.commit().await?;
                    return Ok(Some(claimed));
                }
                // The reconciliation still commits; the item stays a candidate
                // for nobody else in this claim.
                Ok(None) => {
                    tx.commit().await?;
                    skipped_items.push(item_id);
                }
                // Owner policy, contention or an unsupported run: roll back
                // and skip the run. Identity and infrastructure errors stop.
                Err(Error::Forbidden | Error::Conflict | Error::Invalid | Error::NotFound) => {
                    drop(tx);
                    skipped_runs.push(run_id);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
    pub async fn update_work(
        &self,
        scope: &Scope,
        claim: &Claim,
        command: WorkCommand,
    ) -> Result<WorkView> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(&mut client, scope.mutation_context(command.action()))
            .await?;
        let result = self.update_work_in(&mut tx, scope, claim, command).await?;
        tx.commit().await?;
        Ok(result)
    }
    /// Leaseholder calls inside the owner's transaction, so business writes
    /// and the completion commit together; a refusal poisons the whole
    /// transaction. The token must match a live lease. A repeated completion,
    /// failure or block on an item that is already terminal returns it
    /// unchanged; heartbeat always needs the live lease.
    pub async fn update_work_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        claim: &Claim,
        command: WorkCommand,
    ) -> Result<WorkView> {
        let context = tx.context().clone();
        let (engine, scope, claim) = (self.clone(), scope.clone(), claim.clone());
        tx.mutate(move |sql| {
            Box::pin(async move {
                command.validate()?;
                if !claim.valid() {
                    return Err(Error::Invalid);
                }
                engine.writer(&context, &scope, claim.run_id)?;
                let scope = engine.live(sql, &scope).await?;
                let (run, _, items, dependencies) = engine
                    .locked_run(sql, &scope, claim.run_id, command.operation())
                    .await?;
                let item = items
                    .iter()
                    .find(|i| i.fact.id == claim.work_item_id)
                    .ok_or(Error::NotFound)?;
                let mut changes = Vec::new();
                let terminal = TERMINAL_WORK.contains(&item.fact.status.as_str());
                if !(terminal && !matches!(command, WorkCommand::Heartbeat { .. })) {
                    let (token, live): (Option<String>, Option<bool>) = sql.query_one("SELECT lease_token,lease_expires_at>now() FROM base_workflow_process_work_items WHERE id=$1", &[&item.fact.id]).await.map(|row| (row.get(0), row.get(1)))?;
                    // A lease that was reconciled away, re-leased to another
                    // worker or simply expired is no longer this worker's.
                    if item.fact.status != "leased"
                        || !token.is_some_and(|saved| same_token(&saved, &claim.lease_token))
                        || live != Some(true)
                    {
                        return Err(Error::Conflict);
                    }
                    work(sql, &run, item, &items, &dependencies, &command, &mut changes).await?;
                }
                changes.push(Change {
                    kind: "workflow.work_command".into(),
                    id: item.fact.id,
                    old: json!({}),
                    new: json!({"action":command.action(),"run":run.id}),
                });
                Ok((item_view(sql, run.id, item.fact.id).await?, changes))
            })
        })
        .await
    }
}
async fn work(
    sql: &Transaction<'_>,
    run: &Run,
    item: &Item,
    items: &[Item],
    dependencies: &[Dependency],
    command: &WorkCommand,
    changes: &mut Vec<Change>,
) -> Result<()> {
    let fact = &item.fact;
    match command {
        WorkCommand::Heartbeat { lease_seconds } => {
            let expires: String = sql.query_one("UPDATE base_workflow_process_work_items SET heartbeat_at=now(),lease_expires_at=now()+make_interval(secs=>$2::integer),updated_at=now() WHERE id=$1 RETURNING lease_expires_at::text", &[&fact.id, lease_seconds]).await?.get(0);
            sql.execute(
                "UPDATE base_workflow_process_runs SET heartbeat_at=now(),updated_at=now() WHERE id=$1",
                &[&run.id],
            )
            .await?;
            changes.push(Change {
                kind: "workflow.process_work".into(),
                id: fact.id,
                old: json!({"lease_expires_at":fact.lease_expires_at}),
                new: json!({"lease_expires_at":expires}),
            });
            Ok(())
        }
        WorkCommand::Complete {
            outcome,
            output,
            result_ref,
        } => {
            let finish = Finish::completed(item, outcome, output, result_ref.as_deref());
            finish_item(sql, run, item, finish, items.len(), dependencies, changes).await
        }
        WorkCommand::Fail {
            error,
            retryable,
            category,
            retry_at,
        } => {
            let failure = json!({"retryable":retryable,"category":category});
            if *retryable && fact.attempts < fact.max_attempts {
                let at: String = sql.query_one("UPDATE base_workflow_process_work_items SET status='pending',available_at=coalesce(to_timestamp($2::bigint),now()),last_error=$3,lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL,heartbeat_at=NULL,updated_at=now(),version=version+1 WHERE id=$1 RETURNING available_at::text", &[&fact.id, retry_at, error]).await?.get(0);
                changes.push(Change {kind:"workflow.process_work".into(),id:fact.id,old:json!({"status":fact.status,"version":fact.version}),new:json!({"status":"pending","available_at":at,"failure":failure,"version":fact.version+1})});
                append_event(
                    sql,
                    run,
                    Some(fact.id),
                    "work.retry_scheduled",
                    json!({"error":error,"failure":failure,"available_at":at}),
                    changes,
                )
                .await?;
                let items = load_items(sql, run, items.len() as i64, false).await?;
                settle(sql, run, items, dependencies, changes).await
            } else {
                let finish = Finish {
                    status: "failed",
                    outcome: "failed".into(),
                    output: Some(json!({"failure":failure})),
                    result_ref: fact.result_ref.clone(),
                    error: Some(error.clone()),
                    event: "work.failed",
                    payload: json!({"error":error,"failure":failure}),
                };
                finish_item(sql, run, item, finish, items.len(), dependencies, changes).await
            }
        }
        WorkCommand::Block {
            reason,
            output,
            result_ref,
        } => {
            let result_ref = result_ref.clone().or_else(|| fact.result_ref.clone());
            let finish = Finish {
                status: "blocked",
                outcome: "blocked".into(),
                output: Some(output.clone()),
                payload: json!({"outcome":"blocked","reason":reason,"output":output,"result_ref":result_ref}),
                result_ref,
                error: Some(reason.clone()),
                event: "work.blocked",
            };
            finish_item(sql, run, item, finish, items.len(), dependencies, changes).await
        }
    }
}
