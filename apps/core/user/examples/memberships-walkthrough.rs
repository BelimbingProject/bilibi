//! Manual proof of issue 147's invitation and acceptance service against a
//! migrated disposable database. It follows emailed links from the dev mailbox.
//! It also proves, for a person with accounts in two tenants, that one
//! authenticator works in both (#534), that a password change, reset or
//! administrator set ends the sign-in's sessions and AI connections in every
//! tenant (#532), and that "Forgot your password?" still mails a working link
//! after the last-used tenant account is removed (#535), with the reset
//! token's issue audited (#536).
use bilibi_audit::RequestInfo;
use bilibi_database::{pool, Error, Result};
use bilibi_mail::Mailbox;
use bilibi_user::{
    AccountError, CredentialError, InvitationError, InvitationInput, Login, TenantEntry,
    TwoFactorError, UserService,
};
use std::{path::PathBuf, time::Duration};

const EMAIL: &str = "manager@example.test";

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

fn mailbox_dir() -> PathBuf {
    std::env::var("BILIBI_MAIL_DIR")
        .unwrap_or_else(|_| ".local/mailbox".into())
        .into()
}

fn count(mailbox: &Mailbox) -> usize {
    mailbox.entries().map(|entries| entries.len()).unwrap_or(0)
}

/// The link in the newest message after `before` arrived.
async fn newest_link(mailbox: &Mailbox, before: usize) -> Result<String> {
    for _ in 0..50 {
        if count(mailbox) > before {
            let newest = mailbox
                .entries()
                .map_err(|_| Error::Internal)?
                .into_iter()
                .next()
                .ok_or(Error::Internal)?;
            let message = mailbox.read(&newest.id).map_err(|_| Error::Internal)?;
            return message
                .lines()
                .find(|line| line.starts_with("http"))
                .map(str::to_owned)
                .ok_or(Error::Internal);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Error::Internal)
}

/// Signs the two-tenant sign-in in and moves the new session into `tenant`,
/// the way a person (or someone holding their password) reaches it.
async fn session_in(
    users: &UserService,
    app: &bilibi_web::App,
    password: &str,
    tenant: i64,
) -> Result<Login> {
    let login = users
        .login(EMAIL, password)
        .await
        .map_err(|_| Error::Unauthorized)?;
    if app.security.guard(&login.token).await?.tenant() == tenant {
        return Ok(login);
    }
    match users
        .choose_tenant(&login.token, &login.csrf, tenant, &RequestInfo::default())
        .await
    {
        Ok(TenantEntry::Entered(moved)) => Ok(moved),
        Ok(_) => Err(Error::Internal),
        Err(refusal) => {
            println!("tenant choice refused: {refusal:?}");
            Err(Error::Internal)
        }
    }
}

/// An AI connection of the account, as Core Agent stores one.
async fn connection(client: &tokio_postgres::Client, tenant: i64, user: i64) -> Result<i64> {
    Ok(client
        .query_one(
            "INSERT INTO core_agent_connections(tenant_id,user_id,agent_id) VALUES($1,$2,'Walkthrough agent') RETURNING id",
            &[&tenant, &user],
        )
        .await?
        .get(0))
}

/// Why the connection ended, or `None` while it still works.
async fn ended(client: &tokio_postgres::Client, id: i64) -> Result<Option<String>> {
    Ok(client
        .query_one(
            "SELECT CASE WHEN revoked_at IS NOT NULL THEN end_reason END FROM core_agent_connections WHERE id=$1",
            &[&id],
        )
        .await?
        .get(0))
}

/// Whether each session still signs in.
async fn standing(app: &bilibi_web::App, sessions: &[&Login]) -> Vec<bool> {
    let mut stands = Vec::with_capacity(sessions.len());
    for login in sessions {
        stands.push(app.security.guard(&login.token).await.is_ok());
    }
    stands
}

async fn invitation_token(mailbox: &Mailbox, before: usize) -> Result<String> {
    for _ in 0..50 {
        if count(mailbox) > before {
            let newest = mailbox
                .entries()
                .map_err(|_| Error::Internal)?
                .into_iter()
                .next()
                .ok_or(Error::Internal)?;
            let message = mailbox.read(&newest.id).map_err(|_| Error::Internal)?;
            let link = message
                .lines()
                .find(|line| line.starts_with("http"))
                .ok_or(Error::Internal)?;
            return link
                .rsplit('/')
                .next()
                .filter(|token| token.len() == 64)
                .map(str::to_owned)
                .ok_or(Error::Internal);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Error::Internal)
}

/// The reset token in the newest mail after `before`.
async fn reset_token(mailbox: &Mailbox, before: usize) -> Result<String> {
    for _ in 0..50 {
        if count(mailbox) > before {
            let newest = mailbox
                .entries()
                .map_err(|_| Error::Internal)?
                .into_iter()
                .next()
                .ok_or(Error::Internal)?;
            let message = mailbox.read(&newest.id).map_err(|_| Error::Internal)?;
            return message
                .lines()
                .find_map(|line| line.split("/reset-password/").nth(1))
                .and_then(|rest| rest.split('?').next())
                .map(str::to_owned)
                .ok_or(Error::Internal);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Error::Internal)
}

fn now_step() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64 / 30)
        .unwrap_or_default()
}

/// The authenticator code for `key` (base32, as the setup page shows it) at
/// 30-second `step`.
fn code(key: &str, step: i64) -> Result<String> {
    use hmac::{Hmac, KeyInit, Mac};
    let secret = data_encoding::BASE32_NOPAD
        .decode(key.replace(' ', "").to_ascii_uppercase().as_bytes())
        .map_err(|_| Error::Internal)?;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).map_err(|_| Error::Internal)?;
    mac.update(&(step as u64).to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let at = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[at] & 0x7f,
        digest[at + 1],
        digest[at + 2],
        digest[at + 3],
    ]);
    Ok(format!("{:06}", binary % 1_000_000))
}

/// The next 30-second step after `used`, waiting for it.
async fn next_step(used: i64) -> i64 {
    while now_step() <= used {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    now_step()
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_MEMBERSHIPS_DEMO").as_deref() != Ok("disposable") {
        return Err(Error::Invalid);
    }
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(
        db.clone(),
        &std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?,
    )?;
    let grants = app
        .services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let users = app.services.get::<UserService>("core/user.accounts")?;
    let mailbox = Mailbox::new(mailbox_dir(), 100);
    let client = db.get().await?;
    for capability in [
        "admin.user.invite",
        "admin.user.update",
        "admin.user.delete",
    ] {
        client
            .execute(
                "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES(2,5,$1,true) ON CONFLICT(tenant_id,user_id,capability) DO UPDATE SET is_allowed=true",
                &[&capability],
            )
            .await?;
    }

    let inviter_login = users
        .login(
            "manager@other.example.test",
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await
        .map_err(|_| Error::Unauthorized)?;
    let inviter = app.security.guard(&inviter_login.token).await?;
    let existing_login = users
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|_| Error::Unauthorized)?;
    let existing = app.security.guard(&existing_login.token).await?;

    let before = count(&mailbox);
    let invitation = users
        .invite_member(
            &inviter,
            &InvitationInput {
                name: "Ada in the other workspace".into(),
                email: "manager@example.test".into(),
                roles: Vec::new(),
            },
        )
        .await
        .map_err(|_| Error::Invalid)?;
    let existing_token = invitation_token(&mailbox, before).await?;
    let joined = users
        .accept_member_invitation(&existing, &existing_token)
        .await
        .map_err(|_| Error::Invalid)?;
    let identity_count: i64 = client
        .query_one(
            "SELECT count(DISTINCT identity_id) FROM users WHERE id IN ($1,$2) AND tenant_id IN (1,2)",
            &[&existing.user(), &joined],
        )
        .await?
        .get(0);
    println!(
        "Existing sign-in joined tenant 2: user={} one_identity={}",
        joined,
        identity_count == 1
    );
    if invitation.email != "manager@example.test" || identity_count != 1 {
        return Err(Error::Internal);
    }

    // #534: the authenticator is set up in the home tenant...
    let enrollment = users
        .begin_two_factor(&existing, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|_| Error::Invalid)?;
    let mut used = now_step();
    users
        .confirm_two_factor(&existing, &code(&enrollment.key, used)?)
        .await
        .map_err(|_| Error::Invalid)?;
    // ...and the person then signs in to the other tenant, used last.
    client
        .execute(
            "UPDATE user_identities SET last_tenant_id=2 WHERE id=(SELECT identity_id FROM users WHERE tenant_id=1 AND id=$1)",
            &[&existing.user()],
        )
        .await?;
    let other_login = users
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|_| Error::Unauthorized)?;
    let in_other = app.security.guard(&other_login.token).await?;
    if other_login.pending || in_other.tenant() != 2 || in_other.user() != joined {
        return Err(Error::Internal);
    }
    used = next_step(used).await;
    let right = code(&enrollment.key, used)?;
    let wrong = format!("{:06}", (right.parse::<u32>().unwrap_or(0) + 1) % 1_000_000);
    let refused = users.step_up(&in_other, &wrong).await;
    let accepted = users.step_up(&in_other, &right).await;
    println!(
        "Authenticator from tenant 1 in tenant 2: wrong code {refused:?}, right code {accepted:?}"
    );
    if !matches!(refused, Err(TwoFactorError::InvalidCode)) || accepted.is_err() {
        return Err(Error::Internal);
    }

    // Issue 532. The sign-in now has accounts in tenants 1 and 2.
    let home = existing.user();
    let mut password = bilibi_walkthrough_fixture::PASSWORD.to_owned();
    let elsewhere = session_in(&users, &app, &password, 2).await?;
    let changing = session_in(&users, &app, &password, 1).await?;
    let agent = connection(&client, 2, joined).await?;
    // Refused: an outside tenant's administrator, and a wrong current password.
    if !matches!(
        users.set_password(&inviter, joined, "outside-reset").await,
        Err(AccountError::Access(Error::Forbidden))
    ) || !matches!(
        users
            .change_password(&changing.token, &changing.csrf, "not-it", "changed-532")
            .await,
        Err(CredentialError::IncorrectPassword)
    ) {
        return Err(Error::Internal);
    }
    let kept = standing(&app, &[&elsewhere, &changing]).await;
    let kept_agent = ended(&client, agent).await?;
    println!(
        "Refused password changes end nothing: sessions={kept:?} connection_ended={kept_agent:?}"
    );
    if kept != [true, true] || kept_agent.is_some() {
        return Err(Error::Internal);
    }

    // Change in tenant 1: the changing session stays, tenant 2's ends.
    users
        .change_password(&changing.token, &changing.csrf, &password, "changed-532")
        .await
        .map_err(|_| Error::Internal)?;
    password = "changed-532".into();
    let after = standing(&app, &[&changing, &elsewhere]).await;
    let agent_end = ended(&client, agent).await?;
    let marked: i64 = client
        .query_one(
            "SELECT count(*) FROM sessions WHERE tenant_id=2 AND user_id=$1 AND ended_reason='terminated'",
            &[&joined],
        )
        .await?
        .get(0);
    println!("Change in tenant 1: [changing, tenant 2] sessions={after:?} tenant 2 connection ended={agent_end:?} marked for the sweep={marked}");
    if after != [true, false] || agent_end.as_deref() != Some("password_changed") || marked < 1 {
        return Err(Error::Internal);
    }

    // Reset by emailed link: it acts on the last-used tenant (2), and tenant
    // 1's session and connection end too.
    let first = session_in(&users, &app, &password, 1).await?;
    let second = session_in(&users, &app, &password, 2).await?;
    let agent = connection(&client, 1, home).await?;
    let before = count(&mailbox);
    users
        .request_password_reset(EMAIL)
        .await
        .map_err(|_| Error::Internal)?;
    let link = newest_link(&mailbox, before).await?;
    let token = link
        .split("/reset-password/")
        .nth(1)
        .and_then(|rest| rest.split('?').next())
        .ok_or(Error::Internal)?
        .to_owned();
    if !matches!(
        users.reset_password(EMAIL, &token, "short").await,
        Err(CredentialError::Policy(_))
    ) || standing(&app, &[&first, &second]).await != [true, true]
    {
        return Err(Error::Internal);
    }
    users
        .reset_password(EMAIL, &token, "reset-532-pw")
        .await
        .map_err(|_| Error::Internal)?;
    password = "reset-532-pw".into();
    let after = standing(&app, &[&first, &second]).await;
    let agent_end = ended(&client, agent).await?;
    println!("Reset in tenant 2: [tenant 1, tenant 2] sessions={after:?} tenant 1 connection ended={agent_end:?}");
    if after != [false, false] || agent_end.as_deref() != Some("password_reset") {
        return Err(Error::Internal);
    }

    // An administrator of the home tenant sets the password: both end.
    client
        .execute(
            "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES(1,4,'admin.user.update',true) ON CONFLICT(tenant_id,user_id,capability) DO UPDATE SET is_allowed=true",
            &[],
        )
        .await?;
    let admin_login = users
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|_| Error::Unauthorized)?;
    let admin = app.security.guard(&admin_login.token).await?;
    let first = session_in(&users, &app, &password, 1).await?;
    let second = session_in(&users, &app, &password, 2).await?;
    let agent = connection(&client, 2, joined).await?;
    users
        .set_password(&admin, home, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|_| Error::Internal)?;
    let after = standing(&app, &[&first, &second, &admin_login]).await;
    let agent_end = ended(&client, agent).await?;
    println!("Administrator set in tenant 1: [tenant 1, tenant 2, administrator] sessions={after:?} tenant 2 connection ended={agent_end:?}");
    if after != [false, false, true] || agent_end.as_deref() != Some("admin_action") {
        return Err(Error::Internal);
    }
    users
        .delete_account(&inviter, joined)
        .await
        .map_err(|_| Error::Invalid)?;

    // #535: the last-used tenant account is gone; the reset still reaches the
    // home account, and its link works once.
    let before = count(&mailbox);
    users
        .request_password_reset("manager@example.test")
        .await
        .map_err(|_| Error::Invalid)?;
    let token = reset_token(&mailbox, before).await?;
    let wrong_token = users
        .reset_password(
            "manager@example.test",
            &"A".repeat(43),
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await;
    users
        .reset_password(
            "manager@example.test",
            &token,
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await
        .map_err(|_| Error::Invalid)?;
    let reused = users
        .reset_password(
            "manager@example.test",
            &token,
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await;
    // #536: the issued token left a change row attributed to the `sign-in`
    // principal, without it. The action name is on the transaction's action
    // row; the change row's own event is `created`.
    let issued: Vec<String> = client
        .query(
            "SELECT m.new_values::text FROM base_audit_mutations m JOIN base_audit_actions a ON a.trace_id=m.trace_id AND a.tenant_id=m.tenant_id WHERE m.tenant_id=1 AND a.event='user.reset_requested' AND m.auditable_type='user_identity' AND m.actor_type='system' AND m.system_principal='sign-in'",
            &[],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    println!("Reset after the last-used account was removed: link mailed and redeemed once; audited {issued:?}");
    if !matches!(wrong_token, Err(CredentialError::InvalidOrExpiredToken))
        || !matches!(reused, Err(CredentialError::InvalidOrExpiredToken))
        || issued.is_empty()
        || issued.iter().any(|values| values.contains(&token))
    {
        return Err(Error::Internal);
    }
    users
        .login(EMAIL, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|_| Error::Unauthorized)?;
    println!("Outside tenant cannot reset credentials; removal keeps the home sign-in active");

    let before = count(&mailbox);
    let new_invitation = users
        .invite_member(
            &inviter,
            &InvitationInput {
                name: "New Member".into(),
                email: "new-member@example.test".into(),
                roles: Vec::new(),
            },
        )
        .await
        .map_err(|_| Error::Invalid)?;
    let new_token = invitation_token(&mailbox, before).await?;
    let new_user = users
        .accept_new_member_invitation(&new_token, "new-member-password")
        .await
        .map_err(|_| Error::Invalid)?;
    let login = users
        .login("new-member@example.test", "new-member-password")
        .await
        .map_err(|_| Error::Unauthorized)?;
    let scope = app.security.guard(&login.token).await?;
    println!(
        "New sign-in home: tenant={} user={} invited_user={}",
        scope.tenant(),
        scope.user(),
        new_invitation.user
    );
    if scope.tenant() != 2 || new_user != new_invitation.user || scope.user() != new_user {
        return Err(Error::Internal);
    }
    if !matches!(
        users
            .accept_new_member_invitation(&new_token, "new-member-password")
            .await,
        Err(InvitationError::Link)
    ) {
        return Err(Error::Internal);
    }
    let stored_plaintext: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM user_invitations WHERE token_hash=$1::text::bytea)",
            &[&new_token],
        )
        .await?
        .get(0);
    println!("Token one-time and digest-only: reused=false plaintext_stored={stored_plaintext}");
    if stored_plaintext {
        return Err(Error::Internal);
    }
    Ok(())
}
