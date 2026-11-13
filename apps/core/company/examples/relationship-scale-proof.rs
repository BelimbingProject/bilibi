//! Proof for #592 against an explicitly disposable, migrated database: a
//! company with more linked people than the Principal Directory names in one
//! call (2,001) still shows its relationships a page at a time. It checks the
//! relationships page, the record page's panel, a person's panel, paging, an
//! edit of a link past the first page, and the agent list operation, and
//! exits non-zero when a check fails. Never point it at a shared database.
//!
//! ```sh
//! BILIBI_COMPANY_DEMO=disposable cargo run -p bilibi-company --example relationship-scale-proof --locked
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
const PEOPLE: i64 = 2_001;

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
async fn mount(
    pages: &Pages,
    scope: &Scope,
    operation: &'static str,
    path: &HashMap<String, String>,
) -> Result<Box<dyn LiveState<Scope, bilibi_database::Error>>, bilibi_database::Error> {
    pages
        .0
        .mount(PageRequest {
            operation,
            scope: Some(scope),
            path,
            query: &HashMap::new(),
            body: &[],
            framed: false,
        })
        .await?
        .ok_or(bilibi_database::Error::Internal)
}
async fn send(
    state: &mut Box<dyn LiveState<Scope, bilibi_database::Error>>,
    scope: &Scope,
    input: LiveInput<'_>,
) -> Result<(String, HashMap<String, String>), bilibi_database::Error> {
    let body = state
        .update(LiveRequest {
            scope,
            credential: "",
            input,
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
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35575")?;
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
    client.batch_execute("DELETE FROM company_relationships WHERE source_organisation_id IN (SELECT id FROM companies WHERE code LIKE 'scale\\_%') OR target_organisation_id IN (SELECT id FROM companies WHERE code LIKE 'scale\\_%'); DELETE FROM companies WHERE code LIKE 'scale\\_%'; DELETE FROM persons WHERE code LIKE 'scale-person-%'").await?;
    bilibi_company::ensure_default_relationship_roles(client, tenant).await?;
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

    println!("== a company with {PEOPLE} linked people and three linked companies");
    let source = company(&companies, &manager, "Scale Source", "scale_source").await?;
    let zulu = company(&companies, &manager, "Zulu Partner", "scale_zulu").await?;
    let alpha = company(&companies, &manager, "Alpha Supplier", "scale_alpha").await?;
    let beta = company(&companies, &manager, "Beta Buyer", "scale_beta").await?;
    let link = |from: OrganisationId, to: OrganisationId, role: i64| async move {
        client
            .query_one(
                "INSERT INTO company_relationships(tenant_id,source_organisation_id,target_organisation_id,role_id,effective_from) VALUES($1,$2,$3,$4,'2026-01-01') RETURNING id",
                &[&tenant, &from.get(), &to.get(), &role],
            )
            .await
            .map(|row| row.get::<_, i64>(0))
    };
    let zulu_link = link(source, zulu, vendor).await?;
    let alpha_link = link(source, alpha, vendor).await?;
    // Beta buys from the source: on the source's page it is incoming.
    let beta_link = link(beta, source, vendor).await?;
    client
        .execute(
            "INSERT INTO persons(tenant_id,name,kind,code) SELECT $1,'Scale Person '||lpad(n::text,4,'0'),'human','scale-person-'||n FROM generate_series(1,$2::bigint) n",
            &[&tenant, &PEOPLE],
        )
        .await?;
    client
        .execute(
            "INSERT INTO company_relationships(tenant_id,source_organisation_id,target_person_id,role_id,effective_from) SELECT $1,$2,p.id,$3,'2026-01-01' FROM persons p WHERE p.tenant_id=$1 AND p.code LIKE 'scale-person-%' ORDER BY p.id",
            &[&tenant, &source.get(), &customer],
        )
        .await?;
    let person_links: Vec<(i64, i64)> = client
        .query(
            "SELECT r.id,r.target_person_id FROM company_relationships r WHERE r.tenant_id=$1 AND r.source_organisation_id=$2 AND r.target_person_id IS NOT NULL ORDER BY r.id",
            &[&tenant, &source.get()],
        )
        .await?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let total = person_links.len() as i64 + 3;
    let mut checks = Checks::default();

    println!("== the relationships page");
    let page_path = HashMap::from([("id".to_owned(), source.get().to_string())]);
    match mount(&pages, &manager, "company", &page_path).await {
        Err(error) => checks.check("the relationships page opens", false, error),
        Ok(mut state) => match send(&mut state, &manager, LiveInput::Refresh).await {
            Err(error) => checks.check("the relationships page opens", false, error),
            Ok((html, _)) => {
                checks.check("the relationships page opens", true, ());
                let first = rows(&html);
                checks.check(
                    "it draws one page of 25 links",
                    first.len() == 25,
                    first.len(),
                );
                checks.check(
                    "companies come first, by name: Alpha, Beta (incoming), Zulu",
                    first.get(..3) == Some(&[alpha_link, beta_link, zulu_link][..]),
                    first.get(..3),
                );
                let summary = format!("Showing 1 to 25 of {total} results");
                checks.check(
                    "the pager counts every link",
                    html.contains(&summary),
                    summary,
                );
                checks.check(
                    "people are named by Person, not by number",
                    html.contains("Scale Person 0001") && !html.contains("Person 1</"),
                    (),
                );
                match send(
                    &mut state,
                    &manager,
                    LiveInput::Event(&json!({"type":"page","page":"2"})),
                )
                .await
                {
                    Err(error) => checks.check("page 2 opens", false, error),
                    Ok((html, query)) => {
                        let second = rows(&html);
                        checks.check(
                            "page 2 holds the next 25 links",
                            second.len() == 25 && second.iter().all(|id| !first.contains(id)),
                            &second,
                        );
                        checks.check(
                            "the page number is in the address",
                            query.get("page").map(String::as_str) == Some("2"),
                            &query,
                        );
                    }
                }
                // The last link, far past the page shown, opens for editing.
                let last = person_links.last().map_or(0, |link| link.0);
                match send(
                    &mut state,
                    &manager,
                    LiveInput::Event(&json!({"type":"edit","id":last.to_string()})),
                )
                .await
                {
                    Err(error) => {
                        checks.check("a link past the page opens for editing", false, error)
                    }
                    Ok((html, _)) => checks.check(
                        "a link past the page opens for editing",
                        html.contains("id=\"relationship-modal\"")
                            && html.contains("Edit Relationship Dates")
                            && html.contains(&format!("Scale Person {PEOPLE:04}")),
                        (),
                    ),
                }
            }
        },
    }

    println!("== the panel on the company's record page");
    let panel_path = HashMap::from([
        ("panel".to_owned(), "company-relationships-panel".to_owned()),
        ("kind".to_owned(), "organisation".to_owned()),
        ("id".to_owned(), source.get().to_string()),
    ]);
    match mount(&pages, &manager, "panel", &panel_path).await {
        Err(error) => checks.check("the company panel opens", false, error),
        Ok(mut state) => match send(&mut state, &manager, LiveInput::Refresh).await {
            Err(error) => checks.check("the company panel opens", false, error),
            Ok((html, _)) => {
                checks.check("the company panel opens", true, ());
                let shown = rows(&html);
                checks.check("it draws 25 links", shown.len() == 25, shown.len());
                checks.check(
                    "its heading and pager count every link",
                    html.contains(&format!("Showing 1 to 25 of {total} results")),
                    (),
                );
                let last_page = (total + 24) / 25;
                match send(
                    &mut state,
                    &manager,
                    LiveInput::Event(&json!({"type":"page","page":last_page.to_string()})),
                )
                .await
                {
                    Err(error) => checks.check("the panel's last page opens", false, error),
                    Ok((html, _)) => {
                        let shown = rows(&html);
                        checks.check(
                            "the panel's last page holds the remainder",
                            shown.len() as i64 == total - (last_page - 1) * 25,
                            shown.len(),
                        );
                    }
                }
            }
        },
    }

    println!("== the panel on a person's record page");
    let (person_link, person) = person_links[0];
    let person_path = HashMap::from([
        ("panel".to_owned(), "person-relationships-panel".to_owned()),
        ("kinds".to_owned(), "person".to_owned()),
        ("person_id".to_owned(), person.to_string()),
    ]);
    match mount(&pages, &manager, "panel", &person_path).await {
        Err(error) => checks.check("the person panel opens", false, error),
        Ok(mut state) => match send(&mut state, &manager, LiveInput::Refresh).await {
            Err(error) => checks.check("the person panel opens", false, error),
            Ok((html, _)) => checks.check(
                "the person panel shows its one link to Scale Source",
                rows(&html) == [person_link] && html.contains("Scale Source"),
                rows(&html),
            ),
        },
    }

    println!("== the agent list operation");
    let listed = registry
        .call(
            client,
            &manager,
            "core.company.relationship.list",
            &json!({"company_id":source.get()}),
        )
        .await;
    match listed {
        Err(error) => checks.check("core.company.relationship.list answers", false, error),
        Ok(value) => {
            let entries = value["result"].as_array().map_or(0, Vec::len);
            checks.check(
                "core.company.relationship.list answers one page with the total",
                entries == 25 && value["meta"]["page"]["total"] == json!(total),
                (entries, &value["meta"]),
            );
        }
    }
    let last_page = (total + 99) / 100;
    let listed = registry
        .call(
            client,
            &manager,
            "core.company.relationship.list",
            &json!({"company_id":source.get(),"page":last_page,"page_size":100}),
        )
        .await;
    checks.check(
        "its last page of 100 holds the remainder",
        listed.as_ref().is_ok_and(|value: &Value| {
            value["result"].as_array().map_or(0, Vec::len) as i64 == total - (last_page - 1) * 100
        }),
        listed.as_ref().map(|value| value["meta"].clone()),
    );

    if checks.failed > 0 {
        return Err(format!("{} checks failed", checks.failed).into());
    }
    println!("All checks passed.");
    Ok(())
}
