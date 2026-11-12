//! Queue's retention task: a daily scheduled job that deletes finished queue
//! jobs older than `queue.jobs.keep_days`. It lives here because Queue is
//! upstream of Schedule and cannot contribute a definition itself; the
//! deleting is Queue's own (`Queue::prune`). Like every contributed recurrence
//! it is unreviewed, and so runs nothing, until an operator enables it on the
//! board.
use crate::{worker, Definition, Misfire, Overlap, Reach, Schedule, DEFINITIONS, EXECUTE};
use bilibi_database::{Db, Error, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Queue, Worker, HANDLERS, WORKERS,
};
use bilibi_settings::Settings;
use serde_json::{json, Value};
use std::{sync::Arc, time::SystemTime};

/// Global days of finished queue jobs to keep; zero disables pruning.
const KEEP_DAYS: &str = "queue.jobs.keep_days";
const KEEP_DAYS_MIN: i64 = 0;
const KEEP_DAYS_MAX: i64 = 3650;
const KEEP_DAYS_DEFAULT: i64 = 30;

const WORKER: &str = "base/schedule-queue-retention";
macro_rules! provenance {
    () => {
        Provenance {
            module: "base/schedule",
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
inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: 5, validate: |args| worker::validate(args, no_arguments) }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: "base/schedule.queue-retention", capability: EXECUTE }) }
inventory::submit! { Contribution::new(DEFINITIONS, "base/schedule.queue-retention", provenance!(), &Definition {
    name: "Prune finished queue jobs", expression: "47 3 * * *", timezone: "Etc/UTC",
    task_name: "Base Queue job retention", worker: WORKER, args: || json!({}),
    overlap: Overlap::Forbid, misfire: Misfire::Coalesce, reach: Reach::Installation, owner_route: None,
}) }
inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, KEEP_DAYS, provenance!(),
&bilibi_settings::Setting::Definition(bilibi_settings::Definition {
    value_type: bilibi_settings::ValueType::Integer, scopes: &[bilibi_settings::ScopeKind::Global],
    default: || json!(KEEP_DAYS_DEFAULT), nullable: false, encrypted: false,
    minimum: Some(KEEP_DAYS_MIN as f64), maximum: Some(KEEP_DAYS_MAX as f64), validator: None,
    editor: Some(bilibi_settings::Editor { group: "Schedule", label: "Finished job retention",
        help: "Days to keep finished background jobs; 0 keeps them all.", capability: Some(crate::MANAGE), reveal_duration_ms: 5_000 }),
})) }

pub(crate) struct QueueRetention {
    pub schedule: Arc<Schedule>,
    pub queue: Arc<Queue>,
    pub settings: Arc<Settings>,
    pub db: Db,
}
impl Handler for QueueRetention {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(self.schedule.run(execution, args, |_| async {
            let pruned = async {
                let client = self.db.get().await?;
                let days = self
                    .settings
                    .get_global(&**client, KEEP_DAYS)
                    .await?
                    .as_i64()
                    .filter(|days| (KEEP_DAYS_MIN..=KEEP_DAYS_MAX).contains(days))
                    .ok_or(Error::Invalid)?;
                self.queue.prune(&**client, days, SystemTime::now()).await
            }
            .await;
            match pruned {
                Ok(_) => Outcome::Completed,
                Err(_) => Outcome::Retry("queue_store_unavailable"),
            }
        }))
    }
}
