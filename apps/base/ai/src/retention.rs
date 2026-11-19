//! Daily retention of interaction rows past `ai.interaction_retention_days`.
//! Like every contributed recurrence it runs nothing until an operator
//! reviews and enables it on the Schedule board.
use crate::contributions::{RETENTION_DAYS, RETENTION_MAX};
use bilibi_database::{Db, Error, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Worker, HANDLERS, WORKERS,
};
use bilibi_schedule::{
    worker, Definition, Misfire, Overlap, Reach, Schedule, DEFINITIONS, EXECUTE,
};
use bilibi_settings::Settings;
use serde_json::{json, Value};
use std::{sync::Arc, time::SystemTime};

/// The worker and schedule id.
pub const WORKER: &str = "base/ai-interaction-retention";
pub(crate) const SERVICE: &str = "base/ai.retention";
macro_rules! provenance {
    () => {
        Provenance {
            module: crate::MODULE,
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
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: SERVICE, capability: EXECUTE }) }
inventory::submit! { Contribution::new(DEFINITIONS, WORKER, provenance!(), &Definition {
    name: "Prune old AI interaction records", expression: "41 3 * * *", timezone: "Etc/UTC",
    task_name: "Base AI interaction retention", worker: WORKER, args: || json!({}),
    overlap: Overlap::Forbid, misfire: Misfire::Coalesce, reach: Reach::Installation, owner_route: None,
}) }

pub(crate) struct Retention {
    pub schedule: Arc<Schedule>,
    pub settings: Arc<Settings>,
    pub db: Db,
}
impl Handler for Retention {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(self.schedule.run(execution, args, |_| async {
            let pruned = async {
                let client = self.db.get().await?;
                let days = self
                    .settings
                    .get_global(&**client, RETENTION_DAYS)
                    .await?
                    .as_i64()
                    .filter(|days| (1..=RETENTION_MAX).contains(days))
                    .ok_or(Error::Invalid)?;
                crate::store::prune(&**client, days, SystemTime::now()).await
            }
            .await;
            match pruned {
                Ok(_) => Outcome::Completed,
                Err(_) => Outcome::Retry("ai_store_unavailable"),
            }
        }))
    }
}
