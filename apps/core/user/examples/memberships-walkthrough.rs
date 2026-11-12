//! Manual proof of issue 147's invitation and acceptance service against a
//! migrated disposable database. It follows emailed links from the dev mailbox.
//! It also proves, for a person with accounts in two tenants, that one
//! authenticator works in both (#534) and that "Forgot your password?" still
//! mails a working link after the last-used tenant account is removed (#535),
//! with the reset token's issue audited (#536).
use bilibi_database::{pool, Error, Result};
use bilibi_mail::Mailbox;
use bilibi_user::{
    AccountError, CredentialError, InvitationError, InvitationInput, TwoFactorError, UserService,
};
use std::{path::PathBuf, time::Duration};

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

    if !matches!(
        users.set_password(&inviter, joined, "outside-reset").await,
        Err(AccountError::Access(Error::Forbidden))
    ) {
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
    // #536: the issued token left a system-attributed audit row, without it.
    let issued: Vec<String> = client
        .query(
            "SELECT new_values::text FROM base_audit_mutations WHERE tenant_id=1 AND event='user.reset_requested' AND auditable_type='user_identity' AND actor_type='system'",
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
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
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
