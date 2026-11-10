//! Records-grade retention, legal holds, the append-only retention ledger and
//! retention-driven purge. The facade refuses first with typed errors;
//! `migrations/002-retention.sql` refuses the same changes from any other client.
use crate::{
    bounded, evidence, stored, Error, Operation, Owned, Owner, Recovery, Result, Stored, COLUMNS,
};
use bilibi_database::{Change, MutationTransaction};
use serde_json::json;
use std::time::{Duration, SystemTime};
use tokio_postgres::{types::ToSql, Client, GenericClient};

const MAX_REASON: usize = 255;
/// Far enough for any statutory period; stops overflow and typos.
const MAX_HORIZON: Duration = Duration::from_secs(200 * 365 * 24 * 3600);

/// Retention applied when a document is published.
#[derive(Clone, Debug)]
pub struct Retention {
    pub retain_until: SystemTime,
    pub legal_hold: bool,
    /// At most 255 characters; never document content.
    pub reason: Option<String>,
}
impl Retention {
    pub fn until(retain_until: SystemTime) -> Self {
        Self {
            retain_until,
            legal_hold: false,
            reason: None,
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        future(self.retain_until)?;
        reason(self.reason.as_deref())
    }
}
fn future(time: SystemTime) -> Result<()> {
    let now = SystemTime::now();
    if time > now && time <= now + MAX_HORIZON {
        Ok(())
    } else {
        Err(Error::InvalidRetention)
    }
}
fn reason(text: Option<&str>) -> Result<()> {
    match text {
        Some(text) if !bounded(text, MAX_REASON) => Err(Error::InvalidRetention),
        _ => Ok(()),
    }
}

/// One append-only ledger row. PostgreSQL stamps `recorded_at`.
#[derive(Clone, Debug)]
pub struct RetentionEvent {
    pub event: String,
    pub retain_until: String,
    pub legal_hold: bool,
    pub reason: Option<String>,
    pub actor_type: String,
    pub actor_id: i64,
    pub impersonator_id: Option<i64>,
    pub system_principal: Option<String>,
    pub recorded_at: String,
}
/// Retention state of an owned artifact, with no bytes or storage location.
#[derive(Clone, Debug)]
pub struct RetentionStatus {
    pub id: String,
    pub retain_until: Option<SystemTime>,
    pub legal_hold: bool,
    /// The retained period has elapsed and there is no legal hold.
    pub due: bool,
}

/// A document whose purge an operator must retry or resolve.
#[derive(Clone, Debug)]
pub struct PurgeHold {
    pub metadata: crate::Metadata,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub last_attempted_at: Option<String>,
    pub held_at: String,
}

impl<O: Owner> Owned<'_, O> {
    /// Needs the owner's `Retain` grant on a published, undeleted document.
    /// An expired ordinary document may still be retained until it is purged.
    async fn retainable(&self, tx: &MutationTransaction<'_>, id: &str) -> Result<Stored> {
        let row = self.fetch(tx, id).await?;
        self.authorize(tx.sql(), Operation::Retain, Some(&row.metadata.reference))
            .await?;
        if row.ready && !row.deleted {
            Ok(row)
        } else {
            Err(Error::NotFound)
        }
    }
    /// `update` is chosen by this module only; it takes the artifact key first.
    async fn write_retention(
        &self,
        tx: &mut MutationTransaction<'_>,
        row: &Stored,
        update: &'static str,
        params: &[&(dyn ToSql + Sync)],
        event: &str,
        why: Option<&str>,
    ) -> Result<Stored> {
        tx.sql().execute(update, params).await?;
        let next = self.fetch(tx, &row.metadata.id).await?;
        tx.record(Change {
            kind: "base/artifacts.document".into(),
            id: row.key,
            old: evidence(row),
            new: evidence(&next),
        })
        .await?;
        let who = bilibi_audit::Attribution::from_context(tx.context())?;
        tx.sql().execute("INSERT INTO base_artifact_retention_events(artifact_id,tenant_id,company_id,owner_id,event,retain_until,legal_hold,reason,actor_type,actor_id,impersonator_id,system_principal,trace_id) SELECT id,tenant_id,company_id,owner_id,$2,retain_until,legal_hold_at IS NOT NULL,$3,$4,$5,$6,$7,$8 FROM base_artifacts WHERE id=$1",
            &[&row.key,&event,&why,&who.actor_type,&who.actor_id,&who.impersonator_id,&who.system_principal,&who.trace_id]).await?;
        self.audit_with(
            tx,
            &next,
            &format!("artifacts.{event}"),
            json!({"retain_until":next.metadata.retain_until,"legal_hold":next.on_hold()}),
        )
        .await?;
        Ok(next)
    }
    /// Publication-transaction step of `put_retained`.
    pub(crate) async fn retain_published(
        &self,
        tx: &mut MutationTransaction<'_>,
        row: Stored,
        retention: &Retention,
    ) -> Result<Stored> {
        self.write_retention(tx, &row, "UPDATE base_artifacts SET retain_until=$2,legal_hold_at=CASE WHEN $3 THEN clock_timestamp() END,expires_at=NULL WHERE id=$1",
            &[&row.key,&retention.retain_until,&retention.legal_hold], "retained", retention.reason.as_deref()).await
    }
    /// Makes an ordinary document records-grade (its expiry is gone for good) or
    /// moves retain-until later. An equal or earlier time is
    /// `RetentionNotExtended`; the database guard refuses it as well.
    pub async fn retain(
        &self,
        client: &mut Client,
        id: &str,
        retain_until: SystemTime,
        why: Option<&str>,
    ) -> Result<crate::Metadata> {
        let mut tx = self.begin(client, "artifacts.retain").await?;
        let metadata = self.retain_in(&mut tx, id, retain_until, why).await?;
        tx.commit().await?;
        Ok(metadata)
    }
    /// Apply the same retention in the owner's already-open mutation, after
    /// its record has attached the artifact. The owner commits both changes
    /// together; Base still authorizes, audits and records the retention event.
    pub async fn retain_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        id: &str,
        retain_until: SystemTime,
        why: Option<&str>,
    ) -> Result<crate::Metadata> {
        future(retain_until)?;
        reason(why)?;
        let row = self.retainable(tx, id).await?;
        let event = if row.metadata.retain_until.is_some() {
            let later: bool = tx
                .sql()
                .query_one(
                    "SELECT $2::timestamptz > retain_until FROM base_artifacts WHERE id=$1",
                    &[&row.key, &retain_until],
                )
                .await?
                .get(0);
            if !later {
                return Err(Error::RetentionNotExtended);
            }
            "retention_extended"
        } else {
            "retained"
        };
        let next = self
            .write_retention(
                tx,
                &row,
                "UPDATE base_artifacts SET retain_until=$2,expires_at=NULL WHERE id=$1",
                &[&row.key, &retain_until],
                event,
                why,
            )
            .await?;
        Ok(next.metadata)
    }
    /// Refuses deletion and purge until released, whatever the retain-until.
    pub async fn place_hold(
        &self,
        client: &mut Client,
        id: &str,
        why: Option<&str>,
    ) -> Result<crate::Metadata> {
        reason(why)?;
        let mut tx = self.begin(client, "artifacts.hold").await?;
        let row = self.retainable(&tx, id).await?;
        if row.metadata.retain_until.is_none() {
            return Err(Error::NotRetained);
        }
        if row.on_hold() {
            return Err(Error::AlreadyOnHold);
        }
        let next = self
            .write_retention(
                &mut tx,
                &row,
                "UPDATE base_artifacts SET legal_hold_at=clock_timestamp() WHERE id=$1",
                &[&row.key],
                "hold_placed",
                why,
            )
            .await?;
        tx.commit().await?;
        Ok(next.metadata)
    }
    /// The retain-until keeps protecting the document after release.
    pub async fn release_hold(
        &self,
        client: &mut Client,
        id: &str,
        why: Option<&str>,
    ) -> Result<crate::Metadata> {
        reason(why)?;
        let mut tx = self.begin(client, "artifacts.release").await?;
        let row = self.retainable(&tx, id).await?;
        if !row.on_hold() {
            return Err(Error::NotOnHold);
        }
        let next = self
            .write_retention(
                &mut tx,
                &row,
                "UPDATE base_artifacts SET legal_hold_at=NULL WHERE id=$1",
                &[&row.key],
                "hold_released",
                why,
            )
            .await?;
        tx.commit().await?;
        Ok(next.metadata)
    }
    /// Current state for a bounded batch of this owner's attached artifacts.
    /// Each row needs the owner's `Retain` grant. Unknown or unavailable IDs
    /// refuse the whole request, so callers cannot infer cross-tenant files.
    pub async fn retention_statuses(
        &self,
        client: &Client,
        ids: &[String],
    ) -> Result<Vec<RetentionStatus>> {
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        if unique.is_empty() {
            return Ok(Vec::new());
        }
        if unique.len() > 1000 {
            return Err(Error::Invalid);
        }
        let rows = client
            .query(
                &format!("SELECT {COLUMNS},retain_until AS retain_until_instant FROM base_artifacts WHERE public_id=ANY($1) AND tenant_id=$2 AND company_id=$3 AND owner_id=$4 AND owner_adapter=$5 ORDER BY public_id"),
                &[&unique, &self.scope.tenant(), &self.company.get(), &self.owner.owner_id(), &std::any::type_name::<O>()],
            )
            .await?;
        if rows.len() != unique.len() {
            return Err(Error::NotFound);
        }
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let until: Option<SystemTime> = row.get("retain_until_instant");
            let row = stored(row);
            self.authorize(client, Operation::Retain, Some(&row.metadata.reference))
                .await?;
            if !row.ready || row.deleted {
                return Err(Error::NotFound);
            }
            let held = row.on_hold();
            let due = row.metadata.retain_until.is_some() && !row.retained && !held;
            result.push(RetentionStatus {
                id: row.metadata.id,
                retain_until: until,
                legal_hold: held,
                due,
            });
        }
        Ok(result)
    }
    /// Retention events, oldest first. Needs `Read`; tombstones keep history.
    pub async fn retention_history(
        &self,
        client: &Client,
        id: &str,
    ) -> Result<Vec<RetentionEvent>> {
        let row = self.find(client, id, "").await?;
        self.authorize(client, Operation::Read, Some(&row.metadata.reference))
            .await?;
        let rows = client.query("SELECT event,retain_until::text,legal_hold,reason,actor_type,actor_id,impersonator_id,system_principal,recorded_at::text FROM base_artifact_retention_events WHERE artifact_id=$1 AND tenant_id=$2 ORDER BY id", &[&row.key,&self.scope.tenant()]).await?;
        Ok(rows
            .iter()
            .map(|r| RetentionEvent {
                event: r.get(0),
                retain_until: r.get(1),
                legal_hold: r.get(2),
                reason: r.get(3),
                actor_type: r.get(4),
                actor_id: r.get(5),
                impersonator_id: r.get(6),
                system_principal: r.get(7),
                recorded_at: r.get(8),
            })
            .collect())
    }

    async fn number(&self, sql: &(impl GenericClient + Sync), key: &str) -> Result<i64> {
        self.store
            .settings
            .get_global(sql, key)
            .await?
            .as_i64()
            .filter(|n| *n > 0)
            .ok_or(Error::Policy)
    }
    /// Bounded owner/company retention batch: expired ordinary documents and
    /// unfinished deletes, after authorizing `Purge` and before selecting
    /// candidates. A records-grade document has no expiry and is never selected
    /// unless an explicit, permitted delete already tombstoned it. A failed or
    /// refused purge records its attempt and is skipped until the retry interval
    /// passes; after the maximum attempts an operator must `retry_purge` or
    /// `resolve_purge` it. Callers own scheduling and alerting.
    pub async fn purge_expired(&self, client: &mut Client) -> Result<Recovery> {
        self.authorize(&*client, Operation::Purge, None).await?;
        let batch = self.number(&*client, "artifacts.purge_batch_size").await?;
        let retry = self
            .number(&*client, "artifacts.purge_retry_minutes")
            .await?;
        let max = self
            .number(&*client, "artifacts.purge_max_attempts")
            .await?;
        let rows = client.query("SELECT public_id FROM base_artifacts WHERE tenant_id=$1 AND company_id=$2 AND owner_id=$3 AND owner_adapter=$4 AND purged_at IS NULL AND purge_held_at IS NULL AND (expires_at <= clock_timestamp() OR deleted_at IS NOT NULL) AND (purge_attempted_at IS NULL OR purge_attempted_at <= clock_timestamp() - $5::bigint * interval '1 minute') ORDER BY expires_at,id LIMIT $6",
            &[&self.scope.tenant(),&self.company.get(),&self.owner.owner_id(),&std::any::type_name::<O>(),&retry,&batch]).await?;
        let mut report = Recovery::default();
        for candidate in rows {
            let id: String = candidate.get(0);
            match self.purge_one(client, &id, max).await {
                Ok(()) => report.purged.push(id),
                Err(error) => report.failed.push((id, error)),
            }
        }
        Ok(report)
    }
    async fn purge_one(&self, client: &mut Client, id: &str, max: i64) -> Result<()> {
        let error = match self.delete(client, id).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        // Unrecorded attempts would never reach the hold: surface that instead.
        self.record_purge_failure(client, id, &error, max).await?;
        Err(error)
    }
    async fn record_purge_failure(
        &self,
        client: &mut Client,
        id: &str,
        error: &Error,
        max: i64,
    ) -> Result<()> {
        let mut tx = self.begin(client, "artifacts.purge_failed").await?;
        let row = self.fetch(&tx, id).await?;
        self.authorize(tx.sql(), Operation::Purge, None).await?;
        if row.purged {
            return Ok(());
        }
        let attempts = i64::from(row.attempts) + 1;
        tx.sql().execute("UPDATE base_artifacts SET purge_attempts=$2::integer,purge_last_error=$3,purge_attempted_at=clock_timestamp(),purge_held_at=CASE WHEN $2::integer >= $4::bigint THEN clock_timestamp() END WHERE id=$1",
            &[&row.key,&(attempts as i32),&error.code(),&max]).await?;
        let next = self.fetch(&tx, id).await?;
        tx.record(Change {
            kind: "base/artifacts.document".into(),
            id: row.key,
            old: evidence(&row),
            new: evidence(&next),
        })
        .await?;
        let facts = json!({"attempts":next.attempts,"reason":error.code()});
        self.audit_with(&tx, &next, "artifacts.purge_failed", facts.clone())
            .await?;
        if next.held_at.is_some() {
            self.audit_with(&tx, &next, "artifacts.purge_held", facts)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// Documents whose purge an operator must retry or resolve.
    pub async fn list_purge_holds(&self, client: &Client) -> Result<Vec<PurgeHold>> {
        self.authorize(client, Operation::Purge, None).await?;
        let rows = client.query(&format!("SELECT {COLUMNS} FROM base_artifacts WHERE tenant_id=$1 AND company_id=$2 AND owner_id=$3 AND owner_adapter=$4 AND purged_at IS NULL AND purge_held_at IS NOT NULL ORDER BY purge_held_at,id"),
            &[&self.scope.tenant(),&self.company.get(),&self.owner.owner_id(),&std::any::type_name::<O>()]).await?;
        Ok(rows
            .into_iter()
            .map(stored)
            .map(|row| PurgeHold {
                attempts: row.attempts,
                last_error: row.last_error,
                last_attempted_at: row.attempted_at,
                held_at: row.held_at.unwrap_or_default(),
                metadata: row.metadata,
            })
            .collect())
    }
    async fn held(&self, tx: &MutationTransaction<'_>, id: &str) -> Result<Stored> {
        let row = self.fetch(tx, id).await?;
        if row.held_at.is_some() && !row.purged {
            Ok(row)
        } else {
            Err(Error::NotFound)
        }
    }
    /// Releases the hold and purges now, rechecking delete authority.
    pub async fn retry_purge(&self, client: &mut Client, id: &str) -> Result<()> {
        self.authorize(&*client, Operation::Purge, None).await?;
        let max = self
            .number(&*client, "artifacts.purge_max_attempts")
            .await?;
        let mut tx = self.begin(client, "artifacts.purge_released").await?;
        let row = self.held(&tx, id).await?;
        tx.sql().execute("UPDATE base_artifacts SET purge_attempts=0,purge_last_error=NULL,purge_attempted_at=NULL,purge_held_at=NULL WHERE id=$1", &[&row.key]).await?;
        let next = self.fetch(&tx, id).await?;
        tx.record(Change {
            kind: "base/artifacts.document".into(),
            id: row.key,
            old: evidence(&row),
            new: evidence(&next),
        })
        .await?;
        self.audit_with(
            &tx,
            &row,
            "artifacts.purge_released",
            json!({"attempts":row.attempts,"reason":row.last_error}),
        )
        .await?;
        tx.commit().await?;
        self.purge_one(client, id, max).await
    }
    /// Records bytes an operator removed out of band. Refused while the private
    /// file exists, and while a hold or retain-until still protects the document.
    pub async fn resolve_purge(&self, client: &mut Client, id: &str) -> Result<()> {
        self.authorize(&*client, Operation::Purge, None).await?;
        let mut tx = self.begin(client, "artifacts.purge_resolved").await?;
        let row = self.held(&tx, id).await?;
        if let Some(refusal) = row.refusal() {
            return Err(refusal);
        }
        crate::storage::io(|| crate::storage::absent(&row.root, id))?;
        tx.sql().execute("UPDATE base_artifacts SET deleted_at=COALESCE(deleted_at,clock_timestamp()),purged_at=clock_timestamp(),purge_held_at=NULL WHERE id=$1", &[&row.key]).await?;
        let next = self.fetch(&tx, id).await?;
        tx.record(Change {
            kind: "base/artifacts.document".into(),
            id: row.key,
            old: evidence(&row),
            new: evidence(&next),
        })
        .await?;
        self.audit_with(
            &tx,
            &row,
            "artifacts.purge_resolved",
            json!({"attempts":row.attempts,"reason":row.last_error}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
