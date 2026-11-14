//! Proof for #655 against an explicitly disposable, migrated database: a
//! company's relationships page and the agent list operation find a link by
//! the other party's name. People are matched by the name the Principal
//! Directory gives (a person linked last, past 2,000 others, is found at
//! once); companies by name or code, incoming links included; LIKE
//! metacharacters are literal. A company linked to more people than the
//! directory can search is refused visibly. Exits non-zero when a check
//! fails. Never point it at a shared database.
//!
//! ```sh
//! BILIBI_COMPANY_DEMO=disposable cargo run -p bilibi-company --example relationship-search-proof --locked
//! ```
use bilibi_agent_api::Registry;
use bilibi_company::{CompanyService, CreateOutcome, NewCompany, SERVICE};
use bilibi_party::OrganisationId;
use bilibi_tenancy::Scope;
use bilibi_ui::page::{LiveInput, LiveRequest, LiveState, PageRequest, PageService};
use serde_json::{json, Value};
use std::collections::HashMap;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Error = Box<dyn std::error::Error>;
type Pages = PageService<Scope, bilibi_database::Error>;
type State = Box<dyn LiveState<Scope, bilibi_database::Error>>;
const PEOPLE: i64 = 2_001;
/// One more than the Principal Directory searches at once.
const CROWD: i64 = 10_001;

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
/// The link ids of the rows a relationships table drew, in order.
fn rows(html: &str) -> Vec<i64> {
    let mut ids = Vec::new();
    for part in html.split("id=\"company-relationship-").skip(1) {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(id) = digits.parse() {
            if part[digits.len()..].starts_with('"') && !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}
async fn company(
    companies: &CompanyService,
    scope: &Scope,
    name: &str,
    code: &str,
) -> Result<OrganisationId, Error> {
    match companies
        .create_company(
            scope,
            &NewCompany {
                name: name.into(),
                code: Some(code.into()),
                ..NewCompany::default()
            },
        )
        .await?
    {
        CreateOutcome::Created(company) => Ok(company.organisation_id()?),
        CreateOutcome::Refused(errors) => Err(format!("create refused: {errors:?}").into()),
    }
}
async fn open(pages: &Pages, scope: &Scope, company: OrganisationId) -> Result<State, Error> {
    let path = HashMap::from([("id".to_owned(), company.get().to_string())]);
    pages
        .0
        .mount(PageRequest {
            operation: "company",
            scope: Some(scope),
            path: &path,
            query: &HashMap::new(),
            body: &[],
            framed: false,
        })
        .await?
        .ok_or_else(|| "the relationships page did not mount".into())
}
/// Types `text` in the page's search box, as the toolbar posts it.
async fn search(
    state: &mut State,
    scope: &Scope,
    text: &str,
) -> Result<(String, HashMap<String, String>), bilibi_database::Error> {
    let body = state
        .update(LiveRequest {
            scope,
            credential: "",
            input: LiveInput::Event(&json!({"type":"filters","filters[search]":text})),
        })
        .await?;
    Ok((body.html, body.query))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    if std::env::var("BILIBI_COMPANY_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_COMPANY_DEMO=disposable for this walkthrough".into());
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35577")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let companies = app.services.get::<CompanyService>(SERVICE)?;
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let pages = app
        .services
        .get::<Pages>("core/company.relationship_pages")?;
    let login = app
        .user
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let manager = app.security.guard(&login.token).await?;
    let tenant = manager.tenant();
    let client = db.get().await?;
    let client: &tokio_postgres::Client = &client;
    // Re-runs start from the seeded state.
    client.batch_execute("DELETE FROM company_relationships WHERE source_organisation_id IN (SELECT id FROM companies WHERE code LIKE 'search\\_%') OR target_organisation_id IN (SELECT id FROM companies WHERE code LIKE 'search\\_%'); DELETE FROM companies WHERE code LIKE 'search\\_%'; DELETE FROM persons WHERE code LIKE 'search-person-%' OR code LIKE 'crowd-person-%' OR code='search-rare'").await?;
    let role = |code: &'static str| async move {
        client
            .query_one(
                "SELECT id FROM company_relationship_roles WHERE tenant_id=$1 AND code=$2",
                &[&tenant, &code],
            )
            .await
            .map(|row| row.get::<_, i64>(0))
    };
    let (vendor, customer) = (role("vendor").await?, role("customer").await?);

    println!("== a company with {PEOPLE} linked people, one more linked last, and three companies");
    let source = company(&companies, &manager, "Search Source", "search_source").await?;
    let zulu = company(&companies, &manager, "Zulu Partner", "search_zulu").await?;
    let alpha = company(&companies, &manager, "Alpha Supplier", "search_alpha").await?;
    let beta = company(&companies, &manager, "Beta Buyer", "search_beta").await?;
    let link = |from: OrganisationId, to: OrganisationId| async move {
        client
            .query_one(
                "INSERT INTO company_relationships(tenant_id,source_organisation_id,target_organisation_id,role_id,effective_from) VALUES($1,$2,$3,$4,'2026-01-01') RETURNING id",
                &[&tenant, &from.get(), &to.get(), &vendor],
            )
            .await
            .map(|row| row.get::<_, i64>(0))
    };
    let zulu_link = link(source, zulu).await?;
    let alpha_link = link(source, alpha).await?;
    // Beta buys from the source: on the source's page it is incoming.
    let beta_link = link(beta, source).await?;
    let people = |source: OrganisationId, prefix: &'static str, label: &'static str, count: i64| async move {
        client
            .execute(
                &format!("INSERT INTO persons(tenant_id,name,kind,code) SELECT $1,'{label} '||lpad(n::text,5,'0'),'human','{prefix}-'||n FROM generate_series(1,$2::bigint) n"),
                &[&tenant, &count],
            )
            .await?;
        client
            .execute(
                &format!("INSERT INTO company_relationships(tenant_id,source_organisation_id,target_person_id,role_id,effective_from) SELECT $1,$2,p.id,$3,'2026-01-01' FROM persons p WHERE p.tenant_id=$1 AND p.code LIKE '{prefix}-%' ORDER BY p.id DESC"),
                &[&tenant, &source.get(), &customer],
            )
            .await
    };
    // Linked in reverse name order, so link order never passes for name order.
    people(source, "search-person", "Search Person", PEOPLE).await?;
    let rare_link: i64 = client
        .query_one(
            "WITH p AS (INSERT INTO persons(tenant_id,name,kind,code) VALUES($1,'Quentin Rarename','human','search-rare') RETURNING id) INSERT INTO company_relationships(tenant_id,source_organisation_id,target_person_id,role_id,effective_from) SELECT $1,$2,p.id,$3,'2026-01-01' FROM p RETURNING id",
            &[&tenant, &source.get(), &customer],
        )
        .await?
        .get(0);
    let total = PEOPLE + 1 + 3;
    let crowd = company(&companies, &manager, "Search Crowd", "search_crowd").await?;
    people(crowd, "crowd-person", "Crowd Person", CROWD).await?;
    let mut checks = Checks::default();

    println!("== the relationships page's search box");
    let mut state = open(&pages, &manager, source).await?;
    match search(&mut state, &manager, "rarename").await {
        Err(error) => checks.check("searching a person's name answers", false, error),
        Ok((html, query)) => {
            checks.check(
                "a person linked last is found by name, alone",
                rows(&html) == [rare_link] && html.contains("Quentin Rarename"),
                rows(&html),
            );
            checks.check(
                "the pager counts only the matches",
                html.contains("Showing 1 to 1 of 1 results"),
                (),
            );
            checks.check(
                "the search is in the address",
                query.get("search").map(String::as_str) == Some("rarename"),
                &query,
            );
        }
    }
    // The names holding "Person 002" (five-digit numbers) are 00200-00299: 100.
    match search(&mut state, &manager, "Person 002").await {
        Err(error) => checks.check("searching many people answers", false, error),
        Ok((html, _)) => {
            let first = html.find("Search Person 00200");
            let second = html.find("Search Person 00201");
            checks.check(
                "every person whose name matches is counted",
                html.contains("Showing 1 to 25 of 100 results"),
                (),
            );
            checks.check(
                "matching people come in name order",
                first.is_some() && second.is_some() && first < second && rows(&html).len() == 25,
                (first, second),
            );
        }
    }
    let alone = |text: &'static str, want: i64, label: &'static str| (text, want, label);
    for (text, want, label) in [
        alone("alpha sup", alpha_link, "a company is found by name"),
        alone(
            "BETA",
            beta_link,
            "an incoming link is found by its source's name",
        ),
        alone(
            "search_zulu",
            zulu_link,
            "a company is found by code, _ taken literally",
        ),
    ] {
        match search(&mut state, &manager, text).await {
            Err(error) => checks.check(label, false, error),
            Ok((html, _)) => checks.check(label, rows(&html) == [want], rows(&html)),
        }
    }
    match search(&mut state, &manager, "%").await {
        Err(error) => checks.check("% matches only a literal %", false, error),
        Ok((html, _)) => checks.check(
            "% matches only a literal %",
            rows(&html).is_empty() && html.contains("No relationships match this search."),
            rows(&html).len(),
        ),
    }
    match search(&mut state, &manager, "").await {
        Err(error) => checks.check("clearing the search answers", false, error),
        Ok((html, _)) => checks.check(
            "clearing the search shows every link again",
            html.contains(&format!("Showing 1 to 25 of {total} results")),
            (),
        ),
    }

    println!("== a company linked to {CROWD} people");
    let mut crowded = open(&pages, &manager, crowd).await?;
    match search(&mut crowded, &manager, "Crowd Person 00001").await {
        Err(error) => checks.check("the page says why it cannot search", false, error),
        Ok((html, _)) => checks.check(
            "the page says why it cannot search, and lists nothing",
            html.contains("company-relationships-search-refused")
                && html.contains("too many to search by name")
                && rows(&html).is_empty(),
            rows(&html).len(),
        ),
    }

    println!("== the agent list operation");
    let listed = registry
        .call(
            client,
            &manager,
            "core.company.relationship.list",
            &json!({"company_id":source.get(),"search":"Rarename"}),
        )
        .await;
    checks.check(
        "core.company.relationship.list finds the person by name",
        listed.as_ref().is_ok_and(|value: &Value| {
            value["result"].as_array().map_or(0, Vec::len) == 1
                && value["result"].to_string().contains("Quentin Rarename")
                && value["meta"]["page"]["total"] == json!(1)
        }),
        listed.as_ref().map(|value| value["meta"].clone()),
    );
    let refused = registry
        .call(
            client,
            &manager,
            "core.company.relationship.list",
            &json!({"company_id":crowd.get(),"search":"Crowd"}),
        )
        .await;
    checks.check(
        "it refuses the crowded company's search on the search field",
        matches!(&refused, Err(bilibi_agent_api::Error::InvalidInput(errors)) if errors.contains_key("search")),
        refused.as_ref().err().map(ToString::to_string),
    );

    if checks.failed > 0 {
        return Err(format!("{} checks failed", checks.failed).into());
    }
    Ok(())
}
