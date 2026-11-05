//! Manual proof of shared, reviewed principal administration on a disposable DB.
//! Run after migrate and seed-production in Dev, alongside the private app:
//! BILIBI_PRINCIPAL_PROOF=disposable cargo run -p bilibi-web --example system-principal-grants
//! No unit-test runner, invented identities, credentials or codes in output.
use bilibi_agent_api::plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans};
use bilibi_authz::{effective_system, grants::Grants, SystemScope};
use bilibi_database::{pool, Error};
use bilibi_queue::Queue;
use bilibi_tenancy::{ProofFuture, Scope};
use serde_json::json;
use tokio_postgres::Transaction;

const PRINCIPAL: &str = "media-scanner";
const CAPABILITY: &str = "media.file.scan";
const SET: &str = "base.authz_agent.system_principal.set";
struct Browser<'a> {
    app: &'a bilibi_web::App,
    token: &'a str,
}
impl PlanAuthority for Browser<'_> {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            let scope = self.app.security.sessions.hold(tx, self.token).await?;
            Ok(PlanIdentity {
                scope,
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}
fn operation(principal: &str, key: &str, rule: &str, expected: &str) -> PlannedOperation {
    PlannedOperation {
        key: SET.into(),
        input: json!({"principal":principal,"capability":key,"rule":rule,"expected_rule":expected}),
    }
}
fn change(rule: &str, expected: &str) -> [PlannedOperation; 1] {
    [operation(PRINCIPAL, CAPABILITY, rule, expected)]
}
async fn state(app: &bilibi_web::App, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let client = app.db.get().await?;
    for tenant in [1, 2] {
        let current = effective_system(&**client, tenant, PRINCIPAL).await?;
        let audit:i64 = client.query_one("SELECT count(*) FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type='authz.system_principal'", &[&tenant]).await?.get(0);
        println!(
            "{label}: tenant={tenant} effective={} allow={} deny={} audit={audit}",
            current.allowed.contains(CAPABILITY),
            current.direct_allowed.contains(CAPABILITY),
            current.direct_denied.contains(CAPABILITY)
        );
    }
    Ok(())
}
async fn attempt(
    plans: &Plans,
    scope: &Scope,
    browser: &Browser<'_>,
    label: &str,
    ops: &[PlannedOperation],
) {
    println!("{label} preview: {:?}", plans.preview(browser, ops).await);
    println!(
        "{label} commit: {:?}",
        plans.commit(scope, browser, ops).await
    );
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_PRINCIPAL_PROOF").as_deref() != Ok("disposable") {
        return Err(Error::Invalid.into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &std::env::var("APP_ORIGIN")?)?;
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin = app.security.guard(&login.token).await?;
    let browser = Browser {
        app: &app,
        token: &login.token,
    };
    let manager_login = app
        .user
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let manager = app.security.guard(&manager_login.token).await?;
    let manager_browser = Browser {
        app: &app,
        token: &manager_login.token,
    };
    let registry = app
        .services
        .get::<bilibi_agent_api::Registry>("base/agent_api.registry")?;
    println!(
        "installed access read: {:?}",
        registry
            .read(
                &db,
                &admin,
                "base.authz_agent.system_principal.list",
                &json!({})
            )
            .await
    );
    state(&app, "initial standing grant").await?;
    for (rule, expected, label) in [
        ("allow", "clear", "grant without standing access"),
        ("clear", "allow", "withdraw without standing access"),
    ] {
        attempt(
            &plans,
            &admin,
            &browser,
            label,
            &[operation(
                bilibi_base::events::PRINCIPAL,
                bilibi_base::events::DELIVER,
                rule,
                expected,
            )],
        )
        .await;
    }
    attempt(
        &plans,
        &manager,
        &manager_browser,
        "insufficient authority",
        &change("deny", "clear"),
    )
    .await;
    attempt(
        &plans,
        &admin,
        &browser,
        "undeclared principal",
        &[operation("not-installed", CAPABILITY, "allow", "clear")],
    )
    .await;
    attempt(
        &plans,
        &admin,
        &browser,
        "undeclared capability",
        &[operation(
            PRINCIPAL,
            "admin.authz.role.update",
            "allow",
            "clear",
        )],
    )
    .await;
    attempt(
        &plans,
        &admin,
        &browser,
        "deny standing grant",
        &change("deny", "clear"),
    )
    .await;
    state(&app, "deny wins and other tenant unchanged").await?;
    attempt(
        &plans,
        &admin,
        &browser,
        "allow declared capability",
        &change("allow", "deny"),
    )
    .await;
    let stale = change("clear", "allow");
    println!(
        "review before concurrent change: {:?}",
        plans.preview(&browser, &stale).await
    );
    attempt(
        &plans,
        &admin,
        &browser,
        "concurrent withdrawal",
        &change("deny", "allow"),
    )
    .await;
    println!(
        "stale commit: {:?}",
        plans.commit(&admin, &browser, &stale).await
    );
    state(&app, "stale review left deny intact").await?;
    attempt(
        &plans,
        &admin,
        &browser,
        "clear restores standing grant",
        &change("clear", "deny"),
    )
    .await;
    grants
        .set_direct(&admin, admin.user(), CAPABILITY, Some(false))
        .await?;
    attempt(
        &plans,
        &admin,
        &browser,
        "administrator direct deny",
        &change("allow", "clear"),
    )
    .await;
    grants
        .set_direct(&admin, admin.user(), CAPABILITY, None)
        .await?;
    // Disposable configuration only: enable the real host's step-up enforcement,
    // then restore Dev. No policy or credential bypass is added to the product.
    db.get().await?.execute("UPDATE base_settings SET value='\"production\"'::jsonb WHERE key='instance.role' AND tenant_id=0 AND scope_type='global' AND scope_id=0", &[]).await?;
    attempt(
        &plans,
        &admin,
        &browser,
        "step-up required",
        &change("allow", "clear"),
    )
    .await;
    db.get().await?.execute("UPDATE base_settings SET value='\"development\"'::jsonb WHERE key='instance.role' AND tenant_id=0 AND scope_type='global' AND scope_id=0", &[]).await?;
    attempt(
        &plans,
        &admin,
        &browser,
        "withdraw before queued work",
        &change("deny", "clear"),
    )
    .await;
    let reviewed = change("clear", "deny");
    println!(
        "review before authority withdrawal: {:?}",
        plans.preview(&browser, &reviewed).await
    );
    grants
        .set_direct(
            &admin,
            admin.user(),
            "admin.authz.system-principal.revoke",
            Some(false),
        )
        .await?;
    println!(
        "authority withdrawn commit: {:?}",
        plans.commit(&admin, &browser, &reviewed).await
    );
    grants
        .set_direct(
            &admin,
            admin.user(),
            "admin.authz.system-principal.revoke",
            None,
        )
        .await?;
    let mut foreign = change("allow", "deny");
    foreign[0].input["tenant_id"] = json!(2);
    println!(
        "caller-supplied tenant refused: {:?}",
        plans.preview(&browser, &foreign).await
    );
    let queue = app.services.get::<Queue>("base/queue.store")?;
    let mut client = db.get().await?;
    let system = SystemScope::rehydrate(&**client, admin.tenant(), Some(PRINCIPAL)).await?;
    let mut tx = app
        .mutations
        .begin(
            &mut client,
            system.mutation_context("authz.principal-proof.enqueue"),
        )
        .await?;
    let job = queue
        .enqueue_system(
            &mut tx,
            &system,
            "base/media-scan",
            json!({"asset_id":i64::MAX,"generation":0}),
        )
        .await?;
    tx.commit().await?;
    drop(client);
    println!(
        "Queue withdrawal proof job={}; the running app must cancel it as actor_refused",
        job.id()
    );
    for _ in 0..20 {
        let client = db.get().await?;
        let row = bilibi_queue::lookup(&**client, admin.tenant(), job.id()).await?;
        println!("queue state={:?} code={:?}", row.state, row.failure_code);
        if row.state.terminal() {
            break;
        }
        drop(client);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    state(&app, "final retained withdrawal").await?;
    app.user
        .logout(&manager_login.token, &manager_login.csrf)
        .await?;
    app.user.logout(&login.token, &login.csrf).await?;
    Ok(())
}
