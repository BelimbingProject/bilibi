use super::preview;
use bilibi_database::{Db, Error};
use bilibi_module_registry::Contribution;
use bilibi_queue::{Execution, Handler, HandlerFuture, Outcome};
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) const SERVICE: &str = "base/templates.preview-sweep";
const WORKER: &str = "base/templates-preview-sweep";

fn no_arguments(args: Value) -> bilibi_database::Result<Value> {
    match args.as_object() {
        Some(object) if object.is_empty() => Ok(args),
        _ => Err(Error::Invalid),
    }
}

inventory::submit! { Contribution::new(bilibi_queue::WORKERS, WORKER, crate::provenance!(), &bilibi_queue::Worker {
    max_attempts: 3, validate: |args| bilibi_schedule::worker::validate(args, no_arguments),
}) }
inventory::submit! { Contribution::new(bilibi_queue::HANDLERS, WORKER, crate::provenance!(), &bilibi_queue::HandlerBinding {
    service: SERVICE, capability: bilibi_schedule::EXECUTE,
}) }
inventory::submit! { Contribution::new(bilibi_schedule::DEFINITIONS, WORKER, crate::provenance!(), &bilibi_schedule::Definition {
    name: "Purge expired print previews", expression: "47 * * * *", timezone: "Etc/UTC",
    task_name: "Base Templates preview sweep", worker: WORKER, args: || json!({}),
    overlap: bilibi_schedule::Overlap::Forbid, misfire: bilibi_schedule::Misfire::Coalesce,
    reach: bilibi_schedule::Reach::Installation, owner_route: None,
}) }

pub(crate) struct Sweep {
    pub(crate) schedule: Arc<bilibi_schedule::Schedule>,
    pub(crate) artifacts: Arc<bilibi_artifacts::Artifacts>,
    pub(crate) db: Db,
}

impl Handler for Sweep {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(self.schedule.run(execution, args, |_| async {
            let Ok(mut client) = self.db.get().await else {
                return Outcome::Retry("database_unavailable");
            };
            match self
                .artifacts
                .sweep_expired(&mut client, &preview::OWNER)
                .await
            {
                Ok(report) => {
                    for (_, error) in report.failed {
                        eprintln!("warning: expired print preview purge: {}", error.code());
                        execution.report_warning(
                            "print_preview_purge_failed",
                            "An expired print preview could not be removed.",
                        );
                    }
                    Outcome::Completed
                }
                Err(error) => {
                    eprintln!("warning: print preview sweep: {}", error.code());
                    execution.report_warning(
                        "print_preview_sweep_failed",
                        "Expired print previews could not be cleared.",
                    );
                    Outcome::Retry("files_unavailable")
                }
            }
        }))
    }
}
