//! Public actions: a POST from the public site's own pages that makes a
//! record with no session behind it. Before the owner is called, this module
//! proves a signed one-time ticket the page showed (a replay answers with the
//! first receipt), the trap field a person never fills, the time a person
//! takes to fill the form, and a place in the subject's hour, counted in
//! PostgreSQL under the subject's lock so every server shares it. A request
//! caught as spam never reaches the owner: it gets a receipt whose page reads
//! as accepted, so a robot learns nothing.
//!
//! A receipt keeps a ticket's digest, the outcome and the words a refusal
//! shows, never a typed value. Abuse counts (trap, too fast, busy, expired)
//! are kept per subject and hour, written at most once a minute per subject
//! by each server.
use crate::{contributions::PublicAction, Local, Public, PublicScope};
use bilibi_database::{Change, Context, Error, Result};
use bilibi_ui::{Field, Html, InputKind};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

type Mac256 = Hmac<Sha256>;
/// The form the ticket's fields post under: `public[ticket]`, `public[check]`.
const FORM: &str = "public";
const TICKET: &str = "ticket";
/// The trap field: a person never sees or fills it.
const TRAP: &str = "check";
/// A ticket older than this is refused: the page was open too long.
pub const TICKET_SECONDS: i64 = 24 * 3600;
/// How long the page shows a receipt's result.
const RECEIPT_HOURS: i32 = 24;
/// Receipts are kept this long, then removed by later actions in batches.
const KEEP_DAYS: i32 = 7;
/// Abuse counts are kept this long, then removed by later writes in batches.
const COUNT_DAYS: i32 = 30;
const PRUNE_BATCH: i64 = 100;
/// The longest subject a page may name.
pub const SUBJECT_MAX: usize = 100;
/// Posted fields one action reads at most; more are refused.
const FIELDS_MAX: usize = 100;
/// Who audit names for a receipt or a count: this module, for a visitor.
const PRINCIPAL: &str = "public-site";

pub type ActionFuture<'a, Error> =
    Pin<Box<dyn Future<Output = std::result::Result<Acted, Error>> + Send + 'a>>;

/// A checked submission: the posted fields (without the ticket and the trap
/// field), the subject the page's ticket named, and the site.
pub struct ActionRequest<'a> {
    pub operation: &'static str,
    pub scope: &'a PublicScope,
    /// What the page named when it asked for the ticket (a form and its
    /// version): the owner's own key, which the hourly places count by.
    pub subject: &'a str,
    pub path: &'a HashMap<String, String>,
    /// Each posted name's first value.
    pub fields: &'a HashMap<String, String>,
}

/// The owner's answer, which settles the receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Acted {
    Accepted,
    /// Words the visitor sees on the form, about what they typed (at most
    /// 500 characters). Anything that would tell a stranger about the
    /// tenant's records answers `Accepted` instead.
    Refused(String),
}

/// The owner of an action: called only with a checked request, writing as
/// its own system principal in `request.scope.tenant()`.
pub trait Action<Error>: Send + Sync {
    fn act<'a>(&'a self, request: ActionRequest<'a>) -> ActionFuture<'a, Error>;
}
/// Registry service type (the host uses Database Error): contribute its
/// service ID in `PublicAction.service`.
pub struct ActionService<Error>(pub Arc<dyn Action<Error>>);

/// What a receipt's page says, for a day after it was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Receipt {
    /// The owner took it, it is being taken, or it was caught as spam.
    Accepted,
    /// The owner's words about what was typed, or `None` for the page's own
    /// "could not be sent; try again" words.
    Refused(Option<String>),
}

/// A page's one-time ticket for one action and subject. Put [`Ticket::fields`]
/// inside the form that posts to the action.
pub struct Ticket {
    value: String,
    trap: bool,
}
impl Ticket {
    /// The hidden ticket field and, when the action asks for one, the trap
    /// field (visually hidden, out of the tab order, never autofilled).
    pub fn fields(&self) -> Result<Vec<Html>> {
        let render = |field: Field| Html::render(&field).map_err(|_| Error::Internal);
        let mut fields = vec![render(
            Field::in_form(FORM, TICKET)
                .id("public-ticket")
                .kind(InputKind::Hidden)
                .value(&self.value),
        )?];
        if self.trap {
            fields.push(render(
                Field::in_form(FORM, TRAP)
                    .id("public-check")
                    .label("Leave this field empty")
                    .wrapper_class("hidden")
                    .autocomplete("off")
                    .attr("tabindex", "-1"),
            )?);
        }
        Ok(fields)
    }
}

/// Abuse counts one server holds for a subject's hour until it writes them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub trap: i32,
    pub too_fast: i32,
    pub busy: i32,
    pub expired: i32,
}
impl Tally {
    fn empty(&self) -> bool {
        *self == Tally::default()
    }
}
#[derive(Clone, Copy)]
enum Abuse {
    Trap,
    TooFast,
    Busy,
    Expired,
}
pub(crate) type Tallies = HashMap<(i64, String, String), (i64, Tally, i64)>;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(text: &str) -> Vec<u8> {
    Sha256::digest(text.as_bytes()).to_vec()
}
fn mac(key: &[u8], message: &str) -> Result<String> {
    let mut mac = Mac256::new_from_slice(key).map_err(|_| Error::Internal)?;
    mac.update(message.as_bytes());
    Ok(hex(&mac.finalize().into_bytes()))
}
/// A subject a page may name: printable ASCII letters, digits and `:._-`.
fn subject_shaped(subject: &str) -> bool {
    (1..=SUBJECT_MAX).contains(&subject.len())
        && subject
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b":._-".contains(&b))
}
/// A ticket's subject and issue time, when its signature holds for `action`.
fn read_ticket(key: &[u8], action: &str, ticket: &str) -> Option<(String, i64)> {
    let mut parts = ticket.split('~');
    let (version, subject, issued, nonce, signed) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if parts.next().is_some()
        || version != "v1"
        || !subject_shaped(subject)
        || nonce.len() != 32
        || signed.len() != 64
    {
        return None;
    }
    let issued: i64 = issued.parse().ok()?;
    let mut check = Mac256::new_from_slice(key).ok()?;
    check.update(format!("{action}~{subject}~{issued}~{nonce}").as_bytes());
    let given: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(signed.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    check.verify_slice(&given).ok()?;
    Some((subject.to_owned(), issued))
}

/// The action's own path with a query: where the visitor is sent back.
fn back(path: &str, query: &str) -> Result<Local> {
    Local::parse(&format!("{path}?{query}")).ok_or(Error::Invalid)
}

impl Public {
    async fn ticket_key(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        tenant: i64,
    ) -> Result<Vec<u8>> {
        client
            .query_opt(
                "SELECT ticket_key FROM public_sites WHERE tenant_id=$1",
                &[&tenant],
            )
            .await?
            .map(|row| row.get(0))
            .ok_or(Error::NotFound)
    }

    /// A fresh one-time ticket for the page of `action` (the contribution ID
    /// of a `PublicAction`) about `subject`: what the owner's places count by
    /// and what its request names. The page renders [`Ticket::fields`] in
    /// its form; nothing is written.
    pub async fn ticket(&self, scope: &PublicScope, action: &str, subject: &str) -> Result<Ticket> {
        if !subject_shaped(subject) {
            return Err(Error::Invalid);
        }
        let checks = self
            .actions
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(action)
            .copied()
            .ok_or(Error::NotFound)?;
        let client = self.db.get().await?;
        let key = self.ticket_key(&**client, scope.tenant).await?;
        let issued = now();
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| Error::Internal)?;
        let nonce = hex(&nonce);
        let signed = mac(&key, &format!("{action}~{subject}~{issued}~{nonce}"))?;
        Ok(Ticket {
            value: format!("v1~{subject}~{issued}~{nonce}~{signed}"),
            trap: checks.trap_field,
        })
    }

    /// What the page of `action` says about the receipt `token`, for a day:
    /// `None` for an unknown or older one, or another tenant's.
    pub async fn receipt(
        &self,
        scope: &PublicScope,
        action: &str,
        token: &str,
    ) -> Result<Option<Receipt>> {
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let client = self.db.get().await?;
        let row = client
            .query_opt(
                "SELECT outcome,message FROM public_receipts WHERE tenant_id=$1 AND action=$2 AND receipt_hash=$3 AND created_at>now()-make_interval(hours => $4)",
                &[&scope.tenant, &action, &digest(token), &RECEIPT_HOURS],
            )
            .await?;
        Ok(row.map(|row| match row.get::<_, &str>(0) {
            "refused" => Receipt::Refused(row.get(1)),
            _ => Receipt::Accepted,
        }))
    }

    /// The abuse counts of `action` and `subject` over the last `hours`
    /// hours in `tenant`, for the owner's staff page, which has already
    /// authorized its reader. What a server has not written yet is missing.
    pub async fn tally(
        &self,
        client: &dyn bilibi_tenancy::ReadClient,
        tenant: i64,
        action: &str,
        subject: &str,
        hours: i32,
    ) -> Result<Tally> {
        let rows = client
            .query(
                "SELECT COALESCE(sum(trap),0)::int,COALESCE(sum(too_fast),0)::int,COALESCE(sum(busy),0)::int,COALESCE(sum(expired),0)::int FROM public_abuse_counts WHERE tenant_id=$1 AND action=$2 AND subject=$3 AND hour>now()-make_interval(hours => $4)",
                &[&tenant, &action, &subject, &hours],
            )
            .await?;
        let row = rows.first().ok_or(Error::Internal)?;
        Ok(Tally {
            trap: row.get(0),
            too_fast: row.get(1),
            busy: row.get(2),
            expired: row.get(3),
        })
    }

    /// Counts one request that took no place. A server writes a subject's
    /// counts at most once a minute, with the request that finds the last
    /// write a minute old or the hour changed; until then they wait here (a
    /// restart loses them).
    async fn count(&self, tenant: i64, action: &str, subject: &str, abuse: Abuse) -> Result<()> {
        let (at, hour) = (now(), now() / 3600);
        let due = {
            let mut all = self.tallies.lock().unwrap_or_else(|p| p.into_inner());
            if all.len() > 10_000 {
                all.retain(|_, (counted, tally, _)| *counted == hour && !tally.empty());
            }
            let (counted, tally, written) = all
                .entry((tenant, action.to_owned(), subject.to_owned()))
                .or_insert((hour, Tally::default(), 0));
            let mut due = None;
            if *counted != hour {
                due = Some((*counted, std::mem::take(tally)));
                *counted = hour;
            }
            match abuse {
                Abuse::Trap => tally.trap += 1,
                Abuse::TooFast => tally.too_fast += 1,
                Abuse::Busy => tally.busy += 1,
                Abuse::Expired => tally.expired += 1,
            }
            if due.is_none() && at - *written >= 60 {
                due = Some((hour, std::mem::take(tally)));
            }
            if due.is_some() {
                *written = at;
            }
            due
        };
        let Some((hour, tally)) = due.filter(|(_, tally)| !tally.empty()) else {
            return Ok(());
        };
        let (action, subject) = (action.to_owned(), subject.to_owned());
        let mut client = self.db.get().await?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(tenant, PRINCIPAL, "base.public.abuse"),
            )
            .await?;
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "INSERT INTO public_abuse_counts(tenant_id,action,subject,hour,trap,too_fast,busy,expired) VALUES($1,$2,$3,to_timestamp($4::float8*3600),$5,$6,$7,$8) ON CONFLICT (tenant_id,action,subject,hour) DO UPDATE SET trap=public_abuse_counts.trap+excluded.trap,too_fast=public_abuse_counts.too_fast+excluded.too_fast,busy=public_abuse_counts.busy+excluded.busy,expired=public_abuse_counts.expired+excluded.expired",
                    &[&tenant, &action, &subject, &(hour as f64), &tally.trap, &tally.too_fast, &tally.busy, &tally.expired],
                )
                .await?;
                // Old hours go, a batch at a time.
                sql.execute(
                    "DELETE FROM public_abuse_counts WHERE (tenant_id,action,subject,hour) IN (SELECT tenant_id,action,subject,hour FROM public_abuse_counts WHERE tenant_id=$1 AND hour<now()-make_interval(days => $2) LIMIT $3)",
                    &[&tenant, &COUNT_DAYS, &PRUNE_BATCH],
                )
                .await?;
                let change = Change {
                    kind: "public.abuse".into(),
                    id: tenant,
                    old: json!({}),
                    new: json!({"action": action, "subject": subject, "trap": tally.trap, "too_fast": tally.too_fast, "busy": tally.busy, "expired": tally.expired}),
                };
                Ok(((), vec![change]))
            })
        })
        .await?;
        tx.commit().await
    }

    /// A receipt for a ticket, in a transaction of its own: `Ok(None)` when
    /// the ticket was used meanwhile (its first receipt stands), and
    /// `Err(Refused(BUSY))` when `places` were all taken this hour.
    async fn record(
        &self,
        ticket: &Used<'_>,
        outcome: &'static str,
        reason: Option<&'static str>,
        places: Option<i64>,
    ) -> Result<Option<i64>> {
        let Used {
            tenant,
            action,
            subject,
            ticket_hash,
            receipt_hash,
        } = *ticket;
        let mut client = self.db.get().await?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(tenant, PRINCIPAL, "base.public.receipt"),
            )
            .await?;
        if let Some(limit) = places {
            // Every server counts the subject's hour under one lock.
            tx.sql()
                .execute(
                    "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                    &[&format!("base/public.places:{tenant}:{action}:{subject}")],
                )
                .await?;
            let taken: i64 = tx
                .sql()
                .query_one(
                    "SELECT count(*) FROM public_receipts WHERE tenant_id=$1 AND action=$2 AND subject=$3 AND outcome<>'spam' AND created_at>=date_trunc('hour',now())",
                    &[&tenant, &action, &subject],
                )
                .await?
                .get(0);
            if taken >= limit {
                return Err(Error::Refused(BUSY.into()));
            }
        }
        let (action, subject, ticket_hash, receipt_hash) = (
            action.to_owned(),
            subject.to_owned(),
            ticket_hash.to_vec(),
            receipt_hash.to_vec(),
        );
        let id = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    // Old receipts go, a batch at a time.
                    sql.execute(
                        "DELETE FROM public_receipts WHERE id IN (SELECT id FROM public_receipts WHERE tenant_id=$1 AND created_at<now()-make_interval(days => $2) LIMIT $3)",
                        &[&tenant, &KEEP_DAYS, &PRUNE_BATCH],
                    )
                    .await?;
                    let row = sql
                        .query_opt(
                            "INSERT INTO public_receipts(tenant_id,action,subject,ticket_hash,receipt_hash,outcome,reason,settled_at) VALUES($1,$2,$3,$4,$5,$6,$7,CASE WHEN $6='pending' THEN NULL ELSE now() END) ON CONFLICT (tenant_id,action,ticket_hash) DO NOTHING RETURNING id",
                            &[&tenant, &action, &subject, &ticket_hash, &receipt_hash, &outcome, &reason],
                        )
                        .await?;
                    let id: Option<i64> = row.map(|row| row.get(0));
                    let changes = id
                        .map(|id| Change {
                            kind: "public.receipt".into(),
                            id,
                            old: json!({}),
                            new: json!({"action": action, "subject": subject, "outcome": outcome, "reason": reason}),
                        })
                        .into_iter()
                        .collect();
                    Ok((id, changes))
                })
            })
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    async fn settle(&self, tenant: i64, id: i64, acted: &Acted) -> Result<()> {
        let (outcome, message) = match acted {
            Acted::Accepted => ("accepted", None),
            Acted::Refused(message) => (
                "refused",
                Some(message.chars().take(500).collect::<String>())
                    .filter(|m| !m.trim().is_empty()),
            ),
        };
        let mut client = self.db.get().await?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(tenant, PRINCIPAL, "base.public.receipt"),
            )
            .await?;
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "UPDATE public_receipts SET outcome=$3,message=$4,reason=CASE WHEN $3='refused' THEN 'owner' END,settled_at=now() WHERE tenant_id=$1 AND id=$2 AND outcome='pending'",
                    &[&tenant, &id, &outcome, &message],
                )
                .await?;
                let change = Change {
                    kind: "public.receipt".into(),
                    id,
                    old: json!({"outcome": "pending"}),
                    new: json!({"outcome": outcome}),
                };
                Ok(((), vec![change]))
            })
        })
        .await?;
        tx.commit().await
    }

    /// The host's call for a POST to an action: every check in order, then
    /// the owner, and the address the visitor is sent back to.
    pub async fn submit(&self, submission: Submission<'_>) -> Result<Local> {
        let Submission {
            scope,
            id,
            action,
            owner,
            page,
            path,
            posted,
        } = submission;
        let tenant = scope.tenant;
        let mut fields: HashMap<String, String> = HashMap::new();
        for (name, value) in posted {
            if fields.len() >= FIELDS_MAX && !fields.contains_key(&name) {
                return Err(Error::Invalid);
            }
            fields.entry(name).or_insert(value);
        }
        let ticket = fields
            .remove(&format!("{FORM}[{TICKET}]"))
            .unwrap_or_default();
        let trap = fields
            .remove(&format!("{FORM}[{TRAP}]"))
            .unwrap_or_default();
        fields.retain(|name, _| !name.starts_with("public["));
        // The form is shown again with the visit's carried fields.
        let again = |what: &str| -> Result<Local> {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            query.append_pair(what, "1");
            for name in action.carry {
                if let Some(value) = fields.get(*name).filter(|v| !v.trim().is_empty()) {
                    query.append_pair(name, &value.chars().take(255).collect::<String>());
                }
            }
            back(page, &query.finish())
        };
        let key = {
            let client = self.db.get().await?;
            self.ticket_key(&**client, tenant).await?
        };
        let Some((subject, issued)) = read_ticket(&key, id, &ticket) else {
            self.count(tenant, id, "-", Abuse::Expired).await?;
            return again("expired");
        };
        let token = mac(&key, &format!("receipt~{ticket}"))?;
        let (ticket_hash, receipt_hash) = (digest(&ticket), digest(&token));
        let receipt = || back(page, &format!("receipt={token}"));
        let used = Used {
            tenant,
            action: id,
            subject: &subject,
            ticket_hash: &ticket_hash,
            receipt_hash: &receipt_hash,
        };
        // A ticket used before keeps its first answer.
        let replay = {
            let client = self.db.get().await?;
            client
                .query_opt(
                    "SELECT 1 FROM public_receipts WHERE tenant_id=$1 AND action=$2 AND ticket_hash=$3",
                    &[&tenant, &id, &ticket_hash],
                )
                .await?
                .is_some()
        };
        if replay {
            return receipt();
        }
        let age = now() - issued;
        if !(0..=TICKET_SECONDS).contains(&age) {
            self.count(tenant, id, &subject, Abuse::Expired).await?;
            return again("expired");
        }
        // Spam is decided before the hour's places, so it never spends a
        // visitor's place: a marker that uses its ticket up, and a count.
        let spam = if action.checks.trap_field && !trap.trim().is_empty() {
            Some(("trap", Abuse::Trap))
        } else if age < i64::from(action.checks.min_fill_seconds) {
            Some(("too_fast", Abuse::TooFast))
        } else {
            None
        };
        if let Some((reason, abuse)) = spam {
            if self
                .record(&used, "spam", Some(reason), None)
                .await?
                .is_some()
            {
                self.count(tenant, id, &subject, abuse).await?;
            }
            return receipt();
        }
        let places = match action.checks.places_per_hour {
            Some(setting) => {
                let client = self.db.get().await?;
                Some(
                    self.settings
                        .get_tenant(&**client, tenant, setting)
                        .await?
                        .as_i64()
                        .ok_or(Error::Internal)?,
                )
            }
            None => None,
        };
        let receipt_id = match self.record(&used, "pending", None, places).await {
            Ok(Some(receipt_id)) => receipt_id,
            // The same ticket was used meanwhile: its receipt stands.
            Ok(None) => return receipt(),
            Err(Error::Refused(why)) if why == BUSY => {
                self.count(tenant, id, &subject, Abuse::Busy).await?;
                return again("busy");
            }
            Err(error) => return Err(error),
        };
        let acted = owner
            .0
            .act(ActionRequest {
                operation: action.operation,
                scope,
                subject: &subject,
                path,
                fields: &fields,
            })
            .await;
        match acted {
            Ok(acted) => self.settle(tenant, receipt_id, &acted).await?,
            Err(error) => {
                // The visitor gets the page's own words; the fault is logged
                // by the host when it is the server's.
                self.settle(tenant, receipt_id, &Acted::Refused(String::new()))
                    .await?;
                if !matches!(
                    error,
                    Error::Invalid | Error::NotFound | Error::Forbidden | Error::Refused(_)
                ) {
                    return Err(error);
                }
            }
        }
        receipt()
    }
}

/// A ticket that passed its signature, as its receipt names it.
#[derive(Clone, Copy)]
struct Used<'a> {
    tenant: i64,
    action: &'a str,
    subject: &'a str,
    ticket_hash: &'a [u8],
    receipt_hash: &'a [u8],
}

/// One POST to an action, as the host hands it over.
pub struct Submission<'a> {
    pub scope: &'a PublicScope,
    /// The action's contribution ID, which its tickets are signed for.
    pub id: &'a str,
    pub action: &'a PublicAction,
    pub owner: &'a ActionService<Error>,
    /// The request's own path, where the visitor is sent back.
    pub page: &'a str,
    pub path: &'a HashMap<String, String>,
    /// The urlencoded body's pairs, in order.
    pub posted: Vec<(String, String)>,
}

/// The refusal a full hour answers with, inside this module.
const BUSY: &str = "base/public.busy";
