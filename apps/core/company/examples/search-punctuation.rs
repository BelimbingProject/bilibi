//! Proof for #654: names and codes that PostgreSQL's text-search parser keeps
//! as one token with their dots, dashes or slashes are found by their own
//! text, in Base Search and in `core.company.list`, in a disposable database.
//! Also proves the title fallback ranks and trims hits in that same search.
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

async fn title_matching(
    search: &Search,
    manager: &Scope,
    viewer: &Scope,
    client: &tokio_postgres::Client,
) -> Result<(), Failure> {
    client.batch_execute(
        "INSERT INTO companies (id,tenant_id,name,code,status,deleted_at) VALUES
         (3000020,1,'Zephyr','typo_exact','active',NULL),
         (3000021,1,'Zephyr Dental','typo_prefix','active',NULL),
         (3000022,1,'Azephyron','typo_substring','active',NULL),
         (3000023,1,'Zephir','typo_near','active',NULL),
         (3000024,1,'Zephir Deleted','typo_deleted','active',now()),
         (3000025,2,'Zephir Other Tenant','typo_other','active',NULL);
         INSERT INTO persons (id,tenant_id,name,code,kind,email) VALUES
         (3000020,1,'River Allen','typo_email','human','searchsecret@example.test');
         INSERT INTO role_capabilities (tenant_id,role_id,capability)
         SELECT 1,2,key FROM unnest(ARRAY['base.search.records.list','admin.person.list','admin.person.view']) key
         ON CONFLICT DO NOTHING;
         INSERT INTO field_restrictions (tenant_id,role_id,table_id,field_id,mode)
         VALUES (1,2,'persons','email','mask') ON CONFLICT DO NOTHING;",
    ).await?;
    let page = search
        .find(manager, "zephyr", Some("companies"), 1, 20)
        .await?;
    let box_ = search.suggest(manager, "zephyr").await?;
    let expected = ["Zephyr", "Zephyr Dental", "Azephyron", "Zephir"];
    for result in [&page, &box_] {
        let titles: Vec<_> = result
            .hits
            .iter()
            .filter(|hit| hit.kind.id == "companies")
            .map(|hit| hit.title.as_str())
            .collect();
        prove(
            "company search ranks exact, prefix, substring, typo and trims hidden rows",
            titles == expected,
        )?;
    }
    prove(
        "the count excludes the deleted and other-tenant company",
        page.total(Some("companies")) == 4,
    )?;

    let find_person = |text: &'static str, scope: &'_ Scope| {
        let scope = scope.clone();
        async move {
            let result = search.find(&scope, text, Some("persons"), 1, 20).await?;
            Ok::<_, Failure>(result.hits.into_iter().find(|hit| hit.id == "3000020"))
        }
    };
    prove(
        "ordinary prefix finds a contact by email for an administrator",
        find_person("searchsecret", manager).await?.is_some(),
    )?;
    prove(
        "the viewer can find the contact by a close title",
        find_person("river alen", viewer).await?.is_some(),
    )?;
    prove(
        "a masked email does not match the viewer's prefix or a close spelling",
        find_person("searchsecret", viewer).await?.is_none()
            && find_person("searchsecrt", viewer).await?.is_none()
            && find_person("searchsecrt", manager).await?.is_none(),
    )?;

    client
        .execute(
            "INSERT INTO principal_capabilities (tenant_id,user_id,capability,is_allowed)
         VALUES ($1,$2,'admin.company.view',false)",
            &[&viewer.tenant(), &viewer.user()],
        )
        .await?;
    let denied = search
        .find(viewer, "zephyr", Some("companies"), 1, 20)
        .await?;
    prove(
        "a direct record-page denial removes the hits and count on the next search",
        denied.hits.is_empty() && denied.total(Some("companies")) == 0,
    )?;
    Ok(())
}

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
    let client = db
        .get_detached("the walkthrough's own checking connection")
        .await?;
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
            let client = db
                .get_detached("the walkthrough's own checking connection")
                .await?;
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
    let viewer = login(&app, "viewer@example.test").await?;
    title_matching(&search, &manager, &viewer, &client).await?;
    println!("search-punctuation: all checks passed");
    Ok(())
}
