//! Inspect tenant identity and explicit desks through the real User/Company
//! facades for ADR 19, then the account takeover bounds (#530, #531).
//! Run on a freshly migrated disposable database: its instance role is
//! Production, so step-up is enforced.
use bilibi_audit::RequestInfo;
use bilibi_company::{CompanyService, SERVICE as COMPANIES};
use bilibi_database::pool;
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{Authentication, OrganisationDirectory};
use bilibi_user::{AccountError, UserService};
use std::{sync::Arc, time::Duration};
use tokio::sync::Barrier;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

/// A wrong password is counted after the sign-in answers (#536): waits until
/// the account's sign-in counter reads `failures` failures and `locks` locks.
async fn counted(
    client: &tokio_postgres::Client,
    tenant: i64,
    user: i64,
    failures: i32,
    locks: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..100 {
        let row = client
            .query_opt(
                "SELECT a.failures,a.locks FROM user_credential_attempts a JOIN users u ON u.identity_id=a.identity_id WHERE u.tenant_id=$1 AND u.id=$2 AND a.kind='sign_in'",
                &[&tenant, &user],
            )
            .await?;
        if row.is_some_and(|row| row.get::<_, i32>(0) == failures && row.get::<_, i32>(1) == locks)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(format!("account {user}: counter never reached {failures} failures, {locks} locks").into())
}

/// The lock notices mailed to `address` so far.
fn lock_notices(mailbox: &bilibi_mail::Mailbox, address: &str) -> usize {
    mailbox
        .entries()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| mailbox.read(&entry.id).ok())
        .filter(|message| {
            message.contains("Your Bilibi sign-in is locked") && message.contains(address)
        })
        .count()
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_IDENTITY_DEMO").as_deref() != Ok("disposable") {
        return Err(
            "use BILIBI_IDENTITY_DEMO=disposable and a migrated disposable DATABASE_URL".into(),
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
    let companies = app.services.get::<CompanyService>(COMPANIES)?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let manager_login = users
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let manager = app.security.guard(&manager_login.token).await?;
    // A successful portal sign-in after a wrong password records both the
    // counter reset and admission as the customer, not as a staff user.
    {
        let boundary = app
            .services
            .get::<bilibi_database::MutationBoundary>("base/database.mutations")?;
        let mut client = db.get().await?;
        let mut tx = boundary
            .begin(
                &mut client,
                manager.mutation_context("identity.customer.create"),
            )
            .await?;
        let customer = users
            .create_customer_in(
                &mut tx,
                &bilibi_user::NewCustomer {
                    name: "Identity customer",
                    email: "identity-customer@example.test",
                    password: bilibi_walkthrough_fixture::PASSWORD,
                    person: manager.actor().person().ok_or("fixture person missing")?,
                    affiliation: None,
                },
            )
            .await?;
        tx.commit().await?;
        assert!(users
            .portal_login(1, "identity-customer@example.test", "incorrect-password")
            .await
            .is_err());
        counted(&client, 1, customer, 1, 0).await?;
        let customer_login = users
            .portal_login(
                1,
                "identity-customer@example.test",
                bilibi_walkthrough_fixture::PASSWORD,
            )
            .await?;
        assert!(!customer_login.pending);
        let rows = client.query("SELECT event,actor_role FROM base_audit_actions WHERE tenant_id=1 AND actor_id=$1 AND event IN ('user.sign_in.passed','user.sign_in.password_verified') ORDER BY id", &[&customer]).await?;
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(
                row.get::<_, Option<String>>("actor_role").as_deref(),
                Some("customer")
            );
        }
        println!("PASS: portal credential-counter reset and session admission both retain customer audit attribution");
        // The owner API must return the same customer identity on both transitions,
        // deny another tenant and a customer actor, and end the live session.
        let customer_scope = authentication
            .authenticate(&**client, 1, customer, String::new())
            .await?;
        let other_login = users
            .login(
                "manager@other.example.test",
                bilibi_walkthrough_fixture::PASSWORD,
            )
            .await?;
        let other = app.security.guard(&other_login.token).await?;
        for (scope, forbidden) in [(&other, false), (&customer_scope, true)] {
            let mut tx = boundary
                .begin(
                    &mut client,
                    scope.mutation_context("identity.customer.denied"),
                )
                .await?;
            let result = users
                .set_customer_active_in(&mut tx, scope, customer, false)
                .await;
            assert!(if forbidden {
                matches!(result, Err(bilibi_database::Error::Forbidden))
            } else {
                matches!(result, Err(bilibi_database::Error::NotFound))
            });
            tx.rollback().await?;
        }
        let mut tx = boundary
            .begin(
                &mut client,
                manager.mutation_context("identity.customer.suspend"),
            )
            .await?;
        let suspended = users
            .set_customer_active_in(&mut tx, &manager, customer, false)
            .await?;
        assert_eq!(
            (suspended.id, suspended.email.as_str(), suspended.active),
            (customer, "identity-customer@example.test", false)
        );
        tx.commit().await?;
        assert!(app
            .security
            .guard_portal(&customer_login.token)
            .await
            .is_err());
        assert!(users
            .portal_login(
                1,
                "identity-customer@example.test",
                bilibi_walkthrough_fixture::PASSWORD
            )
            .await
            .is_err());
        let mut tx = boundary
            .begin(
                &mut client,
                manager.mutation_context("identity.customer.restore"),
            )
            .await?;
        let restored = users
            .set_customer_active_in(&mut tx, &manager, customer, true)
            .await?;
        assert_eq!(
            (restored.id, restored.email.as_str(), restored.active),
            (customer, "identity-customer@example.test", true)
        );
        tx.commit().await?;
        assert!(app
            .security
            .guard_portal(&customer_login.token)
            .await
            .is_err());
        assert!(users
            .portal_login(
                1,
                "identity-customer@example.test",
                bilibi_walkthrough_fixture::PASSWORD
            )
            .await
            .is_ok());
        println!("PASS: customer suspension and restoration preserve identity and isolate access");

        // #536: one lock notice per run of locks. Two locks in a row for the
        // viewer (the first lock is made to expire) mail one notice.
        let mailbox = bilibi_mail::Mailbox::new(
            std::env::var("BILIBI_MAIL_DIR")
                .unwrap_or_else(|_| ".local/mailbox".into())
                .into(),
            100,
        );
        let viewer = "viewer@example.test";
        let before = lock_notices(&mailbox, viewer);
        for run in 1..=2 {
            for attempt in 1..=5 {
                assert!(users.login(viewer, "incorrect-password").await.is_err());
                let (failures, locks) = if attempt == 5 {
                    (0, run)
                } else {
                    (attempt, run - 1)
                };
                counted(&client, 1, 2, failures, locks).await?;
            }
            if run == 1 {
                for _ in 0..50 {
                    if lock_notices(&mailbox, viewer) > before {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                client
                    .execute(
                        "UPDATE user_credential_attempts SET locked_until=now()-interval '1 second' WHERE identity_id=(SELECT identity_id FROM users WHERE tenant_id=1 AND id=2) AND kind='sign_in'",
                        &[],
                    )
                    .await?;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let notices = lock_notices(&mailbox, viewer) - before;
        println!("Two sign-in locks in a row mailed {notices} notice(s)");
        assert_eq!(notices, 1);

        // #536: an administrator reads and lifts the lock of a staff account
        // only; a customer account of the same tenant is not one.
        for _ in 1..=5 {
            assert!(users
                .portal_login(1, "identity-customer@example.test", "incorrect-password")
                .await
                .is_err());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        counted(&client, 1, customer, 0, 1).await?;
        let admin_login = users
            .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        let admin = app.security.guard(&admin_login.token).await?;
        assert!(users
            .sign_in_locked_until(&admin, customer)
            .await?
            .is_none());
        assert!(matches!(
            users.unlock_sign_in(&admin, customer).await,
            Err(AccountError::NotFound)
        ));
        counted(&client, 1, customer, 0, 1).await?;
        assert!(users.sign_in_locked_until(&admin, 2).await?.is_some());
        users
            .unlock_sign_in(&admin, 2)
            .await
            .map_err(|error| format!("unlock the viewer: {error}"))?;
        assert!(users.sign_in_locked_until(&admin, 2).await?.is_none());
        println!("PASS: lock notices once per run; Unlock reaches staff accounts only");
    }
    let login = users
        .login(
            "contractor@example.test",
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await?;
    let contractor = app.security.guard(&login.token).await?;
    println!(
        "Staff identity: tenant {}, person {:?}, company facet {:?}, desk {:?}",
        contractor.tenant(),
        contractor.actor().person().map(|id| id.get()),
        contractor.actor().organisation().map(|id| id.get()),
        contractor.current_company().map(|id| id.get())
    );
    let person = contractor
        .actor()
        .person()
        .ok_or("fixture person missing")?
        .get();
    let manager_person = manager
        .actor()
        .person()
        .ok_or("fixture manager person missing")?
        .get();
    users.set_desk(&manager, 1, person, true).await?;
    users.set_desk(&manager, 2, manager_person, true).await?;
    let sql = db.get().await?;
    let chosen = authentication
        .authenticate_session(
            &**sql,
            contractor.tenant(),
            contractor.user(),
            None,
            Some(2),
            String::new(),
        )
        .await?;
    bilibi_company::authorize_target(&**sql, &chosen, 2, bilibi_company::UPDATE).await?;
    let selectable = bilibi_company::Directory
        .selectable(&**sql, &chosen, bilibi_company::UPDATE)
        .await?;
    if selectable
        .iter()
        .map(|company| company.id.get())
        .collect::<Vec<_>>()
        != vec![2]
    {
        return Err("Company administration chooser did not stay on the current desk".into());
    }
    if !matches!(
        bilibi_company::authorize_target(&**sql, &chosen, 1, bilibi_company::UPDATE).await,
        Err(bilibi_database::Error::Forbidden)
    ) {
        return Err("An extra desk widened Company administration".into());
    }
    println!("Current desk company is administrable; the extra desk is refused");
    println!(
        "Two desks: {:?}",
        users
            .list_company_accounts(&manager, 1)
            .await?
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>()
    );
    let manager_a = authentication
        .authenticate_session(
            &**sql,
            manager.tenant(),
            manager.user(),
            None,
            Some(1),
            String::new(),
        )
        .await?;
    let manager_b = authentication
        .authenticate_session(
            &**sql,
            manager.tenant(),
            manager.user(),
            None,
            Some(2),
            String::new(),
        )
        .await?;
    if manager_a.current_company().map(|company| company.get()) != Some(1)
        || manager_b.current_company().map(|company| company.get()) != Some(2)
    {
        return Err("Opposite-company manager scopes did not select their requested desks".into());
    }
    let barrier = Arc::new(Barrier::new(3));
    let users_a = users.clone();
    let barrier_a = barrier.clone();
    let remove_b = tokio::spawn(async move {
        barrier_a.wait().await;
        users_a.set_desk(&manager_a, 2, person, false).await
    });
    let users_b = users.clone();
    let barrier_b = barrier.clone();
    let remove_a = tokio::spawn(async move {
        barrier_b.wait().await;
        users_b.set_desk(&manager_b, 1, person, false).await
    });
    barrier.wait().await;
    let (remove_b, remove_a) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(remove_b, remove_a)
    })
    .await
    .map_err(|_| "Opposite-company desk writes did not finish")?;
    remove_b??;
    remove_a??;
    println!("Opposite-company desk writes completed without deadlock: true");
    users.set_desk(&manager, 1, person, true).await?;
    users
        .set_desk(&manager, 2, person, true)
        .await
        .map_err(|error| format!("Could not restore company 2 desk for archive proof: {error}"))?;
    let mut proof_client = db.get().await?;
    let proof = proof_client.transaction().await?;
    if !bilibi_company::reachable(&*proof, &chosen, OrganisationId::new(2)?).await? {
        return Err("Exact desk proof was refused".into());
    }
    let remove_users = users.clone();
    let remove_manager = manager.clone();
    let mut removal = tokio::spawn(async move {
        remove_users
            .set_desk(&remove_manager, 2, person, false)
            .await
    });
    if tokio::time::timeout(Duration::from_millis(100), &mut removal)
        .await
        .is_ok()
    {
        return Err("Desk removal did not wait for the exact work proof".into());
    }
    proof.commit().await?;
    removal.await??;
    println!("Desk removal waited for the exact work transaction: true");
    let refreshed = authentication.reprove(&**sql, &chosen).await?;
    println!(
        "Removed chosen desk 2; desk after explicit reproof {:?}",
        refreshed.current_company().map(|id| id.get())
    );
    println!(
        "Desk removal ended the account's sessions: {}",
        app.security.guard(&login.token).await.is_err()
    );
    println!(
        "Cross-tenant assignment refused: {}",
        users.set_desk(&manager, 100, person, true).await.is_err()
    );
    users
        .set_desk(&manager, 2, person, true)
        .await
        .map_err(|error| format!("Could not prepare company 2 desk for archive: {error}"))?;
    sql.execute(
        "INSERT INTO authz_identity_policies(tenant_id,source,company_id,role_ids) VALUES($1,$2,$3,$4)",
        &[&1_i64, &"walkthrough:archive", &2_i64, &vec![2_i64]],
    )
    .await?;
    sql.execute(
        "INSERT INTO authz_identity_roles(tenant_id,source,user_id,role_id) VALUES($1,$2,$3,$4)",
        &[&1_i64, &"walkthrough:archive", &contractor.user(), &2_i64],
    )
    .await?;
    if !bilibi_authz::has_effective_role(&**sql, &manager, 2, contractor.user()).await? {
        return Err("Provider-sourced role fixture was not effective".into());
    }
    let other_desk_login = users
        .login(
            "contractor@example.test",
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await
        .map_err(|error| format!("Could not create the retained-desk session: {error}"))?;
    let archived_desk_login = users
        .login(
            "contractor@example.test",
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await
        .map_err(|error| format!("Could not create the archived-desk session: {error}"))?;
    users
        .choose_company(
            &archived_desk_login.token,
            &archived_desk_login.csrf,
            Some(2),
            &RequestInfo::default(),
        )
        .await?;
    let impersonation_login = users
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|error| format!("Could not create the impersonation session: {error}"))?;
    users
        .impersonate(
            &impersonation_login.token,
            &impersonation_login.csrf,
            contractor.user(),
            &RequestInfo::default(),
        )
        .await
        .map_err(|error| format!("Could not enter the effective-default impersonation: {error}"))?;
    let impersonated_company: Option<i64> = sql
        .query_one(
            "SELECT company_id FROM sessions WHERE id=$1",
            &[&bilibi_session::session_id(&impersonation_login.token)],
        )
        .await?
        .get(0);
    if impersonated_company != Some(2) {
        return Err("Impersonation did not persist the viewed account's default desk".into());
    }
    let archive_barrier = Arc::new(Barrier::new(3));
    let archive_companies = companies.clone();
    let archive_manager = manager.clone();
    let archive_ready = archive_barrier.clone();
    let archive = tokio::spawn(async move {
        archive_ready.wait().await;
        archive_companies
            .archive_company(&archive_manager, 2, Some("Disposable desk walkthrough"))
            .await
    });
    let write_users = users.clone();
    let write_manager = manager.clone();
    let write_ready = archive_barrier.clone();
    let concurrent_write = tokio::spawn(async move {
        write_ready.wait().await;
        write_users.set_desk(&write_manager, 2, person, true).await
    });
    archive_barrier.wait().await;
    let (archive, concurrent_write) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(archive, concurrent_write)
    })
    .await
    .map_err(|_| "Archive and opposite-company write did not finish")?;
    archive??;
    match concurrent_write? {
        // If archive owns the company row first, desk assignment observes an
        // archived target and is refused. If assignment owns the row first,
        // the desk already exists, so the write changes nothing and
        // MutationBoundary refuses it as invalid.
        Ok(_) | Err(bilibi_database::Error::Forbidden) | Err(bilibi_database::Error::Invalid) => {}
        Err(error) => return Err(error.into()),
    }
    let other_desk = app.security.guard(&other_desk_login.token).await?;
    let archived_session = app.security.guard(&archived_desk_login.token).await?;
    let impersonated_session = app.security.guard(&impersonation_login.token).await?;
    let retained_desk: bool = sql
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM user_desks WHERE tenant_id=$1 AND person_id=$2 AND company_id=$3)",
            &[&1_i64, &person, &1_i64],
        )
        .await?
        .get(0);
    let archived_desk: bool = sql
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM user_desks WHERE tenant_id=$1 AND person_id=$2 AND company_id=$3)",
            &[&1_i64, &person, &2_i64],
        )
        .await?
        .get(0);
    let remembered_desk: Option<i64> = sql
        .query_one(
            "SELECT last_desk_id FROM users WHERE tenant_id=$1 AND id=$2",
            &[&1_i64, &contractor.user()],
        )
        .await?
        .get(0);
    let impersonation_audited: bool = sql
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM base_audit_actions WHERE tenant_id=$1 AND event='impersonation.started' AND payload->'subject'->>'id'=$2)",
            &[&1_i64, &contractor.user().to_string()],
        )
        .await?
        .get(0);
    let provider_role =
        bilibi_authz::has_effective_role(&**sql, &manager, 2, contractor.user()).await?;
    let manual_role =
        bilibi_authz::has_effective_role(&**sql, &manager, 3, contractor.user()).await?;
    let reaches_archived =
        bilibi_company::reachable(&**sql, &archived_session, OrganisationId::new(2)?).await?;
    if other_desk.current_company().map(|id| id.get()) != Some(1)
        || archived_session.current_company().map(|id| id.get()) != Some(2)
        || impersonated_session.current_company().map(|id| id.get()) != Some(2)
        || !reaches_archived
        || !retained_desk
        || !archived_desk
        || remembered_desk != Some(2)
        || !provider_role
        || !manual_role
        || !impersonation_audited
    {
        return Err("Company archive changed workplace access".into());
    }
    println!(
        "Archive and opposite-company write completed; the archived company kept its desks, remembered choice, provider and manual roles and the sessions working there (read-only), other workplace access stayed, and the impersonation entry audit was kept: true"
    );
    // Access to an archived company only narrows: a desk there can be
    // removed, and no new desk is given.
    users.set_desk(&manager, 2, person, false).await?;
    let reassigned = users.set_desk(&manager, 2, person, true).await;
    if !matches!(reassigned, Err(bilibi_database::Error::Forbidden)) {
        return Err(
            format!("A desk in an archived company was not refused: {reassigned:?}").into(),
        );
    }
    println!("Archived company desk removed; a new desk there was refused: true");
    println!(
        "Archived workplace does not freeze tenant account: {}",
        users
            .login(
                "contractor@example.test",
                bilibi_walkthrough_fixture::PASSWORD
            )
            .await
            .is_ok()
    );
    users.set_desk(&manager, 1, person, false).await?;
    let no_desk = authentication
        .authenticate(
            &**sql,
            contractor.tenant(),
            contractor.user(),
            String::new(),
        )
        .await?;
    println!(
        "No desks: {:?}; tenant account remains active",
        no_desk.current_company().map(|id| id.get())
    );
    account_bounds(&app, &db, &users).await
}

/// An account's current version, which an inline edit carries.
async fn version(sql: &tokio_postgres::Client, user: i64) -> Result<i64, tokio_postgres::Error> {
    Ok(sql
        .query_one(
            "SELECT version FROM users WHERE tenant_id=1 AND id=$1",
            &[&user],
        )
        .await?
        .get(0))
}

/// The step-up operation that guards setting another person's password or
/// sign-in email (`core/user` `accounts.rs`).
const SET_CREDENTIALS: &str = "core.user.account.credentials.set";

/// Account takeover bounds (#530, #531): a User Editor cannot set the password
/// or sign-in email of, or delete, an account holding more than they do;
/// setting another person's password or email needs step-up; impersonation
/// reaches only accounts holding nothing the operator lacks; and an
/// impersonated session creates, changes and deletes no account.
///
/// Mallory is the viewer (id 2) given the system role User Editor and
/// `admin.user.impersonate`. The administrator (id 4) holds a full-access role.
/// The contractor (id 7) is left with no role, so Mallory covers it, and staff
/// (id 3) is made a second User Editor for the impersonated writes.
async fn account_bounds(
    app: &bilibi_web::App,
    db: &bilibi_database::Db,
    users: &UserService,
) -> Result<(), Box<dyn std::error::Error>> {
    use bilibi_database::Error;
    use bilibi_user::{
        impersonation::Refusal, AccountError, AccountField, AccountInput, InvitationError,
        InvitationInput,
    };
    let password = bilibi_walkthrough_fixture::PASSWORD;
    let (mallory_id, admin_id, staff_id, contractor_id) = (2_i64, 4_i64, 3_i64, 7_i64);
    let sql = db.get().await?;
    let editor_role: i64 = sql
        .query_opt(
            "SELECT id FROM roles WHERE tenant_id=1 AND code='user_editor'",
            &[],
        )
        .await?
        .ok_or("the User Editor system role was not reconciled")?
        .get(0);
    sql.execute(
        "DELETE FROM principal_roles WHERE tenant_id=1 AND user_id IN ($1,$2)",
        &[&staff_id, &contractor_id],
    )
    .await?;
    sql.execute(
        "DELETE FROM authz_identity_roles WHERE tenant_id=1 AND user_id IN ($1,$2)",
        &[&staff_id, &contractor_id],
    )
    .await?;
    sql.execute(
        "INSERT INTO principal_roles(tenant_id,user_id,role_id) VALUES(1,$1,$3),(1,$2,$3) ON CONFLICT DO NOTHING",
        &[&mallory_id, &staff_id, &editor_role],
    )
    .await?;
    sql.execute(
        "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES(1,$1,'admin.user.impersonate',true) ON CONFLICT(tenant_id,user_id,capability) DO UPDATE SET is_allowed=true",
        &[&mallory_id],
    )
    .await?;
    let mallory_login = users.login("viewer@example.test", password).await?;
    let mallory = app.security.guard(&mallory_login.token).await?;

    // #530: the User Editor cannot take over or remove the administrator.
    let password_refused = users
        .set_password(&mallory, admin_id, "mallory-chose-this-password")
        .await;
    let email_refused = users
        .update_account(
            &mallory,
            admin_id,
            version(&sql, admin_id).await?,
            AccountField::Email,
            "mallory@example.test",
        )
        .await;
    let delete_refused = users.delete_account(&mallory, admin_id).await;
    if !matches!(password_refused, Err(AccountError::Outranked))
        || !matches!(email_refused, Err(AccountError::Outranked))
        || !matches!(delete_refused, Err(AccountError::Outranked))
    {
        return Err(format!(
            "A User Editor changed a full-access account: password {password_refused:?}, email {:?}, delete {delete_refused:?}",
            email_refused.map(|account| account.id)
        )
        .into());
    }
    users
        .login("admin@example.test", password)
        .await
        .map_err(|error| format!("The administrator's own password stopped working: {error}"))?;
    println!("PASS: a User Editor cannot set the password or sign-in email of, or delete, a full-access account; its password still signs in");

    // #530: setting another person's password or email asks for step-up,
    // which is on by default (this database's instance role is Production).
    let step_up = app
        .services
        .get::<bilibi_step_up::StepUp>(bilibi_step_up::SERVICE)?;
    if !step_up.required(&mallory, SET_CREDENTIALS).await? {
        return Err("Setting another person's password is not guarded by step-up by default; run on a freshly migrated database".into());
    }
    let password_step_up = users
        .set_password(&mallory, contractor_id, "covered-new-password")
        .await;
    let email_step_up = users
        .update_account(
            &mallory,
            contractor_id,
            version(&sql, contractor_id).await?,
            AccountField::Email,
            "contractor-moved@example.test",
        )
        .await;
    for (what, refused) in [
        ("password", password_step_up.as_ref().err()),
        ("email", email_step_up.as_ref().err()),
    ] {
        if !matches!(
            refused,
            Some(AccountError::Access(Error::StepUpRequired {
                operation: SET_CREDENTIALS,
                ..
            }))
        ) {
            return Err(format!(
                "Setting another person's {what} did not ask for step-up: {refused:?}"
            )
            .into());
        }
    }
    users
        .login("contractor@example.test", password)
        .await
        .map_err(|error| format!("A refused password change still took effect: {error}"))?;
    println!("PASS: setting another person's password or sign-in email is refused until step-up passes ({SET_CREDENTIALS})");

    // With enforcement off, as on a Dev server, a covered account's credentials change.
    step_up.set_enforced(false);
    users
        .set_password(&mallory, contractor_id, "covered-new-password")
        .await
        .map_err(|error| format!("A User Editor could not set a covered password: {error:?}"))?;
    let new_password_signs_in = users
        .login("contractor@example.test", "covered-new-password")
        .await
        .is_ok();
    users
        .update_account(
            &mallory,
            contractor_id,
            version(&sql, contractor_id).await?,
            AccountField::Email,
            "contractor-moved@example.test",
        )
        .await
        .map_err(|error| format!("A User Editor could not set a covered email: {error:?}"))?;
    if !new_password_signs_in {
        return Err("The covered account's new password does not sign in".into());
    }
    println!("PASS: once step-up passes, a User Editor sets the password and sign-in email of an account holding nothing more");

    // #531: impersonation reaches only an account holding nothing the
    // operator lacks, and the impersonated session changes no account.
    let outranked = users
        .impersonate(
            &mallory_login.token,
            &mallory_login.csrf,
            admin_id,
            &RequestInfo::default(),
        )
        .await;
    if !matches!(outranked, Err(Refusal::Outranked)) {
        return Err(
            format!("Impersonating a full-access account was not refused: {outranked:?}").into(),
        );
    }
    let csrf = users
        .impersonate(
            &mallory_login.token,
            &mallory_login.csrf,
            staff_id,
            &RequestInfo::default(),
        )
        .await
        .map_err(|error| format!("Impersonating a covered account was refused: {error:?}"))?;
    let viewing = app.security.guard(&mallory_login.token).await?;
    if viewing.user() != staff_id || viewing.impersonator().is_none() {
        return Err("The impersonation did not switch the session".into());
    }
    let set = users
        .set_password(&viewing, contractor_id, "impersonated-password")
        .await;
    let renamed = users
        .update_account(
            &viewing,
            contractor_id,
            version(&sql, contractor_id).await?,
            AccountField::Name,
            "Renamed while impersonating",
        )
        .await;
    let deleted = users.delete_account(&viewing, contractor_id).await;
    let created = users
        .create_account(
            &viewing,
            &AccountInput {
                name: "Created while impersonating".into(),
                email: "impersonated-create@example.test".into(),
                password: "impersonated-create-password".into(),
                ..AccountInput::default()
            },
        )
        .await;
    let invited = users
        .invite_member(
            &viewing,
            &InvitationInput {
                name: "Invited while impersonating".into(),
                email: "impersonated-invite@example.test".into(),
                roles: Vec::new(),
            },
        )
        .await;
    if !matches!(set, Err(AccountError::Impersonating))
        || !matches!(renamed, Err(AccountError::Impersonating))
        || !matches!(deleted, Err(AccountError::Impersonating))
        || !matches!(created, Err(AccountError::Impersonating))
        || !matches!(invited, Err(InvitationError::Access(Error::Refused(_))))
    {
        return Err(format!(
            "An impersonated session changed an account: password {set:?}, name {:?}, delete {deleted:?}, create {:?}, invite {:?}",
            renamed.map(|account| account.id),
            created.map(|account| account.id),
            invited.map(|invitation| invitation.user)
        )
        .into());
    }
    users
        .login("contractor-moved@example.test", "covered-new-password")
        .await
        .map_err(|error| format!("An impersonated password change took effect: {error}"))?;
    users
        .leave_impersonation(&mallory_login.token, &csrf, &RequestInfo::default())
        .await?;
    println!("PASS: impersonating a full-access account is refused; an impersonated session cannot set a password, edit, delete, create or invite an account");

    // Back as herself, Mallory may delete the covered account.
    let mallory = app.security.guard(&mallory_login.token).await?;
    if mallory.impersonator().is_some() || mallory.user() != mallory_id {
        return Err("Leaving impersonation did not restore the operator".into());
    }
    users
        .delete_account(&mallory, contractor_id)
        .await
        .map_err(|error| format!("A User Editor could not delete a covered account: {error:?}"))?;
    step_up.set_enforced(true);
    println!("PASS: a User Editor deletes an account holding nothing more");
    Ok(())
}
