//! Daily retention of recorded events. An event older than its tenant's
//! `events.retention_days` is deleted with its deliveries once nothing is
//! owed for it: no delivery of it is pending or failing, and every mounted
//! subscriber that reads its name has a delivery for it. An event no mounted
//! subscriber reads goes at the keep time too; one a legal hold names stays
//! (ADR 05). Like every contributed recurrence the job runs nothing until an
//! operator reviews and enables it on the Schedule board.
use crate::{privacy::KIND, Events, REPLAY};
use bilibi_authz::{SystemAuthentication, SystemScope};
use bilibi_database::{Change, Error, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Worker, HANDLERS, WORKERS,
};
use bilibi_schedule::{worker, Misfire, Overlap, Reach, Schedule, EXECUTE};
use bilibi_settings::{Editor, ScopeKind, Setting, Target, ValueType};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio_postgres::Client;

/// Days to keep a recorded event: a tenant's own value, else the global one.
pub const RETENTION_DAYS: &str = "events.retention_days";
const DEFAULT_DAYS: i64 = 90;
const MIN_DAYS: i64 = 7;
const MAX_DAYS: i64 = 3650;
const WORKER: &str = "base/events-retention";
pub(crate) const SERVICE: &str = "base/events.retention";
/// Events read, and at most deleted, by one statement.
const BATCH: i64 = 500;

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/events",
            source: file!(),
            line: line!(),
        }
    };
}
fn no_arguments(args: Value) -> Result<Value> {
    match args.as_object() {
        Some(object) if object.is_empty() => Ok(args),
        _ => Err(Error::Invalid),
    }
}
inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, RETENTION_DAYS, provenance!(), &Setting::Definition(bilibi_settings::Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global, ScopeKind::Tenant], default: || json!(DEFAULT_DAYS), nullable: false, encrypted: false,
    minimum: Some(MIN_DAYS as f64), maximum: Some(MAX_DAYS as f64), validator: None,
    editor: Some(Editor { group: "Events", label: "Event retention (days)",
        help: "Days to keep an event after it is recorded. An event that a subscriber has not received yet, or that is under a legal hold, is kept longer.",
        capability: Some(REPLAY), reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: 5, validate: |args| worker::validate(args, no_arguments) }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: SERVICE, capability: EXECUTE }) }
inventory::submit! { Contribution::new(bilibi_schedule::DEFINITIONS, "base/events-retention", provenance!(), &bilibi_schedule::Definition {
    name: "Delete old delivered events", expression: "47 3 * * *", timezone: "Etc/UTC",
    task_name: "Base Events retention", worker: WORKER, args: || json!({}),
    overlap: Overlap::Forbid, misfire: Misfire::Coalesce, reach: Reach::Installation, owner_route: None,
}) }

// One page of a tenant's events older than the keep time, oldest first
// through `base_events_recorded`, each with whether anything is still owed
// for it. `$2` and `$3` pair each mounted subscriber with an event name it
// reads. An erased event is never handed over again, so a subscriber without
// a delivery for it does not keep it.
const OLD: &str = "SELECT e.id,e.recorded_at,(\
 NOT EXISTS (SELECT 1 FROM base_events_deliveries d WHERE d.event_id=e.id AND d.delivered_at IS NULL) \
 AND (e.erased_at IS NOT NULL OR NOT EXISTS (SELECT 1 FROM unnest($2::text[],$3::text[]) AS m(subscriber,name) WHERE m.name=e.name \
 AND NOT EXISTS (SELECT 1 FROM base_events_deliveries d WHERE d.event_id=e.id AND d.subscriber=m.subscriber)))) \
 FROM base_events e WHERE e.tenant_id=$1 AND e.recorded_at<$4 AND (e.recorded_at,e.id)>($5::timestamptz,$6::bigint) \
 ORDER BY e.recorded_at,e.id LIMIT $7";
// The same test again on the events a delete has locked.
const SETTLED: &str = "SELECT e.id FROM base_events e WHERE e.tenant_id=$1 AND e.id=ANY($4) \
 AND NOT EXISTS (SELECT 1 FROM base_events_deliveries d WHERE d.event_id=e.id AND d.delivered_at IS NULL) \
 AND (e.erased_at IS NOT NULL OR NOT EXISTS (SELECT 1 FROM unnest($2::text[],$3::text[]) AS m(subscriber,name) WHERE m.name=e.name \
 AND NOT EXISTS (SELECT 1 FROM base_events_deliveries d WHERE d.event_id=e.id AND d.subscriber=m.subscriber))) \
 ORDER BY e.id";

impl Events {
    /// Applies each active tenant's `events.retention_days` at `now` and
    /// answers how many events were deleted. The daily job calls it; a second
    /// call finds nothing left to delete. A tenant that fails does not stop
    /// the others: the call then answers an error after visiting them all.
    pub async fn prune(&self, client: &mut Client, now: SystemTime) -> Result<u64> {
        let tenants: Vec<i64> = client
            .query(
                "SELECT id FROM tenants WHERE status='active' AND deleted_at IS NULL ORDER BY id",
                &[],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let (mut pruned, mut failed) = (0, false);
        for tenant in tenants {
            match self.prune_tenant(client, tenant, now).await {
                Ok(count) => pruned += count,
                Err(_) => failed = true,
            }
        }
        if failed {
            Err(Error::Internal)
        } else {
            Ok(pruned)
        }
    }

    /// One tenant, as its `scheduler` principal: the setting is read and the
    /// deletions are audited under that scope.
    async fn prune_tenant(&self, client: &mut Client, tenant: i64, now: SystemTime) -> Result<u64> {
        let system =
            SystemScope::rehydrate(&*client, tenant, Some(bilibi_schedule::PRINCIPAL)).await?;
        let scope = self.authentication.for_system(&*client, &system).await?;
        let days = self
            .settings
            .get(&*client, &scope, Target::Tenant, RETENTION_DAYS)
            .await?
            .value
            .as_i64()
            .filter(|days| (MIN_DAYS..=MAX_DAYS).contains(days))
            .ok_or(Error::Invalid)?;
        let cutoff = now - Duration::from_secs(days as u64 * 86_400);
        let (mut readers, mut names) = (Vec::new(), Vec::new());
        for (key, subscriber) in &self.subscribers {
            for on in subscriber.events {
                readers.push(*key);
                names.push(on.event);
            }
        }
        // Keyset over (recorded_at, id): a kept event is passed, not read again.
        let (mut pruned, mut after) = (0, (SystemTime::UNIX_EPOCH, 0_i64));
        loop {
            let rows = client
                .query(
                    OLD,
                    &[
                        &tenant, &readers, &names, &cutoff, &after.0, &after.1, &BATCH,
                    ],
                )
                .await?;
            let done = (rows.len() as i64) < BATCH;
            let mut settled = Vec::new();
            for row in &rows {
                let id: i64 = row.get(0);
                after = (row.get(1), id);
                if row.get::<_, bool>(2) {
                    settled.push(id);
                }
            }
            if !settled.is_empty() {
                let holds = bilibi_personal_data::RecordHolds::acquire(tenant).await?;
                let held = holds.records(KIND, &settled).await?;
                settled.retain(|id| !held.contains_key(id));
                if !settled.is_empty() {
                    pruned += self
                        .delete(client, &scope, settled, &readers, &names, days)
                        .await?;
                }
                holds.release().await?;
            }
            if done {
                return Ok(pruned);
            }
        }
    }

    /// Deletes the events of `ids` that are still settled once they are
    /// locked, with their deliveries, as one audited change for the batch.
    async fn delete(
        &self,
        client: &mut Client,
        scope: &Scope,
        ids: Vec<i64>,
        readers: &[&'static str],
        names: &[&'static str],
        days: i64,
    ) -> Result<u64> {
        let tenant = scope.tenant();
        let mut tx = self
            .boundary
            .begin(client, scope.mutation_context("events.retention.prune"))
            .await?;
        // A replay share-locks an event before it creates a delivery for it,
        // so once these rows are locked no new delivery can appear; the test
        // is then repeated on what is committed now.
        tx.sql()
            .query(
                "SELECT id FROM base_events WHERE tenant_id=$1 AND id=ANY($2) ORDER BY id FOR UPDATE",
                &[&tenant, &ids],
            )
            .await?;
        let settled: Vec<i64> = tx
            .sql()
            .query(SETTLED, &[&tenant, &readers, &names, &ids])
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let (Some(&first), Some(&last)) = (settled.first(), settled.last()) else {
            tx.rollback().await?;
            return Ok(0);
        };
        let deleted = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    sql.execute(
                        "DELETE FROM base_events_deliveries WHERE event_id=ANY($1)",
                        &[&settled],
                    )
                    .await?;
                    let deleted = sql
                        .execute(
                            "DELETE FROM base_events WHERE tenant_id=$1 AND id=ANY($2)",
                            &[&tenant, &settled],
                        )
                        .await?;
                    // One change for the batch, as a replay writes one: the
                    // events' own recording is already in the audit.
                    let change = Change {
                        kind: "base/events.retention".into(),
                        id: first,
                        old: Value::Null,
                        new: json!({"deleted": deleted, "from": first, "through": last, "keep_days": days}),
                    };
                    Ok((deleted, vec![change]))
                })
            })
            .await?;
        tx.commit().await?;
        Ok(deleted)
    }
}

pub(crate) struct Retention {
    pub events: Arc<Events>,
    pub schedule: Arc<Schedule>,
}
impl Handler for Retention {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(self.schedule.run(execution, args, |_| async {
            let pruned = async {
                let mut client = self.events.db.get().await?;
                self.events.prune(&mut client, SystemTime::now()).await
            }
            .await;
            match pruned {
                Ok(_) => Outcome::Completed,
                Err(_) => Outcome::Retry("event_store_unavailable"),
            }
        }))
    }
}
