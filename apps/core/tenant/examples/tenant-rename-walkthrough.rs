//! Reviewed tenant rename on a disposable database, including denied writes.
use bilibi_agent_api::plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans};
use bilibi_database::pool;
use bilibi_tenancy::ProofFuture;
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
fn rename(id: i64, version: i64, name: &str) -> [PlannedOperation; 1] {
    [PlannedOperation {
        key: bilibi_tenant::RENAME.into(),
        input: json!({"tenant_id":id,"version":version,"name":name}),
    }]
}
#[tokio::main]
async fn main() -> Result<(), Error> {
    if std::env::var("BILIBI_TENANT_DEMO").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_TENANT_DEMO=disposable and DATABASE_URL to a disposable database".into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_module_registry::discover()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    db.get().await?.batch_execute("INSERT INTO role_capabilities(tenant_id,role_id,capability) VALUES(2,5,'tenancy.tenant.rename'),(2,5,'tenancy.tenant.view') ON CONFLICT DO NOTHING").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35573")?;
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let password = bilibi_walkthrough_fixture::PASSWORD;
    let admin = app.user.login("admin@example.test", password).await?;
    let admin_scope = app.security.guard(&admin.token).await?;
    let admin_browser = Browser {
        app: &app,
        token: &admin.token,
    };
    let own = app
        .user
        .login("manager@other.example.test", password)
        .await?;
    let own_scope = app.security.guard(&own.token).await?;
    let own_browser = Browser {
        app: &app,
        token: &own.token,
    };
    let viewer = app.user.login("viewer@example.test", password).await?;
    let viewer_scope = app.security.guard(&viewer.token).await?;
    let viewer_browser = Browser {
        app: &app,
        token: &viewer.token,
    };
    println!(
        "review operator rename: {}",
        plans
            .preview(&admin_browser, &rename(2, 1, "  Renamed workspace  "))
            .await?
    );
    println!(
        "operator commit: {}",
        plans
            .commit(
                &admin_scope,
                &admin_browser,
                &rename(2, 1, "  Renamed workspace  ")
            )
            .await?
    );
    println!(
        "own tenant review: {}",
        plans
            .preview(&own_browser, &rename(2, 2, "Own workspace"))
            .await?
    );
    println!(
        "own tenant commit: {}",
        plans
            .commit(&own_scope, &own_browser, &rename(2, 2, "Own workspace"))
            .await?
    );
    println!(
        "unauthorized viewer refused: {:?}",
        plans
            .commit(&viewer_scope, &viewer_browser, &rename(1, 1, "Forbidden"))
            .await
    );
    println!(
        "cross-tenant owner refused: {:?}",
        plans
            .commit(&own_scope, &own_browser, &rename(1, 1, "Forbidden"))
            .await
    );
    println!(
        "stale edit refused: {:?}",
        plans
            .commit(&admin_scope, &admin_browser, &rename(2, 1, "Stale"))
            .await
    );
    let client = db.get().await?;
    for row in client.query("SELECT auditable_id, old_values::text, new_values::text FROM base_audit_mutations WHERE auditable_type='tenancy.tenant' ORDER BY id", &[]).await? {
        println!("audit tenant {}: {} -> {}", row.get::<_,String>(0), row.get::<_,Option<String>>(1).unwrap_or_default(), row.get::<_,Option<String>>(2).unwrap_or_default());
    }
    Ok(())
}
