//! The installation schedule checks for a release; it has no install authority.
use crate::Software;
use bilibi_database::{Change, Error, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_notifications::Notice;
use bilibi_queue::{Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Worker};
use bilibi_schedule::{Definition, Misfire, Overlap, Reach, Schedule};
use serde_json::{json, Value};
use std::sync::Arc;

const WORKER: &str = "base/software-check-update";
pub(super) const SERVICE: &str = "base/software.update-notice";
macro_rules! source {
    () => {
        Provenance {
            module: "base/software",
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! { Contribution::new(bilibi_queue::WORKERS, WORKER, source!(), &Worker {
    max_attempts: 3, validate: |args| bilibi_schedule::worker::validate(args, |v|
        if v == json!({}) {Ok(v)} else {Err(Error::Invalid)})
}) }
inventory::submit! { Contribution::new(bilibi_queue::HANDLERS, WORKER, source!(), &HandlerBinding {service:SERVICE, capability:bilibi_schedule::EXECUTE}) }
inventory::submit! { Contribution::new(bilibi_schedule::DEFINITIONS, "base/software.updates", source!(), &Definition {
    name:"Check for software updates", expression:"*/15 * * * *", timezone:"Etc/UTC",
    task_name:"Check for software updates", worker:WORKER, args:|| json!({}), overlap:Overlap::Forbid,
    misfire:Misfire::Coalesce, reach:Reach::Installation, owner_route:Some("/system/software?tab=releases"),
}) }
pub(super) struct Check {
    pub software: Arc<Software>,
    pub schedule: Arc<Schedule>,
}
impl Handler for Check {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(self.schedule.run(execution, args, |_| async {
            match self.check(execution).await {
                Ok(()) => Outcome::Completed,
                Err(error) => {
                    eprintln!("software update check: {error}");
                    Outcome::Retry("software_update_check_failed")
                }
            }
        }))
    }
}
impl Check {
    async fn check(&self, execution: &Execution) -> Result<()> {
        let Some(paths) = self.software.paths.as_ref() else {
            return Ok(());
        };
        let scope = execution.scope().ok_or(Error::Internal)?;
        let listing = self.software.github_listing(scope, false, false).await?;
        let update = super::update::discover(Some(paths), &listing);
        if update["state"] != "ready" {
            return Ok(());
        }
        let tag = update["release"]["tag"].as_str().ok_or(Error::Internal)?;
        let repo = listing["repository_id"].as_i64().ok_or(Error::Internal)?;
        let mut client = self.software.db.get().await?;
        let mut tx = execution
            .begin(&mut client, "software.update.available")
            .await?;
        let Some(owner) = self.software.notifications.installation_owner(&tx).await? else {
            return Ok(());
        };
        // Re-prove the recipient's access to the release notes. The check
        // still runs and commits under the Queue identity.
        let recipient = self
            .software
            .authentication
            .authenticate(
                tx.sql(),
                scope.tenant(),
                owner,
                format!("update-notice-{owner}"),
            )
            .await?;
        if !bilibi_authz::holds_proof(tx.sql(), &recipient, crate::VIEW).await? {
            return Ok(());
        }
        let receipt = tx.sql().query_opt(
            "INSERT INTO base_software_update_notices(tenant_id,recipient,repository_id,tag) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING RETURNING id",
            &[&scope.tenant(),&owner,&repo,&tag]).await?;
        if let Some(row) = receipt {
            tx.record(Change {
                kind: "base/software.update-notice".into(),
                id: row.get(0),
                old: Value::Null,
                new: json!({"recipient":owner,"repository_id":repo,"tag":tag}),
            })
            .await?;
            let title = format!(
                "{} is ready",
                update["release"]["version"]
                    .as_str()
                    .ok_or(Error::Internal)?
            );
            let notice = Notice::new(title)
                .kind("software.update")
                .body(
                    update["release"]["notes"]
                        .as_str()
                        .filter(|notes| !notes.is_empty())
                        .unwrap_or("No change notes were supplied."),
                )
                .url("/system/software?tab=releases");
            self.software
                .notifications
                .notify(&mut tx, scope, &[owner], &notice)
                .await?;
        }
        tx.commit().await
    }
}
