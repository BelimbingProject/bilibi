//! Proof for #594 against an explicitly disposable, migrated database: with
//! 300 more companies in the tenant, Authz's `visible_companies` (behind
//! every roles and users page) names only the live companies that own a
//! role instead of every company, and role visibility is unchanged. The
//! organisation directory's `selectable` still offers what it offered: every
//! unarchived company with the tenant-wide reach, sorted by name, and at most
//! the current company without it. Exits non-zero when a check fails. Never
//! point it at a shared database.
//!
//! ```sh
//! BILIBI_COMPANY_DEMO=disposable cargo run -p bilibi-company --example directory-bounds-proof --locked
//! ```
use bilibi_authz::{
    grants::{CreatedRole, Grants},
    roles::{self, NewRole, Query, Sort},
};
use bilibi_tenancy::{PartyProofs, Scope};
use std::collections::BTreeSet;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Error = Box<dyn std::error::Error>;
const COMPANIES: i64 = 300;
const SETTINGS_MANAGE: &str = "base.settings.company.manage";

#[derive(Default)]
struct Checks {
    failed: usize,
}
impl Checks {
    fn check(&mut self, label: &str, passed: bool, detail: impl std::fmt::Debug) {
        if passed {
            println!("PASS {label}");
        } else {
            self.failed += 1;
            println!("FAIL {label}: {detail:?}");
        }
    }
}
fn bound(companies: &[bilibi_tenancy::NamedOrganisation]) -> Vec<&str> {
    companies
        .iter()
        .map(|c| c.name.as_str())
        .filter(|name| name.starts_with("Bound Co "))
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    if std::env::var("BILIBI_COMPANY_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_COMPANY_DEMO=disposable for this walkthrough".into());
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35576")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let parties = app.services.get::<PartyProofs>("base/tenancy.parties")?;
    async fn login(app: &bilibi_web::App, email: &str) -> Result<Scope, Error> {
        let login = app
            .user
            .login(email, bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        Ok(app.security.guard(&login.token).await?)
    }
    let admin = login(&app, "admin@example.test").await?;
    let manager = login(&app, "manager@example.test").await?;
    let tenant = admin.tenant();
    let client = db.get().await?;
    let client: &tokio_postgres::Client = &client;
    // Re-runs start from the seeded state.
    client.batch_execute("DELETE FROM roles WHERE code LIKE 'bound\\_%'; DELETE FROM companies WHERE code LIKE 'bound\\_%'").await?;
    let mut checks = Checks::default();

    println!("== a tenant with {COMPANIES} more companies, two of them owning a role");
    client
        .execute(
            "INSERT INTO companies(tenant_id,name,status,code) SELECT $1,'Bound Co '||lpad(n::text,4,'0'),'active','bound_'||n FROM generate_series(1,$2::bigint) n",
            &[&tenant, &COMPANIES],
        )
        .await?;
    let company = |n: i64| async move {
        Ok::<i64, Error>(
            client
                .query_one(
                    "SELECT id FROM companies WHERE tenant_id=$1 AND code=$2",
                    &[&tenant, &format!("bound_{n}")],
                )
                .await?
                .get(0),
        )
    };
    let (owner, gone, archived) = (
        company(150).await?,
        company(151).await?,
        company(299).await?,
    );
    for (company_id, code) in [(owner, "bound_owned"), (gone, "bound_gone")] {
        let created = grants
            .create_role(
                &admin,
                &NewRole {
                    company_id: Some(company_id),
                    name: format!("Bound {code}"),
                    code: code.into(),
                    description: String::new(),
                },
            )
            .await?;
        if !matches!(created, CreatedRole::Created(_)) {
            return Err(format!("{code} was not created").into());
        }
    }
    client
        .execute(
            "UPDATE companies SET deleted_at=now() WHERE tenant_id=$1 AND id=$2",
            &[&tenant, &gone],
        )
        .await?;
    client
        .execute(
            "UPDATE companies SET status='archived' WHERE tenant_id=$1 AND id=$2",
            &[&tenant, &archived],
        )
        .await?;

    println!("== the roles and users pages' company filter");
    let visible = grants.visible_companies(client, &admin).await?;
    // The expectation is read straight from the tables, not through the code under test.
    let owners: BTreeSet<i64> = client
        .query(
            "SELECT DISTINCT r.company_id FROM roles r JOIN companies c ON c.tenant_id=r.tenant_id AND c.id=r.company_id WHERE r.tenant_id=$1 AND c.deleted_at IS NULL",
            &[&tenant],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let named: BTreeSet<i64> = visible.iter().copied().collect();
    checks.check(
        "it names only the live companies that own a role",
        named == owners,
        (visible.len(), owners.len()),
    );
    checks.check("a deleted owner is not named", !named.contains(&gone), gone);
    let page = roles::list(
        client,
        &admin,
        &visible,
        &Query {
            search: "bound".into(),
            sort: Sort::Name,
            descending: false,
            page: 1,
            page_size: 25,
        },
    )
    .await?;
    let codes: Vec<&str> = page
        .entries
        .iter()
        .filter_map(|r| r.code.as_deref())
        .collect();
    checks.check(
        "the role of a live company is listed",
        codes.contains(&"bound_owned"),
        &codes,
    );
    checks.check(
        "the role of a deleted company is not",
        !codes.contains(&"bound_gone"),
        &codes,
    );

    println!("== the directory's selectable companies");
    let offered = parties
        .selectable_organisations(client, &admin, SETTINGS_MANAGE)
        .await?;
    let names = bound(&offered);
    checks.check(
        "with the tenant-wide reach it offers every unarchived company",
        names.len() == (COMPANIES - 2) as usize && !names.contains(&"Bound Co 0299"),
        names.len(),
    );
    checks.check(
        "it never offers an archived or deleted company",
        !offered.iter().any(|c| c.archived || c.id.get() == gone),
        offered.iter().filter(|c| c.archived).count(),
    );
    let sorted = offered.windows(2).all(|w| {
        (w[0].name.to_lowercase(), w[0].id.get()) <= (w[1].name.to_lowercase(), w[1].id.get())
    });
    checks.check("it is sorted by name", sorted, names.first());
    let current = manager.current_company().map(|c| c.get());
    // The manager may update companies; a direct deny takes away its
    // tenant-wide reach for this check.
    let deny = "DELETE FROM principal_capabilities WHERE tenant_id=$1 AND user_id=$2 AND capability='admin.company.tenant-wide.manage'";
    client.execute(deny, &[&tenant, &manager.user()]).await?;
    client.execute("INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES($1,$2,'admin.company.tenant-wide.manage',false)", &[&tenant, &manager.user()]).await?;
    let managed: Vec<i64> = parties
        .selectable_organisations(client, &manager, bilibi_company::UPDATE)
        .await?
        .iter()
        .map(|c| c.id.get())
        .collect();
    checks.check(
        "without it, only the current company is offered",
        current.is_some() && managed == current.into_iter().collect::<Vec<_>>(),
        (current, &managed),
    );
    client.execute(deny, &[&tenant, &manager.user()]).await?;

    if checks.failed > 0 {
        return Err(format!("{} checks failed", checks.failed).into());
    }
    Ok(())
}
