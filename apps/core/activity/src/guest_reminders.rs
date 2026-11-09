//! Guest email reminders: the sweep (`reminders.rs`) queues this worker
//! (`core/activity-guest-reminders`) as the activity's assignee when a call or
//! meeting with email reminders on is due one. It emails each guest without
//! an account who was sent the invitation and has not declined, once per
//! reminder.
//!
//! The mail is transactional: a reminder about a meeting the guest was
//! invited to, never marketing, so Core asks no consent of its own. Every
//! contributed [`InvitationGate`](crate::InvitationGate) is asked first, as
//! it is for the invitation, so a Domain's contact rules (a suppression, a
//! consent) can still refuse a guest. `core_activity_guest_reminders` holds
//! one row per guest and reminder sent, so a retried job emails nobody twice.
//! README, Reminders.
use crate::invitations::{allowed, bounded, gates, person_email, sender_label, when, Invitee};
use crate::store::{self, system};
use crate::{Activity, GUEST_RECORD, MANAGE};
use bilibi_database::{Change, Db, Error, Result};
use bilibi_mail::{Address, Mailer, Message};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_party_ids::PersonId;
use bilibi_principal_directory::PrincipalDirectory;
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Worker, HANDLERS, WORKERS,
};
use bilibi_settings::Settings;
use bilibi_tenancy::Authentication;
use jiff::Timestamp;
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) const WORKER: &str = "core/activity-guest-reminders";
pub(crate) const HANDLER: &str = "core/activity.guest_reminders";

macro_rules! provenance {
    () => {
        Provenance {
            module: crate::MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

/// `{activity_id, at, minutes}` and nothing else: the activity, the time it
/// starts or is due, and the offset the reminder is for.
fn arguments(args: Value) -> Result<Value> {
    let object = args.as_object().ok_or(Error::Invalid)?;
    let id = object.get("activity_id").and_then(Value::as_i64);
    let at = object
        .get("at")
        .and_then(Value::as_str)
        .filter(|at| at.len() <= 40)
        .and_then(|at| at.parse::<Timestamp>().ok());
    let minutes = object.get("minutes").and_then(Value::as_i64);
    if object.len() != 3 || id.is_none_or(|id| id <= 0) || at.is_none() || minutes.is_none() {
        return Err(Error::Invalid);
    }
    Ok(args)
}
inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: 5, validate: arguments }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: HANDLER, capability: MANAGE }) }

/// The reminder's subject and text: what, when and where, the join link and
/// who invited them. No link into Bilibi: the guest has no account.
fn reminder(activity: &Activity, sender: &str, when: &str) -> (String, String) {
    let kind = activity.kind.label().to_lowercase();
    let subject = bounded(&format!("Reminder: {}", activity.title), 200);
    let mut body = format!(
        "A reminder of the {kind} {sender} invited you to: {}\n\nWhen: {when}\n",
        activity.title
    );
    if let Some(location) = &activity.location {
        body.push_str(&format!("Where: {location}\n"));
    }
    if let Some(url) = &activity.join_url {
        body.push_str(&format!("Join: {url}\n"));
    }
    body.push_str(&format!(
        "\nIf you can no longer come, please reply to {sender}.\n"
    ));
    (subject, body)
}

pub(crate) struct GuestReminderJob {
    pub db: Db,
    pub mailer: Arc<Mailer>,
    pub settings: Arc<Settings>,
    pub directory: Arc<PrincipalDirectory>,
    pub authentication: Arc<Authentication>,
}
impl Handler for GuestReminderJob {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            match self.run(execution, &args).await {
                Ok(true) => Outcome::Completed,
                Ok(false) => Outcome::Retry("guest_reminder_delivery_failed"),
                // The assignee may no longer see or change the activity.
                Err(Error::Forbidden | Error::NotFound | Error::Unauthorized) => {
                    Outcome::Cancel("guest_reminder_refused")
                }
                Err(error) => {
                    eprintln!("error: activity guest reminders: {error}");
                    Outcome::Retry("guest_reminder_failed")
                }
            }
        })
    }
}
impl GuestReminderJob {
    /// `Ok(false)` when a message could not be delivered and the job should
    /// try the guests still unreminded again.
    async fn run(&self, execution: &Execution, args: &Value) -> Result<bool> {
        let scope = execution.scope().ok_or(Error::Unauthorized)?.clone();
        let id = args["activity_id"].as_i64().ok_or(Error::Invalid)?;
        let at: Timestamp = args["at"]
            .as_str()
            .and_then(|at| at.parse().ok())
            .ok_or(Error::Invalid)?;
        let minutes = args["minutes"]
            .as_i64()
            .and_then(|m| i32::try_from(m).ok())
            .ok_or(Error::Invalid)?;
        let mut client = self.db.get().await?;
        let activity = store::get(&**client, &scope, id).await?;
        // A closed activity, or one moved since the reminder was due, is not
        // reminded of: the sweep reminds again at the new time.
        if activity.is_closed()
            || !activity.kind.timed()
            || activity.starts_at.map(|t| t.as_microsecond()) != Some(at.as_microsecond())
        {
            return Ok(true);
        }
        // The sender must be allowed to ask every gate, as for invitations.
        if !crate::invitations::may_ask(&**client, &scope).await? {
            eprintln!(
                "error: activity guest reminders: the assignee may not ask an invitation gate (job {})",
                execution.job_id
            );
            return Ok(true);
        }
        let targets = client
            .query(
                "SELECT g.id,g.guest_person_id,g.guest_email FROM core_activity_guests g
                 WHERE g.tenant_id=$1 AND g.activity_id=$2 AND g.guest_user_id IS NULL AND g.response<>'declined'
                   AND g.invite_sent_at IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM core_activity_guest_reminders r WHERE r.tenant_id=g.tenant_id AND r.guest_id=g.id AND r.at=$3 AND r.minutes=$4)
                 ORDER BY g.id",
                &[&scope.tenant(), &id, &system(Some(at)), &minutes],
            )
            .await?;
        if targets.is_empty() {
            return Ok(true);
        }
        let when = when(&self.settings, &**client, &scope, &activity).await?;
        let sender = sender_label(&self.directory, &**client, &scope).await?;
        let reply_to = self
            .authentication
            .authenticate(&**client, scope.tenant(), scope.user(), String::new())
            .await
            .ok()
            .and_then(|account| Address::new(Some(&sender), &account.email).ok());
        let (subject, body) = reminder(&activity, &sender, &when);
        let mut delivered = true;
        for row in &targets {
            let guest: i64 = row.get(0);
            let person = row
                .get::<_, Option<i64>>(1)
                .and_then(|p| PersonId::new(p).ok());
            let address: Option<String> = row.get(2);
            let email = match (person, address) {
                (Some(person), _) => person_email(&**client, &scope, person).await,
                (None, address) => address,
            };
            let Some(email) = email else {
                continue;
            };
            let invitee = Invitee {
                person,
                email: email.clone(),
            };
            let key = format!(
                "core.activity.reminder:{guest}:{}:{minutes}",
                at.as_microsecond()
            );
            if gates().next().is_some()
                && !allowed(execution, &mut client, &scope, &invitee, &key, guest).await?
            {
                continue;
            }
            let Ok(to) = Address::new(None, &email) else {
                continue;
            };
            let mut message = Message::text(to, subject.clone(), body.clone());
            message.reply_to = reply_to.clone();
            if self.mailer.deliver(&message).await.is_err() {
                eprintln!(
                    "error: core/activity could not deliver a guest reminder (job {})",
                    execution.job_id
                );
                delivered = false;
                continue;
            }
            let mut tx = execution
                .begin(&mut client, "core.activity.guest_reminder")
                .await?;
            let tenant = scope.tenant();
            tx.mutate(move |sql| {
                Box::pin(async move {
                    let rows = sql
                        .query(
                            "INSERT INTO core_activity_guest_reminders(tenant_id,guest_id,activity_id,at,minutes) VALUES ($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING RETURNING guest_id",
                            &[&tenant, &guest, &id, &system(Some(at)), &minutes],
                        )
                        .await?;
                    let changes = rows
                        .iter()
                        .map(|row| Change {
                            kind: GUEST_RECORD.into(),
                            id: row.get(0),
                            old: json!({"activity_id": id, "reminder_sent": false}),
                            new: json!({"activity_id": id, "reminder_sent": true, "at": at.to_string(), "minutes": minutes}),
                        })
                        .collect();
                    Ok(((), changes))
                })
            })
            .await?;
            tx.commit().await?;
        }
        Ok(delivered)
    }
}
