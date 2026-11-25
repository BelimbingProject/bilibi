//! Proves the lost-phone request and administrator reset on a disposable database.
use bilibi_agent_api::page::Call;
use bilibi_database::{pool, Error};
use bilibi_user::UserService;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

fn code(key: &str, offset: i64) -> String {
    let secret = data_encoding::BASE32_NOPAD
        .decode(key.replace(' ', "").as_bytes())
        .expect("base32 key");
    let step = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        / 30
        + offset) as u64;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).expect("key");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let at = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[at] & 0x7f,
        digest[at + 1],
        digest[at + 2],
        digest[at + 3],
    ]);
    format!("{:06}", binary % 1_000_000)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_TWO_FACTOR_RESET_REQUEST_DEMO").as_deref() != Ok("disposable") {
        return Err("use a disposable migrated database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &std::env::var("APP_ORIGIN")?)?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let users = app.services.get::<UserService>("core/user.accounts")?;
    let password = bilibi_walkthrough_fixture::PASSWORD;

    let admin_login = users.login("admin@example.test", password).await?;
    let admin = app.security.guard(&admin_login.token).await?;
    let admin_enrollment = users
        .begin_two_factor(&admin, password)
        .await
        .map_err(|_| Error::Invalid)?;
    users
        .confirm_two_factor(&admin, &code(&admin_enrollment.key, -1))
        .await
        .map_err(|_| Error::Invalid)?;

    let viewer_login = users.login("viewer@example.test", password).await?;
    let viewer = app.security.guard(&viewer_login.token).await?;
    let viewer_enrollment = users
        .begin_two_factor(&viewer, password)
        .await
        .map_err(|_| Error::Invalid)?;
    users
        .confirm_two_factor(&viewer, &code(&viewer_enrollment.key, -1))
        .await
        .map_err(|_| Error::Invalid)?;

    let client = db.get().await?;
    client.execute("INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES('step_up.policy',1,'tenant',1,$1,false)", &[&json!({bilibi_user::SIGN_IN:"on"})]).await?;
    drop(client);
    bilibi_walkthrough_fixture::set_instance_role(&db, "production").await?;

    let pending = users.login("viewer@example.test", password).await?;
    assert!(pending.pending && app.security.guard(&pending.token).await.is_err());
    let first = users
        .request_sign_in_two_factor_reset(&pending.token, &pending.csrf)
        .await?;
    let repeat = users
        .request_sign_in_two_factor_reset(&pending.token, &pending.csrf)
        .await?;
    assert!(first.created && !repeat.created && first.requested_at == repeat.requested_at);
    assert_eq!(first.recipient, "your administrators");
    let client = db.get().await?;
    let request: (String, bool) = client.query_one("SELECT recipient_kind,closed_at IS NULL FROM user_two_factor_reset_requests WHERE tenant_id=1 AND user_id=$1", &[&viewer.user()]).await.map(|row|(row.get(0),row.get(1)))?;
    assert_eq!(request, ("tenant_administrators".into(), true));
    assert!(client.query_one("SELECT EXISTS(SELECT 1 FROM notifications WHERE tenant_id=1 AND type='two_factor_reset_request')", &[]).await?.get::<_,bool>(0));
    assert!(client.query_one("SELECT EXISTS(SELECT 1 FROM base_audit_actions WHERE tenant_id=1 AND event='user.two_factor.reset_requested')", &[]).await?.get::<_,bool>(0));
    println!("PASS: a password-proved sign-in filed one audited request in 24 hours and notified tenant administrators without admission");
    drop(client);

    let contracts = app
        .services
        .get::<bilibi_agent_api::page::PageContracts>(bilibi_agent_api::page::SERVICE)?;
    let presented = admin.clone().presenting_step_up(&format!(
        "core.user.authenticator:{}",
        code(&admin_enrollment.key, 0)
    ));
    contracts
        .submit(
            &Call {
                scope: &presented,
                credential: &admin_login.token,
                service: "core/user.administration-pages",
                event: "reset_two_factor",
                contract: "core.user.two_factor.reset",
            },
            json!({"user_id":viewer.user()}),
        )
        .await?;
    let client = db.get().await?;
    let state: (bool, bool) = client.query_one("SELECT NOT EXISTS(SELECT 1 FROM user_two_factor WHERE identity_id=(SELECT identity_id FROM users WHERE tenant_id=1 AND id=$1)),NOT EXISTS(SELECT 1 FROM user_two_factor_reset_requests WHERE tenant_id=1 AND user_id=$1 AND closed_at IS NULL)", &[&viewer.user()]).await.map(|row|(row.get(0),row.get(1)))?;
    assert_eq!(state, (true, true));
    assert!(app.security.guard(&viewer_login.token).await.is_err());
    assert!(users
        .request_sign_in_two_factor_reset(&pending.token, &pending.csrf)
        .await
        .is_err());
    println!("PASS: the reviewed administrator operation spent fresh 2FA, reset the factor, closed the request and ended every session and pending sign-in");
    Ok(())
}
