//! Reminders: a per-tenant scheduled sweep (ADR 17) marks each open task,
//! call or meeting whose time has come as reminded, rings its assignee's
//! bell (and a call's or meeting's account guests' who have not declined)
//! and, when the tenant asks, emails the assignee. The bell's To do lists the
//! reader's reminded open activities until they close them; a row opens the
//! related record. README, Reminders.
use crate::contributions::{REMINDER_EMAIL, REMINDER_MINUTES};
use crate::{Kind, Reference, RECORD, VIEW};
use bilibi_authz::holds_proof;
use bilibi_database::{Change, Db, Error, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_notifications::{NotificationBoundary, TodoFuture, TodoItem, TodoList, TodoSource};
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Worker, HANDLERS, WORKERS,
};
use bilibi_records::Records;
use bilibi_schedule::{
    worker, Definition, Misfire, Overlap, Reach, Schedule, DEFINITIONS, EXECUTE,
};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{Authentication, Scope};
use serde_json::{json, Value};
use std::{sync::Arc, time::SystemTime};

pub(crate) const HANDLER: &str = "core/activity.reminders";
const WORKER: &str = "core/activity-reminders";
/// Reminders per transaction, and transactions per run: a run reminds at
/// most `BATCH * BATCHES`; the next minute's run takes the rest.
const BATCH: i64 = 200;
const BATCHES: usize = 10;
/// Rows the bell shows, and the most it counts before saying `N+`.
const TODO_ROWS: i64 = 5;
const TODO_COUNT: i64 = 100;

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
inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: 3, validate: |args| worker::validate(args, no_arguments) }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: HANDLER, capability: EXECUTE }) }
inventory::submit! { Contribution::new(bilibi_queue::CAPABILITY_POLICIES, WORKER, provenance!(), &bilibi_schedule::TENANT_POLICY) }
inventory::submit! { Contribution::new(DEFINITIONS, "core/activity.reminders", provenance!(), &Definition {
    name: "Remind assignees of due activities", expression: "* * * * *", timezone: "Etc/UTC",
    task_name: "Activity reminders", worker: WORKER, args: || json!({}),
    overlap: Overlap::Forbid, misfire: Misfire::Coalesce, reach: Reach::EachTenant, owner_route: None,
}) }

/// One activity the sweep reminded someone of.
struct Reminded {
    id: i64,
    assignee: i64,
    kind: Kind,
    title: String,
    due: Option<jiff::Timestamp>,
}

pub(crate) struct Sweep {
    pub schedule: Arc<Schedule>,
    pub db: Db,
    pub authentication: Arc<Authentication>,
    pub notifications: Arc<NotificationBoundary>,
    pub settings: Arc<Settings>,
    pub mailer: Arc<bilibi_mail::Mailer>,
    /// The public origin emailed links point at (`APP_ORIGIN`).
    pub origin: String,
}
impl Handler for Sweep {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(self.schedule.run(execution, args, |_| async {
            match self.sweep(execution).await {
                Ok(()) => Outcome::Completed,
                Err(error) => {
                    eprintln!("error: activity reminders: {error}");
                    Outcome::Retry("activity_reminders_unavailable")
                }
            }
        }))
    }
}
impl Sweep {
    /// Finds what is due now in the job's tenant (level-triggered: a missed
    /// minute is caught by the next run) and reminds each assignee once.
    async fn sweep(&self, execution: &Execution) -> Result<()> {
        let scope = execution.scope().ok_or(Error::Internal)?;
        let (minutes, email) = {
            let client = self.db.get().await?;
            let minutes = self
                .settings
                .get(&**client, scope, Target::Tenant, REMINDER_MINUTES)
                .await?
                .value
                .as_i64()
                .unwrap_or(15);
            let email = self
                .settings
                .get(&**client, scope, Target::Tenant, REMINDER_EMAIL)
                .await?
                .value
                .as_bool()
                .unwrap_or(false);
            (minutes, email)
        };
        let mut reminded = Vec::new();
        for _ in 0..BATCHES {
            let batch = self.batch(execution, minutes).await?;
            let full = batch.len() as i64 == BATCH;
            reminded.extend(batch);
            if !full {
                break;
            }
        }
        // Mail goes after the reminders commit, never inside the
        // transaction. The bell already holds each one; a refused message is
        // logged and not sent again.
        if email {
            for reminder in &reminded {
                self.mail(execution.tenant(), reminder).await;
            }
        }
        Ok(())
    }

    /// One transaction: marks up to `BATCH` due activities reminded and
    /// wakes their assignees' bells after commit.
    async fn batch(&self, execution: &Execution, minutes: i64) -> Result<Vec<Reminded>> {
        let tenant = execution.tenant();
        let mut client = self.db.get().await?;
        let mut tx = execution
            .begin(&mut client, "core.activity.reminders")
            .await?;
        let lead = jiff::SignedDuration::from_mins(minutes);
        let horizon = SystemTime::from(jiff::Timestamp::now() + lead);
        // Lock what is due first: a write with no change is refused, so a
        // run with nothing due writes nothing.
        let due: Vec<i64> = tx
            .sql()
            .query(
                "SELECT b.id FROM core_activities b
                 WHERE b.tenant_id=$1 AND b.deleted_at IS NULL AND b.closed_at IS NULL AND b.reminded_at IS NULL AND b.kind <> 'note'
                   AND COALESCE(b.starts_at, b.due_at) <= $2
                 ORDER BY COALESCE(b.starts_at, b.due_at), b.id
                 LIMIT $3 FOR UPDATE SKIP LOCKED",
                &[&tenant, &horizon, &BATCH],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        if due.is_empty() {
            tx.rollback().await?;
            return Ok(Vec::new());
        }
        let reminded = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let rows = sql
                        .query(
                            "UPDATE core_activities a SET reminded_at=now()
                             WHERE a.tenant_id=$1 AND a.id=ANY($2) AND a.reminded_at IS NULL
                             RETURNING a.id, a.assignee_user_id, a.kind, a.subject, COALESCE(a.starts_at, a.due_at), a.reminded_at",
                            &[&tenant, &due],
                        )
                        .await?;
                    let mut reminded = Vec::with_capacity(rows.len());
                    let mut changes = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let kind = Kind::parse(row.get::<_, &str>(2)).ok_or(Error::Internal)?;
                        let due = row
                            .get::<_, Option<SystemTime>>(4)
                            .and_then(|t| jiff::Timestamp::try_from(t).ok());
                        let at = jiff::Timestamp::try_from(row.get::<_, SystemTime>(5))
                            .map_err(|_| Error::Internal)?;
                        changes.push(Change {
                            kind: RECORD.into(),
                            id: row.get(0),
                            old: json!({"reminded_at": null}),
                            new: json!({"reminded_at": at.to_string()}),
                        });
                        reminded.push(Reminded {
                            id: row.get(0),
                            assignee: row.get(1),
                            kind,
                            title: row.get(3),
                            due,
                        });
                    }
                    Ok((reminded, changes))
                })
            })
            .await?;
        // The assignees, and the account guests of each call and meeting who
        // have not declined: the same people its To do row shows for.
        let mut people: Vec<i64> = reminded.iter().map(|r| r.assignee).collect();
        let timed: Vec<i64> = reminded
            .iter()
            .filter(|r| r.kind.timed())
            .map(|r| r.id)
            .collect();
        if !timed.is_empty() {
            let rows = tx
                .sql()
                .query(
                    "SELECT DISTINCT guest_user_id FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=ANY($2) AND guest_user_id IS NOT NULL AND response<>'declined'",
                    &[&tenant, &timed],
                )
                .await?;
            people.extend(rows.iter().map(|row| row.get::<_, i64>(0)));
        }
        self.notifications.todos_changed_for(&mut tx, &people)?;
        tx.commit().await?;
        Ok(reminded)
    }

    /// Emails one reminder to its assignee: the kind, title and when, and a
    /// link to the activity. Notes and the description stay in Bilibi.
    async fn mail(&self, tenant: i64, reminder: &Reminded) {
        let assignee = match self.db.get().await {
            Ok(client) => self
                .authentication
                .authenticate(&**client, tenant, reminder.assignee, String::new())
                .await
                .ok(),
            Err(_) => None,
        };
        // An assignee who can no longer sign in is not mailed.
        let Some(assignee) = assignee else {
            return;
        };
        let Ok(address) = bilibi_mail::Address::new(Some(&assignee.user_name), &assignee.email)
        else {
            return;
        };
        let title: String = reminder.title.chars().take(150).collect();
        let when = reminder
            .due
            .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        let what = what(reminder.kind);
        let message = bilibi_mail::Message::text(
            address,
            format!("Reminder: {title}"),
            format!(
                "{what}: {title}\n{}\n\nOpen it, signed in:\n{}/activities/{}\n",
                if reminder.kind == Kind::Task {
                    format!("Due {when}")
                } else {
                    format!("Starts {when}")
                },
                self.origin,
                reminder.id
            ),
        );
        if self.mailer.deliver(&message).await.is_err() {
            eprintln!(
                "error: activity reminder {} could not be emailed",
                reminder.id
            );
        }
    }
}

fn what(kind: Kind) -> &'static str {
    match kind {
        Kind::Task => "Task due",
        Kind::Call => "Call",
        Kind::Meeting => "Meeting",
        Kind::Note => "Note",
    }
}

/// The bell's To do: the reader's reminded open activities, soonest first:
/// those assigned to them, and calls and meetings they are an account guest
/// of and have not declined. A row opens the related record when the reader
/// may view it, else the activity.
pub(crate) struct Todos {
    pub db: Db,
    pub authentication: Arc<Authentication>,
    pub records: Arc<Records>,
}
impl TodoSource for Todos {
    fn read<'a>(&'a self, scope: &'a Scope) -> TodoFuture<'a> {
        Box::pin(async move {
            let (rows, count) = {
                let client = self.db.get().await?;
                let scope = self.authentication.reprove(&**client, scope).await?;
                if !holds_proof(&**client, &scope, VIEW).await? {
                    return Ok(TodoList::default());
                }
                const DUE: &str = "a.tenant_id=$1 AND a.deleted_at IS NULL AND a.closed_at IS NULL AND a.reminded_at IS NOT NULL
                    AND (a.assignee_user_id=$2 OR (a.kind IN ('call','meeting') AND EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_user_id=$2 AND g.response<>'declined')))";
                let rows = client
                    .query(
                        &format!("SELECT a.id, a.kind, a.subject, a.related_kind, a.related_id FROM core_activities a WHERE {DUE} ORDER BY COALESCE(a.starts_at, a.due_at) NULLS LAST, a.id LIMIT $3"),
                        &[&scope.tenant(), &scope.user(), &TODO_ROWS],
                    )
                    .await?;
                let count: i64 = client
                    .query_one(
                        &format!("SELECT count(*) FROM (SELECT 1 FROM core_activities a WHERE {DUE} LIMIT $3) due"),
                        &[&scope.tenant(), &scope.user(), &(TODO_COUNT + 1)],
                    )
                    .await?
                    .get(0);
                (rows, count)
            };
            let related: Vec<Reference> = rows
                .iter()
                .filter_map(|row| {
                    Some(Reference::new(
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })
                .collect();
            let named = self.records.resolve(scope, &related).await?;
            let mut named = named.into_iter();
            let mut items = Vec::with_capacity(rows.len());
            for row in &rows {
                let id: i64 = row.get(0);
                let kind = Kind::parse(row.get::<_, &str>(1)).ok_or(Error::Internal)?;
                let has_related = row.get::<_, Option<String>>(3).is_some();
                let path = if has_related {
                    named
                        .next()
                        .filter(|r| r.label.is_some())
                        .and_then(|r| r.path)
                } else {
                    None
                };
                items.push(TodoItem {
                    title: format!("{}: {}", what(kind), row.get::<_, String>(2)),
                    url: path.unwrap_or_else(|| format!("/activities/{id}")),
                });
            }
            Ok(TodoList {
                items,
                count: count.min(TODO_COUNT) as usize,
                more: count > TODO_COUNT,
                see_all: Some("/activities".into()),
            })
        })
    }
}
