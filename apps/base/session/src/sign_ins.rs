//! The append-only sign-in history (`session_sign_ins`): what each successful
//! browser sign-in recorded, the previous sign-in the account popup shows, and
//! the new-device judgement behind the new-device sign-in email, with its
//! once-a-day claim (`session_sign_in_alerts`).
//!
//! A device is the label `client_name` gives the browser's user agent: browser
//! family and operating system ("Chrome on Windows"). A sign-in is from a new
//! device when the account's retained history (`SIGN_IN_HISTORY_DAYS`) already
//! holds an earlier sign-in and none of them carries the same label. The very
//! first sign-in on record is not new: there is nothing to compare it with.
use crate::Sessions;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_tenancy::{AccountKind, Scope};
use serde_json::json;
use std::time::SystemTime;
use tokio_postgres::GenericClient;

const MAX_IP: usize = 45;
const MAX_USER_AGENT: usize = 512;
pub(crate) const SIGN_IN_HISTORY_DAYS: i32 = 90;

/// The successful sign-in before the current session, from the retained
/// sign-in history. No bearer or row id leaves this value.
#[derive(Clone, Debug)]
pub struct PreviousSignIn {
    pub signed_in_at: SystemTime,
    pub ip_address: Option<String>,
    pub place: Option<String>,
    pub user_agent: Option<String>,
}

/// One retained sign-in of an account, as the personal-data export lists it.
/// No bearer or row id leaves this value.
#[derive(Clone, Debug)]
pub struct SignInRecord {
    pub user_id: i64,
    /// RFC 3339, UTC.
    pub signed_in_at: String,
    pub device: String,
    pub ip_address: Option<String>,
    pub place: Option<String>,
    pub user_agent: Option<String>,
    /// RFC 3339, UTC: when the retention sweep deletes the record.
    pub retained_until: String,
}

/// A new-device email claim still held (the sweep removes claims after a
/// day or two), as the personal-data export lists it.
#[derive(Clone, Debug)]
pub struct SignInAlertRecord {
    pub user_id: i64,
    pub device: String,
    /// The UTC day, `YYYY-MM-DD`.
    pub alerted_on: String,
}

/// An account's retained sign-in records for an export.
#[derive(Clone, Debug, Default)]
pub struct SignInExport {
    pub sign_ins: Vec<SignInRecord>,
    pub alerts: Vec<SignInAlertRecord>,
}

/// A sign-in from a device the account's retained history has not seen, as
/// `describe` judged it. It names no account: the alert re-derives the
/// account from the session it just admitted.
#[derive(Clone, Debug)]
pub struct NewDevice {
    /// The `client_name` label, for example "Chrome on Windows".
    pub device: String,
    pub ip_address: Option<String>,
    pub place: Option<String>,
    pub signed_in_at: SystemTime,
}

/// A modest device label from a browser's self-reported user agent. It is
/// presentation only and never grants authority.
pub fn client_name(user_agent: Option<&str>) -> String {
    let user_agent = user_agent.unwrap_or_default();
    let browser = if user_agent.contains("Edg/")
        || user_agent.contains("EdgiOS/")
        || user_agent.contains("EdgA/")
    {
        Some("Edge")
    } else if user_agent.contains("Chrome/")
        || user_agent.contains("CriOS/")
        || user_agent.contains("HeadlessChrome/")
    {
        Some("Chrome")
    } else if user_agent.contains("Firefox/") || user_agent.contains("FxiOS/") {
        Some("Firefox")
    } else if user_agent.contains("OPR/") || user_agent.contains("Opera/") {
        Some("Opera")
    } else if user_agent.contains("Safari/") && user_agent.contains("Version/") {
        Some("Safari")
    } else {
        None
    };
    let system = if user_agent.contains("Windows") {
        Some("Windows")
    } else if user_agent.contains("iPhone")
        || user_agent.contains("iPad")
        || user_agent.contains("iPod")
    {
        Some("iOS")
    } else if user_agent.contains("Android") {
        Some("Android")
    } else if user_agent.contains("CrOS") {
        Some("ChromeOS")
    } else if user_agent.contains("Macintosh") || user_agent.contains("Mac OS X") {
        Some("macOS")
    } else if user_agent.contains("Linux") {
        Some("Linux")
    } else {
        None
    };
    match (browser, system) {
        (Some(browser), Some(system)) => format!("{browser} on {system}"),
        (Some(browser), None) => browser.to_owned(),
        (None, Some(system)) => format!("Browser on {system}"),
        (None, None) => "Browser".to_owned(),
    }
}

/// The account holder's own sign-in, not an operator, agent or system
/// principal acting for it.
fn holder(scope: &Scope) -> Result<()> {
    if scope.impersonator().is_some()
        || scope.agent().is_some()
        || scope.principal().is_some()
        || scope.actor().kind() != AccountKind::Staff
    {
        return Err(Error::Forbidden);
    }
    Ok(())
}

impl Sessions {
    /// Records the signing-in client once, bounded; neither value is ever
    /// interpreted, only listed. Appends the sign-in to the history and
    /// returns `Some` when it came from a new device (module docs). An
    /// impersonated session records no history and is never new.
    pub async fn describe(
        &self,
        client: &(impl GenericClient + Sync),
        token: &str,
        ip_address: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<Option<NewDevice>> {
        let ip = ip_address
            .map(str::trim)
            .filter(|ip| !ip.is_empty() && ip.len() <= MAX_IP)
            .map(str::to_owned);
        let agent = user_agent
            .map(|agent| {
                agent
                    .trim()
                    .chars()
                    .take(MAX_USER_AGENT)
                    .collect::<String>()
            })
            .filter(|agent| !agent.is_empty());
        let device = client_name(agent.as_deref());
        let place = self.places.find(client, ip.as_deref()).await;
        let id = crate::session_id(token);
        client
            .execute(
                "UPDATE sessions SET ip_address=$2,place=$3,user_agent=$4 WHERE id=$1",
                &[&id, &ip, &place, &agent],
            )
            .await?;
        // The history reads share the statement's snapshot, so they see only
        // the sign-ins before this one, never the row it appends.
        let Some(row) = client
            .query_opt(
                "WITH s AS (SELECT tenant_id,user_id,signed_in_at FROM sessions WHERE id=$1 AND impersonator_id IS NULL), \
                 added AS (INSERT INTO session_sign_ins(tenant_id,user_id,signed_in_at,ip_address,place,user_agent,device) SELECT tenant_id,user_id,signed_in_at,$2,$3,$4,$5 FROM s RETURNING signed_in_at) \
                 SELECT added.signed_in_at, \
                 EXISTS(SELECT 1 FROM session_sign_ins h JOIN s ON h.tenant_id=s.tenant_id AND h.user_id=s.user_id), \
                 EXISTS(SELECT 1 FROM session_sign_ins h JOIN s ON h.tenant_id=s.tenant_id AND h.user_id=s.user_id AND h.device=$5) \
                 FROM added",
                &[&id, &ip, &place, &agent, &device],
            )
            .await?
        else {
            return Ok(None);
        };
        let (seen_before, device_seen): (bool, bool) = (row.get(1), row.get(2));
        Ok((seen_before && !device_seen).then(|| NewDevice {
            device,
            ip_address: ip,
            place,
            signed_in_at: row.get(0),
        }))
    }

    /// The latest successful sign-in of this account before the current
    /// session, kept for `SIGN_IN_HISTORY_DAYS`. An
    /// impersonating operator is not the account holder and cannot read it.
    pub async fn previous_sign_in(
        &self,
        client: &impl GenericClient,
        scope: &Scope,
    ) -> Result<Option<PreviousSignIn>> {
        holder(scope)?;
        Ok(client
            .query_opt(
                "SELECT signed_in_at,ip_address,place,user_agent FROM session_sign_ins WHERE tenant_id=$1 AND user_id=$2 AND signed_in_at<(SELECT signed_in_at FROM sessions WHERE tenant_id=$1 AND user_id=$2 AND csrf=$3 AND impersonator_id IS NULL) ORDER BY signed_in_at DESC,id DESC LIMIT 1",
                &[&scope.tenant(), &scope.user(), &scope.csrf],
            )
            .await?
            .map(|row| PreviousSignIn {
                signed_in_at: row.get(0),
                ip_address: row.get(1),
                place: row.get(2),
                user_agent: row.get(3),
            }))
    }

    /// Claims the one new-device email this account may get for `device`
    /// today (UTC) inside the sender's transaction, and audits it. `Ok(false)`
    /// means the day's email for this device was already claimed, and nothing
    /// was written. A concurrent claim that wins the race makes this one
    /// `Conflict`; the caller drops the transaction and sends nothing.
    pub async fn claim_new_device_alert(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        device: &NewDevice,
    ) -> Result<bool> {
        holder(scope)?;
        let (tenant, user) = (scope.tenant(), scope.user());
        let label = device.device.clone();
        let claimed: bool = tx
            .sql()
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM session_sign_in_alerts WHERE tenant_id=$1 AND user_id=$2 AND device=$3 AND alerted_on=(now() AT TIME ZONE 'UTC')::date)",
                &[&tenant, &user, &label],
            )
            .await?
            .get(0);
        if claimed {
            return Ok(false);
        }
        let signed_in_at = jiff::Timestamp::try_from(device.signed_in_at)
            .map_err(|_| Error::Internal)?
            .to_string();
        tx.mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_opt(
                        "INSERT INTO session_sign_in_alerts(tenant_id,user_id,device,alerted_on) VALUES($1,$2,$3,(now() AT TIME ZONE 'UTC')::date) ON CONFLICT DO NOTHING RETURNING id,alerted_on::text",
                        &[&tenant, &user, &label],
                    )
                    .await?
                    .ok_or(Error::Conflict)?;
                let alerted_on: String = row.get(1);
                Ok((
                    true,
                    vec![Change {
                        kind: "session_sign_in_alert".into(),
                        id: row.get(0),
                        old: json!({}),
                        new: json!({"tenant_id": tenant, "user_id": user, "device": label, "signed_in_at": signed_in_at, "alerted_on": alerted_on, "email": "new-device sign-in"}),
                    }],
                ))
            })
        })
        .await
    }

    /// The retained sign-in history (time, device, address, user agent) and
    /// held new-device email claims of `users`, for an approved personal-data
    /// export. Only records inside the retention period are listed, newest
    /// first per account, so a record the sweep has not yet reached is never
    /// disclosed or reported as kept. `grant` must be for `scope`'s tenant; the
    /// caller chooses `users` from the person.
    pub async fn sign_in_history(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        grant: &bilibi_personal_data::Disclosure,
        users: &[i64],
    ) -> Result<SignInExport> {
        grant.check(scope)?;
        let sign_ins = client
            .query(
                "SELECT user_id,to_char(signed_in_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),device,ip_address,place,user_agent,\
                 to_char((signed_in_at+make_interval(days => $3::int)) AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM session_sign_ins WHERE tenant_id=$1 AND user_id=ANY($2) AND signed_in_at>=now()-make_interval(days => $3::int) \
                 ORDER BY user_id,signed_in_at DESC,id DESC",
                &[&scope.tenant(), &users, &SIGN_IN_HISTORY_DAYS],
            )
            .await?
            .iter()
            .map(|row| SignInRecord {
                user_id: row.get(0),
                signed_in_at: row.get(1),
                device: row.get(2),
                ip_address: row.get(3),
                place: row.get(4),
                user_agent: row.get(5),
                retained_until: row.get(6),
            })
            .collect();
        let alerts = client
            .query(
                "SELECT user_id,device,alerted_on::text FROM session_sign_in_alerts WHERE tenant_id=$1 AND user_id=ANY($2) ORDER BY user_id,alerted_on DESC,id DESC",
                &[&scope.tenant(), &users],
            )
            .await?
            .iter()
            .map(|row| SignInAlertRecord {
                user_id: row.get(0),
                device: row.get(1),
                alerted_on: row.get(2),
            })
            .collect();
        Ok(SignInExport { sign_ins, alerts })
    }

    /// When the sweep deletes the newest retained sign-in of `user` (RFC 3339,
    /// UTC), or `None` when none is retained. Erasure leaves the sign-in
    /// history in place as security evidence and reports it as kept until then.
    pub async fn sign_in_retained_until(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        grant: &bilibi_personal_data::Erasure,
        user: i64,
    ) -> Result<Option<String>> {
        grant.check(scope)?;
        Ok(client
            .query_one(
                "SELECT to_char((max(signed_in_at)+make_interval(days => $3::int)) AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM session_sign_ins WHERE tenant_id=$1 AND user_id=$2 AND signed_in_at>=now()-make_interval(days => $3::int)",
                &[&scope.tenant(), &user, &SIGN_IN_HISTORY_DAYS],
            )
            .await?
            .get(0))
    }
}
