//! Invitations: Send invites tells account guests in the bell in its own
//! transaction and queues this worker (`core/activity-invites`), which runs as
//! the sender and emails each guest without an account once per Send invites:
//! the address, or the person's email as Core Person lets the sender read it.
//!
//! Before each message every contributed [`InvitationGate`] is asked, in a
//! transaction of its own, whether this person or address may be emailed now
//! (a Domain's consent check). One refusal skips the guest. A guest's
//! `invite_sent_at` is stamped after its message has gone, so a retried job
//! skips it; a crash between the two sends it once more (at least once).
//! README, Invitations.
use crate::model::email_shape;
use crate::store::{self, system};
use crate::{Activity, GuestRow, GUEST_RECORD, MANAGE};
use bilibi_database::{Change, Db, Error, MutationTransaction, Result};
use bilibi_datetime::{DisplayContext, Format, Mode, Precision};
use bilibi_mail::{Address, Mailer, Message};
use bilibi_module_registry::{Consumer, Contribution, Entry, Provenance};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_principal_directory::{PrincipalDirectory, PrincipalKind, PrincipalRef, Query};
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Worker, HANDLERS, WORKERS,
};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{Authentication, ProofClient, ProofFuture, ReadClient, Scope};
use jiff::Timestamp;
use serde_json::{json, Value};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

/// The registry Domains contribute an [`InvitationGate`] to.
pub const INVITATION_GATES: &str = "core/activity.invitation_gates";
pub(crate) const WORKER: &str = "core/activity-invites";
pub(crate) const HANDLER: &str = "core/activity.invites";

/// Who an invitation email would go to: the guest's person, when it is one,
/// and the address it would be sent to.
#[derive(Clone, Debug)]
pub struct Invitee {
    pub person: Option<PersonId>,
    pub email: String,
}

pub type GateFuture<'a> = Pin<Box<dyn Future<Output = Result<bool>> + Send + 'a>>;

/// A Domain's say on emailing an invitee, contributed under
/// [`INVITATION_GATES`] (CX asks its consent register). Core never names the
/// Domain; with no gate contributed, every usable address is emailed.
pub struct InvitationGate {
    /// Whether the sender may ask this gate (holds its capability). Send
    /// invites is refused when anyone is to be emailed and a gate says no.
    pub may_ask: for<'a> fn(&'a dyn ProofClient, &'a Scope) -> ProofFuture<'a, bool>,
    /// Whether `invitee` may be emailed now, asked as the sender inside the
    /// worker's transaction, so the gate can log its answer there. `key` is
    /// the same for every retry of one invitation to one guest, so a gate
    /// that logs can answer a retry from its log.
    pub allow: for<'a> fn(
        &'a mut MutationTransaction<'_>,
        &'a Scope,
        &'a Invitee,
        &'a str,
    ) -> GateFuture<'a>,
}

fn validate(entries: &[Entry]) -> bilibi_module_registry::Result<()> {
    for entry in entries {
        entry.get::<InvitationGate>()?;
    }
    Ok(())
}
inventory::submit! { Consumer::new::<InvitationGate>(INVITATION_GATES, crate::MODULE, validate) }

pub(crate) fn gates() -> impl Iterator<Item = &'static InvitationGate> {
    inventory::iter::<Contribution>
        .into_iter()
        .filter(|entry| entry.consumer == INVITATION_GATES)
        .filter_map(|entry| entry.get::<InvitationGate>().ok())
}

/// Whether the sender may ask every gate.
pub(crate) async fn may_ask(client: &dyn ProofClient, scope: &Scope) -> Result<bool> {
    for gate in gates() {
        if !(gate.may_ask)(client, scope).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

macro_rules! provenance {
    () => {
        Provenance {
            module: crate::MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

/// `{activity_id, invited_at}` and nothing else.
fn arguments(args: Value) -> Result<Value> {
    let object = args.as_object().ok_or(Error::Invalid)?;
    let id = object.get("activity_id").and_then(Value::as_i64);
    let at = object
        .get("invited_at")
        .and_then(Value::as_str)
        .filter(|at| at.len() <= 40)
        .and_then(|at| at.parse::<Timestamp>().ok());
    if object.len() != 2 || id.is_none_or(|id| id <= 0) || at.is_none() {
        return Err(Error::Invalid);
    }
    Ok(args)
}
inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: 5, validate: arguments }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: HANDLER, capability: MANAGE }) }

/// A person's email as the reader may read it: none when restricted or
/// masked for them, retired, or not an address.
pub(crate) async fn person_email(
    client: &dyn ReadClient,
    scope: &Scope,
    person: PersonId,
) -> Option<String> {
    let person = bilibi_person::read_person(client, scope, person)
        .await
        .ok()?;
    if person.retired_at.is_some() || person.masked.iter().any(|f| f == "email") {
        return None;
    }
    person.email.filter(|email| email_shape(email))
}

/// Guest email addresses the reader may use, by guest id: an address
/// guest's own, a person's through Core Person. Account guests are reached in
/// the bell and have none here.
pub(crate) async fn guest_emails(
    client: &dyn ReadClient,
    scope: &Scope,
    guests: &[GuestRow],
) -> HashMap<i64, String> {
    let mut emails = HashMap::new();
    for guest in guests {
        let email = match (guest.user_id, guest.person, &guest.email) {
            (None, Some(person), _) => person_email(client, scope, person).await,
            (None, None, Some(email)) => Some(email.clone()),
            _ => None,
        };
        if let Some(email) = email {
            emails.insert(guest.id, email);
        }
    }
    emails
}

/// The activity's time as its filing company reads it, named, and in UTC:
/// "6 Nov 2026, 10:00 – 11:00 (Asia/Kuala_Lumpur); 02:00 – 03:00 UTC".
pub(crate) async fn when(
    settings: &Settings,
    client: &dyn ReadClient,
    scope: &Scope,
    activity: &Activity,
) -> Result<String> {
    let company = OrganisationId::new(activity.company_id).map_err(|_| Error::Internal)?;
    // A company the sender can no longer prove (archived since) falls back
    // to the default zone rather than stopping the message.
    let zone = match settings
        .get(
            client,
            scope,
            Target::Company(company),
            bilibi_datetime::TIMEZONE_KEY,
        )
        .await
    {
        Ok(resolved) => resolved.value.as_str().map(str::to_owned),
        Err(Error::NotFound | Error::Forbidden) => None,
        Err(error) => return Err(error),
    }
    .filter(|zone| bilibi_datetime::valid_timezone(zone))
    .unwrap_or_else(|| bilibi_datetime::DEFAULT_TIMEZONE.to_owned());
    let display = DisplayContext {
        mode: Mode::Company,
        timezone: zone.clone(),
        locale: bilibi_locale::FALLBACK,
    };
    let show = |at: Timestamp| display.render(at, Format::DateTime, Precision::Minute);
    let Some(start) = activity.starts_at else {
        return Ok(String::new());
    };
    let start = show(start);
    let end = activity.ends_at.map(show);
    let (local, utc) = match &end {
        Some(end) => (
            format!("{} – {}", start.company, end.company),
            format!("{} – {}", start.utc, end.utc),
        ),
        None => (start.company.clone(), start.utc.clone()),
    };
    Ok(if local == utc {
        utc
    } else {
        format!("{local} ({zone}); {utc}")
    })
}

/// At most `max` bytes of `text`, cut on a character boundary.
pub(crate) fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max.saturating_sub(1);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The invitation's subject and text: what, when, where and the join link,
/// the description, and who sent it. Internal notes are never in it, and
/// neither is a link into Bilibi: the guest has no account. A reply goes to
/// the sender (`Reply-To`).
fn invitation(activity: &Activity, sender: &str, when: &str) -> (String, String) {
    let kind = activity.kind.label().to_lowercase();
    let subject = bounded(&format!("Invitation: {}", activity.title), 200);
    let mut body = format!(
        "{sender} invites you to a {kind}: {}\n\nWhen: {when}\n",
        activity.title
    );
    if let Some(location) = &activity.location {
        body.push_str(&format!("Where: {location}\n"));
    }
    if let Some(url) = &activity.join_url {
        body.push_str(&format!("Join: {url}\n"));
    }
    if let Some(description) = &activity.description {
        body.push_str(&format!("\n{description}\n"));
    }
    body.push_str(&format!(
        "\nPlease reply to {sender} to accept or decline.\n"
    ));
    (subject, body)
}

pub(crate) struct InviteJob {
    pub db: Db,
    pub mailer: Arc<Mailer>,
    pub settings: Arc<Settings>,
    pub directory: Arc<PrincipalDirectory>,
    pub authentication: Arc<Authentication>,
}
impl Handler for InviteJob {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            match self.run(execution, &args).await {
                Ok(true) => Outcome::Completed,
                Ok(false) => Outcome::Retry("invite_delivery_failed"),
                // The sender may no longer see the activity, or is gone.
                Err(Error::Forbidden | Error::NotFound | Error::Unauthorized) => {
                    Outcome::Cancel("invite_refused")
                }
                Err(error) => {
                    eprintln!("error: activity invitations: {error}");
                    Outcome::Retry("invite_failed")
                }
            }
        })
    }
}
impl InviteJob {
    /// `Ok(false)` when a message could not be delivered and the job should
    /// try the guests still unsent again.
    async fn run(&self, execution: &Execution, args: &Value) -> Result<bool> {
        let scope = execution.scope().ok_or(Error::Unauthorized)?.clone();
        let id = args["activity_id"].as_i64().ok_or(Error::Invalid)?;
        let invited_at: Timestamp = args["invited_at"]
            .as_str()
            .and_then(|at| at.parse().ok())
            .ok_or(Error::Invalid)?;
        let mut client = self.db.get().await?;
        // Read as the sender: an activity they can no longer see is not sent.
        let activity = store::get(&**client, &scope, id).await?;
        if activity.is_closed() {
            return Ok(true);
        }
        let targets = client
            .query(
                "SELECT id,guest_person_id,guest_email FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 AND invited_at=$3 AND guest_user_id IS NULL AND (invite_sent_at IS NULL OR invite_sent_at < invited_at) ORDER BY id",
                &[&scope.tenant(), &id, &system(Some(invited_at))],
            )
            .await?;
        if targets.is_empty() {
            return Ok(true);
        }
        let when = when(&self.settings, &**client, &scope, &activity).await?;
        let sender = self.sender(&**client, &scope).await?;
        // Replies go to the sender's own address, when their account has a
        // usable one; otherwise to the installation sender.
        let reply_to = self
            .authentication
            .authenticate(&**client, scope.tenant(), scope.user(), String::new())
            .await
            .ok()
            .and_then(|account| Address::new(Some(&sender), &account.email).ok());
        let (subject, body) = invitation(&activity, &sender, &when);
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
            // Each gate answers in its own transaction under a key a retry of
            // this invitation reuses.
            let key = format!(
                "core.activity.invite:{guest}:{}",
                invited_at.as_microsecond()
            );
            if !allowed(execution, &mut client, &scope, &invitee, &key, guest).await? {
                continue;
            }
            let Ok(to) = Address::new(None, &email) else {
                continue;
            };
            let mut message = Message::text(to, subject.clone(), body.clone());
            message.reply_to = reply_to.clone();
            if self.mailer.deliver(&message).await.is_err() {
                // The transport's detail may name the address; the job's
                // failure code is enough for the operator's board.
                eprintln!(
                    "error: core/activity could not deliver an invitation (job {})",
                    execution.job_id
                );
                delivered = false;
                continue;
            }
            let mut tx = execution.begin(&mut client, "core.activity.invite").await?;
            let tenant = scope.tenant();
            tx.mutate(move |sql| {
                Box::pin(async move {
                    let rows = sql
                        .query(
                            "UPDATE core_activity_guests SET invite_sent_at=clock_timestamp() WHERE tenant_id=$1 AND activity_id=$2 AND id=$3 AND invited_at=$4 RETURNING id",
                            &[&tenant, &id, &guest, &system(Some(invited_at))],
                        )
                        .await?;
                    let changes = rows
                        .iter()
                        .map(|row| Change {
                            kind: GUEST_RECORD.into(),
                            id: row.get(0),
                            old: json!({"activity_id": id, "invite_sent": false}),
                            new: json!({"activity_id": id, "invite_sent": true, "invited_at": invited_at.to_string()}),
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
    async fn sender(&self, client: &dyn ReadClient, scope: &Scope) -> Result<String> {
        sender_label(&self.directory, client, scope).await
    }
}

/// How the acting account is named to a guest, as the principal directory
/// ranks it for them.
pub(crate) async fn sender_label(
    directory: &PrincipalDirectory,
    client: &dyn ReadClient,
    scope: &Scope,
) -> Result<String> {
    let me = PrincipalRef::new(PrincipalKind::User, scope.user()).map_err(|_| Error::Internal)?;
    let ranked = directory
        .rank(&store::Directory(client), scope, &[me], &Query::default())
        .await
        .map_err(|_| Error::Internal)?;
    Ok(ranked.label(me))
}

/// Asks every contributed gate, each in a transaction of its own under `key`,
/// whether `invitee` may be emailed now. One refusal says no. A question a
/// gate will not answer (an address it cannot take, a sender it refuses) is
/// a no, said in the log without the address; any other error stops the job
/// so it is tried again.
pub(crate) async fn allowed(
    execution: &Execution,
    client: &mut tokio_postgres::Client,
    scope: &Scope,
    invitee: &Invitee,
    key: &str,
    guest: i64,
) -> Result<bool> {
    for gate in gates() {
        let mut tx = execution.begin(client, "core.activity.invite").await?;
        match (gate.allow)(&mut tx, scope, invitee, key).await {
            Ok(true) => tx.commit().await?,
            Ok(false) => {
                tx.commit().await?;
                return Ok(false);
            }
            Err(error @ (Error::Invalid | Error::Forbidden | Error::NotFound)) => {
                tx.rollback().await?;
                eprintln!(
                    "error: an invitation gate did not answer (job {}, guest {guest}): {error}",
                    execution.job_id
                );
                return Ok(false);
            }
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        }
    }
    Ok(true)
}
