//! Proof for #654: names and codes that PostgreSQL's text-search parser keeps
//! as one token with their dots, dashes or slashes are found by their own
//! text, in Base Search and in `core.company.list`, in a disposable database.
//! Each check prints PASS or stops the walkthrough with FAIL.
//!
//! ```sh
//! BILIBI_SEARCH_PUNCTUATION=disposable cargo run -p bilibi-company --example search-punctuation --locked
//! ```
use bilibi_agent_api::Registry;
use bilibi_database::pool;
use bilibi_search::Search;
use bilibi_tenancy::Scope;
use serde_json::json;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Failure = Box<dyn std::error::Error>;

fn prove(label: &str, passed: bool) -> Result<(), Failure> {
    if !passed {
        return Err(format!("FAIL: {label}").into());
    }
    println!("PASS: {label}");
    Ok(())
}

async fn login(app: &bilibi_web::App, email: &str) -> Result<Scope, Failure> {
    let login = app
        .user
        .login(email, bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    Ok(app.security.guard(&login.token).await?)
}

/// (name, code) of each company the walkthrough adds.
const COMPANIES: [(&str, &str); 5] = [
    ("Amazon.com Sdn Bhd", "pw_one"),
    ("Purchase Desk", "PO-2024-001"),
    ("abc-123", "pw_three"),
    ("acme/west", "pw_four"),
    ("Plainword Holdings", "pw_five"),
];

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_SEARCH_PUNCTUATION").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_SEARCH_PUNCTUATION=disposable on a freshly migrated disposable DB".into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:43231")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let manager = login(&app, "admin@example.test").await?;
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let search = app.services.get::<Search>(bilibi_search::SERVICE)?;
    let client = db.get().await?;
    for (id, (name, code)) in (3_000_001i64..).zip(COMPANIES) {
        client
            .execute(
                "INSERT INTO companies(id,tenant_id,name,code,status) VALUES ($1,$2,$3,$4,'active')",
                &[&id, &manager.tenant(), &name, &code],
            )
            .await?;
    }

    // The names core.company.list returns for `text`.
    let list = |text: &'static str| {
        let (registry, manager, db) = (registry.clone(), manager.clone(), db.clone());
        async move {
            let client = db.get().await?;
            let value = registry
                .call(
                    &**client,
                    &manager,
                    "core.company.list",
                    &json!({"search": text, "page_size": 50}),
                )
                .await?;
            let names: Vec<String> = value["result"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
                .collect();
            Ok::<_, Failure>(names)
        }
    };
    // The company titles Base Search's results page and search box return.
    let found = |text: &'static str| {
        let (search, manager) = (search.clone(), manager.clone());
        async move {
            let page = search.find(&manager, text, None, 1, 50).await?;
            let suggested = search.suggest(&manager, text).await?;
            let titles = |hits: &[bilibi_search::Hit]| -> Vec<String> {
                hits.iter()
                    .filter(|hit| hit.kind.id == "companies")
                    .map(|hit| hit.title.clone())
                    .collect()
            };
            Ok::<_, Failure>((titles(&page.hits), titles(&suggested.hits)))
        }
    };

    for (text, expected) in [
        ("Amazon.com", "Amazon.com Sdn Bhd"),
        ("amazon.com sdn bhd", "Amazon.com Sdn Bhd"),
        ("PO-2024-001", "Purchase Desk"),
        ("abc-123", "abc-123"),
        ("ACME/WEST", "acme/west"),
    ] {
        let listed = list(text).await?;
        println!("  core.company.list {text:?}: {listed:?}");
        prove(
            &format!("core.company.list finds {expected:?} by {text:?}"),
            listed.iter().any(|name| name == expected),
        )?;
        let (page, box_) = found(text).await?;
        println!("  Base Search {text:?}: page {page:?}, box {box_:?}");
        prove(
            &format!("Base Search's page and box find {expected:?} by {text:?}"),
            page.iter().any(|name| name == expected) && box_.iter().any(|name| name == expected),
        )?;
    }

    // Controls: word prefixes still match, every word is still required, and
    // a query with an exclusion keeps its meaning.
    prove(
        "a word prefix still finds a name, in both",
        list("plainw").await? == ["Plainword Holdings"]
            && found("plainw").await?.0 == ["Plainword Holdings"],
    )?;
    prove(
        "the first part of a dotted name still finds it by prefix",
        list("amazon")
            .await?
            .contains(&"Amazon.com Sdn Bhd".to_owned()),
    )?;
    prove(
        "every word must still match",
        list("plainword nomatch").await?.is_empty()
            && found("plainword nomatch").await?.0.is_empty(),
    )?;
    let excluded = found("desk -purchase").await?.0;
    let listed = list("desk -purchase").await?;
    println!("  \"desk -purchase\": Base Search {excluded:?}, list {listed:?}");
    prove(
        "an excluded word still leaves its company out, in both",
        !excluded.contains(&"Purchase Desk".to_owned())
            && !listed.contains(&"Purchase Desk".to_owned())
            && found("desk").await?.0.contains(&"Purchase Desk".to_owned()),
    )?;
    println!("search-punctuation: all checks passed");
    Ok(())
}
