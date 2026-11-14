//! Abandoned drafts (issue 570): an uploader keeps at most [`MAX_DRAFTS`]
//! unretained drafts per tenant, and drafts older than [`DRAFT_AGE`] go,
//! either when their uploader needs room, through `cleanup_drafts` (the
//! `media-cleanup` host command) or through the `base/media-draft-cleanup`
//! schedule in every tenant that allows it. A legal hold on a draft or on its
//! asset keeps it.
use super::{bound, Cleanup, Uploader, CLEANUP_BATCH, MAX_BIND, SELECT};
use crate::{Error, Media, Result};
use bilibi_database::{Context, Db, Principal};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_personal_data::RecordHolds;
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Worker, HANDLERS, WORKERS,
};
use bilibi_schedule::{
    worker, Definition, Misfire, Overlap, Reach, Schedule, DEFINITIONS, EXECUTE,
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio_postgres::Client;

/// Unretained drafts one uploader may keep in a tenant: one submit binds at
/// most [`MAX_BIND`], so more could never be submitted together.
pub(crate) const MAX_DRAFTS: i64 = MAX_BIND as i64;
/// How old an unretained draft is before it counts as abandoned: the host
/// command's default.
pub(crate) const DRAFT_AGE: Duration = Duration::from_secs(24 * 3600);
const CLEANUP_PRINCIPAL: &str = "media.attachments.cleanup";
const WORKER: &str = "base/media-draft-cleanup";
pub(crate) const DRAFT_HANDLER: &str = "base/media.draft_cleanup";
macro_rules! provenance {
    () => {
        Provenance {
            module: "base/media",
            source: file!(),
            line: line!(),
        }
    };
}

fn no_arguments(args: Value) -> bilibi_database::Result<Value> {
    match args.as_object() {
        Some(object) if object.is_empty() => Ok(args),
        _ => Err(bilibi_database::Error::Invalid),
    }
}
inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: 3, validate: |args| worker::validate(args, no_arguments) }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: DRAFT_HANDLER, capability: EXECUTE }) }
inventory::submit! { Contribution::new(bilibi_queue::CAPABILITY_POLICIES, WORKER, provenance!(), &bilibi_schedule::TENANT_POLICY) }
inventory::submit! { Contribution::new(DEFINITIONS, "base/media-draft-cleanup", provenance!(), &Definition {
    name: "Remove abandoned attachment drafts", expression: "47 * * * *", timezone: "Etc/UTC",
    task_name: "Base Media draft cleanup", worker: WORKER, args: || json!({}),
    overlap: Overlap::Forbid, misfire: Misfire::Coalesce, reach: Reach::EachTenant, owner_route: None,
}) }

/// The `base/media-draft-cleanup` job: one tenant's drafts older than
/// [`DRAFT_AGE`].
pub(crate) struct CleanupJob {
    pub schedule: Arc<Schedule>,
    pub media: Arc<Media>,
    pub db: Db,
}
impl Handler for CleanupJob {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(self.schedule.run(execution, args, |_| async {
            let cleaned = async {
                let mut client = self.db.get().await.map_err(bilibi_database::Error::from)?;
                self.media
                    .cleanup_drafts(&mut client, execution.tenant(), DRAFT_AGE)
                    .await
            }
            .await;
            match cleaned {
                // Files that would not go are counted for the operator; the
                // rows are gone, so a retry would find nothing.
                Ok(_) => Outcome::Completed,
                Err(_) => Outcome::Retry("draft_cleanup_unavailable"),
            }
        }))
    }
}

/// `uploader`'s unretained drafts in `tenant`.
async fn unretained(client: &Client, tenant: i64, uploader: &Uploader) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT count(*) FROM base_media_attachments WHERE tenant_id=$1 AND state='draft' AND retained_at IS NULL \
             AND (uploaded_by_id=$2 OR uploaded_by_principal=$3::text)",
            &[&tenant, &uploader.user(), &uploader.principal()],
        )
        .await?
        .get(0))
}

impl Media {
    /// Remove one tenant's abandoned drafts: unretained, not submitted, older
    /// than `older_than`, and named by no legal hold (on the attachment or
    /// its asset). Trusted callers only (an operator command or the
    /// schedule); there is no user to authorize, so the writes are attributed
    /// to the system principal `media.attachments.cleanup` in the named
    /// tenant. Each candidate is locked and rechecked, so a draft submitted,
    /// retained or discarded meanwhile survives.
    pub async fn cleanup_drafts(
        &self,
        client: &mut Client,
        tenant: i64,
        older_than: Duration,
    ) -> Result<Cleanup> {
        if older_than.is_zero() {
            return Err(Error::InvalidRules);
        }
        let live = client
            .query_opt(
                "SELECT 1 FROM tenants WHERE id=$1 AND deleted_at IS NULL",
                &[&tenant],
            )
            .await?;
        if live.is_none() {
            return Err(Error::NotFound);
        }
        self.remove_abandoned(client, tenant, older_than, None)
            .await
    }

    /// Before an upload stores bytes: `TooManyDrafts` while `uploader` keeps
    /// [`MAX_DRAFTS`] unretained drafts in `tenant`, after removing its own
    /// abandoned ones. Concurrent uploads may pass it together, so the cap
    /// holds to within the uploader's own concurrency.
    pub(super) async fn room_for_draft(
        &self,
        client: &mut Client,
        tenant: i64,
        uploader: &Uploader,
    ) -> Result<()> {
        if unretained(client, tenant, uploader).await? < MAX_DRAFTS {
            return Ok(());
        }
        self.remove_abandoned(client, tenant, DRAFT_AGE, Some(uploader))
            .await?;
        if unretained(client, tenant, uploader).await? < MAX_DRAFTS {
            Ok(())
        } else {
            Err(Error::TooManyDrafts)
        }
    }

    /// Every abandoned draft of `tenant` (of `uploader` only, when named),
    /// in id order, one transaction each; a held one is passed over.
    async fn remove_abandoned(
        &self,
        client: &mut Client,
        tenant: i64,
        older_than: Duration,
        uploader: Option<&Uploader>,
    ) -> Result<Cleanup> {
        let age = older_than.as_secs_f64();
        let (user, principal) = (
            uploader.and_then(Uploader::user),
            uploader.and_then(Uploader::principal),
        );
        let mut report = Cleanup::default();
        let mut after = 0_i64;
        loop {
            let candidates = client
                .query(
                    "SELECT id,public_id,media_asset_id FROM base_media_attachments WHERE tenant_id=$1 AND state='draft' AND retained_at IS NULL \
                     AND created_at < now()-make_interval(secs => $2) AND id>$3 \
                     AND ($4::bigint IS NULL OR uploaded_by_id=$4) AND ($5::text IS NULL OR uploaded_by_principal=$5) ORDER BY id LIMIT $6",
                    &[&tenant, &age, &after, &user, &principal, &CLEANUP_BATCH],
                )
                .await?;
            let Some(last) = candidates.last() else {
                break;
            };
            after = last.get("id");
            let holds = RecordHolds::acquire(tenant).await?;
            let removed = self
                .remove_batch(client, tenant, age, &candidates, &holds, &mut report)
                .await;
            holds.release().await?;
            removed?;
            if (candidates.len() as i64) < CLEANUP_BATCH {
                break;
            }
        }
        Ok(report)
    }
    async fn remove_batch(
        &self,
        client: &mut Client,
        tenant: i64,
        age: f64,
        candidates: &[tokio_postgres::Row],
        holds: &RecordHolds,
        report: &mut Cleanup,
    ) -> Result<()> {
        let ids: Vec<i64> = candidates.iter().map(|row| row.get("id")).collect();
        let assets: Vec<i64> = candidates
            .iter()
            .map(|row| row.get("media_asset_id"))
            .collect();
        let held = holds.records(super::ATTACHMENT_KIND, &ids).await?;
        let held_assets = holds.records(super::ASSET_KIND, &assets).await?;
        for candidate in candidates {
            if held.contains_key(&candidate.get::<_, i64>("id"))
                || held_assets.contains_key(&candidate.get::<_, i64>("media_asset_id"))
            {
                continue;
            }
            let public_id: String = candidate.get("public_id");
            let context = Context {
                tenant,
                actor: Principal {
                    kind: "system".into(),
                    id: CLEANUP_PRINCIPAL.into(),
                },
                impersonator: None,
                actor_role: None,
                action: "media.attachment.cleanup".into(),
                trace_id: uuid::Uuid::new_v4().to_string(),
                agent_self_report: None,
                client_identity: None,
                agent_connection: None,
            };
            let mut tx = self.boundary.begin(client, context).await?;
            let row = tx
                .sql()
                .query_opt(
                    &format!("{SELECT} WHERE a.tenant_id=$1 AND a.public_id=$2 AND a.state='draft' AND a.retained_at IS NULL AND a.created_at < now()-make_interval(secs => $3) FOR UPDATE OF a"),
                    &[&tenant, &public_id, &age],
                )
                .await?;
            let Some(row) = row else {
                continue;
            };
            let file = self.remove_draft(&mut tx, tenant, &bound(&row)?).await?;
            tx.commit().await?;
            report.removed += 1;
            if self.remove_files(client, &[file]).await.is_err() {
                report.file_failures += 1;
            }
        }
        Ok(())
    }
}
