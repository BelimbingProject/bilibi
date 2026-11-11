//! Public visitor sessions: a short-lived, renewable bearer proof for someone
//! with no account, bound to one tenant, one site of a Domain's channel and
//! the browser origin it was issued to. The store is apart from `sessions`, so
//! no staff or customer check can read it, and what it seals is a
//! [`VisitorScope`], never a `Scope`. Contract: `README.md`.
use bilibi_audit::{record_action, Attribution};
use bilibi_database::{Error, Result};
use bilibi_settings::Settings;
use bilibi_tenancy::Scope;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;
use tokio_postgres::GenericClient;

mod channel;
mod contributions;
mod origin;

pub use channel::{
    valid_key, Admission, Channel, ChannelFuture, ChannelService, Frame, Hello, Outbox, Site,
    Upload, VisitorChannel, VisitorScope, CHANNELS,
};
pub use contributions::{
    MANAGE, MESSAGES_PER_MINUTE_KEY, SESSIONS_PER_SENDER_KEY, SESSIONS_PER_SITE_KEY,
    SESSION_MAX_HOURS_KEY, SESSION_MINUTES_KEY, SOCKETS_PER_SESSION_KEY, SOCKETS_TOTAL_KEY,
    UPLOADS_PER_MINUTE_KEY, UPLOAD_MAX_BYTES_KEY,
};
pub use origin::{parse as parse_origin, MAX_ORIGINS};

/// The embeddable browser client, version 1: one dependency-free script the
/// host serves at `/visitor/v1/client.js`. A breaking change is a new major
/// path, so a page that embeds this one keeps working.
pub const CLIENT_V1: &str = include_str!("../assets/client-v1.js");
/// The registry service ID of [`Visitors`].
pub const SERVICE: &str = "base/visitor.sessions";
/// Every token starts with this, so a visitor token is never mistaken for, or
/// accepted as, any other credential.
pub const TOKEN_PREFIX: &str = "vst_";
const TOKEN_LEN: usize = TOKEN_PREFIX.len() + 64;
/// The longest subject and cursor a channel may return.
pub const SUBJECT_MAX: usize = 128;
/// Expired and ended rows are kept this long, then removed by the next
/// session opened, in batches of [`PRUNE_BATCH`].
/// How long one reading of the operator's limits is reused.
pub const POLICY_FRESH: Duration = Duration::from_secs(2);
const KEEP_DAYS: i64 = 1;
const PRUNE_BATCH: i64 = 100;

/// The operator's limits for public sessions, read on every use so a change
/// applies to the next request.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub session_seconds: u64,
    pub session_max_seconds: u64,
    pub sessions_per_sender: u32,
    pub sessions_per_site: u32,
    pub messages_per_minute: u32,
    pub sockets_per_session: u32,
    pub sockets_total: u32,
    pub upload_max_bytes: u64,
    pub uploads_per_minute: u32,
}

/// A session just opened. The token is shown once and never stored.
pub struct Opened {
    pub token: String,
    pub session: String,
    pub expires_in: u64,
}
/// A session's live end time after a renewal.
#[derive(Clone, Copy, Debug)]
pub struct Renewed {
    pub expires_in: u64,
}
/// One or more sessions staff ended: every socket of a matching one closes.
#[derive(Clone, Debug)]
pub struct Revoked {
    pub tenant: i64,
    pub channel: String,
    /// Set when one session was ended, `None` when every session of the
    /// subject was.
    pub session: Option<String>,
    pub subject: Option<String>,
}

pub struct Visitors {
    settings: Arc<Settings>,
    policy: Mutex<Option<(Instant, Policy)>>,
    revoked: broadcast::Sender<Revoked>,
}
impl Visitors {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self {
            settings,
            policy: Mutex::default(),
            revoked: broadcast::channel(64).0,
        }
    }

    /// The operator's current limits. Every frame of every open socket asks,
    /// so a reading is reused for [`POLICY_FRESH`]: a changed setting applies
    /// within seconds.
    pub async fn policy(&self, client: &(impl GenericClient + Sync)) -> Result<Policy> {
        if let Some((read, policy)) = *self.policy.lock().unwrap_or_else(|p| p.into_inner()) {
            if read.elapsed() < POLICY_FRESH {
                return Ok(policy);
            }
        }
        let policy = self.read_policy(client).await?;
        *self.policy.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), policy));
        Ok(policy)
    }
    async fn read_policy(&self, client: &(impl GenericClient + Sync)) -> Result<Policy> {
        let read = |key: &'static str| async move { self.number(client, key).await };
        let narrow = |value: u64| u32::try_from(value).map_err(|_| Error::Internal);
        Ok(Policy {
            session_seconds: read(SESSION_MINUTES_KEY).await? * 60,
            session_max_seconds: read(SESSION_MAX_HOURS_KEY).await? * 3_600,
            sessions_per_sender: narrow(read(SESSIONS_PER_SENDER_KEY).await?)?,
            sessions_per_site: narrow(read(SESSIONS_PER_SITE_KEY).await?)?,
            messages_per_minute: narrow(read(MESSAGES_PER_MINUTE_KEY).await?)?,
            sockets_per_session: narrow(read(SOCKETS_PER_SESSION_KEY).await?)?,
            sockets_total: narrow(read(SOCKETS_TOTAL_KEY).await?)?,
            upload_max_bytes: read(UPLOAD_MAX_BYTES_KEY).await?,
            uploads_per_minute: narrow(read(UPLOADS_PER_MINUTE_KEY).await?)?,
        })
    }

    async fn number(&self, client: &(impl GenericClient + Sync), key: &str) -> Result<u64> {
        self.settings
            .get_global(client, key)
            .await?
            .as_u64()
            .ok_or(Error::Internal)
    }

    /// Starts a session for the visitor a channel just admitted, on the origin
    /// a site's allow-list accepted. The tenant must be live. Sessions that
    /// ended or expired more than a day ago are removed in a bounded batch.
    pub async fn open(
        &self,
        client: &(impl GenericClient + Sync),
        admission: &Admission,
        channel: &str,
        subject: &str,
    ) -> Result<Opened> {
        if !valid_key(channel) || !channel::printable(subject, SUBJECT_MAX) {
            return Err(Error::Invalid);
        }
        let policy = self.policy(client).await?;
        let lifetime = policy.session_seconds.min(policy.session_max_seconds);
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|_| Error::Internal)?;
        let token = format!("{TOKEN_PREFIX}{}", hex(&raw));
        let digest = digest(&token);
        client
            .execute(
                "DELETE FROM visitor_sessions WHERE id IN (SELECT id FROM visitor_sessions \
                 WHERE absolute_expires_at < now() - make_interval(days => $1::int) \
                 OR revoked_at < now() - make_interval(days => $1::int) LIMIT $2)",
                &[&(KEEP_DAYS as i32), &PRUNE_BATCH],
            )
            .await?;
        let row = client
            .query_opt(
                "INSERT INTO visitor_sessions (tenant_id, channel, site, subject, origin, token_digest, expires_at, absolute_expires_at) \
                 SELECT t.id, $2, $3, $4, $5, $6, \
                        now() + make_interval(secs => $7::double precision), \
                        now() + make_interval(secs => $8::double precision) \
                 FROM tenants t WHERE t.id = $1 AND t.status = 'active' AND t.deleted_at IS NULL \
                 RETURNING id::text",
                &[
                    &admission.tenant,
                    &channel,
                    &admission.site,
                    &subject,
                    &admission.origin,
                    &digest.as_slice(),
                    &(lifetime as f64),
                    &(policy.session_max_seconds as f64),
                ],
            )
            .await?
            .ok_or(Error::Forbidden)?;
        let session: String = row.get(0);
        record_action(
            client,
            &guest(admission.tenant),
            None,
            "visitor.session.opened",
            json!({"session": session, "channel": channel, "site": admission.site}),
            false,
        )
        .await?;
        Ok(Opened {
            token,
            session,
            expires_in: lifetime,
        })
    }

    /// Proves a token for the browser origin that presents it, without
    /// writing. The row must be live (not ended, expired or past its limit),
    /// its tenant active, and `origin` the one the session was issued to.
    pub async fn check(
        &self,
        client: &(impl GenericClient + Sync),
        token: &str,
        origin: Option<&str>,
    ) -> Result<VisitorScope> {
        let digest = well_formed(token)?;
        let row = client
            .query_opt(
                "SELECT v.id::text, v.tenant_id, v.channel, v.site, v.subject, v.origin \
                 FROM visitor_sessions v JOIN tenants t ON t.id = v.tenant_id AND t.status = 'active' AND t.deleted_at IS NULL \
                 WHERE v.token_digest = $1 AND v.revoked_at IS NULL AND v.expires_at > now() AND v.absolute_expires_at > now()",
                &[&digest.as_slice()],
            )
            .await?
            .ok_or(Error::Unauthorized)?;
        let issued: String = row.get(5);
        if origin != Some(issued.as_str()) {
            return Err(Error::Forbidden);
        }
        Ok(VisitorScope {
            session: row.get(0),
            tenant: row.get(1),
            channel: row.get(2),
            site: row.get(3),
            subject: row.get(4),
        })
    }

    /// Renews a live session for another lifetime, never past its limit.
    pub async fn renew(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &VisitorScope,
    ) -> Result<Renewed> {
        let policy = self.policy(client).await?;
        let row = client
            .query_opt(
                "UPDATE visitor_sessions SET renewed_at = now(), \
                   expires_at = LEAST(now() + make_interval(secs => $3::double precision), absolute_expires_at) \
                 WHERE id = $1::text::uuid AND tenant_id = $2 AND revoked_at IS NULL AND expires_at > now() AND absolute_expires_at > now() \
                 RETURNING GREATEST(0, EXTRACT(EPOCH FROM expires_at - now()))::bigint",
                &[&scope.session, &scope.tenant, &(policy.session_seconds as f64)],
            )
            .await?
            .ok_or(Error::Unauthorized)?;
        Ok(Renewed {
            expires_in: row.get::<_, i64>(0).max(0) as u64,
        })
    }

    /// The visitor ends their own session.
    pub async fn end(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &VisitorScope,
    ) -> Result<()> {
        let ended = client
            .execute(
                "UPDATE visitor_sessions SET revoked_at = now(), revoked_reason = 'visitor' \
                 WHERE id = $1::text::uuid AND tenant_id = $2 AND revoked_at IS NULL",
                &[&scope.session, &scope.tenant],
            )
            .await?;
        if ended == 1 {
            record_action(
                client,
                &guest(scope.tenant),
                None,
                "visitor.session.ended",
                json!({"session": scope.session, "channel": scope.channel, "site": scope.site}),
                false,
            )
            .await?;
        }
        let _ = self.revoked.send(Revoked {
            tenant: scope.tenant,
            channel: scope.channel.clone(),
            session: Some(scope.session.clone()),
            subject: None,
        });
        Ok(())
    }

    /// Staff end one visitor session of their own tenant: it is refused from
    /// then on and its open sockets close. The Domain checks its own
    /// capability first; this proves only that `scope` is a live staff scope
    /// and records who ended it. Returns whether a live session was ended.
    pub async fn revoke_session(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        channel: &str,
        session: &str,
    ) -> Result<bool> {
        let id = uuid_text(session)?;
        let ended = client
            .execute(
                "UPDATE visitor_sessions SET revoked_at = now(), revoked_reason = 'staff' \
                 WHERE id = $1::text::uuid AND tenant_id = $2 AND channel = $3 AND revoked_at IS NULL",
                &[&id, &scope.tenant(), &channel],
            )
            .await?;
        self.revoked_by(client, scope, channel, Some(id), None, ended)
            .await?;
        Ok(ended == 1)
    }

    /// Staff end every live session of one subject (one conversation) of
    /// their own tenant. Returns how many.
    pub async fn revoke_subject(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        channel: &str,
        subject: &str,
    ) -> Result<u64> {
        let ended = client
            .execute(
                "UPDATE visitor_sessions SET revoked_at = now(), revoked_reason = 'staff' \
                 WHERE tenant_id = $1 AND channel = $2 AND subject = $3 AND revoked_at IS NULL",
                &[&scope.tenant(), &channel, &subject],
            )
            .await?;
        self.revoked_by(
            client,
            scope,
            channel,
            None,
            Some(subject.to_owned()),
            ended,
        )
        .await?;
        Ok(ended)
    }

    async fn revoked_by(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        channel: &str,
        session: Option<String>,
        subject: Option<String>,
        ended: u64,
    ) -> Result<()> {
        if ended > 0 {
            record_action(
                client,
                &Attribution::from_scope(scope),
                None,
                "visitor.session.revoked",
                json!({"channel": channel, "session": session, "subject": subject, "ended": ended}),
                false,
            )
            .await?;
        }
        let _ = self.revoked.send(Revoked {
            tenant: scope.tenant(),
            channel: channel.to_owned(),
            session,
            subject,
        });
        Ok(())
    }

    /// Wakes a host whose sockets may hold a session just ended; a socket
    /// also rechecks its session on every frame and on a timer.
    pub fn subscribe_revocations(&self) -> broadcast::Receiver<Revoked> {
        self.revoked.subscribe()
    }
}

/// A public caller is a guest of the tenant its site belongs to.
fn guest(tenant: i64) -> Attribution {
    Attribution {
        tenant: Some(tenant),
        actor_type: "guest",
        actor_id: 0,
        impersonator_id: None,
        actor_role: None,
        system_principal: None,
        trace_id: None,
        agent_self_report: None,
        client_identity: None,
    }
}
fn well_formed(token: &str) -> Result<[u8; 32]> {
    if token.len() != TOKEN_LEN
        || !token.starts_with(TOKEN_PREFIX)
        || !token[TOKEN_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Unauthorized);
    }
    Ok(digest(token))
}
fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
/// `text` as a lowercase hyphenated UUID, or `Invalid`: never a request's
/// text straight into SQL.
fn uuid_text(text: &str) -> Result<String> {
    let shape = [8, 4, 4, 4, 12];
    let groups: Vec<&str> = text.split('-').collect();
    if groups.len() == 5
        && groups.iter().zip(shape).all(|(group, len)| {
            group.len() == len
                && group
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        Ok(text.to_owned())
    } else {
        Err(Error::Invalid)
    }
}

inventory::submit! {
    bilibi_module_registry::Module {
        id: "base/visitor", layer: bilibi_module_registry::Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/tenancy", "base/settings", "base/authz", "base/audit"],
        migrations: &[("base-visitor-001-sessions", include_str!("../migrations/001-sessions.sql"))],
        services: &[
            bilibi_module_registry::Service { id: SERVICE, factory: |services| Ok(Arc::new(Visitors::new(services.get("base/settings.store")?))) },
        ],
    }
}
