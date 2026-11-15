//! Running rules, after the trigger's transaction commits and never inside it.
//!
//! 1. Base Events hands a trigger event to the `base/rules.triggers`
//!    subscriber. In the delivery transaction it records one queued run per
//!    rule switched on for that event in the tenant, pinned to the rule's
//!    approved version, with the chain it belongs to (`Event::cause`), and
//!    hands the event to the `base/rules-run` worker (`Handoff`).
//! 2. The worker runs as the `rules` principal under its own tenant grants.
//!    For each queued run it checks the chain budget, the tenant's work
//!    bound, that the pinned version is still the one switched on, reads the
//!    permitted facts through Grid, evaluates the conditions and records
//!    why the run matched or not.
//! 3. Each action runs through the owner's contract on the job door
//!    (`JobContracts::commit`), in a transaction that also writes the
//!    action's receipt under the key (tenant, version, event, position): a
//!    retry after a crash finds the receipt and does nothing twice.
use crate::definition::Definition;
use crate::store::Rules;
use crate::{ACTIONS_PER_RUN, CHAIN_DEPTH, RUNS_PER_MINUTE};
use bilibi_authz::SystemScope;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_events::{Event, Handoff, On, Subscribe, SubscriberFuture};
use bilibi_grid::FieldType;
use bilibi_module_registry::Catalog;
use bilibi_queue::{Ended, Ending, EndingFuture, Execution, Handler, HandlerFuture, Outcome};
use bilibi_tenancy::Scope;
use serde_json::{json, Map, Value};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};
use tokio_postgres::Transaction;

/// The service, bound once at boot, for the subscriber (a static) to reach.
static RULES: OnceLock<Arc<Rules>> = OnceLock::new();
pub(crate) fn bind(rules: Arc<Rules>) -> bool {
    RULES.set(rules).is_ok()
}
fn rules() -> Result<&'static Arc<Rules>> {
    RULES.get().ok_or(Error::Internal)
}

/// The cause the worker names for events its actions record.
const CAUSE: &str = "base/rules.run:";
/// Queued runs one job takes in a pass.
const BATCH: i64 = 100;

pub(crate) struct Triggers;
pub(crate) static TRIGGERS: Triggers = Triggers;
impl Subscribe for Triggers {
    fn reads(&self, catalog: &Catalog) -> bilibi_module_registry::Result<Option<Vec<&'static On>>> {
        crate::catalog::trigger_events(catalog).map(Some)
    }
    fn handle<'a>(
        &'a self,
        sql: &'a Transaction<'_>,
        _scope: &'a SystemScope,
        event: &'a Event,
    ) -> SubscriberFuture<'a> {
        Box::pin(async move {
            let rules = rules()?;
            let Some(trigger) = rules.trigger(&event.name) else {
                return Ok(Vec::new());
            };
            let subject = match event.payload.pointer(trigger.definition.subject) {
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::String(s)) if !s.is_empty() && s.len() <= 200 => s.clone(),
                // The owner's payload lacks its documented record key: no rule
                // can bind it, and refusing would hold the subject's later
                // events behind it for ever.
                _ => return Ok(Vec::new()),
            };
            // The run whose action recorded this event, when one did.
            let cause = match event
                .cause
                .as_deref()
                .and_then(|c| c.strip_prefix(CAUSE))
                .and_then(|id| id.parse::<i64>().ok())
            {
                Some(id) => sql
                    .query_opt(
                        "SELECT id,depth FROM base_rule_runs WHERE tenant_id=$1 AND id=$2",
                        &[&event.tenant, &id],
                    )
                    .await?
                    .map(|row| (row.get::<_, i64>(0), row.get::<_, i32>(1))),
                None => None,
            };
            let (cause_id, depth) = (cause.map(|c| c.0), cause.map_or(1, |c| c.1 + 1));
            let rows = sql.query(
                "INSERT INTO base_rule_runs(tenant_id,rule_id,version_id,event_id,event_name,subject,cause_run_id,depth,state) SELECT tenant_id,id,active_version_id,$3,$2::varchar,$4,$5,$6,'queued' FROM base_rules WHERE tenant_id=$1 AND trigger=$2::varchar AND active_version_id IS NOT NULL AND disabled_at IS NULL ORDER BY id LIMIT 100 ON CONFLICT (tenant_id,version_id,event_id) DO NOTHING RETURNING id,rule_id",
                &[&event.tenant, &event.name, &event.id, &subject, &cause_id, &depth],
            ).await?;
            Ok(rows
                .iter()
                .map(|row| Change {
                    kind: "base/rules.run".into(),
                    id: row.get(0),
                    old: Value::Null,
                    new: json!({"rule": row.get::<_, i64>(1), "event": event.id, "subject": subject, "depth": depth, "state": "queued"}),
                })
                .collect())
        })
    }
    fn handoff(&self, event: &Event, changes: &[Change]) -> Option<Handoff> {
        changes
            .iter()
            .any(|c| c.kind == "base/rules.run")
            .then(|| Handoff {
                worker: crate::WORKER,
                principal: crate::PRINCIPAL,
                args: json!({"event_id": event.id}),
                at: None,
            })
    }
}

pub(crate) fn arguments(args: Value) -> Result<Value> {
    match args.as_object() {
        Some(o)
            if o.len() == 1
                && o.get("event_id")
                    .and_then(Value::as_i64)
                    .is_some_and(|id| id > 0) =>
        {
            Ok(args)
        }
        _ => Err(Error::Invalid),
    }
}

pub(crate) struct Worker {
    pub rules: Arc<Rules>,
}
impl Handler for Worker {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let Some(event) = args.get("event_id").and_then(Value::as_i64) else {
                return Outcome::Cancel("invalid_event_reference");
            };
            match self.event(execution, event).await {
                Ok(()) => Outcome::Completed,
                Err(Error::Unauthorized | Error::Forbidden) => Outcome::Cancel("actor_refused"),
                Err(_) => Outcome::Retry("rules_unavailable"),
            }
        })
    }
}

/// A queued run, as the worker reads it.
struct Run {
    id: i64,
    rule: i64,
    version: i64,
    event: i64,
    subject: String,
    depth: i32,
    state: String,
}

/// Why a run did not act; each is a bounded code shown on the receipt.
enum Stop {
    Refused(&'static str),
    NoMatch,
}

impl Worker {
    async fn event(&self, execution: &Execution, event: i64) -> Result<()> {
        let scope = execution.scope().ok_or(Error::Forbidden)?.clone();
        let tenant = execution.tenant();
        let rules = &self.rules;
        let client = rules.db.get().await?;
        let (depth, per_minute, actions) = (
            rules.limit(&**client, &scope, CHAIN_DEPTH).await?,
            rules.limit(&**client, &scope, RUNS_PER_MINUTE).await?,
            rules.limit(&**client, &scope, ACTIONS_PER_RUN).await?,
        );
        let runs: Vec<Run> = client.query(
            "SELECT id,rule_id,version_id,event_id,subject,depth,state FROM base_rule_runs WHERE tenant_id=$1 AND event_id=$2 AND state IN ('queued','matched') ORDER BY id LIMIT $3",
            &[&tenant, &event, &BATCH],
        ).await?.iter().map(|row| Run {
            id: row.get(0), rule: row.get(1), version: row.get(2), event: row.get(3),
            subject: row.get(4), depth: row.get(5), state: row.get(6),
        }).collect();
        drop(client);
        for run in runs {
            let mut client = rules.db.get().await?;
            // The tenant's work bound: runs started in the last minute. Past
            // it the event's runs wait for a later job, so one tenant's burst
            // never holds the shared queue.
            let started: i64 = client.query_one(
                "SELECT count(*) FROM base_rule_runs WHERE tenant_id=$1 AND started_at > now() - interval '1 minute'",
                &[&tenant],
            ).await?.get(0);
            if run.state == "queued" && started >= per_minute {
                let mut tx = execution.begin(&mut client, "rules.run.deferred").await?;
                let system =
                    SystemScope::rehydrate(tx.sql(), tenant, Some(crate::PRINCIPAL)).await?;
                rules
                    .queue
                    .enqueue_system_at_within(
                        &mut tx,
                        &system,
                        crate::WORKER,
                        json!({"event_id": event}),
                        SystemTime::now() + Duration::from_secs(60),
                    )
                    .await?;
                tx.commit().await?;
                return Ok(());
            }
            drop(client);
            self.run(execution, &scope, run, depth, actions).await?;
        }
        Ok(())
    }

    async fn run(
        &self,
        execution: &Execution,
        scope: &Scope,
        run: Run,
        depth: i64,
        actions: i64,
    ) -> Result<()> {
        let rules = &self.rules;
        let tenant = scope.tenant();
        let mut client = rules.db.get().await?;
        let row = client.query_opt(
            "SELECT r.trigger,r.active_version_id,r.disabled_at IS NOT NULL,v.definition FROM base_rules r JOIN base_rule_versions v ON v.tenant_id=r.tenant_id AND v.id=$3 WHERE r.tenant_id=$1 AND r.id=$2",
            &[&tenant, &run.rule, &run.version],
        ).await?.ok_or(Error::NotFound)?;
        let (trigger, active, off): (String, Option<i64>, bool) =
            (row.get(0), row.get(1), row.get(2));
        let mut problems = Vec::new();
        let definition =
            Definition::parse(&row.get::<_, Value>(3), &mut problems).ok_or(Error::Invalid)?;
        let stop = if i64::from(run.depth) > depth {
            Some(Stop::Refused("chain_budget"))
        } else if off {
            Some(Stop::Refused("rule_off"))
        } else if active != Some(run.version) {
            Some(Stop::Refused("stale_version"))
        } else if definition.actions.len() as i64 > actions {
            Some(Stop::Refused("action_limit"))
        } else {
            None
        };
        if let Some(stop) = stop {
            return self.finish(execution, &mut client, &run, stop, None).await;
        }
        let Some(offered) = rules.trigger(&trigger) else {
            return self
                .finish(
                    execution,
                    &mut client,
                    &run,
                    Stop::Refused("trigger_unmounted"),
                    None,
                )
                .await;
        };
        // Facts and the decision are kept once taken, so a retry after a
        // crash acts on what was decided, never on a second reading.
        let (facts, outcomes) = if run.state == "matched" {
            let saved = client
                .query_one(
                    "SELECT facts,outcomes FROM base_rule_runs WHERE tenant_id=$1 AND id=$2",
                    &[&tenant, &run.id],
                )
                .await?;
            (
                saved.get::<_, Option<Value>>(0).unwrap_or(Value::Null),
                saved.get::<_, Option<Value>>(1).unwrap_or(Value::Null),
            )
        } else {
            // Grid's read takes a connection of its own: hold none meanwhile.
            drop(client);
            let read = self
                .facts(scope, offered.definition.table, &run.subject, &definition)
                .await?;
            client = rules.db.get().await?;
            let (facts, types) = match read {
                Ok(read) => read,
                Err(code) => {
                    return self
                        .finish(execution, &mut client, &run, Stop::Refused(code), None)
                        .await
                }
            };
            let (matched, outcomes) = definition.evaluate(&facts, &|path| {
                types
                    .iter()
                    .find(|(p, _)| p == path)
                    .map_or(FieldType::String, |(_, ty)| *ty)
            });
            let decided = (Value::Object(facts), json!(outcomes));
            if !matched {
                return self
                    .finish(execution, &mut client, &run, Stop::NoMatch, Some(decided))
                    .await;
            }
            self.decide(execution, &mut client, &run, &decided).await?;
            decided
        };
        let mut refused = 0;
        for (position, call) in definition.actions.iter().enumerate() {
            match self
                .act(execution, &mut client, &run, position as i32, call)
                .await?
            {
                Acted::Done => {}
                Acted::Refused => refused += 1,
                Acted::Stopped(code) => {
                    return self
                        .finish(
                            execution,
                            &mut client,
                            &run,
                            Stop::Refused(code),
                            Some((facts, outcomes)),
                        )
                        .await;
                }
            }
        }
        let state = match refused {
            0 => "done",
            n if n == definition.actions.len() => "refused",
            _ => "partial",
        };
        self.settle(
            execution,
            &mut client,
            &run,
            state,
            (refused > 0).then_some("action_refused"),
            Some((facts, outcomes)),
        )
        .await
    }

    /// The conditions' fields of the subject, read through Grid as the
    /// `rules` principal. A field it may not read, or reads masked, refuses
    /// the run: a rule never reads a restricted field.
    async fn facts(
        &self,
        scope: &Scope,
        table: &str,
        subject: &str,
        definition: &Definition,
    ) -> Result<std::result::Result<(Map<String, Value>, Vec<(String, FieldType)>), &'static str>>
    {
        let fields = definition.fields();
        if fields.is_empty() {
            return Ok(Ok((Map::new(), Vec::new())));
        }
        let mut types = Vec::new();
        {
            let client = self.rules.db.get().await?;
            let catalog = bilibi_grid::Catalog::for_scope(&**client, scope)
                .await
                .map_err(Error::from)?;
            let Some(root) = catalog.table(table) else {
                return Ok(Err("read_refused"));
            };
            for path in &fields {
                match catalog.resolve(root, path) {
                    Ok(column)
                        if column.agg.is_none()
                            && column.many.is_none()
                            && column
                                .field
                                .is_some_and(|f| !catalog.masked(column.table, f.id)) =>
                    {
                        types.push((path.clone(), column.ty));
                    }
                    _ => return Ok(Err("restricted_field")),
                }
            }
        }
        let key = match subject.parse::<i64>() {
            Ok(id) => json!(id),
            Err(_) => json!(subject),
        };
        let projection =
            match bilibi_grid::project(&self.rules.db, scope, table, &[key], &fields).await {
                Ok(projection) => projection,
                Err(bilibi_grid::QueryError::Database(error)) => return Err(error),
                Err(bilibi_grid::QueryError::ForbiddenRoot) => return Ok(Err("read_refused")),
                Err(_) => return Ok(Err("restricted_field")),
            };
        let Some(row) = projection.rows.first() else {
            return Ok(Err("subject_unavailable"));
        };
        let mut facts = Map::new();
        for path in &fields {
            let id = bilibi_grid::column::dom_id(path);
            facts.insert(
                path.clone(),
                row.cells.get(&id).cloned().unwrap_or(Value::Null),
            );
        }
        Ok(Ok((facts, types)))
    }

    /// Records that the run matched, with the facts and outcomes.
    async fn decide(
        &self,
        execution: &Execution,
        client: &mut tokio_postgres::Client,
        run: &Run,
        decided: &(Value, Value),
    ) -> Result<()> {
        let mut tx = execution.begin(client, "rules.run.matched").await?;
        let (tenant, id, facts, outcomes) = (
            tx.context().tenant,
            run.id,
            decided.0.clone(),
            decided.1.clone(),
        );
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute(
                "UPDATE base_rule_runs SET state='matched',facts=$3,outcomes=$4,started_at=COALESCE(started_at,now()) WHERE tenant_id=$1 AND id=$2 AND state='queued'",
                &[&tenant, &id, &facts, &outcomes],
            ).await?;
            Ok(((), vec![Change { kind: "base/rules.run".into(), id, old: json!({"state": "queued"}), new: json!({"state": "matched", "facts": facts, "outcomes": outcomes}) }]))
        })).await?;
        tx.commit().await
    }

    async fn finish(
        &self,
        execution: &Execution,
        client: &mut tokio_postgres::Client,
        run: &Run,
        stop: Stop,
        decided: Option<(Value, Value)>,
    ) -> Result<()> {
        let (state, reason) = match stop {
            Stop::NoMatch => ("no_match", None),
            Stop::Refused(code) => ("refused", Some(code)),
        };
        self.settle(execution, client, run, state, reason, decided)
            .await
    }

    async fn settle(
        &self,
        execution: &Execution,
        client: &mut tokio_postgres::Client,
        run: &Run,
        state: &'static str,
        reason: Option<&'static str>,
        decided: Option<(Value, Value)>,
    ) -> Result<()> {
        let mut tx = execution.begin(client, "rules.run.finished").await?;
        let (tenant, id) = (tx.context().tenant, run.id);
        let (facts, outcomes) = decided.unzip();
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute(
                "UPDATE base_rule_runs SET state=$3,reason=$4,facts=COALESCE($5,facts),outcomes=COALESCE($6,outcomes),started_at=COALESCE(started_at,now()),finished_at=now() WHERE tenant_id=$1 AND id=$2 AND state IN ('queued','matched')",
                &[&tenant, &id, &state, &reason, &facts, &outcomes],
            ).await?;
            Ok(((), vec![Change { kind: "base/rules.run".into(), id, old: Value::Null, new: json!({"state": state, "reason": reason}) }]))
        })).await?;
        tx.commit().await
    }

    /// One action, once: its receipt and the owner's effect commit together.
    async fn act(
        &self,
        execution: &Execution,
        client: &mut tokio_postgres::Client,
        run: &Run,
        position: i32,
        call: &crate::definition::ActionCall,
    ) -> Result<Acted> {
        let rules = &self.rules;
        let tenant = execution.tenant();
        let Some(action) = rules.action(&call.operation) else {
            return Ok(Acted::Stopped("action_unmounted"));
        };
        let Some(input) = crate::review::bind(
            &action.input,
            action.definition.subject,
            &run.subject,
            &call.input,
        ) else {
            return Ok(Acted::Stopped("subject_unbound"));
        };
        let mut tx = execution.begin(client, "rules.run.acted").await?;
        // Off or replaced since the run was decided: stop at the boundary.
        let current = tx.sql().query_opt(
            "SELECT active_version_id=$3 AND disabled_at IS NULL FROM base_rules WHERE tenant_id=$1 AND id=$2 FOR SHARE",
            &[&tenant, &run.rule, &run.version],
        ).await?.and_then(|row| row.get::<_, Option<bool>>(0));
        if current != Some(true) {
            tx.rollback().await?;
            return Ok(Acted::Stopped("stale_version"));
        }
        if let Some(row) = tx.sql().query_opt(
            "SELECT state FROM base_rule_effects WHERE tenant_id=$1 AND version_id=$2 AND event_id=$3 AND position=$4",
            &[&tenant, &run.version, &run.event, &position],
        ).await? {
            // Already acted (a retry after a crash): never twice.
            tx.rollback().await?;
            return Ok(if row.get::<_, &str>(0) == "done" { Acted::Done } else { Acted::Refused });
        }
        rules
            .events
            .cause(&mut tx, &format!("{CAUSE}{}", run.id))
            .await?;
        let outcome = rules
            .jobs
            .commit(&mut tx, execution, action.key, &input)
            .await;
        let refusal = match outcome {
            Ok(result) => {
                self.receipt(
                    &mut tx,
                    run,
                    position,
                    action.key,
                    "done",
                    None,
                    bounded(&result["result"]),
                )
                .await?;
                tx.commit().await?;
                return Ok(Acted::Done);
            }
            Err(error) => refusal(error)?,
        };
        tx.rollback().await?;
        let mut tx = execution.begin(client, "rules.run.refused").await?;
        self.receipt(
            &mut tx,
            run,
            position,
            action.key,
            "refused",
            Some(refusal.0),
            bounded(&refusal.1),
        )
        .await?;
        tx.commit().await?;
        Ok(Acted::Refused)
    }

    #[allow(clippy::too_many_arguments)]
    async fn receipt(
        &self,
        tx: &mut MutationTransaction<'_>,
        run: &Run,
        position: i32,
        operation: &'static str,
        state: &'static str,
        reason: Option<&'static str>,
        result: Value,
    ) -> Result<()> {
        let (tenant, run_id, version, event) =
            (tx.context().tenant, run.id, run.version, run.event);
        tx.mutate(move |sql| Box::pin(async move {
            let id: i64 = sql.query_one(
                "INSERT INTO base_rule_effects(tenant_id,run_id,version_id,event_id,position,operation,state,reason,result) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id",
                &[&tenant, &run_id, &version, &event, &position, &operation, &state, &reason, &result],
            ).await?.get(0);
            Ok(((), vec![Change { kind: "base/rules.effect".into(), id, old: Value::Null, new: json!({"run": run_id, "position": position, "operation": operation, "state": state, "reason": reason}) }]))
        })).await
    }
}

enum Acted {
    Done,
    Refused,
    /// The run stops here (the rule changed, an action is gone).
    Stopped(&'static str),
}

/// An owner's refusal as a bounded code and the owner's own words for it
/// (field messages, a refusal sentence), or an infrastructure failure to retry.
fn refusal(error: bilibi_agent_api::Error) -> Result<(&'static str, Value)> {
    use bilibi_agent_api::Error as E;
    Ok(match error {
        E::Forbidden | E::Backend(Error::Forbidden) => ("forbidden", Value::Null),
        E::InvalidInput(fields) => ("invalid", json!({"fields": fields})),
        E::Backend(Error::Invalid) => ("invalid", Value::Null),
        E::Backend(Error::Refused(why)) => ("refused", json!({"refusal": why})),
        E::NotFound | E::Backend(Error::NotFound) => ("not_found", Value::Null),
        E::Backend(Error::Conflict) => ("conflict", Value::Null),
        E::WritesNotEnabled | E::ChannelNotAllowed | E::UnknownOperation => {
            ("not_callable", Value::Null)
        }
        E::Backend(Error::Unauthorized) => ("actor_refused", Value::Null),
        E::Backend(Error::StepUpRequired { .. }) => ("step_up", Value::Null),
        E::Backend(error) => return Err(error),
        _ => ("refused", Value::Null),
    })
}

/// A result kept on the receipt: small, or only that it was too large.
fn bounded(result: &Value) -> Value {
    if serde_json::to_vec(result).map_or(usize::MAX, |b| b.len()) <= 4096 {
        result.clone()
    } else {
        json!({"omitted": "larger than 4 KiB"})
    }
}

/// Queue tells this once when a rules job is cancelled (its principal was
/// refused: a suspended tenant, a withdrawn grant) or runs out of attempts:
/// the event's waiting runs are refused with that code, so none waits forever.
pub(crate) struct Settled;
impl Ending for Settled {
    fn ended<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        _scope: &'a Scope,
        job: &'a Ended,
    ) -> EndingFuture<'a> {
        Box::pin(async move {
            let Some(event) = job.args.get("event_id").and_then(Value::as_i64) else {
                return Ok(());
            };
            let tenant = tx.context().tenant;
            let code = job
                .failure_code
                .clone()
                .unwrap_or_else(|| "job_ended".into());
            let code: String = code.chars().take(64).collect();
            tx.mutate(move |sql| Box::pin(async move {
                let ids: Vec<i64> = sql.query(
                    "UPDATE base_rule_runs SET state='refused',reason=$3,finished_at=now() WHERE tenant_id=$1 AND event_id=$2 AND state IN ('queued','matched') RETURNING id",
                    &[&tenant, &event, &code],
                ).await?.iter().map(|row| row.get(0)).collect();
                Ok(((), ids.into_iter().map(|id| Change { kind: "base/rules.run".into(), id, old: Value::Null, new: json!({"state": "refused", "reason": code}) }).collect()))
            })).await
        })
    }
}
