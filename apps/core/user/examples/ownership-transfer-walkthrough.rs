//! Running proof of the two-person installation ownership transfer on a disposable database.
use bilibi_database::pool;
use bilibi_security::Authentication;
use bilibi_system::Instances;
use bilibi_tenancy::{prove_owner, Scope};
use bilibi_user::OwnershipError;
use std::time::Duration;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        println!("ok: {message}");
        Ok(())
    } else {
        Err(format!("contract violated: {message}").into())
    }
}

fn now_step() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        / 30
}

fn code(key: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut secret = Vec::new();
    let (mut buffer, mut bits) = (0u64, 0u32);
    for character in key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
    {
        let value = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .find(character.to_ascii_uppercase())
            .expect("base32") as u64;
        buffer = buffer << 5 | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            secret.push((buffer >> bits) as u8);
        }
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).expect("key");
    mac.update(&(now_step() as u64).to_be_bytes());
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

struct Account {
    scope: Scope,
    key: String,
    used: i64,
}

impl Account {
    async fn enroll(app: &bilibi_web::App, email: &str) -> Result<Self> {
        let login = app
            .user
            .login(email, bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        let scope = app.security.guard(&login.token).await?;
        let enrollment = app
            .user
            .begin_two_factor(&scope, bilibi_walkthrough_fixture::PASSWORD)
            .await
            .map_err(|error| format!("begin 2FA: {error:?}"))?;
        let first = code(&enrollment.key);
        app.user
            .confirm_two_factor(&scope, &first)
            .await
            .map_err(|error| format!("confirm 2FA: {error:?}"))?;
        Ok(Self {
            scope,
            key: enrollment.key,
            used: now_step(),
        })
    }

    async fn step_up(&mut self) -> Scope {
        while now_step() <= self.used {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.used = now_step();
        self.scope.clone().presenting_step_up(&code(&self.key))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_OWNERSHIP_DEMO").as_deref() != Ok("disposable") {
        return Err("requires BILIBI_OWNERSHIP_DEMO=disposable".into());
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let client = db.get().await?;
    client
        .execute(
            "INSERT INTO user_bootstrap(id,tenant_name,company_name,company_code,admin_email,tenant_id,user_id,company_id,affiliation_settled_at)
             SELECT 1,t.name,c.name,c.code,i.email,t.id,u.id,c.id,now()
             FROM tenants t JOIN platform_operator o ON o.tenant_id=t.id AND o.singleton
             JOIN tenant_primary_companies pc ON pc.tenant_id=t.id
             JOIN companies c ON c.tenant_id=t.id AND c.id=pc.company_id
             JOIN users u ON u.tenant_id=t.id AND u.id=1
             JOIN user_identities i ON i.id=u.identity_id",
            &[],
        )
        .await?;
    drop(client);
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35579")?;
    let mut owner = Account::enroll(&app, "manager@example.test").await?;
    let mut next = Account::enroll(&app, "contractor@example.test").await?;

    let before_roles: Vec<(i64, i64)> = db
        .get()
        .await?
        .query(
            "SELECT user_id,role_id FROM principal_roles WHERE tenant_id=1 AND user_id=ANY($1) ORDER BY user_id,role_id",
            &[&vec![1_i64, 7]],
        )
        .await?
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();

    let agent_owner = owner
        .scope
        .clone()
        .for_agent(901, None, "ownership-proof".into(), None);
    ensure(
        matches!(
            app.user.offer_ownership(&agent_owner, 7).await,
            Err(OwnershipError::BrowserOnly)
        ),
        "an agent session cannot start a transfer",
    )?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let client = db.get().await?;
    let impersonated_owner = authentication
        .authenticate_session(&**client, 1, 1, Some(4), None, String::new())
        .await?;
    drop(client);
    ensure(
        matches!(
            app.user.offer_ownership(&impersonated_owner, 7).await,
            Err(OwnershipError::BrowserOnly)
        ),
        "an impersonated session cannot start a transfer",
    )?;
    let owner_code = owner.step_up().await;
    match app.user.offer_ownership(&owner_code, 5).await {
        Err(OwnershipError::Target) => {
            println!("ok: an account in another tenant cannot be offered ownership")
        }
        outcome => {
            return Err(format!(
                "contract violated: an account in another tenant cannot be offered ownership ({outcome:?})"
            )
            .into());
        }
    }

    let owner_code = owner.step_up().await;
    let expired = app.user.offer_ownership(&owner_code, 7).await?;
    db.get()
        .await?
        .execute(
            "UPDATE user_ownership_transfers SET expires_at=now()-interval '1 second' WHERE id=$1",
            &[&expired],
        )
        .await?;
    let next_code = next.step_up().await;
    ensure(
        matches!(
            app.user.accept_ownership(&next_code, expired).await,
            Err(OwnershipError::Expired)
        ),
        "an expired offer leaves the owner unchanged",
    )?;

    let owner_code = owner.step_up().await;
    let accepted = app.user.offer_ownership(&owner_code, 7).await?;
    let agent_next = next
        .scope
        .clone()
        .for_agent(902, None, "ownership-proof".into(), None);
    ensure(
        matches!(
            app.user.accept_ownership(&agent_next, accepted).await,
            Err(OwnershipError::BrowserOnly)
        ),
        "an agent session cannot accept a transfer",
    )?;
    let client = db.get().await?;
    let impersonated_next = authentication
        .authenticate_session(&**client, 1, 7, Some(4), None, String::new())
        .await?;
    drop(client);
    ensure(
        matches!(
            app.user
                .accept_ownership(&impersonated_next, accepted)
                .await,
            Err(OwnershipError::BrowserOnly)
        ),
        "an impersonated session cannot accept a transfer",
    )?;
    use bilibi_base::software::{MANAGE, VIEW};
    let client = db.get().await?;
    client
        .execute(
            "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed)
             VALUES(1,1,$1,true),(1,7,$2,false)",
            &[&VIEW, &MANAGE],
        )
        .await?;
    drop(client);
    let next_code = next.step_up().await;
    app.user.accept_ownership(&next_code, accepted).await?;

    let client = db.get().await?;
    ensure(
        prove_owner(&**client, &next.scope).await.is_ok(),
        "the new owner holds owner standing",
    )?;
    ensure(
        prove_owner(&**client, &owner.scope).await.is_err(),
        "the previous owner no longer holds owner standing",
    )?;
    drop(client);
    let instances = app.services.get::<Instances>(bilibi_system::SERVICE)?;
    let mode = instances.installation_mode().await?;
    ensure(
        instances
            .change_installation_mode(&next.scope, mode, mode)
            .await
            .is_ok(),
        "the new owner can use the existing owner-only installation policy action",
    )?;
    ensure(
        instances
            .change_installation_mode(&owner.scope, mode, mode)
            .await
            .is_err(),
        "the previous owner cannot use the existing owner-only installation policy action",
    )?;

    let software = app
        .services
        .get::<bilibi_base::software::Software>(bilibi_base::software::SERVICE)?;
    ensure(
        software.status(&next.scope).await.is_ok(),
        "the owner can open Software without a role grant",
    )?;
    ensure(
        software.status(&owner.scope).await.is_ok(),
        "the former owner's ordinary Software read grant still works",
    )?;

    let client = db.get().await?;
    for (account, reason) in [
        (&next, "denied_explicitly"),
        (&owner, "denied_owner_standing"),
    ] {
        let decision = bilibi_authz::decide(&**client, &account.scope, MANAGE).await?;
        ensure(
            !decision.allowed && decision.reason == reason,
            "Software management requires owner standing with no direct deny",
        )?;
        let effective = bilibi_authz::effective_capabilities(&**client, &account.scope).await?;
        ensure(
            !effective.contains(MANAGE) && effective.contains(VIEW),
            "effective Software capabilities agree with the decisions",
        )?;
    }
    client
        .execute(
            "DELETE FROM principal_capabilities WHERE tenant_id=1 AND user_id=7 AND capability=$1",
            &[&MANAGE],
        )
        .await?;
    ensure(
        bilibi_authz::decide(&**client, &next.scope, MANAGE)
            .await?
            .allowed,
        "clearing the deny restores the new owner's Software management",
    )?;
    let after_roles: Vec<(i64, i64)> = client
        .query(
            "SELECT user_id,role_id FROM principal_roles WHERE tenant_id=1 AND user_id=ANY($1) ORDER BY user_id,role_id",
            &[&vec![1_i64, 7]],
        )
        .await?
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    ensure(
        before_roles == after_roles,
        "both accounts keep exactly their existing roles",
    )?;
    let owner_changes: i64 = client.query_one(
        "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type='user.bootstrap' AND old_values->>'user_id'='1' AND new_values->>'user_id'='7'",
        &[],
    ).await?.get(0);
    ensure(
        owner_changes == 1,
        "one audited owner-record change moved the receipt",
    )?;
    println!("Ownership transfer proof passed.");
    Ok(())
}
