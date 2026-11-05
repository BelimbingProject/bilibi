//! The expiry reminder: one Core Activity task per document that has an
//! expiry date, due the tenant's lead time before it
//! (`core.document.expiry_reminder_days`). Core Activity's own sweep rings the
//! assignee's bell and optionally emails; there is no second reminder system.
//! The task is private to the person who set the date, so it is kept only when
//! that person may create activities; otherwise the date is stored without a
//! reminder. Changing or clearing the date, or deleting the document, removes
//! the old task when its owner may still change it.
use crate::store::Store;
use crate::{Document, WriteResult, RECORD};
use bilibi_activity::{Common, Reference, TaskInput};
use bilibi_database::{Change, Error, MutationTransaction};
use bilibi_settings::Target;
use bilibi_tenancy::Scope;
use jiff::Span;

/// The tenant's lead time in days.
pub const REMINDER_DAYS: &str = "core.document.expiry_reminder_days";
const DEFAULT_DAYS: i64 = 14;

impl Store<'_> {
    /// Replaces the reminder of `document` after its expiry date changed:
    /// removes `previous`, creates a task when there is a date, and stores
    /// the new task's id. Returns the document as stored.
    pub(crate) async fn refresh_reminder(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        document: &Document,
        previous: Option<i64>,
    ) -> WriteResult<Document> {
        if let Some(id) = previous {
            self.remove_reminder(tx, scope, id).await;
        }
        let mut activity = None;
        if let Some(expires_on) = document.expires_on {
            if bilibi_authz::holds_proof(tx.sql(), scope, bilibi_activity::MANAGE).await? {
                let days = self
                    .settings
                    .get(tx.sql(), scope, Target::Tenant, REMINDER_DAYS)
                    .await?
                    .value
                    .as_i64()
                    .unwrap_or(DEFAULT_DAYS)
                    .clamp(0, 365);
                let zone = self
                    .display
                    .display(tx.sql(), scope, scope.current_company(), "en")
                    .await?
                    .timezone;
                let lead = expires_on
                    .checked_sub(Span::new().days(days))
                    .map_err(|_| Error::Internal)?;
                let due = lead
                    .at(9, 0, 0, 0)
                    .in_tz(&zone)
                    .map_err(|_| Error::Internal)?
                    .timestamp();
                let title: String = format!("Document expires: {}", document.name)
                    .chars()
                    .take(160)
                    .collect();
                let input = TaskInput {
                    common: Common {
                        related: Some(Reference::new(RECORD, document.id.to_string())),
                        title,
                        description: Some(format!(
                            "“{}” expires on {expires_on}. Open it to renew or replace it.",
                            document.name
                        )),
                        ..Common::default()
                    },
                    due_at: Some(due),
                    ..TaskInput::default()
                };
                match self.activities.create_task(tx, scope, &input).await {
                    Ok(task) => activity = Some(task.id),
                    Err(error) => {
                        eprintln!(
                            "warning: the expiry reminder of document {} was not created: {error}",
                            document.id
                        );
                    }
                }
            }
        }
        if activity == previous {
            return Ok(document.clone());
        }
        let (tenant, id) = (scope.tenant(), document.id);
        let before = document.clone();
        tx.mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_one(
                        &format!("UPDATE core_documents d SET expiry_activity_id=$3 WHERE d.tenant_id=$1 AND d.id=$2 RETURNING {}", crate::store::COLUMNS),
                        &[&tenant, &id, &activity],
                    )
                    .await?;
                let after = crate::store::from_row(&row)?;
                let change = Change {
                    kind: RECORD.into(),
                    id,
                    old: crate::store::audit_values(&before),
                    new: crate::store::audit_values(&after),
                };
                Ok((after, vec![change]))
            })
        })
        .await
        .map_err(crate::store::write_error)
    }

    /// Deletes a reminder task when its owner may; a task already gone or not
    /// the actor's to delete is left alone.
    pub(crate) async fn remove_reminder(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
    ) {
        if let Ok(task) = self.activities.get(scope, id).await {
            if self
                .activities
                .may_edit(scope, &task)
                .await
                .unwrap_or(false)
            {
                let _ = self
                    .activities
                    .delete(tx, scope, task.id, task.version)
                    .await;
            }
        }
    }
}
