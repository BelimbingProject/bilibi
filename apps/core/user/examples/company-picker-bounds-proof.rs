//! Proof for #697 against an explicitly disposable, migrated database: with
//! 3,000 more companies in the tenant, the company pickers a tenant-wide
//! administrator uses no longer draw every company. Each picker names at most
//! one page of companies, and typing finds the last company: role create
//! saves a role owned by it, account create takes it as the first desk, the
//! role grant and the privacy request offer it, and the settings scope finds
//! it. Exits non-zero when a check fails. Never point it at a shared database.
//!
//! ```sh
//! BILIBI_USER_DEMO=disposable cargo run -p bilibi-user --example company-picker-bounds-proof --locked
//! ```
use bilibi_tenancy::Scope;
use bilibi_ui::page::{LiveInput, LiveRequest, LiveState, PageRequest, PageService};
use serde_json::{json, Value};
use std::collections::HashMap;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Error = Box<dyn std::error::Error>;
type Pages = PageService<Scope, bilibi_database::Error>;
type State = Box<dyn LiveState<Scope, bilibi_database::Error>>;
const COMPANIES: i64 = 3_000;
/// A picker shows its chosen company and at most one page of hits.
const BOUND: usize = 25;

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
async fn mount(
    pages: &Pages,
    scope: &Scope,
    operation: &'static str,
    path: HashMap<String, String>,
    query: HashMap<String, String>,
) -> Result<State, bilibi_database::Error> {
    pages
        .0
        .mount(PageRequest {
            operation,
            scope: Some(scope),
            path: &path,
            query: &query,
            body: &[],
            framed: false,
        })
        .await?
        .ok_or(bilibi_database::Error::Internal)
}
async fn send(
    state: &mut State,
    scope: &Scope,
    credential: &str,
    event: Option<Value>,
) -> Result<String, bilibi_database::Error> {
    let input = match &event {
        Some(value) => LiveInput::Event(value),
        None => LiveInput::Refresh,
    };
    Ok(state
        .update(LiveRequest {
            scope,
            credential,
            input,
        })
        .await?
        .html)
}
/// How many of the seeded companies a page names.
fn listed(html: &str) -> usize {
    html.matches("Bound Pick ").count()
}
fn named(html: &str, name: &str) -> bool {
    html.contains(name)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    if std::env::var("BILIBI_USER_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_USER_DEMO=disposable for this walkthrough".into());
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35575")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let login = app
        .user
        .login("ai@agent.my", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let token = login.token.as_str();
    let admin = app.security.guard(token).await?;
    let tenant = admin.tenant();
    let client = db.get().await?;
    let client: &tokio_postgres::Client = &client;
    client
        .execute(
            "INSERT INTO companies(tenant_id,name,status,code) SELECT $1,'Bound Pick '||lpad(n::text,4,'0'),'active','bound_pick_'||n FROM generate_series(1,$2::bigint) n",
            &[&tenant, &COMPANIES],
        )
        .await?;
    let last = "Bound Pick 3000";
    let last_id: i64 = client
        .query_one(
            "SELECT id FROM companies WHERE tenant_id=$1 AND code='bound_pick_3000'",
            &[&tenant],
        )
        .await?
        .get(0);
    let record = format!("company:{last_id}");
    let mut checks = Checks::default();
    println!("== a tenant with {COMPANIES} more companies; the operator's administrator picks");

    println!("-- role create");
    let authz = app.services.get::<Pages>("base/authz.pages")?;
    let mut state = mount(
        &authz,
        &admin,
        "role-create",
        HashMap::new(),
        HashMap::new(),
    )
    .await?;
    let html = send(&mut state, &admin, token, None).await?;
    checks.check(
        "role create names at most one page of companies",
        listed(&html) <= BOUND,
        listed(&html),
    );
    let _ = send(
        &mut state,
        &admin,
        token,
        Some(json!({"type": "change-company"})),
    )
    .await;
    let html = send(&mut state, &admin, token, Some(json!({"type": "role-change", "role[company_id_search]": last, "role[name]": "Bound owner", "role[code]": "bound_owner", "role[description]": ""}))).await;
    checks.check(
        "typing finds the last company",
        html.as_deref().is_ok_and(|html| named(html, last)),
        html.as_ref().map(|html| listed(html)),
    );
    let _ = send(
        &mut state,
        &admin,
        token,
        Some(json!({"type": "pick-company", "record": record})),
    )
    .await;
    let _ = send(&mut state, &admin, token, Some(json!({"type": "create-role", "role[company_id]": last_id.to_string(), "role[name]": "Bound owner", "role[code]": "bound_owner", "role[description]": ""}))).await;
    let owner: Option<i64> = client
        .query_opt(
            "SELECT company_id FROM roles WHERE tenant_id=$1 AND code='bound_owner'",
            &[&tenant],
        )
        .await?
        .and_then(|row| row.get(0));
    checks.check(
        "a role owned by the last company is created",
        owner == Some(last_id),
        owner,
    );

    println!("-- account create");
    let users = app.services.get::<Pages>("core/user.pages")?;
    let mut state = mount(&users, &admin, "create", HashMap::new(), HashMap::new()).await?;
    let html = send(&mut state, &admin, token, None).await?;
    checks.check(
        "account create names at most one page of companies",
        listed(&html) <= BOUND,
        listed(&html),
    );
    let _ = send(
        &mut state,
        &admin,
        token,
        Some(json!({"type": "change_desk"})),
    )
    .await;
    let html = send(
        &mut state,
        &admin,
        token,
        Some(json!({"type": "validate", "user[company_id_search]": last, "user[name]": "Bound Person"})),
    )
    .await?;
    checks.check(
        "typing finds the last company as a first desk",
        named(&html, last),
        listed(&html),
    );
    let html = send(
        &mut state,
        &admin,
        token,
        Some(json!({"type": "pick_desk", "record": record})),
    )
    .await;
    checks.check(
        "choosing it makes it the first desk",
        html.as_deref()
            .is_ok_and(|html| named(html, last) && html.contains(&format!("value=\"{last_id}\""))),
        html.as_ref().map(|html| listed(html)),
    );

    println!("-- role grant on an account");
    let mut path = HashMap::new();
    path.insert("id".to_owned(), "3".to_owned());
    let mut state = mount(&users, &admin, "detail", path, HashMap::new()).await?;
    let _ = send(&mut state, &admin, token, None).await?;
    let html = send(
        &mut state,
        &admin,
        token,
        Some(json!({"type": "toggle_assign_roles"})),
    )
    .await?;
    checks.check(
        "the role grant names at most one page of companies",
        listed(&html) <= BOUND,
        listed(&html),
    );
    let html = send(
        &mut state,
        &admin,
        token,
        Some(json!({"type": "select_roles", "assignment[company_search]": last})),
    )
    .await?;
    checks.check(
        "typing finds the last company for the grant",
        named(&html, last),
        listed(&html),
    );

    println!("-- privacy request");
    let privacy = app.services.get::<Pages>("base/privacy.pages")?;
    let mut state = mount(&privacy, &admin, "requests", HashMap::new(), HashMap::new()).await?;
    let _ = send(&mut state, &admin, token, None).await?;
    let html = send(
        &mut state,
        &admin,
        token,
        Some(json!({"type": "new_request"})),
    )
    .await?;
    checks.check(
        "the New request dialog names at most one page of companies",
        listed(&html) <= BOUND,
        listed(&html),
    );
    let html = send(
        &mut state,
        &admin,
        token,
        Some(json!({"type": "request_change", "request[company_id_search]": last})),
    )
    .await;
    checks.check(
        "typing finds the last company for the request",
        html.as_deref().is_ok_and(|html| named(html, last)),
        html.as_ref().map(|html| listed(html)),
    );

    println!("-- settings scope");
    let settings = app.services.get::<Pages>("base/settings_editor.pages")?;
    let mut state = mount(&settings, &admin, "global", HashMap::new(), HashMap::new()).await?;
    let html = send(&mut state, &admin, token, None).await?;
    checks.check(
        "the settings scope names at most one page of companies",
        listed(&html) <= BOUND,
        listed(&html),
    );
    let mut query = HashMap::new();
    query.insert("company_search".to_owned(), last.to_owned());
    let mut state = mount(&settings, &admin, "global", HashMap::new(), query).await?;
    let html = send(&mut state, &admin, token, None).await?;
    checks.check(
        "its search finds the last company",
        named(&html, last) && listed(&html) <= BOUND,
        listed(&html),
    );

    if checks.failed > 0 {
        return Err(format!("{} check(s) failed", checks.failed).into());
    }
    Ok(())
}
