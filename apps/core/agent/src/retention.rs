//! The scheduled cleanup of device receipts: hourly, once an operator enables
//! it on the Schedule board. `POST /oauth/device_authorization` answers
//! callers nobody signed in and writes one receipt per admitted request, so
//! without this the table only grows. A receipt is useless ten minutes after
//! it was written, whatever became of it (never approved, refused, approved
//! but never collected, or spent); it is kept one more day so a late poll
//! still gets its own answer, then deleted. The connection an approval
//! created is another row and stays.
use crate::AgentAccess;
use bilibi_database::{Error, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Worker, HANDLERS, WORKERS,
};
use bilibi_schedule::{
    worker, Definition, Misfire, Overlap, Reach, Schedule, DEFINITIONS, EXECUTE,
};
use serde_json::{json, Value};
use std::sync::Arc;

const WORKER: &str = "core/agent-device-retention";
pub(crate) const SERVICE: &str = "core/agent.device_retention";
/// Days a receipt is kept after it expired.
const KEEP_DAYS: i32 = 1;
/// Rows one statement deletes, and statements one run makes: a run removes
/// at most a million receipts and never holds a long lock.
const BATCH: i64 = 10_000;
const BATCHES: usize = 100;

macro_rules! provenance {
    () => {
        Provenance {
            module: "core/agent",
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
inventory::submit! { Contribution::new(DEFINITIONS, "core/agent.device_retention", provenance!(), &Definition {
    name: "Delete used and expired AI sign-in codes", expression: "41 * * * *", timezone: "Etc/UTC",
    task_name: "Core Agent device code retention", worker: WORKER, args: || json!({}),
    overlap: Overlap::Forbid, misfire: Misfire::Coalesce, reach: Reach::Installation, owner_route: None,
}) }

impl AgentAccess {
    /// Deletes device receipts that expired more than a day ago, in bounded
    /// batches, and says how many went. Receipts are pre-authentication
    /// housekeeping like registrations: they name no tenant or person, so
    /// this is no business mutation and writes no audit row.
    pub async fn prune_devices(&self) -> Result<u64> {
        let client = self.db.get().await?;
        let mut deleted = 0;
        for _ in 0..BATCHES {
            let batch = client
                .execute(
                    "DELETE FROM core_agent_devices WHERE id IN (SELECT id FROM core_agent_devices WHERE expires_at < now()-make_interval(days => $1) ORDER BY expires_at LIMIT $2)",
                    &[&KEEP_DAYS, &BATCH],
                )
                .await?;
            deleted += batch;
            if batch < BATCH as u64 {
                break;
            }
        }
        Ok(deleted)
    }
}

pub(crate) struct DeviceRetention {
    pub schedule: Arc<Schedule>,
    pub access: Arc<AgentAccess>,
}
impl Handler for DeviceRetention {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(self.schedule.run(execution, args, |_| async {
            match self.access.prune_devices().await {
                Ok(_) => Outcome::Completed,
                Err(_) => Outcome::Retry("agent_store_unavailable"),
            }
        }))
    }
}
