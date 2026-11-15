//! Live authorization proof for an already-open impersonated session.
//! Run only on a freshly migrated disposable database.
use bilibi_audit::RequestInfo;
use bilibi_authz::grants::Grants;
use bilibi_database::pool;
use bilibi_tenancy::Scope;
use bilibi_user::UserService;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const OPERATOR: i64 = 1;
const TARGET: i64 = 7;
const IMPERSONATE: &str = "admin.user.impersonate";
const NEW_CAPABILITY: &str = "admin.authz.role.update";
const RESTRICTED_TABLE: &str = "companies";
const RESTRICTED_FIELD: &str = "walkthrough_field";
const REVOKED_CAPABILITY: &str = "companies.view";

// The fixture manager may enter impersonation, but does not hold the stronger
// authorization-administration capability used in the first live-change proof.
inventory::submit! {
    bilibi_module_registry::Contribution::new(
        bilibi_authz::VOCABULARY,
        "walkthrough.impersonation-capabilities",
        bilibi_module_registry::Provenance {
            module: "core/user",
            source: file!(),
            line: line!(),
        },
        &bilibi_authz::VocabularyContribution {
            domains: &[],
            verbs: &[],
            capabilities: &[],
            roles: &[bilibi_authz::RoleDefinition {
                code: "manager",
                name: "manager",
                description: "Walkthrough fixture role",
                grant_all: false,
                capabilities: &[IMPERSONATE],
            }],
            system_principals: &[],
        },
    )
}

async fn assert_intersection_denies(
    client: &Client,
    scope: &Scope,
    key: &str,
    reason: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision = bilibi_authz::decide_in_company(
        client,
        scope,
        key,
        scope.current_company().map(|company| company.get()),
    )
    .await?;
    assert!(!decision.allowed);
    assert_eq!(decision.reason, reason);
    assert!(!bilibi_authz::holds_proof(client, scope, key).await?);
    assert!(!bilibi_authz::effective_capabilities(client, scope)
        .await?
        .contains(key));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_IMPERSONATION_CAPABILITIES_DEMO").as_deref() != Ok("disposable") {
        return Err("use a migrated disposable database for this walkthrough".into());
    }

    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let app = bilibi_web::App::new(db.clone(), &std::env::var("APP_ORIGIN")?)?;
    let users = app.services.get::<UserService>("core/user.accounts")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;

    let administrator = app
        .security
        .guard(
            &users
                .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
                .await?
                .token,
        )
        .await?;
    let login = users
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let operator = app.security.guard(&login.token).await?;
    assert_eq!(operator.user(), OPERATOR);
    let client = db.get().await?;
    assert!(
        bilibi_authz::decide(&**client, &operator, REVOKED_CAPABILITY)
            .await?
            .allowed
    );
    assert!(
        !bilibi_authz::decide(&**client, &operator, NEW_CAPABILITY)
            .await?
            .allowed
    );
    drop(client);

    users
        .impersonate(&login.token, &login.csrf, TARGET, &RequestInfo::default())
        .await?;
    let impersonated = app.security.guard(&login.token).await?;
    assert_eq!(impersonated.user(), TARGET);
    assert_eq!(
        impersonated
            .impersonator()
            .map(bilibi_tenancy::Impersonator::user),
        Some(OPERATOR)
    );
    assert_eq!(
        impersonated.current_company().map(|company| company.get()),
        Some(2)
    );

    grants
        .set_direct(&administrator, TARGET, NEW_CAPABILITY, Some(true))
        .await?;
    let client = db.get().await?;
    assert!(
        bilibi_authz::effective_capabilities_of(&**client, &administrator, TARGET)
            .await?
            .allowed
            .contains(NEW_CAPABILITY)
    );
    assert_intersection_denies(
        &client,
        &impersonated,
        NEW_CAPABILITY,
        "denied_missing_capability",
    )
    .await?;
    println!("PASS: a capability granted to the viewed account stays refused");

    drop(client);
    grants
        .set_direct(&administrator, OPERATOR, REVOKED_CAPABILITY, Some(false))
        .await?;
    let client = db.get().await?;
    assert!(
        bilibi_authz::effective_capabilities_of(&**client, &administrator, TARGET)
            .await?
            .allowed
            .contains(REVOKED_CAPABILITY)
    );
    assert_intersection_denies(
        &client,
        &impersonated,
        REVOKED_CAPABILITY,
        "denied_explicitly",
    )
    .await?;
    println!("PASS: a capability revoked from the operator is refused immediately");

    let role: i64 = client
        .query_one(
            "SELECT role_id FROM effective_principal_roles WHERE tenant_id=$1 AND user_id=$2 AND role_id NOT IN (SELECT role_id FROM effective_principal_roles WHERE tenant_id=$1 AND user_id=$3) ORDER BY role_id LIMIT 1",
            &[&impersonated.tenant(), &OPERATOR, &TARGET],
        )
        .await?
        .get(0);
    client
        .execute(
            "INSERT INTO field_restrictions(tenant_id,role_id,table_id,field_id) VALUES($1,$2,$3,$4)",
            &[&impersonated.tenant(), &role, &RESTRICTED_TABLE, &RESTRICTED_FIELD],
        )
        .await?;
    assert!(
        !bilibi_authz::restrictions_for(&**client, &administrator, RESTRICTED_TABLE)
            .await?
            .contains(RESTRICTED_FIELD)
    );
    assert!(
        bilibi_authz::restrictions_for(&**client, &impersonated, RESTRICTED_TABLE)
            .await?
            .contains(RESTRICTED_FIELD)
    );
    println!("PASS: a field the operator is restricted from stays restricted while impersonating");
    Ok(())
}
