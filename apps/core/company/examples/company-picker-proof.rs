//! Proof for #593 against an explicitly disposable, migrated database: with
//! 300 companies and 150 people in the tenant, the company pages no longer
//! ship every company as a select, and the parent and related-party pickers
//! find any company or person by name: the 300th company becomes a parent
//! (on create and on the record page), and the 150th person, past the old
//! first page of 100, gets a relationship. Exits non-zero when a check
//! fails. Never point it at a shared database.
//!
//! ```sh
//! BILIBI_COMPANY_DEMO=disposable cargo run -p bilibi-company --example company-picker-proof --locked
//! ```
use bilibi_company::{CompanyService, CreateOutcome, NewCompany, SERVICE};
use bilibi_tenancy::Scope;
use bilibi_ui::page::{LiveInput, LiveRequest, LiveState, PageRequest, PageService};
use serde_json::{json, Value};
use std::collections::HashMap;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Error = Box<dyn std::error::Error>;
type Pages = PageService<Scope, bilibi_database::Error>;
type State = Box<dyn LiveState<Scope, bilibi_database::Error>>;
const COMPANIES: i64 = 300;
const PEOPLE: i64 = 150;

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
) -> Result<State, bilibi_database::Error> {
    pages
        .0
        .mount(PageRequest {
            operation,
            scope: Some(scope),
            path: &path,
            query: &HashMap::new(),
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
    html.matches(">Picker Co ").count()
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    if std::env::var("BILIBI_COMPANY_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_COMPANY_DEMO=disposable for this walkthrough".into());
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35575")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let companies = app.services.get::<CompanyService>(SERVICE)?;
    let company_pages = app.services.get::<Pages>("core/company.pages")?;
    let relationship_pages = app
        .services
        .get::<Pages>("core/company.relationship_pages")?;
    let login = app
        .user
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let token = login.token.as_str();
    let manager = app.security.guard(token).await?;
    let tenant = manager.tenant();
    let client = db.get().await?;
    let client: &tokio_postgres::Client = &client;
    client.batch_execute("DELETE FROM company_relationships WHERE source_organisation_id IN (SELECT id FROM companies WHERE code LIKE 'picker\\_%'); UPDATE companies SET parent_id=NULL WHERE code LIKE 'picker\\_%'; DELETE FROM companies WHERE code LIKE 'picker\\_%'; DELETE FROM persons WHERE code LIKE 'picker-person-%'").await?;
    bilibi_company::ensure_default_relationship_roles(client, tenant).await?;
    let customer: i64 = client
        .query_one(
            "SELECT id FROM company_relationship_roles WHERE tenant_id=$1 AND code='customer'",
            &[&tenant],
        )
        .await?
        .get(0);

    println!("== a tenant with {COMPANIES} more companies and {PEOPLE} people");
    let source = match companies
        .create_company(
            &manager,
            &NewCompany {
                name: "Picker Source".into(),
                code: Some("picker_source".into()),
                ..NewCompany::default()
            },
        )
        .await?
    {
        CreateOutcome::Created(company) => company.id,
        CreateOutcome::Refused(errors) => return Err(format!("{errors:?}").into()),
    };
    client
        .execute(
            "INSERT INTO companies(tenant_id,name,status,code) SELECT $1,'Picker Co '||lpad(n::text,4,'0'),'active','picker_'||n FROM generate_series(1,$2::bigint) n",
            &[&tenant, &COMPANIES],
        )
        .await?;
    client
        .execute(
            "INSERT INTO persons(tenant_id,name,kind,code) SELECT $1,'Picker Person '||lpad(n::text,4,'0'),'human','picker-person-'||n FROM generate_series(1,$2::bigint) n",
            &[&tenant, &PEOPLE],
        )
        .await?;
    let company_id = |n: i64| async move {
        client
            .query_one(
                "SELECT id FROM companies WHERE tenant_id=$1 AND code=$2",
                &[&tenant, &format!("picker_{n}")],
            )
            .await
            .map(|row| row.get::<_, i64>(0))
    };
    let mut checks = Checks::default();

    println!("== the company record page");
    let mut detail = mount(
        &company_pages,
        &manager,
        "detail",
        HashMap::from([("id".into(), source.to_string())]),
    )
    .await?;
    let html = send(&mut detail, &manager, token, None).await?;
    checks.check(
        "it names none of the other companies until asked",
        listed(&html) == 0,
        listed(&html),
    );
    let html = send(
        &mut detail,
        &manager,
        token,
        Some(json!({"type":"parent_search","parent[parent_search]":"Picker Co 0299"})),
    )
    .await;
    let parent = company_id(299).await?;
    checks.check(
        "searching the parent finds the 299th company",
        html.as_ref().is_ok_and(|html| {
            html.contains(&format!("data-value=\"company:{parent}\""))
                || html.contains(&format!("value=\"company:{parent}\""))
        }),
        html.as_ref().map(|html| listed(html)),
    );
    let picked = send(
        &mut detail,
        &manager,
        token,
        Some(json!({"type":"pick_parent","record":format!("company:{parent}")})),
    )
    .await;
    let saved: Option<i64> = client
        .query_one(
            "SELECT parent_id FROM companies WHERE tenant_id=$1 AND id=$2",
            &[&tenant, &source],
        )
        .await?
        .get(0);
    checks.check(
        "choosing it saves it as the parent",
        picked.is_ok() && saved == Some(parent),
        (picked.err(), saved),
    );

    println!("== the create page");
    match mount(&company_pages, &manager, "create", HashMap::new()).await {
        Err(error) => checks.check("the create page opens", false, error),
        Ok(mut create) => {
            let html = send(&mut create, &manager, token, None).await?;
            checks.check(
                "it names none of the companies until asked",
                listed(&html) == 0,
                listed(&html),
            );
            let last = company_id(COMPANIES).await?;
            let form = |extra: Value| {
                let mut form = json!({"name":"Picker Child","status":"active","code":"picker_child","code_edited":true});
                for (key, value) in extra.as_object().into_iter().flatten() {
                    form[key] = value.clone();
                }
                form
            };
            let html = send(
                &mut create,
                &manager,
                token,
                Some(form(
                    json!({"type":"validate_company","parent[parent_search]":"Picker Co 0300"}),
                )),
            )
            .await;
            checks.check(
                "searching the parent finds the last company",
                html.as_ref()
                    .is_ok_and(|html| html.contains(&format!("company:{last}"))),
                html.as_ref().map(|html| listed(html)),
            );
            let _ = send(
                &mut create,
                &manager,
                token,
                Some(json!({"type":"pick_parent","record":format!("company:{last}")})),
            )
            .await;
            let created = send(
                &mut create,
                &manager,
                token,
                Some(form(json!({"type":"create_company"}))),
            )
            .await;
            let parent: Option<Option<i64>> = client
                .query_opt(
                    "SELECT parent_id FROM companies WHERE tenant_id=$1 AND code='picker_child'",
                    &[&tenant],
                )
                .await?
                .map(|row| row.get(0));
            checks.check(
                "the new company is created under it",
                parent == Some(Some(last)),
                (created.err(), parent),
            );
        }
    }

    println!("== Add Relationship on the relationships page");
    let mut links = mount(
        &relationship_pages,
        &manager,
        "company",
        HashMap::from([("id".into(), source.to_string())]),
    )
    .await?;
    send(&mut links, &manager, token, None).await?;
    let html = send(&mut links, &manager, token, Some(json!({"type":"new"}))).await?;
    checks.check(
        "the dialog names none of the companies until asked",
        listed(&html) == 0,
        listed(&html),
    );
    for (kind, label, target) in [
        ("organisation", "Picker Co 0250", company_id(250).await?),
        (
            "person",
            "Picker Person 0150",
            client
                .query_one(
                    "SELECT id FROM persons WHERE tenant_id=$1 AND code=$2",
                    &[&tenant, &format!("picker-person-{PEOPLE}")],
                )
                .await?
                .get::<_, i64>(0),
        ),
    ] {
        let record = format!(
            "{}:{target}",
            if kind == "person" {
                "person"
            } else {
                "company"
            }
        );
        let values = |event: &str, search: &str| json!({"type":event,"target_kind":kind,"relationship[party_search]":search,"role_id":customer.to_string(),"effective_from":"2026-01-01","effective_to":""});
        if kind == "person" {
            send(&mut links, &manager, token, Some(json!({"type":"new"}))).await?;
        }
        // The radio first, then the search.
        send(&mut links, &manager, token, Some(values("validate", ""))).await?;
        let html = send(&mut links, &manager, token, Some(values("validate", label))).await;
        checks.check(
            &format!("searching finds {label}"),
            html.as_ref().is_ok_and(|html| html.contains(&record)),
            html.as_ref().err(),
        );
        let picked = send(
            &mut links,
            &manager,
            token,
            Some(json!({"type":"pick_party","record":record})),
        )
        .await;
        let saved = send(
            &mut links,
            &manager,
            token,
            Some(values("save_relationship", "")),
        )
        .await;
        let column = if kind == "person" {
            "target_person_id"
        } else {
            "target_organisation_id"
        };
        let exists: bool = client
            .query_one(
                &format!("SELECT EXISTS(SELECT 1 FROM company_relationships WHERE tenant_id=$1 AND source_organisation_id=$2 AND {column}=$3 AND role_id=$4)"),
                &[&tenant, &source, &target, &customer],
            )
            .await?
            .get(0);
        checks.check(
            &format!("choosing {label} and saving links it"),
            exists,
            (picked.err(), saved.err()),
        );
    }

    if checks.failed > 0 {
        return Err(format!("{} checks failed", checks.failed).into());
    }
    println!("All checks passed.");
    Ok(())
}
