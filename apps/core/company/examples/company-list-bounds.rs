//! Proof for #596: the agent list `core.company.list` reads under Grid's
//! bounds and searches the indexed `search_vector`, in a disposable database.
//! Each check prints PASS or stops the walkthrough with FAIL.
//!
//! ```sh
//! BILIBI_COMPANY_LIST_BOUNDS=disposable cargo run -p bilibi-company --example company-list-bounds --locked
//! ```
use bilibi_agent_api::Registry;
use bilibi_company::SearchQuery;
use bilibi_database::pool;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Failure = Box<dyn std::error::Error>;

const BULK: i64 = 100_000;

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

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_COMPANY_LIST_BOUNDS").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_COMPANY_LIST_BOUNDS=disposable on a freshly migrated disposable DB".into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:43231")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let manager = login(&app, "manager@example.test").await?;
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let client = db.get().await?;

    // A large tenant: explicit ids far above the fixture's, so the identity
    // sequence and the fixture rows never collide.
    let started = Instant::now();
    client
        .execute(
            "INSERT INTO companies(id,tenant_id,name,code,status) SELECT 1000000+n,$1,'Bulkfill '||n,'bulkfill_'||n,'active' FROM generate_series(1,$2::bigint) n",
            &[&manager.tenant(), &BULK],
        )
        .await?;
    client
        .execute(
            "INSERT INTO companies(id,tenant_id,name,code,legal_name,email,status) VALUES (2000001,$1,'Zebrafish Holdings','zf_unique','Quokka Ventures','owner@marmot.example','active')",
            &[&manager.tenant()],
        )
        .await?;
    client.batch_execute("ANALYZE companies").await?;
    println!("  inserted {BULK} companies in {:?}", started.elapsed());

    let list = |search: &'static str| {
        let registry = registry.clone();
        let manager = manager.clone();
        let db = db.clone();
        async move {
            let client = db.get().await?;
            let started = Instant::now();
            let result = registry
                .call(
                    &**client,
                    &manager,
                    "core.company.list",
                    &json!({"search": search, "page_size": 5}),
                )
                .await;
            println!("  search {search:?} took {:?}", started.elapsed());
            Ok::<_, Failure>(result)
        }
    };
    let total = |value: &Value| value["meta"]["page"]["total"].as_u64();
    let names = |value: &Value| -> Vec<String> {
        value["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
            .collect()
    };

    // Matching: whole words and word prefixes of name, code and legal name.
    let found = list("zebra").await??;
    prove(
        "a name word prefix finds the company",
        total(&found) == Some(1) && names(&found) == ["Zebrafish Holdings"],
    )?;
    let found = list("Quokka").await??;
    prove(
        "a legal-name word finds the company, case-insensitively",
        names(&found) == ["Zebrafish Holdings"],
    )?;
    let found = list("zf_unique").await??;
    prove(
        "the code finds the company",
        names(&found) == ["Zebrafish Holdings"],
    )?;
    let found = list("zebrafish nomatch").await??;
    prove("every word must match", total(&found) == Some(0))?;
    let found = list("%_\\").await??;
    prove(
        "punctuation alone matches nothing and never errors",
        total(&found) == Some(0),
    )?;
    let found = list("bulkfill").await??;
    prove(
        "a word shared by the whole large tenant counts every match within the bounds",
        total(&found) == Some(BULK as u64) && names(&found).len() == 5,
    )?;
    let expected: Vec<String> = client
        .query(
            "SELECT name FROM companies WHERE tenant_id=$1 AND name LIKE 'Bulkfill %' ORDER BY name, id LIMIT 5 OFFSET 10",
            &[&manager.tenant()],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let third = registry
        .call(
            &**client,
            &manager,
            "core.company.list",
            &json!({"search": "bulkfill", "page": 3, "page_size": 5}),
        )
        .await?;
    prove(
        "a later page holds the next companies by name",
        names(&third) == expected,
    )?;

    // Index: the rows and index entries of `companies` one search reads in
    // this transaction, from PostgreSQL's own per-transaction counters. An
    // index scan counts its heap fetches on the index, so every index of
    // `companies` is summed in.
    let mut reader = db.get().await?;
    let tx = reader.transaction().await?;
    let read = "SELECT sum(pg_stat_get_xact_tuples_returned(oid)+pg_stat_get_xact_tuples_fetched(oid))::bigint FROM (SELECT 'companies'::regclass::oid AS oid UNION ALL SELECT indexrelid FROM pg_index WHERE indrelid='companies'::regclass) relations";
    let before: i64 = tx.query_one(read, &[]).await?.get(0);
    let page = bilibi_company::search(
        &*tx,
        &manager,
        &SearchQuery {
            search: "zebra".into(),
            ..SearchQuery::default()
        },
    )
    .await?;
    let rows: i64 = tx.query_one(read, &[]).await?.get(0);
    let rows = rows - before;
    tx.rollback().await?;
    println!(
        "  search \"zebra\" read {rows} company rows and index entries for {} match",
        page.total
    );
    prove(
        "a rare word reads its matches through the index, not every company of the tenant",
        page.total == 1 && rows < 1_000,
    )?;

    // Bounds: another transaction holds `companies` exclusively. A bounded
    // read gives up at Grid's lock timeout; an unbounded one waits for ever.
    let mut holder = db.get().await?;
    let lock = holder.transaction().await?;
    lock.batch_execute("LOCK TABLE companies IN ACCESS EXCLUSIVE MODE")
        .await?;
    let started = Instant::now();
    let waited = tokio::time::timeout(Duration::from_secs(5), list("zebra")).await;
    let elapsed = started.elapsed();
    lock.rollback().await?;
    drop(holder);
    match &waited {
        Ok(Ok(Err(error))) => println!("  refused after {elapsed:?}: {error}"),
        Ok(Ok(Ok(_))) => println!("  answered after {elapsed:?}"),
        Ok(Err(error)) => println!("  failed after {elapsed:?}: {error}"),
        Err(_) => println!("  still waiting after {elapsed:?}"),
    }
    prove(
        "a search blocked on a locked table is refused within Grid's lock timeout, not held for ever",
        matches!(waited, Ok(Ok(Err(_)))) && elapsed < Duration::from_secs(3),
    )?;
    let found = list("zebra").await??;
    prove(
        "the same search answers once the lock is gone",
        names(&found) == ["Zebrafish Holdings"],
    )?;
    println!("company-list-bounds: all checks passed");
    Ok(())
}
