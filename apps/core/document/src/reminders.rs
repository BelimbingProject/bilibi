//! The expiry reminder: one Core Activity task per document that has an
//! expiry date, due the tenant's lead time before it
//! (`core.document.expiry_reminder_days`). Core Activity's own sweep rings the
//! assignee's bell and optionally emails; there is no second reminder system.
//! The task is private to the person who set the date (`expiry_user_id`), so it
//! is made only when that person may create activities.
//!
//! The task names no document and no date: its title is "Document <id>
//! expiry", so a bell or an email it sends discloses nothing a reader who has
//! lost access could not already have, and its related-record pill names the
//! document only to someone who may still view it. Whoever changes or clears
//! the date, deletes or merges the document, or removes the reminder person's
//! access removes the task through Core Activity's owner-scoped
//! `delete_related`, in the same transaction and whoever created it; a failure
//! to remove it fails the change.
use crate::store::Store;
use crate::{Document, WriteError, WriteResult, RECORD};
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
    /// the new task's id and owner. Returns the document as stored.
    pub(crate) async fn refresh_reminder(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        document: &Document,
        previous: Option<i64>,
    ) -> WriteResult<Document> {
        if let Some(id) = previous {
            self.remove_reminder(tx, scope, document.id, id).await?;
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
                let input = TaskInput {
                    common: Common {
                        related: Some(Reference::new(RECORD, document.id.to_string())),
                        title: format!("Document {} expiry", document.id),
                        description: Some(
                            "Open the related document to renew or replace it.".to_owned(),
                        ),
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
        let user = activity.map(|_| scope.user());
        if activity == document.expiry_activity_id && user == document.expiry_user_id {
            return Ok(document.clone());
        }
        self.store_reminder(tx, scope, document, activity, user)
            .await
    }

    /// Records the reminder task and its owner on the document, audited.
    async fn store_reminder(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        document: &Document,
        activity: Option<i64>,
        user: Option<i64>,
    ) -> WriteResult<Document> {
        let (tenant, id) = (scope.tenant(), document.id);
        let before = document.clone();
        tx.mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_one(
                        &format!("UPDATE core_documents d SET expiry_activity_id=$3,expiry_user_id=$4 WHERE d.tenant_id=$1 AND d.id=$2 RETURNING {}", crate::store::COLUMNS),
                        &[&tenant, &id, &activity, &user],
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

    /// Removes task `task` of `document`, whoever owns it.
    pub(crate) async fn remove_reminder(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        document: i64,
        task: i64,
    ) -> WriteResult<()> {
        self.activities
            .delete_related(tx, scope, task, &Reference::new(RECORD, document.to_string()))
            .await
            .map_err(|error| match error {
                bilibi_activity::WriteError::Stale => WriteError::Stale,
                bilibi_activity::WriteError::Access(error) => WriteError::Access(error),
                error => {
                    eprintln!("error: the expiry reminder of document {document} could not be removed: {error}");
                    WriteError::Access(Error::Internal)
                }
            })
    }

    /// Removes the document's reminder when `user` is the person it belongs
    /// to, before that person's access to the document goes: a task that
    /// outlives its owner's access would still ring their bell.
    pub(crate) async fn drop_reminder_of(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        document: &Document,
        user: i64,
    ) -> WriteResult<()> {
        if document.expiry_user_id != Some(user) {
            return Ok(());
        }
        if let Some(task) = document.expiry_activity_id {
            self.remove_reminder(tx, scope, document.id, task).await?;
        }
        self.store_reminder(tx, scope, document, None, None).await?;
        Ok(())
    }
}
