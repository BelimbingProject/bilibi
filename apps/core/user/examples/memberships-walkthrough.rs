//! Manual proof of issue 147's invitation and acceptance service against a
//! migrated disposable database. It follows emailed links from the dev mailbox.
//! It also proves, for a person with accounts in two tenants, that one
//! authenticator works in both (#534), that a password change or reset ends
//! the sign-in's sessions and AI connections in every
//! tenant (#532), and that "Forgot your password?" still mails a working link
//! after the last-used tenant account is removed (#535), with the reset
//! token's issue audited (#536). It stores the development instance role
//! first: the dev mailbox sends nowhere else.
use bilibi_audit::RequestInfo;
use bilibi_database::{pool, Error, Result};
use bilibi_mail::Mailbox;
use bilibi_user::{
    CredentialError, InvitationError, InvitationInput, Login, TenantEntry, TwoFactorError,
    UserService,
};
use std::{path::PathBuf, time::Duration};

const EMAIL: &str = "manager@example.test";

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

struct WalkthroughProvider;

impl bilibi_user::IdentityProvider for WalkthroughProvider {
    fn start<'a>(
        &'a self,
        request: bilibi_user::IdentityRequest<'a>,
    ) -> bilibi_user::IdentityFuture<'a, String> {
        Box::pin(async move { Ok(request.state.to_owned()) })
    }

    fn finish<'a>(
        &'a self,
        _: bilibi_user::IdentityRequest<'a>,
        response: &'a str,
    ) -> bilibi_user::IdentityFuture<'a, bilibi_user::ExternalIdentity> {
        Box::pin(async move {
            let claims = match response {
                "acme-manager" => serde_json::json!({"amr":["mfa"]}),
                "acme-manager-factor" => serde_json::json!({}),
                _ => return Err(Error::Unauthorized),
            };
            Ok(bilibi_user::ExternalIdentity {
                issuer: "https://identity.example.test".into(),
                subject: "acme-manager".into(),
                claims,
                session_id: None,
                logout_hint: None,
            })
        })
    }

    fn logout<'a>(
        &'a self,
        _: &'a serde_json::Value,
        _: Option<&'a str>,
        _: &'a str,
    ) -> bilibi_user::IdentityFuture<'a, Option<String>> {
        Box::pin(async { Ok(None) })
    }

    fn backchannel<'a>(
        &'a self,
        _: &'a serde_json::Value,
        _: &'a str,
    ) -> bilibi_user::IdentityFuture<'a, bilibi_user::ExternalLogout> {
        Box::pin(async { Err(Error::Unauthorized) })
    }
}

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
            let wire = mailbox.read(&newest.id).map_err(|_| Error::Internal)?;
            let message = bilibi_mail::parse(wire.as_bytes(), &bilibi_mail::ParseLimits::default())
                .map_err(|_| Error::Internal)?
                .text
                .ok_or(Error::Internal)?;
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
    newest_link(mailbox, before)
        .await?
        .rsplit('/')
        .next()
        .filter(|token| token.len() == 64)
        .map(str::to_owned)
        .ok_or(Error::Internal)
}

/// The reset token in the newest mail after `before`.
async fn reset_token(mailbox: &Mailbox, before: usize) -> Result<String> {
    newest_link(mailbox, before)
        .await?
        .split_once("/reset-password/")
        .and_then(|(_, rest)| rest.split('?').next())
        .map(str::to_owned)
        .ok_or(Error::Internal)
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
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
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
    let client = db
        .get_detached("the walkthrough's own checking connection")
        .await?;
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
    let auth = app
        .services
        .get::<bilibi_tenancy::Authentication>("base/tenancy.authentication")?;
    let viewing = auth
        .authenticate_session(
            &**client,
            existing.tenant(),
            existing.user(),
            Some(4),
            None,
            String::new(),
        )
        .await?;
    assert!(matches!(
        users
            .accept_member_invitation(&viewing, &existing_token)
            .await,
        Err(InvitationError::Access(Error::Forbidden))
    ));
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

    let provider_public = uuid::Uuid::new_v4().simple().to_string();
    let provider: i64 = client.query_one(
        "INSERT INTO user_identity_providers(tenant_id,company_id,public_id,configured_by,method,enabled) VALUES(1,1,$1,$2,'walkthrough',true) RETURNING id",
        &[&provider_public, &existing.user()],
    ).await?.get(0);
    // This fixture inserts the provider below the production configuration
    // facade, so it also supplies the empty fail-closed authorization policy.
    client
        .execute(
            "INSERT INTO authz_identity_policies(tenant_id,source,company_id,role_ids) VALUES(1,$1,1,'{}')",
            &[&format!("core.user.sso.{provider}")],
        )
        .await?;
    let issuer = "https://identity.example.test";
    let settings = app
        .services
        .get::<bilibi_base::settings::Settings>("base/settings.store")?;
    let config = bilibi_user::IdentityConfiguration {
        enabled: true,
        method: "walkthrough".into(),
        ..Default::default()
    };
    let sealed = settings.seal_owned(
        "core/user.sso.connection",
        1,
        provider,
        &serde_json::json!({"configuration":config,"client_secret":""}),
    )?;
    client
        .execute(
            "UPDATE user_identity_providers SET sealed_runtime=$2 WHERE tenant_id=1 AND id=$1",
            &[&provider, &sealed],
        )
        .await?;
    client.execute(
        "INSERT INTO user_external_identities(tenant_id,provider_id,company_id,user_id,issuer,subject) VALUES(1,$1,1,$2,$3,'acme-manager')",
        &[&provider, &existing.user(), &issuer],
    ).await?;
    users
        .register_identity_provider("walkthrough", std::sync::Arc::new(WalkthroughProvider))
        .map_err(|_| Error::Internal)?;
    let external = users
        .begin_external_sign_in(
            &provider_public,
            bilibi_user::IdentityPurpose::SignIn,
            None,
            "/dashboard",
        )
        .await?;
    let bilibi_user::IdentityOutcome::SignIn(acme_login) = users
        .finish_external_sign_in(
            &external.url,
            &external.browser_binding,
            "acme-manager",
            None,
        )
        .await?
    else {
        return Err(Error::Internal);
    };
    let session_service = &app.security.sessions;
    let (acme, authority) = session_service
        .activity_with_authority(&**client, &acme_login.token)
        .await?;
    assert!(authority.admits_tenant(1) && !authority.admits_tenant(2) && !authority.is_portable());
    let original = client.query_one(
        "SELECT u.name,i.email,i.password_version FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=1 AND u.id=$1",
        &[&acme.user()],
    ).await?;
    let name: String = original.get(0);
    assert!(matches!(
        users
            .choose_tenant(
                &acme_login.token,
                &acme_login.csrf,
                2,
                &RequestInfo::default()
            )
            .await,
        Ok(TenantEntry::Reauthenticate { tenant: 2, .. })
    ));
    assert!(matches!(
        users
            .update_profile(
                &acme_login.token,
                &acme_login.csrf,
                &name,
                "acme-controlled@example.test"
            )
            .await,
        Err(bilibi_user::AccountError::Access(Error::Forbidden))
    ));
    assert!(matches!(
        users
            .change_password(
                &acme_login.token,
                &acme_login.csrf,
                bilibi_walkthrough_fixture::PASSWORD,
                "acme-controlled-password"
            )
            .await,
        Err(CredentialError::Other(Error::Forbidden))
    ));
    assert!(users
        .begin_two_factor(&acme, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .is_err());
    let after = client.query_one(
        "SELECT i.email,i.password_version FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=1 AND u.id=$1",
        &[&acme.user()],
    ).await?;
    assert_eq!(after.get::<_, String>(0), original.get::<_, String>(1));
    assert_eq!(after.get::<_, i64>(1), original.get::<_, i64>(2));
    assert_eq!(app.security.guard(&acme_login.token).await?.tenant(), 1);
    let beta_login = users
        .login_to_tenant(2, EMAIL, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|_| Error::Unauthorized)?;
    let (beta, authority) = session_service
        .activity_with_authority(&**client, &beta_login.token)
        .await?;
    assert!(authority.is_portable() && beta.tenant() == 2 && beta.user() == joined);
    users
        .retire_replaced_session(&acme_login.token, &beta_login.token)
        .await?;
    assert!(app.security.guard(&acme_login.token).await.is_err());
    println!(
        "Acme provider admission stayed tenant-bound and Beta portable admission replaced it."
    );

    // #534: the authenticator is set up from the first membership...
    let enrollment = users
        .begin_two_factor(&existing, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|_| Error::Invalid)?;
    let mut used = now_step();
    users
        .confirm_two_factor(&existing, &code(&enrollment.key, used)?)
        .await
        .map_err(|_| Error::Invalid)?;
    client.execute(
        "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES('step_up.policy',1,'tenant',1,$1,false)",
        &[&serde_json::json!({(bilibi_user::SIGN_IN): "on"})],
    ).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "production").await?;
    let external = users
        .begin_external_sign_in(
            &provider_public,
            bilibi_user::IdentityPurpose::SignIn,
            None,
            "/dashboard",
        )
        .await?;
    let bilibi_user::IdentityOutcome::SignIn(pending) = users
        .finish_external_sign_in(
            &external.url,
            &external.browser_binding,
            "acme-manager-factor",
            None,
        )
        .await?
    else {
        return Err(Error::Internal);
    };
    assert!(pending.pending);
    assert!(app.security.guard(&pending.token).await.is_err());
    let provenance = client.query_one(
        "SELECT authentication_authority,authentication_tenant_id,authentication_provider_id,authentication_company_id FROM user_sign_ins WHERE tenant_id=1 AND user_id=$1 AND state='pending'",
        &[&acme.user()],
    ).await?;
    assert_eq!(provenance.get::<_, String>(0), "tenant_sso");
    assert_eq!(provenance.get::<_, i64>(1), 1);
    assert_eq!(provenance.get::<_, i64>(2), provider);
    assert_eq!(provenance.get::<_, i64>(3), 1);
    let challenge = users.pending_sign_in(&pending.token).await?;
    let method = &challenge.methods.first().ok_or(Error::Internal)?.id;
    users
        .start_sign_in_challenge(&pending.token, &pending.csrf, method)
        .await?;
    used = next_step(used).await;
    let completed = users
        .complete_sign_in(
            &pending.token,
            &pending.csrf,
            method,
            &code(&enrollment.key, used)?,
        )
        .await?
        .ok_or(Error::Unauthorized)?;
    let (acme, authority) = session_service
        .activity_with_authority(&**client, &completed.token)
        .await?;
    assert!(authority.admits_tenant(1) && !authority.admits_tenant(2) && !authority.is_portable());
    assert!(matches!(
        users
            .begin_move(&acme, &code(&enrollment.key, used + 1)?)
            .await,
        Err(TwoFactorError::Other(Error::Forbidden))
    ));
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
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
    let source_user = existing.user();
    let mut password = bilibi_walkthrough_fixture::PASSWORD.to_owned();
    let elsewhere = session_in(&users, &app, &password, 2).await?;
    let changing = session_in(&users, &app, &password, 1).await?;
    let agent = connection(&client, 2, joined).await?;
    // A wrong current password is refused. Tenant administrators have no
    // operation that writes another person's shared password.
    if !matches!(
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
    let agent = connection(&client, 1, source_user).await?;
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
    let after = standing(&app, &[&first, &second]).await;
    let agent_end = ended(&client, agent).await?;
    println!("Reset in tenant 2: [tenant 1, tenant 2] sessions={after:?} tenant 1 connection ended={agent_end:?}");
    if after != [false, false] || agent_end.as_deref() != Some("password_reset") {
        return Err(Error::Internal);
    }

    // Removing one membership never erases the portable identity.
    users
        .remove_membership(&inviter, joined)
        .await
        .map_err(|_| Error::Invalid)?;
    let identity_survived: bool = client
        .query_one(
            "SELECT i.status='active' AND i.deleted_at IS NULL FROM user_identities i JOIN users u ON u.identity_id=i.id WHERE u.tenant_id=1 AND u.id=$1",
            &[&source_user],
        )
        .await?
        .get(0);
    println!("Removing the Beta membership kept the portable identity: {identity_survived}");
    if !identity_survived {
        return Err(Error::Internal);
    }

    // #535: the last-used membership is gone; reset still reaches the
    // portable identity and its link works once.
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
    println!("Customer administrators cannot reset credentials; membership removal keeps the portable sign-in active");

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
        "New portable sign-in: tenant={} user={} invited_user={}",
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
