//! Graph lifecycle writes. The subject lock precedes the run and graph locks;
//! every policy check, work fact and event shares the caller's audited commit.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub expected_version: i64,
    pub executor_key: String,
    pub outcome: String,
    pub output: Value,
    pub result_ref: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunCommand {
    /// Release due timers and propagate impossible dependency outcomes.
    Reconcile,
    Complete {
        step_key: String,
        completion: Completion,
    },
    Signal {
        name: String,
        payload: Value,
        idempotency_key: String,
    },
    Pause {
        reason: String,
    },
    Resume,
    /// Preserve terminal work and block every unfinished item. Live leases refuse.
    Supersede {
        reason: String,
    },
    /// Owner or operator path: finish one unfinished item, even under a live
    /// lease, whose worker then sees the terminal item. Terminal work is a no-op.
    Waive {
        step_key: String,
        reason: String,
        outcome: String,
    },
    Block {
        step_key: String,
        reason: String,
    },
}
impl RunCommand {
    fn operation(&self) -> Operation {
        match self {
            Self::Reconcile => Operation::ReconcileRun,
            Self::Complete { .. } => Operation::CompleteWork,
            Self::Signal { .. } => Operation::SignalRun,
            Self::Pause { .. } => Operation::PauseRun,
            Self::Resume => Operation::ResumeRun,
            Self::Supersede { .. } => Operation::SupersedeRun,
            Self::Waive { .. } => Operation::WaiveWork,
            Self::Block { .. } => Operation::BlockWork,
        }
    }
    fn action(&self) -> &'static str {
        match self {
            Self::Reconcile => "workflow.process.reconciled",
            Self::Complete { .. } => "workflow.work.completed",
            Self::Signal { .. } => "workflow.signal.received",
            Self::Pause { .. } => "workflow.process.paused",
            Self::Resume => "workflow.process.resumed",
            Self::Supersede { .. } => "workflow.process.superseded",
            Self::Waive { .. } => "workflow.work.waived",
            Self::Block { .. } => "workflow.work.blocked",
        }
    }
    fn validate(&self) -> Result<()> {
        use crate::process::text;
        let valid = match self {
            Self::Complete {
                step_key,
                completion: c,
            } => {
                crate::definitions::key(step_key)
                    && c.expected_version >= 1
                    && text(&c.executor_key)
                    && text(&c.outcome)
                    && c.result_ref.as_deref().is_none_or(text)
            }
            Self::Signal {
                name,
                idempotency_key,
                ..
            } => text(name) && text(idempotency_key),
            Self::Pause { reason } | Self::Supersede { reason } => reason_text(reason),
            Self::Waive {
                step_key,
                reason,
                outcome,
            } => crate::definitions::key(step_key) && reason_text(reason) && text(outcome),
            Self::Block { step_key, reason } => {
                crate::definitions::key(step_key) && reason_text(reason)
            }
            _ => true,
        };
        if !valid || serde_json::to_vec(self).map_err(|_| Error::Invalid)?.len() > 65536 {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
pub(super) fn reason_text(reason: &str) -> bool {
    !reason.trim().is_empty() && !reason.contains('\0') && reason.len() <= 16000
}
impl Engine {
    pub async fn update_run(
        &self,
        scope: &Scope,
        run_id: i64,
        command: RunCommand,
    ) -> Result<RunView> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(&mut client, scope.mutation_context(command.action()))
            .await?;
        let result = self.update_run_in(&mut tx, scope, run_id, command).await?;
        tx.commit().await?;
        Ok(result)
    }
    /// Owner facades use this to commit their business writes with completion.
    /// A refusal or cancellation poisons the entire enclosing transaction.
    /// `ReadRun` never authorizes these operations: both owner adapters receive
    /// the specific command operation after live authentication and locking.
    pub async fn update_run_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        run_id: i64,
        command: RunCommand,
    ) -> Result<RunView> {
        let context = tx.context().clone();
        let engine = self.clone();
        let scope = scope.clone();
        tx.mutate(move |sql| {
            Box::pin(async move {
                command.validate()?;
                engine.writer(&context, &scope, run_id)?;
                let scope = engine.live(sql, &scope).await?;
                let (run, definition, items, dependencies) = engine
                    .locked_run(sql, &scope, run_id, command.operation())
                    .await?;
                let mut changes = Vec::new();
                apply(sql, &run, items, &dependencies, &command, &mut changes).await?;
                let run = sql
                    .query_one(
                        &format!(
                            "SELECT {RUN_COLUMNS} FROM base_workflow_process_runs WHERE id=$1"
                        ),
                        &[&run.id],
                    )
                    .await
                    .map(run_row)?;
                let (items, _) = graph(sql, &run, &definition, false).await?;
                // Idempotent commands still record an authorized attempt, without
                // appending duplicate lifecycle events or rewriting completed work.
                changes.push(Change {
                    kind: "workflow.process_command".into(),
                    id: run.id,
                    old: json!({}),
                    new: json!({"action":command.action(),"status":run.status}),
                });
                Ok((
                    RunView {
                        run,
                        work_items: items.into_iter().map(|i| i.fact).collect(),
                    },
                    changes,
                ))
            })
        })
        .await
    }
    pub(super) async fn locked_run(
        &self,
        sql: &Transaction<'_>,
        scope: &Scope,
        run_id: i64,
        operation: Operation,
    ) -> Result<(Run, ProcessDefinition, Vec<Item>, Vec<Dependency>)> {
        let candidate = sql.query_opt(&format!("SELECT {RUN_COLUMNS} FROM base_workflow_process_runs WHERE id=$1 AND tenant_id=$2"), &[&run_id, &scope.tenant()]).await?.map(run_row).ok_or(Error::NotFound)?;
        let owner = self.registry.owned_subject(&candidate.subject_type)?;
        if candidate.owner != owner.owner {
            return Err(Error::Invalid);
        }
        let subject = self
            .load(
                sql,
                scope,
                owner,
                candidate.subject_id,
                Load::Lock,
                operation,
            )
            .await?;
        let run = sql.query_opt(&format!("SELECT {RUN_COLUMNS} FROM base_workflow_process_runs WHERE id=$1 AND tenant_id=$2 FOR UPDATE"), &[&run_id, &scope.tenant()]).await?.map(run_row).ok_or(Error::NotFound)?;
        if run.subject_type != candidate.subject_type
            || run.subject_id != candidate.subject_id
            || run.owner != candidate.owner
        {
            return Err(Error::Conflict);
        }
        let definition = self.supported(sql, &run, owner).await?;
        definition
            .adapter
            .authorize(sql, scope, &subject, &facts(&run), operation)
            .await?;
        let (items, dependencies) = graph(sql, &run, &definition, true).await?;
        Ok((run, definition, items, dependencies))
    }
}
async fn apply(
    sql: &Transaction<'_>,
    run: &Run,
    items: Vec<Item>,
    dependencies: &[Dependency],
    command: &RunCommand,
    changes: &mut Vec<Change>,
) -> Result<()> {
    match command {
        RunCommand::Reconcile => settle(sql, run, items, dependencies, changes).await,
        RunCommand::Complete {
            step_key,
            completion,
        } => {
            let item = items
                .iter()
                .find(|i| i.fact.step_key == *step_key)
                .ok_or(Error::NotFound)?;
            completable(
                sql,
                run,
                item,
                &completion.executor_key,
                completion.expected_version,
            )
            .await?;
            let finish = Finish::completed(
                item,
                &completion.outcome,
                &completion.output,
                completion.result_ref.as_deref(),
            );
            finish_item(sql, run, item, finish, items.len(), dependencies, changes).await
        }
        RunCommand::Signal {
            name,
            payload,
            idempotency_key,
        } => {
            let key = event_key(format!("signal:{}:{idempotency_key}", run.id));
            let expected = json!({"signal":name,"payload":payload});
            if let Some(previous) = sql.query_opt("SELECT type,payload FROM base_workflow_process_events WHERE run_id=$1 AND tenant_id=$2 AND idempotency_key=$3", &[&run.id, &run.tenant, &key]).await? {
                if previous.get::<_, String>(0) == "signal.received" && previous.get::<_, Option<Value>>(1).as_ref() == Some(&expected) {
                    return Ok(());
                }
                return Err(Error::Conflict);
            }
            if run.status != "running" {
                return Err(Error::Conflict);
            }
            for item in &items {
                if item.fact.required_signal.as_ref() == Some(name)
                    && item.fact.signalled_at.is_none()
                    && !TERMINAL_WORK.contains(&item.fact.status.as_str())
                {
                    sql.execute("UPDATE base_workflow_process_work_items SET signalled_at=now(),signal_payload=$2,updated_at=now() WHERE id=$1", &[&item.fact.id, payload]).await?;
                    changes.push(Change {
                        kind: "workflow.process_work".into(),
                        id: item.fact.id,
                        old: json!({"signalled_at":null}),
                        new: json!({"signal":name,"signal_payload":payload}),
                    });
                }
            }
            append_keyed_event(
                sql,
                run,
                None,
                "signal.received",
                expected,
                Some(&key),
                changes,
            )
            .await?;
            let items = load_items(sql, run, items.len() as i64, false).await?;
            settle(sql, run, items, dependencies, changes).await
        }
        RunCommand::Pause { reason } => {
            if run.status == "paused" {
                return Ok(());
            }
            if run.status != "running" {
                return Err(Error::Conflict);
            }
            sql.execute("UPDATE base_workflow_process_runs SET status='paused',paused_at=now(),pause_reason=$2,heartbeat_at=now(),updated_at=now() WHERE id=$1", &[&run.id, reason]).await?;
            changes.push(Change {
                kind: "workflow.process_run".into(),
                id: run.id,
                old: json!({"status":run.status}),
                new: json!({"status":"paused","pause_reason":reason}),
            });
            append_event(
                sql,
                run,
                None,
                "process.paused",
                json!({"reason":reason}),
                changes,
            )
            .await?;
            Ok(())
        }
        RunCommand::Resume => {
            if run.status == "running" {
                return Ok(());
            }
            if run.status != "paused" {
                return Err(Error::Conflict);
            }
            sql.execute("UPDATE base_workflow_process_runs SET status='running',paused_at=NULL,pause_reason=NULL,heartbeat_at=now(),updated_at=now() WHERE id=$1", &[&run.id]).await?;
            changes.push(Change {
                kind: "workflow.process_run".into(),
                id: run.id,
                old: json!({"status":run.status}),
                new: json!({"status":"running"}),
            });
            append_event(sql, run, None, "process.resumed", Value::Null, changes).await?;
            let mut running = run.clone();
            running.status = "running".into();
            settle(sql, &running, items, dependencies, changes).await
        }
        RunCommand::Supersede { reason } => {
            if !matches!(run.status.as_str(), "running" | "paused") {
                return Ok(());
            }
            let leased: bool = sql.query_one("SELECT EXISTS(SELECT 1 FROM base_workflow_process_work_items WHERE run_id=$1 AND status='leased' AND lease_expires_at>now())", &[&run.id]).await?.get(0);
            if leased {
                return Err(Error::Conflict);
            }
            for item in &items {
                if TERMINAL_WORK.contains(&item.fact.status.as_str()) {
                    continue;
                }
                sql.execute("UPDATE base_workflow_process_work_items SET status='blocked',outcome='blocked',output=NULL,last_error=$2,lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL,heartbeat_at=NULL,completed_at=now(),updated_at=now(),version=version+1 WHERE id=$1", &[&item.fact.id, reason]).await?;
                changes.push(Change {kind:"workflow.process_work".into(),id:item.fact.id,old:json!({"status":item.fact.status,"version":item.fact.version}),new:json!({"status":"blocked","outcome":"blocked","reason":reason,"version":item.fact.version+1})});
                append_event(
                    sql,
                    run,
                    Some(item.fact.id),
                    "work.blocked",
                    json!({"outcome":"blocked","reason":reason,"cause":"process_superseded"}),
                    changes,
                )
                .await?;
            }
            let items = load_items(sql, run, items.len() as i64, false).await?;
            finish_run(sql, run, &items, changes).await?;
            append_event(
                sql,
                run,
                None,
                "process.superseded",
                json!({"reason":reason}),
                changes,
            )
            .await?;
            Ok(())
        }
        RunCommand::Waive {
            step_key,
            reason,
            outcome,
        } => {
            administer(
                sql,
                run,
                &items,
                dependencies,
                step_key,
                "waived",
                outcome,
                reason,
                changes,
            )
            .await
        }
        RunCommand::Block { step_key, reason } => {
            administer(
                sql,
                run,
                &items,
                dependencies,
                step_key,
                "blocked",
                "blocked",
                reason,
                changes,
            )
            .await
        }
    }
}
/// A completion needs a running, error-free run and an `available`, due item
/// of the expected executor at the expected version; anything else is stale.
pub(super) async fn completable(
    sql: &Transaction<'_>,
    run: &Run,
    item: &Item,
    executor_key: &str,
    expected_version: i64,
) -> Result<()> {
    let due: bool = sql.query_one("SELECT r.available_at<=now() AND w.available_at<=now() FROM base_workflow_process_runs r JOIN base_workflow_process_work_items w ON w.run_id=r.id WHERE r.id=$1 AND w.id=$2", &[&run.id, &item.fact.id]).await?.get::<_, Option<bool>>(0).unwrap_or(false);
    if run.status != "running"
        || run.last_error.is_some()
        || item.fact.status != "available"
        || !due
        || item.fact.executor_key != executor_key
        || item.fact.version != expected_version
    {
        return Err(Error::Conflict);
    }
    Ok(())
}
async fn administer(
    sql: &Transaction<'_>,
    run: &Run,
    items: &[Item],
    dependencies: &[Dependency],
    step_key: &str,
    status: &'static str,
    outcome: &str,
    reason: &str,
    changes: &mut Vec<Change>,
) -> Result<()> {
    let item = items
        .iter()
        .find(|i| i.fact.step_key == step_key)
        .ok_or(Error::NotFound)?;
    if TERMINAL_WORK.contains(&item.fact.status.as_str()) {
        return Ok(());
    }
    let finish = Finish {
        status,
        outcome: outcome.into(),
        output: None,
        result_ref: item.fact.result_ref.clone(),
        error: Some(reason.into()),
        event: if status == "waived" {
            "work.waived"
        } else {
            "work.blocked"
        },
        payload: json!({"outcome":outcome,"reason":reason}),
    };
    finish_item(sql, run, item, finish, items.len(), dependencies, changes).await
}
/// One terminal write for an unfinished item: the lease owner and token are
/// cleared and the version is fenced, then the graph settles.
pub(super) struct Finish {
    pub status: &'static str,
    pub outcome: String,
    pub output: Option<Value>,
    pub result_ref: Option<String>,
    pub error: Option<String>,
    pub event: &'static str,
    pub payload: Value,
}
impl Finish {
    pub fn completed(item: &Item, outcome: &str, output: &Value, result_ref: Option<&str>) -> Self {
        let result_ref = result_ref
            .map(str::to_owned)
            .or_else(|| item.fact.result_ref.clone());
        Self {
            status: "completed",
            outcome: outcome.into(),
            output: Some(output.clone()),
            payload: json!({"outcome":outcome,"output":output,"result_ref":result_ref}),
            result_ref,
            error: None,
            event: "work.completed",
        }
    }
}
pub(super) async fn finish_item(
    sql: &Transaction<'_>,
    run: &Run,
    item: &Item,
    finish: Finish,
    count: usize,
    dependencies: &[Dependency],
    changes: &mut Vec<Change>,
) -> Result<()> {
    let fact = &item.fact;
    sql.execute("UPDATE base_workflow_process_work_items SET status=$2,outcome=$3,output=$4,result_ref=$5,last_error=$6,lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL,heartbeat_at=NULL,completed_at=now(),updated_at=now(),version=version+1 WHERE id=$1", &[&fact.id, &finish.status, &finish.outcome, &finish.output, &finish.result_ref, &finish.error]).await?;
    changes.push(Change {kind:"workflow.process_work".into(),id:fact.id,old:json!({"status":fact.status,"version":fact.version}),new:json!({"status":finish.status,"outcome":finish.outcome,"output":finish.output,"result_ref":finish.result_ref,"version":fact.version+1})});
    append_event(
        sql,
        run,
        Some(fact.id),
        finish.event,
        finish.payload,
        changes,
    )
    .await?;
    let items = load_items(sql, run, count as i64, false).await?;
    settle(sql, run, items, dependencies, changes).await
}
