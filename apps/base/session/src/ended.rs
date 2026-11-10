//! Ending sessions, and the notice an ended session leaves for Domains.
//!
//! Every ending path here (sign-out, the holder's or an operator's
//! termination, a credential or account change, expiry) tells the bound
//! [`EndNotice`] inside the ending transaction, which Core User records as the
//! `core/user.session_ended` event on Base Events. A notice names the tenant
//! (the transaction's), the account and why it ended; never the token, its
//! digest or the session id. A transaction that cannot record in the session's
//! tenant (an operator ending another tenant's session) or may not enqueue work
//! (an impersonated write) marks the row ended instead; the expiry sweep
//! deletes it and records the notice in that tenant.
use crate::{OwnTermination, Sessions, Terminated, Termination};
use bilibi_database::{Change, Context, Db, Error, MutationBoundary, MutationTransaction, Result};
use bilibi_module_registry::RegistryError;
use bilibi_tenancy::{AccountKind, Scope};
use serde_json::json;
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};
use tokio_postgres::GenericClient;

/// The audit name of the expiry sweep's per-tenant writes.
const EXPIRY_PRINCIPAL: &str = "session-expiry";

/// Whether the account (`$2`) holds a live session in the tenant (`$1`): one
/// it signed in to itself, or one in which it is the impersonating operator.
/// `$3` is the idle lifetime in seconds.
const LIVE: &str = "SELECT EXISTS(SELECT 1 FROM sessions WHERE tenant_id=$1 AND ((user_id=$2 AND impersonator_id IS NULL) OR impersonator_id=$2) AND expires_at>now() AND last_activity>now()-make_interval(secs => $3::double precision))";

/// Why an account's session ended, as `core/user.session_ended` reports it.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Ending {
    /// The holder ended it: signing out, signing out one or all of their
    /// other devices, or the identity provider's sign-out.
    SignedOut,
    /// Someone or something else ended it: an operator's termination, or a
    /// credential or account change (password change or reset, desk removal,
    /// deprovisioning, erasure, a second-factor reset).
    Terminated,
    /// It lapsed: idle longer than the lifetime setting or past the absolute
    /// expiry. Recorded by the expiry sweep.
    Expired,
}
impl Ending {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SignedOut => "signed_out",
            Self::Terminated => "terminated",
            Self::Expired => "expired",
        }
    }
    /// A marked row's `ended_reason`; an unmarked row that the sweep removes expired.
    fn stored(value: Option<&str>) -> Self {
        match value {
            Some("signed_out") => Self::SignedOut,
            Some("terminated") => Self::Terminated,
            _ => Self::Expired,
        }
    }
}

/// One account whose sessions ended in one transaction for one reason.
#[derive(Clone, Copy, Debug)]
pub struct Ended {
    account: i64,
    reason: Ending,
    sessions: i64,
    still_signed_in: bool,
}
impl Ended {
    /// The account (user ID) in the transaction's tenant.
    pub fn account(self) -> i64 {
        self.account
    }
    pub fn reason(self) -> Ending {
        self.reason
    }
    /// How many of its sessions ended together.
    pub fn sessions(self) -> i64 {
        self.sessions
    }
    /// The account still held another live session when this one ended, as
    /// the ending transaction saw it. Ask [`Sessions::signed_in`] for now.
    pub fn still_signed_in(self) -> bool {
        self.still_signed_in
    }
}

pub type NoticeFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// The owner of the durable session-ended notice (Core User) binds this once
/// during installation; boot is refused without it. It records each
/// [`Ended`] in the supplied transaction, whose tenant is the accounts'
/// tenant, and an error rolls the ending back.
pub trait EndNotice: Send + Sync {
    fn record<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        ended: &'a [Ended],
    ) -> NoticeFuture<'a>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for bilibi_tenancy::Scope {}
    impl Sealed for bilibi_authz::SystemScope {}
}
/// A sealed proof that names the tenant a read is for: a signed-in or job
/// [`Scope`], or the `SystemScope` an event subscriber receives.
pub trait TenantProof: sealed::Sealed {
    fn proven_tenant(&self) -> i64;
}
impl TenantProof for Scope {
    fn proven_tenant(&self) -> i64 {
        self.tenant()
    }
}
impl TenantProof for bilibi_authz::SystemScope {
    fn proven_tenant(&self) -> i64 {
        self.tenant()
    }
}

impl Sessions {
    /// Core User binds the notice during installation; a second binding refuses boot.
    pub fn bind_end_notice(
        &self,
        notice: Arc<dyn EndNotice>,
    ) -> bilibi_module_registry::Result<()> {
        self.end_notice
            .set(notice)
            .map_err(|_| RegistryError::Service("duplicate session end notice binding".into()))
    }
    pub(crate) fn notice_ready(&self) -> bilibi_module_registry::Result<()> {
        self.end_notice
            .get()
            .map(|_| ())
            .ok_or_else(|| RegistryError::Service("missing session end notice".into()))
    }
    /// Whether the account holds any live session in the proven tenant now:
    /// one it signed in to itself, or one in which it is the impersonating
    /// operator. An account impersonated by someone else is not signed in.
    pub async fn signed_in(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &impl TenantProof,
        account: i64,
    ) -> Result<bool> {
        if account <= 0 {
            return Err(Error::Invalid);
        }
        let idle = self.policy(client).await?.idle_seconds as f64;
        Ok(client
            .query_one(LIVE, &[&scope.proven_tenant(), &account, &idle])
            .await?
            .get(0))
    }
    /// Whether the session behind this sealed scope still stands: not signed
    /// out, terminated, moved or expired. Answered from the scope's proof, and
    /// says nothing else; a scope without a browser session never stands.
    pub async fn stands(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Result<bool> {
        if scope.csrf.is_empty() || scope.principal().is_some() {
            return Ok(false);
        }
        let idle = self.policy(client).await?.idle_seconds as f64;
        let impersonator = scope.impersonator().map(|operator| operator.user());
        Ok(client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE tenant_id=$1 AND user_id=$2 AND csrf=$3 AND impersonator_id IS NOT DISTINCT FROM $4 AND expires_at>now() AND last_activity>now()-make_interval(secs => $5::double precision))",
                &[&scope.tenant(), &scope.user(), &scope.csrf, &impersonator, &idle],
            )
            .await?
            .get(0))
    }

    /// The transaction can record the notice itself: it runs in the
    /// session's tenant, and not as an impersonator, whose writes may not
    /// enqueue work (`base/queue`).
    fn records_now(tx: &MutationTransaction<'_>, tenant: i64) -> bool {
        tx.context().tenant == tenant && tx.context().impersonator.is_none()
    }
    /// Tells the bound notice which accounts' sessions this transaction ended,
    /// one entry per account and reason. Any failure forbids the commit, so a
    /// session never ends without its notice.
    async fn notify(
        &self,
        tx: &mut MutationTransaction<'_>,
        ended: impl IntoIterator<Item = (i64, Ending)>,
    ) -> Result<()> {
        let mut groups: BTreeMap<(i64, Ending), i64> = BTreeMap::new();
        for key in ended {
            *groups.entry(key).or_default() += 1;
        }
        if groups.is_empty() {
            return Ok(());
        }
        let result = async {
            let notice = self.end_notice.get().cloned().ok_or(Error::Internal)?;
            let idle = self.policy(tx.sql()).await?.idle_seconds as f64;
            let tenant = tx.context().tenant;
            let mut notices = Vec::with_capacity(groups.len());
            for ((account, reason), sessions) in groups {
                let still_signed_in = tx
                    .sql()
                    .query_one(LIVE, &[&tenant, &account, &idle])
                    .await?
                    .get(0);
                notices.push(Ended {
                    account,
                    reason,
                    sessions,
                    still_signed_in,
                });
            }
            notice.record(tx, &notices).await
        }
        .await;
        if result.is_err() {
            tx.poison();
        }
        result
    }
    fn announce_after(&self, tx: &mut MutationTransaction<'_>, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let sessions = self.terminations.clone();
        tx.after_commit(move || {
            for id in ids {
                let _ = sessions.send(Terminated { id, redirect: None });
            }
        });
    }

    /// An operator ends one listed session. The caller already authorized
    /// `admin.system.session.manage`; the audit row records whose session ended
    /// and from where, never the token. Announced after the commit, so a rolled
    /// back termination signs nobody out. A session of another tenant is
    /// marked ended rather than deleted; the expiry sweep records its notice.
    pub async fn terminate(
        &self,
        tx: &mut MutationTransaction<'_>,
        id: &str,
        current: &str,
    ) -> Result<Termination> {
        if id.is_empty() || current.is_empty() {
            return Err(Error::Invalid);
        }
        if id == current {
            return Ok(Termination::CurrentSession);
        }
        let idle = self.policy(tx.sql()).await?.idle_seconds as f64;
        let Some(row) = tx.sql().query_opt(
            "SELECT tenant_id,user_id,impersonator_id,ip_address,user_agent,last_activity::text,expires_at<=now() OR last_activity<=now()-make_interval(secs => $2::double precision) FROM sessions WHERE id=$1 AND ended_reason IS NULL FOR UPDATE",
            &[&id, &idle],
        ).await? else {
            return Ok(Termination::NotFound);
        };
        let tenant: i64 = row.get(0);
        let user: i64 = row.get(1);
        let account = row.get::<_, Option<i64>>(2).unwrap_or(user);
        let reason = if row.get(6) {
            Ending::Expired
        } else {
            Ending::Terminated
        };
        let old = json!({
            "tenant_id": tenant, "user_id": user,
            "ip_address": row.get::<_, Option<String>>(3),
            "user_agent": row.get::<_, Option<String>>(4),
            "last_activity": row.get::<_, String>(5), "payload": "[redacted]",
        });
        let now = Self::records_now(tx, tenant);
        let marked = (reason == Ending::Terminated).then_some(reason.as_str());
        let ended = id.to_owned();
        tx.mutate(move |sql| {
            Box::pin(async move {
                if now {
                    sql.execute("DELETE FROM sessions WHERE id=$1", &[&ended])
                        .await?;
                } else {
                    sql.execute(
                        "UPDATE sessions SET expires_at=LEAST(expires_at,now()),ended_reason=$2 WHERE id=$1",
                        &[&ended, &marked],
                    )
                    .await?;
                }
                Ok((
                    (),
                    vec![Change {
                        kind: "session".into(),
                        id: user,
                        old,
                        new: json!({}),
                    }],
                ))
            })
        })
        .await?;
        if now {
            self.notify(tx, [(account, reason)]).await?;
        }
        self.announce_after(tx, vec![id.to_owned()]);
        Ok(Termination::Terminated)
    }
    /// Ends every session of one tenant inside the caller's write: what
    /// suspending the tenant does, so nobody stays signed in to it. The caller
    /// decided and authorized that (`core/tenant` holds the platform
    /// capability); the tenant is a request argument only for that composed
    /// operation, never an id taken from a session holder. Each ended session
    /// is a separate audited change that records where it ran and never the
    /// token, and every live page of it is told after the commit, so a rolled
    /// back suspension signs nobody out. Returns the count. Records no
    /// session-ended notice: nothing runs in a suspended tenant, including
    /// event delivery.
    pub async fn end_tenant(&self, tx: &mut MutationTransaction<'_>, tenant: i64) -> Result<usize> {
        if tenant <= 0 {
            return Err(Error::Invalid);
        }
        let ended = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let rows = sql
                        .query(
                            "DELETE FROM sessions WHERE tenant_id=$1 RETURNING id,user_id,impersonator_id,ip_address,user_agent,signed_in_at::text,last_activity::text",
                            &[&tenant],
                        )
                        .await?;
                    let mut ids = Vec::with_capacity(rows.len());
                    let mut changes = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let user: i64 = row.get(1);
                        ids.push(row.get::<_, String>(0));
                        changes.push(Change {
                            kind: "session".into(),
                            id: user,
                            old: json!({
                                "tenant_id": tenant, "user_id": user,
                                "impersonator_id": row.get::<_, Option<i64>>(2),
                                "ip_address": row.get::<_, Option<String>>(3),
                                "user_agent": row.get::<_, Option<String>>(4),
                                "signed_in_at": row.get::<_, Option<String>>(5),
                                "last_activity": row.get::<_, String>(6), "payload": "[redacted]",
                            }),
                            new: json!({"ended_by_tenant_suspension": true}),
                        });
                    }
                    Ok((ids, changes))
                })
            })
            .await?;
        let count = ended.len();
        self.announce_after(tx, ended);
        Ok(count)
    }
    /// The account holder ends one of their own other sessions. Ownership is
    /// part of the locked read and delete, not trusted from mounted page state.
    pub(crate) async fn terminate_own(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: &str,
        current: &str,
    ) -> Result<Termination> {
        if id.is_empty() || current.is_empty() {
            return Err(Error::Invalid);
        }
        if id == current {
            return Ok(Termination::CurrentSession);
        }
        if scope.impersonator().is_some()
            || scope.agent().is_some()
            || scope.principal().is_some()
            || scope.actor().kind() != AccountKind::Staff
            || !Self::records_now(tx, scope.tenant())
        {
            return Err(Error::Forbidden);
        }
        let tenant = scope.tenant();
        let user = scope.user();
        let id = id.to_owned();
        let Some(row) = tx.sql().query_opt(
            "SELECT ip_address,user_agent,signed_in_at::text,last_activity::text FROM sessions WHERE tenant_id=$1 AND user_id=$2 AND id=$3 AND impersonator_id IS NULL AND ended_reason IS NULL FOR UPDATE",
            &[&tenant, &user, &id],
        ).await? else {
            return Ok(Termination::NotFound);
        };
        let ip: Option<String> = row.get(0);
        let agent: Option<String> = row.get(1);
        let signed_in: String = row.get(2);
        let activity: String = row.get(3);
        let ended = id.clone();
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute(
                "DELETE FROM sessions WHERE tenant_id=$1 AND user_id=$2 AND id=$3 AND impersonator_id IS NULL",
                &[&tenant, &user, &ended],
            ).await?;
            Ok(((), vec![Change {
                kind: "session".into(), id: user,
                old: json!({"tenant_id":tenant,"user_id":user,"ip_address":ip,"user_agent":agent,"signed_in_at":signed_in,"last_activity":activity,"payload":"[redacted]"}),
                new: json!({"ended_by_account":true}),
            }]))
        })).await?;
        self.notify(tx, [(user, Ending::SignedOut)]).await?;
        self.announce_after(tx, vec![id]);
        Ok(Termination::Terminated)
    }

    /// The account holder ends every own session except the one making the
    /// request. Each ended session is a separate audited change; the account
    /// gets one notice for all of them.
    pub(crate) async fn terminate_others(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        current: &str,
    ) -> Result<Vec<String>> {
        if current.is_empty() {
            return Err(Error::Invalid);
        }
        if scope.impersonator().is_some()
            || scope.agent().is_some()
            || scope.principal().is_some()
            || scope.actor().kind() != AccountKind::Staff
            || !Self::records_now(tx, scope.tenant())
        {
            return Err(Error::Forbidden);
        }
        let tenant = scope.tenant();
        let user = scope.user();
        let current = current.to_owned();
        let policy = self.policy(tx.sql()).await?;
        let rows = tx.sql().query(
            "SELECT id,ip_address,user_agent,signed_in_at::text,last_activity::text FROM sessions WHERE tenant_id=$1 AND user_id=$2 AND id<>$3 AND impersonator_id IS NULL AND expires_at>now() AND last_activity>now()-make_interval(secs => $4::double precision) FOR UPDATE",
            &[&tenant, &user, &current, &(policy.idle_seconds as f64)],
        ).await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let metadata: Vec<OwnTermination> = rows
            .iter()
            .map(|row| OwnTermination {
                id: row.get(0),
                ip_address: row.get(1),
                user_agent: row.get(2),
                signed_in_at: row.get(3),
                last_activity: row.get(4),
            })
            .collect();
        let ids: Vec<String> = metadata.iter().map(|row| row.id.clone()).collect();
        let deleting = ids.clone();
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "DELETE FROM sessions WHERE tenant_id=$1 AND user_id=$2 AND id=ANY($3) AND impersonator_id IS NULL",
                    &[&tenant, &user, &deleting],
                ).await?;
                let changes = metadata
                    .iter()
                    .map(|row| Change {
                        kind: "session".into(),
                        id: user,
                        old: json!({
                            "tenant_id": tenant,
                            "user_id": user,
                            "ip_address": row.ip_address.clone(),
                            "user_agent": row.user_agent.clone(),
                            "signed_in_at": row.signed_in_at.clone(),
                            "last_activity": row.last_activity.clone(),
                            "payload": "[redacted]",
                        }),
                        new: json!({"ended_by_account": true}),
                    })
                    .collect();
                Ok(((), changes))
            })
        })
        .await?;
        self.notify(tx, ids.iter().map(|_| (user, Ending::SignedOut)))
            .await?;
        self.announce_after(tx, ids.clone());
        Ok(ids)
    }
    /// Ends a user's sessions inside the caller's own write, except the one
    /// named: a credential change keeps the session that made it. A session
    /// the user holds as an impersonating operator ends too. Returns the
    /// ended ids for [`Sessions::announce`] after the caller's commit. The
    /// caller's audit row (the credential change) covers the sign-outs. A
    /// session that had already lapsed is reported expired.
    pub(crate) async fn end_user_sessions(
        &self,
        tx: &mut MutationTransaction<'_>,
        user: i64,
        except: Option<&str>,
        reason: Ending,
    ) -> Result<Vec<String>> {
        let tenant = tx.context().tenant;
        if tenant <= 0
            || user <= 0
            || except.is_some_and(str::is_empty)
            || reason == Ending::Expired
        {
            return Err(Error::Invalid);
        }
        // The holder's agents lose their step-up windows with the sessions.
        bilibi_step_up::end_agent_windows(tx.sql(), tenant, user).await?;
        let idle = self.policy(tx.sql()).await?.idle_seconds as f64;
        if !Self::records_now(tx, tenant) {
            let rows = tx.sql().query(
                "UPDATE sessions SET expires_at=now(),ended_reason=$4 WHERE tenant_id=$1 AND (user_id=$2 OR impersonator_id=$2) AND ($3::text IS NULL OR id<>$3) AND ended_reason IS NULL AND expires_at>now() AND last_activity>now()-make_interval(secs => $5::double precision) RETURNING id",
                &[&tenant, &user, &except, &reason.as_str(), &idle],
            ).await?;
            return Ok(rows.iter().map(|row| row.get(0)).collect());
        }
        let rows = tx.sql().query(
            "DELETE FROM sessions WHERE tenant_id=$1 AND (user_id=$2 OR impersonator_id=$2) AND ($3::text IS NULL OR id<>$3) RETURNING id,COALESCE(impersonator_id,user_id),ended_reason,expires_at<=now() OR last_activity<=now()-make_interval(secs => $4::double precision)",
            &[&tenant, &user, &except, &idle],
        ).await?;
        let ended = rows.iter().map(|row| {
            let stored: Option<&str> = row.get(2);
            let lapsed: bool = row.get(3);
            let why = match stored {
                Some(_) => Ending::stored(stored),
                None if lapsed => Ending::Expired,
                None => reason,
            };
            (row.get::<_, i64>(1), why)
        });
        self.notify(tx, ended.collect::<Vec<_>>()).await?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }
    /// The signed-in holder ends their own session (logout) in the caller's
    /// transaction: nobody decided about someone else, so no session audit
    /// row; the notice is recorded and every other live page of that session
    /// is told after the commit. An impersonated session is marked ended and
    /// its operator's notice recorded by the expiry sweep.
    pub async fn delete(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: &str,
    ) -> Result<()> {
        if id.is_empty() || tx.context().tenant != scope.tenant() {
            return Err(Error::Invalid);
        }
        let ended = if Self::records_now(tx, scope.tenant()) {
            let row = tx.sql().query_opt(
                "DELETE FROM sessions WHERE tenant_id=$1 AND user_id=$2 AND id=$3 RETURNING COALESCE(impersonator_id,user_id)",
                &[&scope.tenant(), &scope.user(), &id],
            ).await?;
            match row {
                Some(row) => {
                    self.notify(tx, [(row.get(0), Ending::SignedOut)]).await?;
                    true
                }
                None => false,
            }
        } else {
            tx.sql().execute(
                "UPDATE sessions SET expires_at=now(),ended_reason='signed_out' WHERE tenant_id=$1 AND user_id=$2 AND id=$3 AND ended_reason IS NULL",
                &[&scope.tenant(), &scope.user(), &id],
            ).await? > 0
        };
        if ended {
            self.announce_after(tx, vec![id.to_owned()]);
        }
        Ok(())
    }
    /// Deletes sessions that can no longer authenticate: past the absolute
    /// expiry, idle longer than the policy allows, or marked ended. Expiry
    /// never depends on this sweep (`check` refuses them already). Each
    /// tenant's rows go in one write as the `session-expiry` system
    /// principal, which records each account's notice (expired, or the
    /// marked reason); no audit row per session. Each removed session is
    /// announced so what it held can be shown as ended. A tenant that fails
    /// keeps its rows for the next run; the others still commit.
    pub async fn prune_expired(&self, db: &Db, boundary: &MutationBoundary) -> Result<u64> {
        let mut client = db.get().await?;
        let idle = self.policy(&**client).await?.idle_seconds as f64;
        // The store is not tenant-owned and this is installation housekeeping
        // (`Reach::Installation`), not tenant work a tenant consents to; each
        // notice must still be recorded in its session's own tenant, so the
        // sweep writes once per tenant that has rows to remove.
        let tenants: Vec<i64> = client
            .query(
                "SELECT DISTINCT tenant_id FROM sessions WHERE expires_at<=now() OR last_activity<=now()-make_interval(secs => $1::double precision) ORDER BY tenant_id",
                &[&idle],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let mut pruned = 0;
        let mut failed = None;
        for tenant in tenants {
            match self.prune_tenant(&mut client, boundary, tenant, idle).await {
                Ok(count) => pruned += count,
                Err(error) => {
                    eprintln!("warning: session expiry in tenant {tenant} failed: {error:?}");
                    failed = Some(error);
                }
            }
        }
        client
            .execute(
                "DELETE FROM session_sign_ins WHERE signed_in_at<now()-make_interval(days => $1::int)",
                &[&crate::SIGN_IN_HISTORY_DAYS],
            )
            .await?;
        // A claim only limits today's new-device email; the audit keeps it.
        client
            .execute(
                "DELETE FROM session_sign_in_alerts WHERE alerted_on<(now() AT TIME ZONE 'UTC')::date-1",
                &[],
            )
            .await?;
        failed.map_or(Ok(pruned), Err)
    }
    async fn prune_tenant(
        &self,
        client: &mut tokio_postgres::Client,
        boundary: &MutationBoundary,
        tenant: i64,
        idle: f64,
    ) -> Result<u64> {
        let mut tx = boundary
            .begin(
                client,
                Context::system(tenant, EXPIRY_PRINCIPAL, "session.expire"),
            )
            .await?;
        let rows = tx.sql().query(
            "DELETE FROM sessions WHERE tenant_id=$1 AND (expires_at<=now() OR last_activity<=now()-make_interval(secs => $2::double precision)) RETURNING id,COALESCE(impersonator_id,user_id),ended_reason",
            &[&tenant, &idle],
        ).await?;
        let ended: Vec<(i64, Ending)> = rows
            .iter()
            .map(|row| (row.get(1), Ending::stored(row.get(2))))
            .collect();
        self.notify(&mut tx, ended).await?;
        let ids: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        let count = ids.len() as u64;
        self.announce_after(&mut tx, ids);
        tx.commit().await?;
        Ok(count)
    }
}
