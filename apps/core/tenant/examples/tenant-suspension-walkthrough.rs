//! Manual proof of tenant suspend and reactivate on a disposable migrated
//! database, through the real reviewed operations (`Plans`). No assertions or
//! unit tests: read the printed lines. It shows what suspension does to the
//! tenant's live sessions and to the two identity checks Queue makes on every
//! attempt of its jobs (`authenticate_job` for a person's job,
//! `SystemScope::rehydrate` for a named system principal's), which Queue
//! answers with `actor_refused` and a cancelled job (`base/queue` README).
//! Never prints a token or a code.
use bilibi_agent_api::plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans};
use bilibi_authz::SystemScope;
use bilibi_database::pool;
use bilibi_tenancy::{Authentication, ProofFuture, Scope};
use serde_json::json;
use tokio_postgres::Transaction;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Error = Box<dyn std::error::Error>;

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
fn operation(key: &str, tenant: i64, reason: &str) -> [PlannedOperation; 1] {
    [PlannedOperation {
        key: key.into(),
        input: json!({"tenant_id":tenant,"reason":reason}),
    }]
}
async fn write(plans: &Plans, scope: &Scope, browser: &Browser<'_>, ops: &[PlannedOperation]) {
    match plans.commit(scope, browser, ops).await {
        Ok(value) => println!("  committed: {value}"),
        Err(error) => println!("  refused: {error:?}"),
    }
}
async fn show(db: &bilibi_database::Db, label: &str, tenant: i64) -> Result<(), Error> {
    let client = db.get().await?;
    let row = client
        .query_one(
            "SELECT status, to_jsonb(suspended_at) IS NOT NULL, suspension_reason, \
             (SELECT count(*) FROM sessions WHERE tenant_id=$1) FROM tenants WHERE id=$1",
            &[&tenant],
        )
        .await?;
    println!(
        "{label}: tenant {tenant} status={} suspended_at_set={} reason={:?} sessions={}",
        row.get::<_, String>(0),
        row.get::<_, bool>(1),
        row.get::<_, Option<String>>(2),
        row.get::<_, i64>(3)
    );
    Ok(())
}
/// The two identity proofs Queue makes before it runs a job's handler.
async fn queue_edges(app: &bilibi_web::App, db: &bilibi_database::Db, tenant: i64, user: i64) {
    let client = db.get().await.expect("connection");
    let auth = app
        .services
        .get::<Authentication>("base/tenancy.authentication")
        .expect("authentication");
    println!(
        "  person's job   (authenticate_job, user {user}): {}",
        match auth.authenticate_job(&**client, tenant, user, None).await {
            Ok(_) => "admitted".to_owned(),
            Err(error) => format!("refused ({error:?}) -> Queue cancels with actor_refused"),
        }
    );
    println!(
        "  system job     (SystemScope::rehydrate, scheduler): {}",
        match SystemScope::rehydrate(&**client, tenant, Some("scheduler")).await {
            Ok(_) => "admitted".to_owned(),
            Err(error) => format!("refused ({error:?}) -> Queue cancels with actor_refused"),
        }
    );
}
#[tokio::main]
async fn main() -> Result<(), Error> {
    if std::env::var("BILIBI_TENANT_DEMO").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_TENANT_DEMO=disposable and DATABASE_URL to a disposable migrated database"
                .into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_module_registry::discover()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    db.get()
        .await?
        .batch_execute(
            "DELETE FROM platform_operator; INSERT INTO platform_operator(singleton,tenant_id) VALUES(true,1)",
        )
        .await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35573")?;
    // This fixture enrols no authenticator, so the second-factor policy is
    // off here (README of base/step_up); the browser proof exercises it.
    app.services
        .get::<bilibi_step_up::StepUp>(bilibi_step_up::SERVICE)?
        .set_enforced(false);
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let password = bilibi_walkthrough_fixture::PASSWORD;
    // The operator (tenant 1) and a person of the tenant to suspend (tenant 2).
    let admin_login = app.user.login("admin@example.test", password).await?;
    let admin = app.security.guard(&admin_login.token).await?;
    let browser = Browser {
        app: &app,
        token: &admin_login.token,
    };
    let member_login = app
        .user
        .login("manager@other.example.test", password)
        .await?;
    app.security.guard(&member_login.token).await?;
    const SUSPEND: &str = "core.tenant.tenant.suspend";
    const REACTIVATE: &str = "core.tenant.tenant.reactivate";

    println!("== 1. before: tenant 2 is active, its person is signed in");
    show(&db, "state", 2).await?;
    queue_edges(&app, &db, 2, 5).await;

    println!("== 2. the review says what suspending does (nothing is written)");
    println!(
        "  {}",
        plans
            .preview(&browser, &operation(SUSPEND, 2, "Unpaid invoices"))
            .await?
    );

    println!("== 3. refusals: no reason, the operator's own tenant, an unknown tenant");
    write(&plans, &admin, &browser, &operation(SUSPEND, 2, "   ")).await;
    write(&plans, &admin, &browser, &operation(SUSPEND, 1, "Test")).await;
    write(&plans, &admin, &browser, &operation(SUSPEND, 999, "Test")).await;
    write(
        &plans,
        &admin,
        &browser,
        &operation(REACTIVATE, 2, "Not suspended yet"),
    )
    .await;
    show(&db, "state (unchanged)", 2).await?;

    println!("== 4. suspend tenant 2 with a reason");
    write(
        &plans,
        &admin,
        &browser,
        &operation(SUSPEND, 2, "Unpaid invoices"),
    )
    .await;
    show(&db, "state", 2).await?;
    println!(
        "  the signed-in person's session now: {:?}",
        app.security
            .guard(&member_login.token)
            .await
            .map(|_| "still valid")
    );
    println!(
        "  signing in again: {:?}",
        app.user
            .login("manager@other.example.test", password)
            .await
            .map(|_| "signed in")
    );
    queue_edges(&app, &db, 2, 5).await;
    write(&plans, &admin, &browser, &operation(SUSPEND, 2, "Again")).await;

    println!("== 5. reactivate with a reason");
    write(
        &plans,
        &admin,
        &browser,
        &operation(REACTIVATE, 2, "Invoices paid"),
    )
    .await;
    show(&db, "state", 2).await?;
    println!(
        "  the old session stays ended: {:?}",
        app.security
            .guard(&member_login.token)
            .await
            .map(|_| "valid")
    );
    println!(
        "  signing in again: {:?}",
        app.user
            .login("manager@other.example.test", password)
            .await
            .map(|_| "signed in")
    );
    queue_edges(&app, &db, 2, 5).await;

    println!("== 6. the audit trail: the status moves with their reasons, and each ended session");
    let client = db.get().await?;
    for row in client
        .query(
            "SELECT auditable_type, auditable_id, event, actor_type, old_values::text, new_values::text \
             FROM base_audit_mutations WHERE auditable_type IN ('tenancy.tenant') ORDER BY id",
            &[],
        )
        .await?
    {
        println!(
            "  {} {} {} by {}: {} -> {}",
            row.get::<_, String>(0),
            row.get::<_, String>(1),
            row.get::<_, String>(2),
            row.get::<_, String>(3),
            row.get::<_, Option<String>>(4).unwrap_or_default(),
            row.get::<_, Option<String>>(5).unwrap_or_default()
        );
    }
    let ended: i64 = client
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE auditable_type='session' AND new_values @> '{\"ended_by_tenant_suspension\":true}'",
            &[],
        )
        .await?
        .get(0);
    println!("  sessions audited as ended by the suspension: {ended}");
    Ok(())
}
