//! Reviewed tenant rename on a disposable database, including denied writes.
use bilibi_agent_api::plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans};
use bilibi_agent_api::Error as ContractError;
use bilibi_database::{pool, Context};
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
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35573")?;
    {
        let grants = app
            .services
            .get::<bilibi_authz::grants::Grants>("base/authz.grants")?;
        let mut client = db.get().await?;
        let mut tx = app
            .mutations
            .begin(
                &mut client,
                Context::system(
                    2,
                    "tenant-rename-walkthrough",
                    "user.tenant_owner.bootstrap",
                ),
            )
            .await?;
        grants.grant_first_tenant_owner_in(&mut tx, 2, 5).await?;
        tx.commit().await?;
    }
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
    for (scope, browser, version, old, input, expected) in [
        (
            &admin_scope,
            &admin_browser,
            1,
            "Other Workspace",
            "  Renamed workspace  ",
            "Renamed workspace",
        ),
        (
            &own_scope,
            &own_browser,
            2,
            "Renamed workspace",
            "Own workspace",
            "Own workspace",
        ),
    ] {
        let operation = rename(2, version, input);
        let review = plans.preview(browser, &operation).await?;
        assert_eq!(review["valid"], true);
        let preview = &review["results"][0]["preview"]["result"];
        assert_eq!(
            preview["tenant"],
            json!({"id":2,"name":old,"name_version":version})
        );
        assert_eq!(preview["name"], expected);
        let commit = plans.commit(scope, browser, &operation).await?;
        assert_eq!(
            commit["results"][0]["result"]["result"],
            json!({"id":2,"name":expected,"name_version":version+1})
        );
        println!("Renamed {old} to {expected}");
    }
    for (scope, browser, operation, expected) in [
        (
            &viewer_scope,
            &viewer_browser,
            rename(1, 1, "Forbidden"),
            json!({"code":"missing_capability"}),
        ),
        (
            &own_scope,
            &own_browser,
            rename(1, 1, "Forbidden"),
            json!({"code":"missing_capability"}),
        ),
        (
            &admin_scope,
            &admin_browser,
            rename(2, 1, "Stale"),
            json!({"code":"rejected","reason":"record_changed"}),
        ),
    ] {
        let Err(ContractError::PlanRejected(results)) =
            plans.commit(scope, browser, &operation).await
        else {
            panic!("expected rename refusal {expected}");
        };
        assert_eq!(results[0]["valid"], false);
        assert_eq!(results[0]["error"], expected);
        println!("Rename refused with {expected}");
    }
    let client = db.get().await?;
    for (id, name, version) in [(1_i64, "Example Labs", 1_i64), (2, "Own workspace", 3)] {
        let row = client
            .query_one("SELECT name, name_version FROM tenants WHERE id=$1", &[&id])
            .await?;
        assert_eq!(row.get::<_, String>(0), name);
        assert_eq!(row.get::<_, i64>(1), version);
    }
    let audit = client
        .query(
            "SELECT old_values->>'name', new_values->>'name'
             FROM base_audit_mutations WHERE tenant_id IN (1,2)
                AND auditable_type='tenancy.tenant' AND auditable_id='2'
             ORDER BY id",
            &[],
        )
        .await?;
    let names: Vec<(String, String)> = audit.iter().map(|row| (row.get(0), row.get(1))).collect();
    assert_eq!(
        names,
        [
            ("Other Workspace".into(), "Renamed workspace".into()),
            ("Renamed workspace".into(), "Own workspace".into()),
        ]
    );
    println!("Audit recorded both name changes");
    Ok(())
}
