use crate::Software;
use bilibi_database::{Change, Context, Error, Result};
use bilibi_notifications::Notice;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, time};

impl Software {
    pub async fn run_update_checks(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        let mut ticks = time::interval(Duration::from_secs(15 * 60));
        ticks.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        while !*stop.borrow() {
            tokio::select! {
                biased;
                _ = stop.changed() => return,
                () = async {
                    ticks.tick().await;
                    if let Err(error) = self.check_for_update().await {
                        eprintln!("warning: software update check failed: {error:?}");
                    }
                } => {}
            }
        }
    }

    async fn check_for_update(&self) -> Result<()> {
        let Some(paths) = self.paths.as_ref() else {
            return Ok(());
        };
        let policy = super::release_proof::Policy::load(paths).map_err(Error::refused)?;
        let listing = {
            let client = self.db.get().await?;
            self.github_listing(&**client, None, false, false, &policy)
                .await?
        };
        if listing.is_null() {
            return Ok(());
        }
        let update = super::update::discover(Some(paths), &listing);
        if matches!(
            update["reason"].as_str(),
            Some("check_failed" | "incomplete_list")
        ) {
            return Err(Error::Internal);
        }
        if update["state"] != "ready" {
            return Ok(());
        }
        let tag = update["release"]["tag"].as_str().ok_or(Error::Internal)?;
        let repo = listing["repository_id"].as_i64().ok_or(Error::Internal)?;
        let mut client = self.db.get().await?;
        let Some(tenant) = bilibi_schedule::scheduler::operator_tenant(&**client).await? else {
            return Ok(());
        };
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(tenant, "software", "software.update.available"),
            )
            .await?;
        let Some(owner) = self.notifications.installation_owner(&tx).await? else {
            return Ok(());
        };
        let recipient = self
            .authentication
            .authenticate(tx.sql(), tenant, owner, format!("update-notice-{owner}"))
            .await?;
        if !bilibi_authz::holds_proof(tx.sql(), &recipient, crate::VIEW).await? {
            return Ok(());
        }
        let receipt = tx.sql().query_opt(
            "INSERT INTO base_software_update_notices(tenant_id,recipient,repository_id,tag) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING RETURNING id",
            &[&recipient.tenant(),&owner,&repo,&tag]).await?;
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
            self.notifications
                .notify(&mut tx, &recipient, &[owner], &notice)
                .await?;
        }
        tx.commit().await
    }
}
