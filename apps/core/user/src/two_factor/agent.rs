//! Installation-owner recovery for a lost second factor. Customer tenant
//! administrators have no operation or page control over the shared factor.
use super::SETTINGS_PATH;
use crate::{notifications, reset::normalize_email, UserService};
use bilibi_database::{Context, Error, MutationTransaction};
use bilibi_mail::{Address, Message};
use bilibi_notifications::Notice;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

static USERS: OnceLock<Arc<UserService>> = OnceLock::new();

pub(crate) fn bind(users: Arc<UserService>) -> Result<(), bilibi_module_registry::RegistryError> {
    USERS.set(users).map_err(|_| {
        bilibi_module_registry::RegistryError::Service(
            "core/user two-factor reset bound twice".into(),
        )
    })
}
/// What `bilibi-web two-factor-reset` did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TwoFactorEmailReset {
    /// The authenticator was on and is now off. The account was signed out.
    Reset,
    /// The account has no confirmed authenticator. Nothing was written.
    AlreadyOff,
}
#[derive(Debug, thiserror::Error)]
pub enum TwoFactorEmailError {
    #[error("No account uses that email.")]
    NotFound,
    #[error("That email address is not valid.")]
    Invalid,
    #[error(transparent)]
    Other(#[from] Error),
}

struct Account {
    id: i64,
    tenant: i64,
    name: String,
    email: String,
    active: bool,
}

/// Staff accounts only: a customer's factor is not a staff reset (ADR 16).
const TARGET_SQL: &str =
    "SELECT DISTINCT ON (i.id) u.id,u.tenant_id,u.name,i.email,u.status='active' FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.kind='staff'";

fn account_from(row: &tokio_postgres::Row) -> Account {
    Account {
        id: row.get(0),
        tenant: row.get(1),
        name: row.get(2),
        email: row.get(3),
        active: row.get(4),
    }
}

fn reset_notice(body: &str) -> Result<Notice, Error> {
    let notice = Notice::new(crate::wording::TWO_FACTOR_SUBJECT)
        .kind("two_factor_reset")
        .body(body)
        .url(SETTINGS_PATH);
    notice.validate()?;
    Ok(notice)
}

/// The packaged reset email; `slot` says who reset it. The
/// reset stands without it, so a mail that cannot be built is only logged.
fn reset_mail(service: &UserService, account: &Account, slot: &str) -> Option<Message> {
    let to = Address::new(Some(&account.name), &account.email).ok()?;
    let wording = service
        .templates
        .platform_wording(crate::wording::TWO_FACTOR_RESET, &[])
        .map_err(|error| {
            eprintln!(
                "mail: 2FA reset notice for user {} was not written: {error}",
                account.id
            )
        })
        .ok()?;
    Some(Message::text(
        to,
        wording["subject"].clone(),
        format!("{}\n\n{}{SETTINGS_PATH}\n", wording[slot], service.origin),
    ))
}

/// Deletes the confirmed authenticator, ends every session of the portable
/// identity across its memberships and records who did it. Returns the ended
/// session ids for announce after commit. Session ids are not written into
/// the audit payload.
async fn clear_confirmed(
    service: &UserService,
    tx: &mut MutationTransaction<'_>,
    account: &Account,
    by: Value,
) -> Result<Vec<String>, Error> {
    // Serialize resets and membership removal through the owning account row.
    tx.sql()
        .query_opt(
            "SELECT id FROM users WHERE tenant_id=$1 AND id=$2 AND deleted_at IS NULL FOR UPDATE",
            &[&tx.context().tenant, &account.id],
        )
        .await?
        .ok_or(Error::NotFound)?;
    if !service.step_up.account_enrolled(tx, account.id).await? {
        return Err(Error::Conflict);
    }
    service
        .step_up
        .lifecycle(tx, account.id, bilibi_step_up::Lifecycle::Reset)
        .await?;
    // The account is named by its ID only: a name or email here would outlive
    // the account's erasure.
    let mut after = json!({"status":"off","user_id":account.id,"signed_out":true});
    if let (Some(after), Some(by)) = (after.as_object_mut(), by.as_object()) {
        after.extend(by.clone());
    }
    tx.record(bilibi_database::Change {
        kind: "user_two_factor_reset".into(),
        id: account.id,
        old: json!({"status":"enabled", "user_id":account.id}),
        new: after,
    })
    .await?;
    let identity = crate::sign_in_identity::id(tx.sql(), account.tenant, account.id).await?;
    crate::sign_in_identity::end_everywhere(&service.sessions, tx, identity, account.id, None).await
}

fn wake(
    service: &UserService,
    tenant: i64,
    user: i64,
    sessions: Vec<String>,
    mail: Option<Message>,
) {
    service.notifications.changed(tenant, &[user]);
    service.sessions.announce(sessions);
    let Some(message) = mail else {
        return;
    };
    let mailer = service.mailer.clone();
    tokio::spawn(async move {
        if let Err(error) = mailer.deliver_for(tenant, &message).await {
            eprintln!("mail: 2FA reset notice for user {user} was not delivered: {error}");
        }
    });
}

impl UserService {
    /// Turns off the confirmed authenticator for the one non-deleted account
    /// with this email. Already off is success and writes nothing. The audit
    /// actor is the system principal `two-factor-reset`.
    pub async fn reset_two_factor_by_email(
        &self,
        email: &str,
    ) -> Result<TwoFactorEmailReset, TwoFactorEmailError> {
        let email = normalize_email(email).map_err(|_| TwoFactorEmailError::Invalid)?;
        let client = self.db.get().await.map_err(Error::from)?;
        let rows = client
            .query(
                &format!("{TARGET_SQL} AND lower(i.email)=$1 AND u.deleted_at IS NULL ORDER BY i.id,(u.tenant_id=i.last_tenant_id) DESC,u.tenant_id"),
                &[&email],
            )
            .await
            .map_err(Error::from)?;
        let account = match rows.as_slice() {
            [] => return Err(TwoFactorEmailError::NotFound),
            [row] => account_from(row),
            _ => return Err(Error::Conflict.into()),
        };
        drop(client);
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(account.tenant, "two-factor-reset", "user.two_factor.reset"),
            )
            .await?;
        let ended = match clear_confirmed(
            self,
            &mut tx,
            &account,
            json!({"reset_by":"system", "reset_by_name":"two-factor-reset"}),
        )
        .await
        {
            Ok(ended) => ended,
            Err(Error::Conflict) => {
                tx.rollback().await?;
                return Ok(TwoFactorEmailReset::AlreadyOff);
            }
            Err(error) => return Err(error.into()),
        };
        if account.active {
            let notice = reset_notice(crate::wording::TWO_FACTOR_SYSTEM_BODY)?;
            notifications::record_in_tenant(&mut tx, account.tenant, &[account.id], &notice)
                .await?;
        }
        let mail = reset_mail(self, &account, "by_server_admin");
        let (tenant, user) = (account.tenant, account.id);
        // Live pages subscribe to the service bound at boot, which is this one.
        let service = USERS.get().cloned().ok_or(Error::Internal)?;
        tx.after_commit(move || wake(&service, tenant, user, ended, mail));
        tx.commit().await?;
        Ok(TwoFactorEmailReset::Reset)
    }
}
