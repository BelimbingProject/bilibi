//! Password change, forgotten-password request and reset. On the staff form
//! the address alone names the account (unique platform-wide); on a portal
//! (ADR 16) it names one of that tenant's customers. A reset is issued and
//! redeemed only for an account whose standing (`identity::standing`) is live.
use crate::{
    identity::Audience,
    password::{self, check_policy},
    sign_in::PRINCIPAL,
    CredentialError, UserService,
};
use bilibi_agent_api::{
    Channels, Handler, Operation, OperationClient, OperationFuture, WriteHandler,
};
use bilibi_database::{Change, Context, Db, Error, MutationBoundary, MutationTransaction};
use bilibi_mail::{Address, Mailer, Message};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{Definition, ScopeKind, Setting, Settings, Target, ValueType, DEFINITIONS};
use bilibi_templates::Value as Fact;
use bilibi_tenancy::{ReadClient, Scope};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use subtle::ConstantTimeEq;

const RESET_LIFETIME: &str = "user.password_reset_minutes";
pub(crate) const MANAGE_LIFETIME: &str = "admin.user.password-reset-lifetime.manage";
const MIN_MINUTES: i64 = 15;
const MAX_MINUTES: i64 = 10080;
inventory::submit! { Contribution::new(DEFINITIONS, RESET_LIFETIME, Provenance { module: "core/user", source: file!(), line: line!() }, &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global], default: || json!(4320),
    nullable: false, encrypted: false, minimum: Some(MIN_MINUTES as f64), maximum: Some(MAX_MINUTES as f64), validator: None,
    editor: None,
})) }

static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();

pub(crate) fn bind(settings: Arc<Settings>) -> bilibi_module_registry::Result<()> {
    SETTINGS.set(settings).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate reset settings binding".into())
    })
}

struct SetLifetime;

fn lifetime_minutes(input: &Value) -> Result<i64, Error> {
    input["minutes"]
        .as_i64()
        .filter(|minutes| (MIN_MINUTES..=MAX_MINUTES).contains(minutes))
        .ok_or(Error::Invalid)
}

impl WriteHandler for SetLifetime {
    fn owner(&self) -> &'static str {
        "core/user"
    }

    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_proof(client, scope, MANAGE_LIFETIME).await?;
            let minutes = lifetime_minutes(input)?;
            Ok(json!({
                "summary": "Set how long newly issued password reset links can be used",
                "minutes": minutes
            })
            .into())
        })
    }

    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_operator(tx.sql(), scope, MANAGE_LIFETIME).await?;
            let minutes = lifetime_minutes(input)?;
            SETTINGS
                .get()
                .ok_or(Error::Internal)?
                .put(tx, scope, Target::Global, RESET_LIFETIME, json!(minutes))
                .await?;
            Ok(json!({"minutes": minutes}).into())
        })
    }
}

inventory::submit! { Contribution::new(bilibi_agent_api::DEFINITIONS, "core/user.reset-settings", Provenance { module: "core/user", source: file!(), line: line!() }, &bilibi_agent_api::Definitions {
    operations: &[Operation {
        key: "core.user.password_reset_lifetime.set",
        title: "Set the password reset link lifetime",
        summary: "Set how long newly issued staff and portal password reset links can be used across the installation.",
        keywords: &["password", "reset", "recovery", "expiry"],
        capability: MANAGE_LIFETIME,
        input: || json!({"type":"object","required":["minutes"],"properties":{"minutes":{"type":"integer","minimum":MIN_MINUTES,"maximum":MAX_MINUTES}}}),
        output: || json!({"type":"object","properties":{"minutes":{"type":"integer"}}}),
        handler: Handler::SafeWrite(&SetLifetime),
        channels: Channels::Every,
        step_up: &[],
    }],
    guides: &[],
}) }
/// A new link may be requested once a minute.
const RESET_THROTTLE_SECONDS: f64 = 60.0;

pub(crate) fn normalize_email(email: &str) -> Result<String, Error> {
    let email = email.trim().to_lowercase();
    if email.len() > 254 || !email.contains('@') || email.chars().any(char::is_whitespace) {
        return Err(Error::Invalid);
    }
    Ok(email)
}
pub(crate) fn random_token() -> Result<String, Error> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::Internal)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}
/// Everything a reset mail needs, owned, so a mail can be issued from a
/// task that outlives the request (the sign-in lock notice is).
#[derive(Clone)]
pub(crate) struct ResetMail {
    pub(crate) db: Db,
    pub(crate) mutations: Arc<MutationBoundary>,
    mailer: Arc<Mailer>,
    settings: Arc<bilibi_settings::Settings>,
    templates: Arc<bilibi_templates::Templates>,
    origin: String,
}

impl ResetMail {
    /// A new one-time reset token for the account, stored hashed, unless one
    /// was issued within the throttle (one a minute). A sign-in has one slot
    /// (`password_reset_tokens` is keyed by identity), so storage stays
    /// bounded; an expired token only fails to redeem until it is replaced.
    /// Audited as `user.reset_requested` by the `sign-in` principal; the
    /// token and its digest never enter the change.
    async fn issue_token(
        &self,
        tenant: i64,
        user: i64,
        address: &str,
        verified: bool,
    ) -> Result<Option<String>, Error> {
        let token = random_token()?;
        let hash = digest(&token);
        let mut client = self.db.get().await?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(tenant, PRINCIPAL, "user.reset_requested"),
            )
            .await?;
        let lifetime = self
            .settings
            .get_global(tx.sql(), RESET_LIFETIME)
            .await?
            .as_i64()
            .ok_or(Error::Internal)? as f64
            * 60.0;
        let address = address.to_owned();
        let issued = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let identity: i64 = sql
                        .query_opt(
                            "INSERT INTO password_reset_tokens(identity_id,token_hash,created_at,expires_at) SELECT i.id,$3,now(),now()+make_interval(secs => $4::double precision) FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1 AND u.id=$2 AND u.deleted_at IS NULL AND u.status='active' AND i.deleted_at IS NULL AND i.status='active' AND i.email=$6 AND (NOT $7 OR i.email_verified_at IS NOT NULL) ON CONFLICT (identity_id) DO UPDATE SET token_hash=excluded.token_hash,created_at=excluded.created_at,expires_at=excluded.expires_at WHERE password_reset_tokens.created_at<=now()-make_interval(secs => $5::double precision) RETURNING identity_id",
                            &[&tenant, &user, &hash, &lifetime, &RESET_THROTTLE_SECONDS, &address, &verified],
                        )
                        .await?
                        .ok_or(Error::Conflict)?
                        .get(0);
                    Ok((
                        (),
                        vec![token_change(identity, "issued")],
                    ))
                })
            })
            .await;
        match issued {
            Ok(()) => {
                tx.commit().await?;
                Ok(Some(token))
            }
            // Throttled: a token issued within the last minute stands.
            Err(Error::Conflict) => {
                tx.rollback().await?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Delivers `message` after the caller returns; a failed delivery
    /// withdraws the token it carried (audited as
    /// `user.reset_withdrawn`), which releases the throttle so the person
    /// can ask again.
    fn deliver(&self, tenant: i64, user: i64, message: Message, reset: Option<(&str, String)>) {
        let mail = self.clone();
        let (hash, action) = match reset {
            Some((token, link)) => (Some(digest(token)), Some(link)),
            None => (None, None),
        };
        tokio::spawn(async move {
            let Err(error) = mail
                .mailer
                .deliver_linked_for(tenant, &message, action.as_deref())
                .await
            else {
                return;
            };
            bilibi_module_registry::server_log!(
                Warning,
                "mail: reset link for user {user} was not delivered: {error}"
            );
            if let Some(hash) = hash {
                if let Err(error) = mail.withdraw(tenant, user, hash).await {
                    bilibi_module_registry::server_log!(
                        Warning,
                        "mail: reset link for user {user} was not withdrawn: {error}"
                    );
                }
            }
        });
    }

    async fn withdraw(&self, tenant: i64, user: i64, hash: Vec<u8>) -> Result<(), Error> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(tenant, PRINCIPAL, "user.reset_withdrawn"),
            )
            .await?;
        let withdrawn = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let identity: i64 = sql
                        .query_opt(
                            "DELETE FROM password_reset_tokens WHERE identity_id=(SELECT identity_id FROM users WHERE tenant_id=$1 AND id=$2) AND token_hash=$3 RETURNING identity_id",
                            &[&tenant, &user, &hash],
                        )
                        .await?
                        .ok_or(Error::NotFound)?
                        .get(0);
                    Ok(((), vec![token_change(identity, "withdrawn")]))
                })
            })
            .await;
        match withdrawn {
            Ok(()) => tx.commit().await,
            // Already redeemed or replaced: nothing to withdraw.
            Err(Error::NotFound) => tx.rollback().await,
            Err(error) => Err(error),
        }
    }

    /// Tells the account its sign-in is locked for about `minutes`, with a
    /// reset link appended when a token can be issued now (a request within
    /// the last minute already holds one; the mail then points to "Forgot
    /// your password?"). A failed delivery releases the token, as for a
    /// requested reset.
    pub(crate) async fn mail_lock_notice(
        &self,
        tenant: i64,
        user: i64,
        minutes: i64,
    ) -> Result<(), Error> {
        let client = self.db.get().await?;
        let Some(row) = client
            .query_opt(
                "SELECT u.name,i.email FROM users u JOIN user_identities i ON i.id=u.identity_id AND i.deleted_at IS NULL AND i.status='active' WHERE u.tenant_id=$1 AND u.id=$2 AND u.deleted_at IS NULL AND u.status='active'",
                &[&tenant, &user],
            )
            .await?
        else {
            return Ok(());
        };
        drop(client);
        let (name, address): (String, String) = (row.get(0), row.get(1));
        let duration = format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" });
        let wording = self.templates.platform_wording(
            crate::wording::SIGN_IN_LOCKED,
            &[("duration", Fact::Text(&duration))],
        )?;
        let token = self.issue_token(tenant, user, &address, false).await?;
        let reset = token
            .as_deref()
            .map(|token| (token, reset_link(&self.origin, &address, token)));
        let link = match &reset {
            Some((_, url)) => format!("{}\n\n{url}\n", wording["reset_with_link"]),
            None => format!("{}\n", wording["reset_without_link"]),
        };
        let message = Message::text(
            Address::new(Some(&name), &address).map_err(|_| Error::Invalid)?,
            wording["subject"].clone(),
            format!("{}\n\n{link}", wording["intro"]),
        );
        self.deliver(tenant, user, message, reset);
        Ok(())
    }

    /// Mails `text` to the verified account with a reset link appended. A
    /// failed delivery releases the token, as for a requested reset.
    pub(crate) async fn mail_with_reset_link(
        &self,
        tenant: i64,
        user: i64,
        subject: &str,
        text: &str,
        verified: bool,
    ) -> Result<(), Error> {
        let client = self.db.get().await?;
        let Some(row) = client
            .query_opt(
                "SELECT u.name,i.email FROM users u JOIN user_identities i ON i.id=u.identity_id AND i.deleted_at IS NULL AND i.status='active' WHERE u.tenant_id=$1 AND u.id=$2 AND u.deleted_at IS NULL AND u.status='active' AND (NOT $3 OR i.email_verified_at IS NOT NULL)",
                &[&tenant, &user, &verified],
            )
            .await?
        else {
            return Ok(());
        };
        drop(client);
        let (name, address): (String, String) = (row.get(0), row.get(1));
        let token = self.issue_token(tenant, user, &address, verified).await?;
        let reset = token
            .as_deref()
            .map(|token| (token, reset_link(&self.origin, &address, token)));
        let link = match &reset {
            Some((_, url)) => format!("Reset your password with this link:\n\n{url}\n"),
            None => {
                "Reset your password with \"Forgot your password?\" on the sign-in page.\n".into()
            }
        };
        let message = Message::text(
            Address::new(Some(&name), &address).map_err(|_| Error::Invalid)?,
            subject,
            format!("{text}\n\n{link}"),
        );
        self.deliver(tenant, user, message, reset);
        Ok(())
    }

    /// Mails the server owner the one-time link that claims the account: the
    /// owner chooses its password there, so only the holder of the address
    /// can sign in. Waits for delivery so the server command can report it.
    /// A link issued within the last minute stands ([`ClaimMail::RecentlySent`]);
    /// a failed delivery withdraws the token so a retry can send again.
    pub(crate) async fn mail_owner_claim(
        &self,
        tenant: i64,
        user: i64,
        name: &str,
        address: &str,
    ) -> Result<ClaimMail, Error> {
        let Some(token) = self.issue_token(tenant, user, address, false).await? else {
            return Ok(ClaimMail::RecentlySent);
        };
        let origin = &self.origin;
        let link = reset_link(origin, address, &token);
        let message = Message::text(
            Address::new(Some(name), address).map_err(|_| Error::Invalid)?,
            "Claim your Bilibi server",
            format!(
                "You are the owner of the Bilibi server at {origin}.\n\n\
                 1. Choose your password with this link:\n\n{link}\n\n\
                 2. Sign in, then set up two-factor authentication with your phone:\n\n{origin}/settings/two-factor?method=core.user.authenticator\n\n\
                 The server is yours once both are done. The link works once. If you did not expect this email, ignore it: nobody can sign in as you without it.\n",
            ),
        );
        match self
            .mailer
            .deliver_linked_for(tenant, &message, Some(&link))
            .await
        {
            Ok(_) => Ok(ClaimMail::Sent),
            Err(error) => {
                self.withdraw(tenant, user, digest(&token)).await?;
                Ok(ClaimMail::NotDelivered(error.to_string()))
            }
        }
    }
}

/// What [`ResetMail::mail_owner_claim`] did.
pub(crate) enum ClaimMail {
    Sent,
    RecentlySent,
    /// The transport refused; the detail is for the operator.
    NotDelivered(String),
}

/// The audit change of a reset token: only that it was issued or withdrawn.
fn token_change(identity: i64, reset_token: &str) -> Change {
    Change {
        kind: "user_identity".into(),
        id: identity,
        old: json!({}),
        new: json!({"reset_token": reset_token}),
    }
}

/// Where the emailed link points; the reset page reads both values.
fn reset_link(origin: &str, email: &str, token: &str) -> String {
    format!(
        "{origin}/reset-password/{token}?email={}",
        encode_component(email)
    )
}

async fn finish_reset_request() -> Result<(), CredentialError> {
    // Every normal outcome pays for this same expensive check so the public
    // response reveals less about whether the address has an active account.
    password::verify("", None).await?;
    Ok(())
}
/// The account a forgotten-password request mails and its link redeems for:
/// per sign-in under the address, the one `identity::password_identity`
/// signs in to (last used, then the lowest live tenant), so a removed
/// or suspended last tenant never strands the request. Columns: account,
/// tenant, account name, address, identity, live token digest.
const RESET_ACCOUNT: &str = "SELECT DISTINCT ON (i.id) u.id,u.tenant_id,u.name,i.email,i.id,r.token_hash FROM user_identities i JOIN users u ON u.identity_id=i.id JOIN tenants t ON t.id=u.tenant_id LEFT JOIN password_reset_tokens r ON r.identity_id=i.id AND r.expires_at>now() WHERE lower(i.email)=$1 AND i.kind=$2 AND u.kind=$2 AND ($3::bigint IS NULL OR i.tenant_id=$3) AND i.deleted_at IS NULL AND i.status='active' AND u.deleted_at IS NULL AND u.status='active' AND t.deleted_at IS NULL AND t.status='active' ORDER BY i.id,(u.tenant_id=i.last_tenant_id) DESC,u.tenant_id LIMIT 2";

fn encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Whether `origin` is a bare `https://host[:port]` (or `http://` for a
/// development copy), so a reset link never carries anything else.
pub(crate) fn origin_shaped(origin: &str) -> bool {
    url::Url::parse(origin).is_ok_and(|url| {
        matches!(url.scheme(), "https" | "http")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && !origin.ends_with('/')
    })
}

impl UserService {
    /// Self-service change: the session proves who, the current password proves
    /// the holder. Other sessions of the account end; this one continues.
    pub async fn change_password(
        &self,
        token: &str,
        csrf: &str,
        current: &str,
        new: &str,
    ) -> Result<(), CredentialError> {
        if current.len() > password::MAX_LENGTH {
            return Err(CredentialError::IncorrectPassword);
        }
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.sessions.activity(&**client, token).await?;
        if scope.csrf != csrf {
            return Err(Error::Forbidden.into());
        }
        self.sessions
            .require_portable_authority(&**client, &scope)
            .await?;
        check_policy(new).map_err(CredentialError::Policy)?;
        let (tenant, user) = (scope.tenant(), scope.user());
        let credential = client
            .query_opt(
                "SELECT i.password_hash,i.id FROM users u JOIN user_identities i ON i.id=u.identity_id AND i.status='active' AND i.deleted_at IS NULL WHERE u.tenant_id=$1 AND u.id=$2 AND u.status='active' AND u.deleted_at IS NULL",
                &[&tenant, &user],
            )
            .await
            .map_err(Error::from)?
            .ok_or(Error::Unauthorized)?;
        let (stored, identity): (String, i64) = (credential.get(0), credential.get(1));
        if !password::verify(current, Some(&stored)).await? {
            return Err(CredentialError::IncorrectPassword);
        }
        let replacement = password::hash(new).await?;
        let mut tx = self
            .mutations
            .begin(&mut client, scope.mutation_context("user.change_password"))
            .await?;
        self.sessions
            .require_portable_authority(tx.sql(), &scope)
            .await?;
        tx.mutate(move |sql| {
            Box::pin(async move {
                // Compare-and-set: a concurrent change makes this one a conflict.
                let changed = sql
                    .execute(
                        "UPDATE user_identities SET password_hash=$3,password_version=password_version+1,updated_at=now() WHERE id=(SELECT identity_id FROM users WHERE tenant_id=$1 AND id=$2) AND password_hash=$4",
                        &[&tenant, &user, &replacement, &stored],
                    )
                    .await?;
                if changed != 1 {
                    return Err(Error::Conflict);
                }

                sql.execute(
                    "DELETE FROM password_reset_tokens WHERE identity_id=(SELECT identity_id FROM users WHERE tenant_id=$1 AND id=$2)",
                    &[&tenant, &user],
                )
                .await?;
                Ok((
                    (),
                    vec![Change {
                        kind: "user_identity".into(),
                        id: identity,
                        old: json!({"password_hash":"previous"}),
                        new: json!({"password_hash":"replaced"}),
                    }],
                ))
            })
        })
        .await?;
        // Other live pages of this account hear it once the change is committed.
        let ended = crate::sign_in_identity::end_everywhere(
            &self.sessions,
            &mut tx,
            identity,
            user,
            Some(token),
        )
        .await?;
        let sessions = self.sessions.clone();
        tx.after_commit(move || sessions.announce(ended));
        tx.commit().await?;
        Ok(())
    }

    /// Always answers `Ok` for an unknown, frozen or throttled account so the
    /// public form gives one response. Every normal outcome performs the same
    /// dummy password verification to reduce account-dependent timing clues.
    /// Mail delivery is asynchronous and is never awaited here.
    pub async fn request_password_reset(&self, email: &str) -> Result<(), CredentialError> {
        self.request_reset(Audience::Staff, None, email).await
    }
    /// [`UserService::request_password_reset`] for one audience; a customer's
    /// link opens the portal's reset page on the public site at `origin`,
    /// the one the request came to.
    pub(crate) async fn request_reset(
        &self,
        audience: Audience,
        origin: Option<&str>,
        email: &str,
    ) -> Result<(), CredentialError> {
        let email = normalize_email(email)?;
        let public = origin;
        let client = self.db.get().await.map_err(Error::from)?;
        // The account sign-in would choose (`identity::password_identity`):
        // last used, then the lowest live membership. Ambiguity (two
        // sign-ins under one address, possible only for customers) is
        // treated as unknown rather than guessed; a frozen account
        // (archived company, retired identity) is unknown here too.
        let candidates = client
            .query(
                RESET_ACCOUNT,
                &[&email, &audience.kind().as_str(), &audience.tenant()],
            )
            .await
            .map_err(Error::from)?;
        let [account] = &candidates[..] else {
            return finish_reset_request().await;
        };
        let (user, tenant, name, address): (i64, i64, String, String) = (
            account.get(0),
            account.get(1),
            account.get(2),
            account.get(3),
        );
        if !matches!(
            crate::identity::standing(&**client, tenant, user).await?,
            crate::identity::Standing::Live(_)
        ) {
            return finish_reset_request().await;
        }
        drop(client);
        let mail = self.reset_mail();
        let wording = self
            .templates
            .platform_wording(crate::wording::PASSWORD_RESET, &[])?;
        let Some(token) = mail.issue_token(tenant, user, &address, false).await? else {
            return finish_reset_request().await;
        };
        let link = match public {
            Some(origin) => format!(
                "{origin}/portal/reset/{token}?email={}",
                encode_component(&address)
            ),
            None => self.reset_link(&address, &token),
        };
        let message = Message::text(
            Address::new(Some(&name), &address).map_err(|_| Error::Invalid)?,
            wording["subject"].clone(),
            crate::wording::around_link(&wording, &link),
        );
        mail.deliver(tenant, user, message, Some((&token, link)));
        finish_reset_request().await
    }

    /// The owned handles a reset mail is issued and delivered with.
    pub(crate) fn reset_mail(&self) -> ResetMail {
        ResetMail {
            db: self.db.clone(),
            mutations: self.mutations.clone(),
            mailer: self.mailer.clone(),
            settings: self.settings.clone(),
            templates: self.templates.clone(),
            origin: self.origin.clone(),
        }
    }

    /// Where the emailed link points; the reset page reads both values.
    pub fn reset_link(&self, email: &str, token: &str) -> String {
        reset_link(&self.origin, email, token)
    }

    /// Check a staff reset link before asking for a new password. Redemption
    /// repeats this check inside its mutation, so concurrent use still loses.
    pub async fn reset_link_valid(&self, email: &str, token: &str) -> Result<bool, Error> {
        self.reset_link_valid_for(Audience::Staff, email, token)
            .await
    }

    pub(crate) async fn reset_link_valid_for(
        &self,
        audience: Audience,
        email: &str,
        token: &str,
    ) -> Result<bool, Error> {
        let Ok(email) = normalize_email(email) else {
            return Ok(false);
        };
        if token.is_empty() || token.len() > 128 {
            return Ok(false);
        }
        let client = self.db.get().await?;
        Ok(reset_account(&**client, audience, &email, &digest(token))
            .await?
            .is_some())
    }

    /// Consumes the one-time token, replaces the password and ends every
    /// session of the account. Unknown, frozen, expired and reused tokens are
    /// indistinguishable.
    pub async fn reset_password(
        &self,
        email: &str,
        token: &str,
        new: &str,
    ) -> Result<(), CredentialError> {
        self.reset(Audience::Staff, email, token, new).await
    }
    /// [`UserService::reset_password`] for one audience.
    pub(crate) async fn reset(
        &self,
        audience: Audience,
        email: &str,
        token: &str,
        new: &str,
    ) -> Result<(), CredentialError> {
        let email = normalize_email(email).map_err(|_| CredentialError::InvalidOrExpiredToken)?;
        if token.is_empty() || token.len() > 128 {
            return Err(CredentialError::InvalidOrExpiredToken);
        }
        let mut client = self.db.get().await.map_err(Error::from)?;
        let presented = digest(token);
        let Some((user, tenant, identity)) =
            reset_account(&**client, audience, &email, &presented).await?
        else {
            password::verify("", None).await?;
            return Err(CredentialError::InvalidOrExpiredToken);
        };
        check_policy(new).map_err(CredentialError::Policy)?;
        let replacement = password::hash(new).await?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::user(tenant, user, "user.reset_password"),
            )
            .await?;
        let claimed = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    // Deleting the row is the single-use claim; a racing reset loses.
                    let claimed = sql
                        .execute(
                            "DELETE FROM password_reset_tokens WHERE identity_id=$1 AND token_hash=$2 AND expires_at>now()",
                            &[&identity, &presented],
                        )
                        .await?;
                    if claimed != 1 {
                        return Err(Error::NotFound);
                    }
                    sql.execute(
                        "UPDATE user_identities SET password_hash=$2,password_version=password_version+1,updated_at=now() WHERE id=$1",
                        &[&identity, &replacement],
                    )
                    .await?;
                    let mut changes = vec![Change {
                        kind: "user_identity".into(),
                        id: identity,
                        old: json!({"password_hash":"previous"}),
                        new: json!({"password_hash":"replaced"}),
                    }];
                    // The emailed link proves the holder: a sign-in lock ends.
                    let kind = crate::credential_attempts::Kind::SignIn;
                    if crate::credential_attempts::forget(sql, tenant, user, kind).await? {
                        changes.push(crate::credential_attempts::change(
                            user,
                            kind,
                            json!({}),
                            json!({"cleared_by": "password_reset"}),
                        ));
                    }
                    Ok(((), changes))
                })
            })
            .await;
        match claimed {
            Ok(()) => {
                let ended = crate::sign_in_identity::end_everywhere(
                    &self.sessions,
                    &mut tx,
                    identity,
                    user,
                    None,
                )
                .await?;
                let sessions = self.sessions.clone();
                tx.after_commit(move || sessions.announce(ended));
                Ok(tx.commit().await?)
            }
            Err(Error::NotFound) => Err(CredentialError::InvalidOrExpiredToken),
            Err(error) => Err(error.into()),
        }
    }
}

async fn reset_account(
    client: &dyn ReadClient,
    audience: Audience,
    email: &str,
    presented: &[u8],
) -> Result<Option<(i64, i64, i64)>, Error> {
    let accounts = client
        .query(
            RESET_ACCOUNT,
            &[&email, &audience.kind().as_str(), &audience.tenant()],
        )
        .await?;
    let [account] = &accounts[..] else {
        return Ok(None);
    };
    let stored: Option<Vec<u8>> = account.get(5);
    if !stored.is_some_and(|stored| bool::from(stored.ct_eq(presented))) {
        return Ok(None);
    }
    let (user, tenant) = (account.get(0), account.get(1));
    if !matches!(
        crate::identity::standing(client, tenant, user).await?,
        crate::identity::Standing::Live(_)
    ) {
        return Ok(None);
    }
    Ok(Some((user, tenant, account.get(4))))
}
