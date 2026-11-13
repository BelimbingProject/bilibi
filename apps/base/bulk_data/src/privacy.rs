//! Cached exports and import leftovers are replaceable copies, not business
//! records. The record owner invalidates its type in the same transaction as
//! erasure. The shared export guard covers reading through publication; the
//! exclusive erasure guard lasts until the owner's transaction commits, and
//! is taken with a bounded wait, so an erasure never sits behind a long
//! export while Queue's row for its job is locked (issue 572).
//!
//! An import run of the type, or of a type whose rows name it as their
//! parent, holds the person's data too (issue 571): the uploaded file whole,
//! and each row's source key and message, which can quote a cell. The owner's
//! transaction only marks those runs, which every read then treats as holding
//! nothing; the rows are blanked and the files deleted after it commits, in
//! bounded batches, so a tenant with a long import history does not make the
//! erasure's own transaction long.
use crate::{files::FILES, BulkData};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_personal_data::{Action, Erased, Erasure, Kept, KeptReason, Progress};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_postgres::Client;

pub(crate) const EXPORT_GUARD: &str =
    "SELECT pg_advisory_lock_shared(hashtextextended('bulk-export-' || $1::bigint::text || '-' || $2::text, 0))";
/// The same key, exclusive, asked without waiting.
const ERASURE_GUARD: &str =
    "SELECT pg_try_advisory_xact_lock(hashtextextended('bulk-export-' || $1::bigint::text || '-' || $2::text, 0))";
/// How often, and how far apart, an erasure asks for its guard before it
/// answers `Conflict` for its job to retry: two seconds in all, well inside
/// the third of Queue's lease that its heartbeat may wait on the job's row.
const GUARD_TRIES: u32 = 20;
const GUARD_PAUSE: Duration = Duration::from_millis(100);
/// Row results blanked per transaction, by row number.
const ROW_BATCH: i32 = 5000;
const CLEAN: &str = "base.bulk_data.erasure.clean";
const EXPORT_CLEANED: &str = "UPDATE base_bulk_data_exports SET cleaned_at=clock_timestamp(),updated_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND cleaned_at IS NULL";
const RUN_CLEANED: &str = "UPDATE base_bulk_data_runs SET cleaned_at=clock_timestamp(),source_artifact=NULL,updated_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND cleaned_at IS NULL";

impl BulkData {
    /// The record types whose import runs can hold a record of this type's
    /// data: the type itself, and each type with a field naming it as the
    /// parent (its file names those records, and its messages quote them).
    fn erasure_types(&self, record_type: &str) -> Vec<String> {
        let mut types = vec![record_type.to_owned()];
        for registered in self.types.values() {
            if registered.key != record_type
                && registered
                    .definition
                    .import
                    .references
                    .iter()
                    .any(|reference| reference.parent == Some(record_type))
            {
                types.push(registered.key.to_owned());
            }
        }
        types
    }

    /// Discard every cached export of the owner's record type, and mark its
    /// import runs for clean-up: filters, linked columns and uploaded files
    /// can include the subject without naming their ID. Call inside the
    /// transaction that erases source data, before changing it. `Conflict`
    /// when an export of the type is still reading: the erasure's job
    /// retries.
    pub async fn erase_exports_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        grant: &Erasure,
        record_type: &str,
    ) -> Result<()> {
        grant.check(scope)?;
        let person = grant.person().ok_or(Error::Forbidden)?.get();
        let tenant = scope.tenant();
        if tx.context().tenant != tenant {
            return Err(Error::Forbidden);
        }
        let mut held = false;
        for attempt in 0..GUARD_TRIES {
            if attempt > 0 {
                tokio::time::sleep(GUARD_PAUSE).await;
            }
            held = tx
                .sql()
                .query_one(ERASURE_GUARD, &[&tenant, &record_type])
                .await?
                .get(0);
            if held {
                break;
            }
        }
        if !held {
            return Err(Error::Conflict);
        }
        // Only what still holds something: an export or run an earlier
        // erasure has cleaned is left alone, so an erasure's cost follows
        // what was exported or imported since, not the tenant's history.
        if tx.sql().query_opt(
            "SELECT id FROM base_bulk_data_exports WHERE tenant_id=$1 AND record_type=$2 AND cleaned_at IS NULL AND NOT ($3=ANY(erased_for)) LIMIT 1",
            &[&tenant, &record_type, &person],
        ).await?.is_some() {
            let record_type = record_type.to_owned();
            tx.mutate(move |sql| Box::pin(async move {
                let rows = sql.query(
                    "UPDATE base_bulk_data_exports SET erased_for=array_append(erased_for,$3),state='failed', \
                     error='The source records changed after this export. Export the list again.', \
                     filter='{}',ids=NULL,except_ids=NULL,access=NULL,finished_at=coalesce(finished_at,now()),version=version+1 \
                     WHERE tenant_id=$1 AND record_type=$2 AND cleaned_at IS NULL AND NOT ($3=ANY(erased_for)) RETURNING id",
                    &[&tenant, &record_type, &person],
                ).await?;
                Ok(((), rows.iter().map(|row| Change {
                    kind: "base/bulk_data.export".into(), id: row.get(0), old: Value::Null,
                    new: json!({"state":"failed","erased_for":person}),
                }).collect()))
            })).await?;
        }
        let types = self.erasure_types(record_type);
        if tx.sql().query_opt(
            "SELECT id FROM base_bulk_data_runs WHERE tenant_id=$1 AND record_type=ANY($2) AND cleaned_at IS NULL AND NOT ($3=ANY(erased_for)) LIMIT 1",
            &[&tenant, &types, &person],
        ).await?.is_some() {
            // A run still working ends here: its file is about to go, and a
            // row recorded after the clean-up would quote it again.
            tx.mutate(move |sql| Box::pin(async move {
                let rows = sql.query(
                    "UPDATE base_bulk_data_runs SET erased_for=array_append(erased_for,$3), \
                     finished_at=CASE WHEN state IN ('previewing','running') THEN clock_timestamp() ELSE finished_at END, \
                     state=CASE WHEN state IN ('previewing','running') THEN 'cancelled' ELSE state END, \
                     version=version+1,updated_at=clock_timestamp() \
                     WHERE tenant_id=$1 AND record_type=ANY($2) AND cleaned_at IS NULL AND NOT ($3=ANY(erased_for)) RETURNING id,state",
                    &[&tenant, &types, &person],
                ).await?;
                Ok(((), rows.iter().map(|row| Change {
                    kind: "base/bulk_data.run".into(), id: row.get(0), old: Value::Null,
                    new: json!({"state":row.get::<_, String>(1),"erased_for":person}),
                }).collect()))
            })).await?;
        }
        Ok(())
    }

    /// Finish the clean-up through Artifacts' grant-checked path: every
    /// published copy of an invalidated export, and each marked import run's
    /// row details and uploaded file. A failed delete retries; retention and
    /// holds are reported, never hidden, and leave the export or run to the
    /// next erasure too.
    pub async fn remove_erased_exports(
        &self,
        client: &mut Client,
        scope: &Scope,
        grant: &Erasure,
        record_type: &str,
        progress: &dyn Progress,
    ) -> Result<()> {
        grant.check(scope)?;
        let person = grant.person().ok_or(Error::Forbidden)?.get();
        let rows = client
            .query(
                "SELECT id FROM base_bulk_data_exports \
             WHERE tenant_id=$1 AND record_type=$2 AND $3=ANY(erased_for) AND cleaned_at IS NULL ORDER BY id",
                &[&scope.tenant(), &record_type, &person],
            )
            .await?;
        for row in rows {
            let id: i64 = row.get(0);
            let subject = format!("export:{id}");
            if self
                .remove_files(client, scope, grant, &subject, "bulk_data_export", progress)
                .await?
            {
                self.cleaned(client, scope, "base/bulk_data.export", EXPORT_CLEANED, id)
                    .await?;
            }
        }
        let types = self.erasure_types(record_type);
        let runs = client
            .query(
                "SELECT id,row_count FROM base_bulk_data_runs \
             WHERE tenant_id=$1 AND record_type=ANY($2) AND $3=ANY(erased_for) AND cleaned_at IS NULL ORDER BY id",
                &[&scope.tenant(), &types, &person],
            )
            .await?;
        for row in runs {
            let (id, count): (i64, i32) = (row.get(0), row.get(1));
            self.clear_rows(client, scope, id, count).await?;
            if self
                .remove_files(
                    client,
                    scope,
                    grant,
                    &id.to_string(),
                    "bulk_data_import",
                    progress,
                )
                .await?
            {
                self.cleaned(client, scope, "base/bulk_data.run", RUN_CLEANED, id)
                    .await?;
            }
        }
        Ok(())
    }

    /// Deletes every unpurged document of one subject; false when retention
    /// or a legal hold kept one.
    async fn remove_files(
        &self,
        client: &mut Client,
        scope: &Scope,
        grant: &Erasure,
        subject: &str,
        kind: &str,
        progress: &dyn Progress,
    ) -> Result<bool> {
        let documents = self
            .artifacts
            .erasable_documents(client, scope, grant, &FILES, subject)
            .await
            .map_err(crate::artifact)?;
        let mut gone = true;
        for (company, document) in documents {
            let artifact = document.id;
            let mut step = Erased::default();
            match self
                .artifacts
                .for_erasure(scope, company, &FILES, grant, subject)
                .delete(client, &artifact)
                .await
            {
                Ok(()) => step.count(kind, Action::Deleted, 1),
                Err(bilibi_artifacts::Error::LegalHold) => {
                    gone = false;
                    step.kept.push(Kept {
                        kind: kind.into(),
                        id: artifact,
                        reason: KeptReason::LegalHold,
                        until: None,
                        hold: None,
                    });
                }
                Err(bilibi_artifacts::Error::Retained) => {
                    gone = false;
                    step.kept.push(Kept {
                        kind: kind.into(),
                        id: artifact,
                        reason: KeptReason::RetainedUntil,
                        until: document.retain_until,
                        hold: None,
                    });
                }
                Err(bilibi_artifacts::Error::Database(error)) => return Err(error),
                Err(_) => return Err(Error::Internal),
            }
            progress.record(step).await?;
        }
        Ok(gone)
    }

    /// Blanks what a run's rows can quote of its file, a batch of row
    /// numbers per transaction. The run has ended, so no row is added behind
    /// a batch; a retried clean-up finds nothing left in the batches done.
    async fn clear_rows(
        &self,
        client: &mut Client,
        scope: &Scope,
        run: i64,
        count: i32,
    ) -> Result<()> {
        let tenant = scope.tenant();
        let mut from = 0_i32;
        while from < count {
            let to = from.saturating_add(ROW_BATCH);
            let mut tx = self
                .boundary
                .begin(&mut *client, scope.mutation_context(CLEAN))
                .await?;
            tx.mutate(move |sql| {
                Box::pin(async move {
                    let cleared = sql
                        .execute(
                            "UPDATE base_bulk_data_rows SET source_key=NULL,message=NULL \
                             WHERE tenant_id=$1 AND run_id=$2 AND row_number>$3 AND row_number<=$4 \
                             AND (source_key IS NOT NULL OR message IS NOT NULL)",
                            &[&tenant, &run, &from, &to],
                        )
                        .await?;
                    let changes = if cleared == 0 {
                        Vec::new()
                    } else {
                        vec![Change {
                            kind: "base/bulk_data.run".into(),
                            id: run,
                            old: Value::Null,
                            new: json!({"cleared_rows": cleared}),
                        }]
                    };
                    Ok(((), changes))
                })
            })
            .await?;
            tx.commit().await?;
            from = to;
        }
        Ok(())
    }

    /// Records that an erasure left nothing of this export or run behind.
    async fn cleaned(
        &self,
        client: &mut Client,
        scope: &Scope,
        kind: &'static str,
        statement: &'static str,
        id: i64,
    ) -> Result<()> {
        let tenant = scope.tenant();
        let mut tx = self
            .boundary
            .begin(client, scope.mutation_context(CLEAN))
            .await?;
        tx.mutate(move |sql| {
            Box::pin(async move {
                let done = sql.execute(statement, &[&tenant, &id]).await?;
                let changes = if done == 0 {
                    Vec::new()
                } else {
                    vec![Change {
                        kind: kind.into(),
                        id,
                        old: Value::Null,
                        new: json!({"cleaned": true}),
                    }]
                };
                Ok(((), changes))
            })
        })
        .await?;
        tx.commit().await
    }
}
