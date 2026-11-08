//! Staff choice proof: a holder of a registered staff-assignment capability (no
//! `admin.user.*` grant) finds active staff of its own tenant by name, at most
//! `STAFF_CHOICE_MAX` at a time, and never a deactivated, deleted, SCIM-off,
//! retired-Person or other-tenant account.
use bilibi_database::pool;
use bilibi_user::{UserService, STAFF_CHOICE_MAX};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
// The proof's own assignment capability, declared by this module and held by the manager only.
const ASSIGN: &str = "admin.staff-proof.update";
bilibi_user::inventory::submit! { bilibi_user::Contribution::new(bilibi_authz::VOCABULARY, "staff-choices.proof", bilibi_user::Provenance { module: "core/user", source: file!(), line: line!() }, &bilibi_authz::VocabularyContribution {
    domains: &[], verbs: &[],
    capabilities: &[bilibi_authz::CapabilityDefinition::tenant(ASSIGN)],
    roles: &[bilibi_authz::RoleDefinition { code: "manager", name: "manager", description: "Walkthrough fixture role", grant_all: false, capabilities: &[ASSIGN] }],
    system_principals: &[],
}) }
bilibi_user::declare_staff_assignment!("core/user", bilibi_user::StaffAssignment::new(ASSIGN));
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_STAFF_CHOICES_DEMO").as_deref() != Ok("disposable") {
        return Err(
            "use BILIBI_STAFF_CHOICES_DEMO=disposable and a migrated disposable DATABASE_URL"
                .into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &std::env::var("APP_ORIGIN")?)?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let users = app.services.get::<UserService>("core/user.accounts")?;
    let sign_in = |email: &'static str| {
        let users = users.clone();
        let security = app.security.clone();
        async move {
            let token = users
                .login(email, bilibi_walkthrough_fixture::PASSWORD)
                .await?
                .token;
            Ok::<_, Box<dyn std::error::Error>>(security.guard(&token).await?)
        }
    };
    let names = |found: Vec<bilibi_user::StaffChoice>| {
        found
            .into_iter()
            .map(|c| format!("{}:{}", c.id, c.name))
            .collect::<Vec<_>>()
    };
    // Lee Viewer holds companies.view but no admin.user.* capability.
    let viewer = sign_in("viewer@example.test").await?;
    let manager = sign_in("manager@example.test").await?;
    println!(
        "Viewer holds `companies.view`, not a registered assignment capability: {:?}",
        users
            .search_staff(&viewer, "companies.view", "a")
            .await
            .err()
    );
    println!(
        "Manager holds `companies.update`, not registered either: {:?}",
        users
            .search_staff(&manager, "companies.update", "a")
            .await
            .err()
    );
    println!(
        "Viewer, registered {ASSIGN} (not held): {:?}",
        users.search_staff(&viewer, ASSIGN, "a").await.err()
    );
    println!(
        "Manager, registered {ASSIGN} (held), text 'a': {:?}",
        names(users.search_staff(&manager, ASSIGN, "a").await?)
    );
    println!(
        "Viewer, unknown capability (unregistered): {:?}",
        users
            .search_staff(&viewer, "no.such.capability", "")
            .await
            .err()
    );
    println!(
        "Word start 'con' finds a second word; 'ontractor' finds nothing: {:?} / {:?}",
        names(users.search_staff(&manager, ASSIGN, "con").await?),
        names(users.search_staff(&manager, ASSIGN, "ontractor").await?)
    );
    println!(
        "Typed wildcard '%' stays literal: {:?}",
        names(users.search_staff(&manager, ASSIGN, "%").await?)
    );
    println!(
        "Over-long text: {:?}",
        users
            .search_staff(&manager, ASSIGN, &"x".repeat(256))
            .await
            .err()
    );
    // Other tenant: tenant 2's account is not offered to tenant 1, and tenant
    // 1's staff are not offered to tenant 2.
    let other = sign_in("manager@other.example.test").await?;
    println!(
        "Tenant 1 empty text sees tenant 2's account: {}",
        users
            .search_staff(&manager, ASSIGN, "")
            .await?
            .iter()
            .any(|c| c.id == 5)
    );
    println!(
        "Tenant 2 empty text: {:?}",
        names(users.search_staff(&other, ASSIGN, "").await.or_else(|e| {
            // The other tenant's manager holds no walkthrough role here.
            println!("  tenant 2 caller refused: {e:?}");
            Ok::<_, bilibi_database::Error>(Vec::new())
        })?)
    );
    // Inactive exclusion, one reason at a time, on the disposable database.
    let client = db.get().await?;
    let still = |label: &'static str, id: i64| {
        let users = users.clone();
        let manager = manager.clone();
        async move {
            let found = users.search_staff(&manager, ASSIGN, "").await?;
            println!(
                "{label}: user {id} offered = {}",
                found.iter().any(|c| c.id == id)
            );
            Ok::<_, bilibi_database::Error>(())
        }
    };
    still("Before any change", 3).await?;
    client
        .execute("UPDATE users SET status='disabled' WHERE id=3", &[])
        .await?;
    still("Account disabled", 3).await?;
    client
        .execute(
            "UPDATE users SET status='active', deleted_at=now() WHERE id=3",
            &[],
        )
        .await?;
    still("Account deleted", 3).await?;
    client
        .execute("UPDATE users SET deleted_at=NULL WHERE id=3", &[])
        .await?;
    client
        .execute(
            "UPDATE user_identities SET status='deleted', deleted_at=now() WHERE id=3",
            &[],
        )
        .await?;
    still("Sign-in identity deleted", 3).await?;
    client
        .execute(
            "UPDATE user_identities SET status='active', deleted_at=NULL WHERE id=3",
            &[],
        )
        .await?;
    // A Person of its own, so retiring it freezes only the account linked to it.
    client
        .execute(
            "INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(900,1,'proof_retiree','Proof Retiree','human') ON CONFLICT DO NOTHING",
            &[],
        )
        .await?;
    client
        .execute("UPDATE users SET person_id=900 WHERE id=3", &[])
        .await?;
    client
        .execute("UPDATE persons SET retired_at=now() WHERE id=900", &[])
        .await?;
    still("Linked Person retired", 3).await?;
    client
        .execute("UPDATE persons SET retired_at=NULL WHERE id=900", &[])
        .await?;
    still("Linked Person live again", 3).await?;
    // The bound: 25 extra staff named 'Bulk Nn'.
    // One batch is one transaction: an identity and its account name each other.
    client
        .batch_execute(
            "INSERT INTO user_identities(id,kind,email,password_hash,home_tenant_id,home_user_id,last_tenant_id) OVERRIDING SYSTEM VALUE SELECT 100+n,'staff','bulk'||n||'@example.test','x',1,100+n,1 FROM generate_series(1,25) n ON CONFLICT DO NOTHING;
             INSERT INTO users(id,tenant_id,identity_id,name) SELECT 100+n,1,100+n,'Bulk '||lpad(n::text,2,'0') FROM generate_series(1,25) n ON CONFLICT DO NOTHING",
        )
        .await?;
    let found = users.search_staff(&manager, ASSIGN, "").await?;
    println!(
        "25 more staff, empty text: {} results (bound {STAFF_CHOICE_MAX}), first {:?}",
        found.len(),
        found.first().map(|c| &c.name)
    );
    println!(
        "Text 'bulk': {} results, last {:?}",
        users.search_staff(&manager, ASSIGN, "bulk").await?.len(),
        users
            .search_staff(&manager, ASSIGN, "BULK")
            .await?
            .last()
            .map(|c| &c.name)
    );
    Ok(())
}
