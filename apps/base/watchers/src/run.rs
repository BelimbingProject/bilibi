//! Running watchers, after the trigger's transaction commits and never inside it.
//!
//! 1. Base Events hands a trigger event to the `base/watchers.triggers`
//!    subscriber. In the delivery transaction it records one queued run per
//!    watcher switched on for that event in the tenant, pinned to the watcher's
//!    approved version, with the chain it belongs to (`Event::cause`), and
//!    hands the event to the `base/watchers-admit` worker (`Handoff`).
//! 2. The worker runs as the `watchers` principal under its own tenant grants.
//!    For each queued run it checks the chain budget, the tenant's work
//!    bound, that the pinned version is still the one switched on, reads the
//!    permitted facts through Grid, evaluates the conditions and records
//!    why the run matched or not.
//! 3. Each action runs through the owner's contract on the job door
//!    (`JobContracts::commit`), in a transaction that also writes the
//!    action's receipt under the key (tenant, version, event, position): a
//!    retry after a crash finds the receipt and does nothing twice.
use crate::definition::Definition;
use crate::store::Watchers;
use crate::{CHAIN_DEPTH, MAX_ATTEMPTS, RUNS_PER_MINUTE};
use bilibi_authz::SystemScope;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_events::{Event, Handoff, On, Subscribe, SubscriberFuture};
use bilibi_grid::FieldType;
use bilibi_module_registry::Catalog;
use bilibi_queue::{Ended, Ending, EndingFuture, Execution, Handler, HandlerFuture, Outcome};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;
use tokio_postgres::Transaction;

/// The service, bound once at boot, for the subscriber (a static) to reach.
static WATCHERS: OnceLock<Arc<Watchers>> = OnceLock::new();
pub(crate) fn bind(watchers: Arc<Watchers>) -> bool {
    WATCHERS.set(watchers).is_ok()
}
fn watchers() -> Result<&'static Arc<Watchers>> {
    WATCHERS.get().ok_or(Error::Internal)
}

/// The cause the worker names for events its actions record.
const CAUSE: &str = "base/watchers.run:";

impl Watchers {
    pub async fn action_identity(
        client: &(impl bilibi_tenancy::ProofClient + ?Sized),
        scope: &Scope,
    ) -> Result<Option<String>> {
        if scope.actor().principal() != Some(crate::PRINCIPAL) {
            return Ok(None);
        }
        Ok(client
            .query_opt(
                "SELECT NULLIF(current_setting('bilibi.watchers.action', true), '')",
                &[],
            )
            .await?
            .ok_or(Error::Internal)?
            .get(0))
    }
}

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
            let watchers = watchers()?;
            let Some(trigger) = watchers.trigger(&event.name) else {
                return Ok(Vec::new());
            };
            let subject = match event.payload.pointer(trigger.definition.subject) {
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::String(s)) if !s.is_empty() && s.len() <= 200 => s.clone(),
                // The owner's payload lacks its documented record key: no watcher
                // can bind it, and refusing would hold the subject's later
                // events behind it for ever.
                _ => return Ok(Vec::new()),
            };
            let logical_event = match trigger.definition.logical_event {
                Some(pointer) => match event.payload.pointer(pointer) {
                    Some(Value::String(key)) if !key.is_empty() && key.len() <= 200 => {
                        json!([event.name, key]).to_string()
                    }
                    Some(Value::Number(key)) if key.is_i64() || key.is_u64() => {
                        json!([event.name, key]).to_string()
                    }
                    _ => return Err(Error::Invalid),
                },
                None => json!([event.name, event.id]).to_string(),
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
                        "SELECT id,depth FROM base_watcher_runs WHERE tenant_id=$1 AND id=$2",
                        &[&event.tenant, &id],
                    )
                    .await?
                    .map(|row| (row.get::<_, i64>(0), row.get::<_, i32>(1))),
                None => None,
            };
            let (cause_id, depth) = (cause.map(|c| c.0), cause.map_or(1, |c| c.1 + 1));
            // Approval and switch transitions take this same lock. A delivery
            // that starts concurrently must see their committed activation.
            let admission_key = format!("base/watchers:{}", event.tenant);
            sql.execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                &[&admission_key],
            )
            .await?;
            let rows = sql.query(
                "WITH candidates AS (
                    SELECT w.id,w.active_version_id,w.enabled_at,w.disabled_at IS NOT NULL AS off,
                           w.disabled_at IS NULL AND $8::text::timestamptz > v.approved_at AND $8::text::timestamptz > w.enabled_at AS eligible,
                           $8::text::timestamptz > v.approved_at AS after_approval
                    FROM base_watchers w JOIN base_watcher_versions v
                      ON v.tenant_id=w.tenant_id AND v.watcher_id=w.id AND v.id=w.active_version_id
                    WHERE w.tenant_id=$1 AND w.trigger=$2::varchar
                 )
                 INSERT INTO base_watcher_runs(tenant_id,watcher_id,version_id,event_id,event_name,subject,cause_run_id,depth,state,logical_event,reason,finished_at,activation_at)
                 SELECT $1,id,active_version_id,$3,$2::varchar,$4,$5,$6,
                        CASE WHEN eligible THEN 'queued' ELSE 'refused' END,$7,
                        CASE WHEN eligible THEN NULL WHEN off THEN 'watcher_off' WHEN NOT after_approval THEN 'event_before_approval' ELSE 'event_before_enabled' END,
                        CASE WHEN eligible THEN NULL ELSE now() END,enabled_at
                 FROM candidates ORDER BY id
                 ON CONFLICT (tenant_id,version_id,event_id) DO NOTHING
                 RETURNING id,watcher_id,state,reason",
                &[&event.tenant, &event.name, &event.id, &subject, &cause_id, &depth, &logical_event, &event.recorded_at],
            ).await?;
            Ok(rows
                .iter()
                .map(|row| Change {
                    kind: "base/watchers.run".into(),
                    id: row.get(0),
                    old: Value::Null,
                    new: json!({"watcher": row.get::<_, i64>(1), "event": event.id, "subject": subject, "depth": depth, "state": row.get::<_, &str>(2), "reason": row.get::<_, Option<&str>>(3)}),
                })
                .collect())
        })
    }
    fn handoff(&self, event: &Event, changes: &[Change]) -> Option<Handoff> {
        changes
            .iter()
            .any(|c| c.kind == "base/watchers.run" && c.new["state"] == "queued")
            .then(|| Handoff {
                worker: crate::ADMISSION_WORKER,
                principal: crate::PRINCIPAL,
                args: json!({"event_id": event.id}),
            })
    }
}

pub(crate) fn admission_arguments(args: Value) -> Result<Value> {
    if args.as_object().is_some_and(|o| o.len() == 1) {
        action_group(&args)?;
        return Ok(args);
    }
    Err(Error::Invalid)
}

pub(crate) fn arguments(args: Value) -> Result<Value> {
    if args.as_object().is_some_and(|o| o.len() == 2)
        && args
            .get("run_id")
            .and_then(Value::as_i64)
            .is_some_and(|id| id > 0)
    {
        action_group(&args)?;
        return Ok(args);
    }
    Err(Error::Invalid)
}

pub(crate) fn action_group(args: &Value) -> Result<i64> {
    args.get("event_id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or(Error::Invalid)
}

pub(crate) struct Worker {
    pub watchers: Arc<Watchers>,
    pub admission: bool,
}
impl Handler for Worker {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let Some(event) = args.get("event_id").and_then(Value::as_i64) else {
                return Outcome::Cancel("invalid_event_reference");
            };
            let maximum = async {
                let scope = execution.scope().ok_or(Error::Forbidden)?;
                let client = self.watchers.db.get().await?;
                self.watchers.limit(&**client, scope, MAX_ATTEMPTS).await
            }
            .await;
            let maximum = match maximum {
                Ok(maximum) => maximum,
                Err(Error::Unauthorized | Error::Forbidden) => {
                    return Outcome::Cancel("actor_refused");
                }
                Err(_) => return Outcome::Retry("watchers_unavailable"),
            };
            if i64::from(execution.attempt) > maximum {
                return Outcome::Cancel("attempt_limit");
            }
            let result = if self.admission {
                self.admit_event(execution, event).await
            } else if let Some(run) = args.get("run_id").and_then(Value::as_i64) {
                self.act_on_run(execution, event, run).await
            } else {
                return Outcome::Cancel("invalid_run_reference");
            };
            match result {
                Ok(()) => Outcome::Completed,
                Err(Error::Unauthorized | Error::Forbidden) => Outcome::Cancel("actor_refused"),
                Err(_) if i64::from(execution.attempt) >= maximum => {
                    Outcome::Cancel("attempt_limit")
                }
                Err(_) => Outcome::Retry("watchers_unavailable"),
            }
        })
    }
}

/// A queued run, as the worker reads it.
struct Run {
    id: i64,
    watcher: i64,
    version: i64,
    event: i64,
    subject: String,
    logical_event: Option<String>,
    depth: i32,
    state: String,
}

/// Why a run did not act; each is a bounded code shown on the receipt.
enum Stop {
    Refused(&'static str),
    NoMatch,
}

impl Worker {
    async fn admit_event(&self, execution: &Execution, event: i64) -> Result<()> {
        let scope = execution.scope().ok_or(Error::Forbidden)?;
        let bilibi_queue::Identity::System(system) = execution.identity() else {
            return Err(Error::Forbidden);
        };
        let tenant = scope.tenant();
        let mut client = self.watchers.db.get().await?;
        let ids = client
            .query(
                "SELECT id FROM base_watcher_runs WHERE tenant_id=$1 AND event_id=$2
                AND started_at IS NULL AND state='queued' ORDER BY id",
                &[&tenant, &event],
            )
            .await?;
        for row in ids {
            let id: i64 = row.get(0);
            let mut tx = execution
                .begin(&mut client, "watchers.run.admission")
                .await?;
            if tx
                .sql()
                .query_opt(
                    "SELECT id FROM base_watcher_runs WHERE tenant_id=$1 AND event_id=$2 AND id=$3
                    AND started_at IS NULL AND state='queued' FOR UPDATE",
                    &[&tenant, &event, &id],
                )
                .await?
                .is_none()
            {
                tx.rollback().await?;
                continue;
            }
            if let Some(run_at) = self.admit(&mut tx, scope, id).await? {
                execution.defer_until(&tx, run_at).await?;
                return tx.commit().await;
            }
            self.watchers
                .queue
                .enqueue_system(
                    &mut tx,
                    system,
                    crate::WORKER,
                    json!({"event_id": event, "run_id": id}),
                )
                .await?;
            tx.commit().await?;
        }
        Ok(())
    }

    async fn act_on_run(&self, execution: &Execution, event: i64, id: i64) -> Result<()> {
        let scope = execution.scope().ok_or(Error::Forbidden)?;
        let client = self.watchers.db.get().await?;
        let row = client.query_opt(
            "SELECT watcher_id,version_id,subject,depth,state,logical_event FROM base_watcher_runs
                WHERE tenant_id=$1 AND event_id=$2 AND id=$3
                    AND started_at IS NOT NULL AND state IN ('queued','matched')",
            &[&scope.tenant(), &event, &id],
        ).await?;
        let Some(row) = row else {
            return Ok(());
        };
        let depth = self.watchers.limit(&**client, scope, CHAIN_DEPTH).await?;
        drop(client);
        let run = Run {
            id,
            event,
            watcher: row.get(0),
            version: row.get(1),
            subject: row.get(2),
            depth: row.get(3),
            state: row.get(4),
            logical_event: row.get(5),
        };
        self.run(execution, scope, run, depth).await
    }

    async fn admit(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
    ) -> Result<Option<SystemTime>> {
        let tenant = scope.tenant();
        self.watchers.lock_admission(tx.sql(), scope).await?;
        let rate = self
            .watchers
            .limit(tx.sql(), scope, RUNS_PER_MINUTE)
            .await?;
        let row = tx.sql().query_one(
            "WITH bucket AS MATERIALIZED (
                SELECT b.tokens, b.refilled_at, GREATEST(b.refilled_at, clock_timestamp()) AS sampled_at
                FROM (VALUES ($1::bigint)) AS tenant(id)
                LEFT JOIN base_watcher_buckets b ON b.tenant_id=tenant.id
            ), refill AS (
                SELECT tokens, refilled_at, sampled_at,
                    LEAST($2::bigint::double precision, COALESCE(tokens +
                        EXTRACT(EPOCH FROM sampled_at - refilled_at)::double precision * $2::bigint / 60.0,
                        $2::bigint::double precision)) AS available
                FROM bucket
            )
            SELECT tokens, refilled_at::text, sampled_at, available,
                sampled_at + make_interval(secs => ceil((1.0 - available) * 60000000.0 / $2::bigint) / 1000000.0)
            FROM refill",
            &[&tenant, &rate],
        ).await?;
        let available: f64 = row.get(3);
        if available < 1.0 {
            return Ok(Some(row.get(4)));
        }
        let old_tokens: Option<f64> = row.get(0);
        let old_at: Option<String> = row.get(1);
        let at: SystemTime = row.get(2);
        let tokens = available - 1.0;
        tx.mutate(move |sql| Box::pin(async move {
            let refilled_at: String = sql.query_one(
                "INSERT INTO base_watcher_buckets (tenant_id,tokens,refilled_at) VALUES ($1,$2,$3)
                ON CONFLICT (tenant_id) DO UPDATE SET tokens=EXCLUDED.tokens,refilled_at=EXCLUDED.refilled_at
                RETURNING refilled_at::text",
                &[&tenant, &tokens, &at],
            ).await?.get(0);
            let started_at: String = sql.query_one(
                "UPDATE base_watcher_runs SET started_at=$3 WHERE tenant_id=$1 AND id=$2 RETURNING started_at::text",
                &[&tenant, &id, &at],
            ).await?.get(0);
            Ok(((), vec![
                Change { kind: "base/watchers.bucket".into(), id: tenant, old: json!({"tokens": old_tokens, "refilled_at": old_at}), new: json!({"tokens": tokens, "refilled_at": refilled_at}) },
                Change { kind: "base/watchers.run".into(), id, old: json!({"started_at": null}), new: json!({"started_at": started_at}) },
            ]))
        })).await?;
        Ok(None)
    }

    async fn run(&self, execution: &Execution, scope: &Scope, run: Run, depth: i64) -> Result<()> {
        let watchers = &self.watchers;
        let tenant = scope.tenant();
        let mut client = watchers.db.get().await?;
        let row = client.query_opt(
            "SELECT r.trigger,r.active_version_id,r.disabled_at IS NOT NULL,v.definition,v.catalog_digest,r.needs_review,v.examples,COALESCE(r.enabled_at=(SELECT activation_at FROM base_watcher_runs WHERE tenant_id=$1 AND id=$4),false) FROM base_watchers r JOIN base_watcher_versions v ON v.tenant_id=r.tenant_id AND v.id=$3 WHERE r.tenant_id=$1 AND r.id=$2",
            &[&tenant, &run.watcher, &run.version, &run.id],
        ).await?.ok_or(Error::NotFound)?;
        let (trigger, active, off): (String, Option<i64>, bool) =
            (row.get(0), row.get(1), row.get(2));
        let mut problems = Vec::new();
        let definition =
            Definition::parse(&row.get::<_, Value>(3), &mut problems).ok_or(Error::Invalid)?;
        let approved_digest: Option<String> = row.get(4);
        let custom = watchers
            .custom_tables(&**client, scope, [&definition])
            .await?;
        if row.get::<_, bool>(5)
            || approved_digest.is_none()
            || approved_digest != watchers.catalog_digest(&trigger, &definition, &custom)
        {
            let mut tx = execution
                .begin(&mut client, "watchers.review_required")
                .await?;
            let watcher = run.watcher;
            let version = run.version;
            tx.mutate(move |sql| Box::pin(async move {
                let changed = sql.execute("UPDATE base_watchers SET needs_review=true,updated_at=now(),version=version+1 WHERE tenant_id=$1 AND id=$2 AND active_version_id=$3 AND NOT needs_review", &[&tenant, &watcher, &version]).await?;
                let changes = if changed == 0 { Vec::new() } else { vec![Change { kind: "base/watchers.watcher".into(), id: watcher, old: json!({"needs_review": false}), new: json!({"needs_review": true}) }] };
                Ok(((), changes))
            })).await?;
            tx.commit().await?;
            return self
                .finish(
                    execution,
                    &mut client,
                    &run,
                    Stop::Refused("catalog_changed"),
                    None,
                )
                .await;
        }
        let stop = if i64::from(run.depth) > depth {
            Some(Stop::Refused("chain_budget"))
        } else if off {
            Some(Stop::Refused("watcher_off"))
        } else if active != Some(run.version) {
            Some(Stop::Refused("stale_version"))
        } else if !row.get::<_, bool>(7) {
            Some(Stop::Refused("stale_activation"))
        } else {
            None
        };
        if let Some(stop) = stop {
            return self.finish(execution, &mut client, &run, stop, None).await;
        }
        let offered = watchers.trigger(&trigger).ok_or(Error::Invalid)?;
        let limits = watchers
            .check_limits(
                &**client,
                scope,
                &definition,
                row.get::<_, Value>(6).as_array().map_or(0, Vec::len),
                offered.definition.version,
            )
            .await?;
        if !limits.is_empty() {
            let code = if limits.iter().any(|(field, _)| field == "do") {
                "action_limit"
            } else {
                "watcher_limit"
            };
            return self
                .finish(execution, &mut client, &run, Stop::Refused(code), None)
                .await;
        }
        // Facts and the decision are kept once taken, so a retry after a
        // crash acts on what was decided, never on a second reading.
        let (facts, outcomes) = if run.state == "matched" {
            let saved = client
                .query_one(
                    "SELECT facts,outcomes FROM base_watcher_runs WHERE tenant_id=$1 AND id=$2",
                    &[&tenant, &run.id],
                )
                .await?;
            (
                saved.get::<_, Option<Value>>(0).unwrap_or(Value::Null),
                saved.get::<_, Option<Value>>(1).unwrap_or(Value::Null),
            )
        } else {
            let fields = definition.fact_fields(offered.definition.version);
            let read = watchers
                .read_facts(
                    &**client,
                    scope,
                    offered.definition.table,
                    &run.subject,
                    &fields,
                )
                .await?;
            let (facts, types) = match read {
                Ok(read) => read,
                Err(code) => {
                    return self
                        .finish(execution, &mut client, &run, Stop::Refused(code), None)
                        .await;
                }
            };
            let subject_version = facts
                .get(offered.definition.version)
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                .ok_or(Error::Invalid)?;
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
            self.decide(execution, &mut client, &run, &decided, subject_version)
                .await?;
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

    /// Records that the run matched, with the facts and outcomes.
    async fn decide(
        &self,
        execution: &Execution,
        client: &mut tokio_postgres::Client,
        run: &Run,
        decided: &(Value, Value),
        subject_version: i64,
    ) -> Result<()> {
        let mut tx = execution.begin(client, "watchers.run.matched").await?;
        let (tenant, id, facts, outcomes) = (
            tx.context().tenant,
            run.id,
            decided.0.clone(),
            decided.1.clone(),
        );
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute(
                "UPDATE base_watcher_runs SET state='matched',facts=$3,outcomes=$4,subject_version=$5,started_at=COALESCE(started_at,now()) WHERE tenant_id=$1 AND id=$2 AND state='queued'",
                &[&tenant, &id, &facts, &outcomes, &subject_version],
            ).await?;
            Ok(((), vec![Change { kind: "base/watchers.run".into(), id, old: json!({"state": "queued"}), new: json!({"state": "matched"}) }]))
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
        let mut tx = execution.begin(client, "watchers.run.finished").await?;
        let (tenant, id) = (tx.context().tenant, run.id);
        let (facts, outcomes) = decided.unzip();
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute(
                "UPDATE base_watcher_runs SET state=$3,reason=$4,facts=COALESCE($5,facts),outcomes=COALESCE($6,outcomes),started_at=COALESCE(started_at,now()),finished_at=now() WHERE tenant_id=$1 AND id=$2 AND state IN ('queued','matched')",
                &[&tenant, &id, &state, &reason, &facts, &outcomes],
            ).await?;
            Ok(((), vec![Change { kind: "base/watchers.run".into(), id, old: Value::Null, new: json!({"state": state, "reason": reason}) }]))
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
        let watchers = &self.watchers;
        let tenant = execution.tenant();
        let Some(logical_event) = &run.logical_event else {
            return Ok(Acted::Stopped("event_identity_missing"));
        };
        let action = watchers.action(&call.operation).ok_or(Error::Invalid)?;
        let Some(mut input) = crate::review::bind(
            &action.input,
            action.definition.subject,
            &run.subject,
            &call.input,
        ) else {
            return Ok(Acted::Stopped("subject_unbound"));
        };
        let mut tx = execution.begin(client, "watchers.run.acted").await?;
        // Off or replaced since the run was decided: stop at the boundary.
        let current = tx.sql().query_opt(
            "SELECT active_version_id=$3,disabled_at IS NOT NULL,enabled_at=(SELECT activation_at FROM base_watcher_runs WHERE tenant_id=$1 AND id=$4) FROM base_watchers WHERE tenant_id=$1 AND id=$2 FOR SHARE",
            &[&tenant, &run.watcher, &run.version, &run.id],
        ).await?;
        let reason = match current {
            None => Some("stale_version"),
            Some(row) if row.get::<_, bool>(1) => Some("watcher_off"),
            Some(row) if row.get::<_, Option<bool>>(0) != Some(true) => Some("stale_version"),
            Some(row) if row.get::<_, Option<bool>>(2) != Some(true) => Some("stale_activation"),
            Some(_) => None,
        };
        if let Some(reason) = reason {
            tx.rollback().await?;
            return Ok(Acted::Stopped(reason));
        }
        let saved = tx.sql().query_one("SELECT subject_version FROM base_watcher_runs WHERE tenant_id=$1 AND id=$2 FOR UPDATE", &[&tenant, &run.id]).await?;
        if let Some(row) = tx.sql().query_opt(
            "SELECT state FROM base_watcher_effects WHERE tenant_id=$1 AND version_id=$2 AND event_id=$3 AND position=$4",
            &[&tenant, &run.version, &run.event, &position],
        ).await? {
            // Already acted (a retry after a crash): never twice.
            tx.rollback().await?;
            return Ok(if row.get::<_, &str>(0) == "done" { Acted::Done } else { Acted::Refused });
        }
        let expected: Option<i64> = saved.get(0);
        let Some(expected) = expected else {
            tx.rollback().await?;
            return Ok(Acted::Stopped("record_changed"));
        };
        input[action.definition.version] = json!(expected);
        let scope = watchers.principal(tx.sql(), tenant).await?;
        let version = tx.sql().query_one("SELECT r.trigger,v.definition,v.catalog_digest,v.examples FROM base_watchers r JOIN base_watcher_versions v ON v.tenant_id=r.tenant_id AND v.id=$3 WHERE r.tenant_id=$1 AND r.id=$2", &[&tenant, &run.watcher, &run.version]).await?;
        let trigger: String = version.get(0);
        let definition = Definition::parse(&version.get::<_, Value>(1), &mut Vec::new())
            .ok_or(Error::Invalid)?;
        let custom = watchers
            .custom_tables(tx.sql(), &scope, [&definition])
            .await?;
        if version.get::<_, Option<String>>(2)
            != watchers.catalog_digest(&trigger, &definition, &custom)
        {
            tx.rollback().await?;
            return Ok(Acted::Stopped("catalog_changed"));
        }
        let trigger = watchers.trigger(&trigger).ok_or(Error::Invalid)?;
        let limits = watchers
            .check_limits(
                tx.sql(),
                &scope,
                &definition,
                version.get::<_, Value>(3).as_array().map_or(0, Vec::len),
                trigger.definition.version,
            )
            .await?;
        if !limits.is_empty() {
            let code = if limits.iter().any(|(field, _)| field == "do") {
                "action_limit"
            } else {
                "watcher_limit"
            };
            tx.rollback().await?;
            return Ok(Acted::Stopped(code));
        }
        let fields = definition.fact_fields(trigger.definition.version);
        if let Err(code) = watchers
            .read_facts(
                tx.sql(),
                &scope,
                trigger.definition.table,
                &run.subject,
                &fields,
            )
            .await?
        {
            tx.rollback().await?;
            return Ok(Acted::Stopped(code));
        }
        watchers
            .events
            .cause(&mut tx, &format!("{CAUSE}{}", run.id))
            .await?;
        tx.sql()
            .execute(
                "SELECT set_config('bilibi.watchers.action', $1, true)",
                &[&json!([run.version, position, logical_event]).to_string()],
            )
            .await?;
        let outcome = watchers
            .jobs
            .commit(&mut tx, execution, action.key, &input)
            .await;
        let refusal = match outcome {
            Ok(result) => {
                if let Ok((facts, _)) = watchers
                    .read_facts(
                        tx.sql(),
                        &scope,
                        trigger.definition.table,
                        &run.subject,
                        &[trigger.definition.version.to_owned()],
                    )
                    .await?
                {
                    let next = facts
                        .get(trigger.definition.version)
                        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
                        .ok_or(Error::Invalid)?;
                    let id = run.id;
                    tx.mutate(move |sql| Box::pin(async move {
                        sql.execute("UPDATE base_watcher_runs SET subject_version=$3 WHERE tenant_id=$1 AND id=$2", &[&tenant, &id, &next]).await?;
                        Ok(((), vec![Change { kind: "base/watchers.run".into(), id, old: json!({"subject_version": expected}), new: json!({"subject_version": next}) }]))
                    })).await?;
                }
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
        let mut tx = execution.begin(client, "watchers.run.refused").await?;
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
                "INSERT INTO base_watcher_effects(tenant_id,run_id,version_id,event_id,position,operation,state,reason,result) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id",
                &[&tenant, &run_id, &version, &event, &position, &operation, &state, &reason, &result],
            ).await?.get(0);
            Ok(((), vec![Change { kind: "base/watchers.effect".into(), id, old: Value::Null, new: json!({"run": run_id, "position": position, "operation": operation, "state": state, "reason": reason}) }]))
        })).await
    }
}

enum Acted {
    Done,
    Refused,
    /// The run stops here (the watcher changed, an action is gone).
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
        E::Backend(Error::Conflict) => ("record_changed", Value::Null),
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

/// Queue tells this once when a watchers job is cancelled (its principal was
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
            let run = if job.worker_id == crate::WORKER {
                let Some(id) = job.args.get("run_id").and_then(Value::as_i64) else {
                    return Ok(());
                };
                Some(id)
            } else {
                None
            };
            let tenant = tx.context().tenant;
            let ids: Vec<i64> = tx
                .sql()
                .query(
                    "SELECT id FROM base_watcher_runs WHERE tenant_id=$1 AND event_id=$2
                    AND state IN ('queued','matched')
                    AND (id=$3 OR ($3::bigint IS NULL AND started_at IS NULL)) FOR UPDATE",
                    &[&tenant, &event, &run],
                )
                .await?
                .iter()
                .map(|row| row.get(0))
                .collect();
            if ids.is_empty() {
                return Ok(());
            }
            let code = job
                .failure_code
                .clone()
                .unwrap_or_else(|| "job_ended".into());
            let code: String = code.chars().take(64).collect();
            tx.mutate(move |sql| {
                Box::pin(async move {
                    sql.execute(
                        "UPDATE base_watcher_runs SET state='refused',reason=$3,finished_at=now()
                        WHERE tenant_id=$1 AND id=ANY($2)",
                        &[&tenant, &ids, &code],
                    )
                    .await?;
                    Ok((
                        (),
                        ids.into_iter()
                            .map(|id| Change {
                                kind: "base/watchers.run".into(),
                                id,
                                old: Value::Null,
                                new: json!({"state": "refused", "reason": code}),
                            })
                            .collect(),
                    ))
                })
            })
            .await
        })
    }
}
