//! Proof for #597: duplicate matching reads a bounded, indexed candidate pool
//! and runs under a statement timeout, in a disposable database. Each check
//! prints PASS or stops the walkthrough with FAIL.
//!
//! ```sh
//! BILIBI_COMPANY_DUPLICATE_BOUNDS=disposable cargo run -p bilibi-company --example company-duplicate-bounds --locked
//! ```
use bilibi_company::{
    create, create_matches, duplicates, CreateAnyway, CreateOutcome, DuplicateCandidate,
    DuplicateQuery, NewCompany,
};
use bilibi_database::{pool, MutationBoundary};
use bilibi_tenancy::Scope;
use std::time::{Duration, Instant};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Failure = Box<dyn std::error::Error>;

const BULK: i64 = 100_000;
const ACME: i64 = 2_000_001;
/// The company rows and index entries a transaction has read so far, from
/// PostgreSQL's own per-transaction counters. An index scan counts its heap
/// fetches on the index, so every index of `companies` is summed in.
const ROWS_READ: &str = "SELECT sum(pg_stat_get_xact_tuples_returned(oid)+pg_stat_get_xact_tuples_fetched(oid))::bigint FROM (SELECT 'companies'::regclass::oid AS oid UNION ALL SELECT indexrelid FROM pg_index WHERE indrelid='companies'::regclass) relations";

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

fn named(name: &str) -> DuplicateQuery {
    DuplicateQuery {
        name: Some(name.into()),
        ..DuplicateQuery::default()
    }
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_COMPANY_DUPLICATE_BOUNDS").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_COMPANY_DUPLICATE_BOUNDS=disposable on a freshly migrated disposable DB"
                .into(),
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
    let mutations = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = app
        .services
        .get::<bilibi_settings::Settings>("base/settings.store")?;

    // A large tenant whose names all share the common word "trading", plus
    // the one real duplicate. Explicit ids stay clear of the fixture's.
    let mut client = db.get().await?;
    let started = Instant::now();
    client
        .execute(
            "INSERT INTO companies(id,tenant_id,name,code,status) SELECT 1000000+n,$1,'Bulk Trading '||n,'bulk_trading_'||n,'active' FROM generate_series(1,$2::bigint) n",
            &[&manager.tenant(), &BULK],
        )
        .await?;
    client
        .execute(
            "INSERT INTO companies(id,tenant_id,name,code,website,status) VALUES ($2,$1,'Acme Trading Sdn Bhd','acme_trading','https://acme.example','active')",
            &[&manager.tenant(), &ACME],
        )
        .await?;
    client.batch_execute("ANALYZE companies").await?;
    println!("  inserted {BULK} companies in {:?}", started.elapsed());

    // One matching read on its own transaction: what it found, and how many
    // company rows it read to find it.
    async fn measured(
        client: &mut tokio_postgres::Client,
        scope: &Scope,
        query: &DuplicateQuery,
    ) -> Result<(Vec<DuplicateCandidate>, i64), Failure> {
        let tx = client.transaction().await?;
        let before: i64 = tx.query_one(ROWS_READ, &[]).await?.get(0);
        let started = Instant::now();
        let found = duplicates(&tx, scope, query).await?;
        let elapsed = started.elapsed();
        let after: i64 = tx.query_one(ROWS_READ, &[]).await?.get(0);
        tx.rollback().await?;
        println!(
            "  match {:?}: {} candidates, {} company rows and index entries read, {elapsed:?}",
            query.name.as_deref().unwrap_or_default(),
            found.len(),
            after - before
        );
        Ok((found, after - before))
    }
    let raw: &mut tokio_postgres::Client = &mut client;

    let (found, rows) = measured(raw, &manager, &named("Acme Trading")).await?;
    prove(
        "the real duplicate is found first",
        found
            .first()
            .is_some_and(|c| c.id == ACME && c.score >= 0.99),
    )?;
    prove(
        "a name with one common word reads a bounded pool, not every company holding the word",
        rows < 5_000,
    )?;
    let (found, rows) = measured(raw, &manager, &named("Bulk Trading")).await?;
    prove(
        "a name of only common words still finds companies holding all of them, from a bounded pool",
        !found.is_empty() && found.iter().all(|c| c.name.starts_with("Bulk Trading ")) && rows < 5_000,
    )?;
    let (found, _) = measured(
        raw,
        &manager,
        &DuplicateQuery {
            name: Some("Zzyzx Unrelated".into()),
            website: Some("www.acme.example/about".into()),
            ..DuplicateQuery::default()
        },
    )
    .await?;
    prove(
        "an exact website match is found whatever the name",
        found
            .iter()
            .any(|c| c.id == ACME && c.matched_facts == ["website"]),
    )?;

    // The statement timeout is set for the matching statement only, and only
    // for a caller without one of its own.
    let tx = raw.transaction().await?;
    duplicates(&tx, &manager, &named("Acme Trading")).await?;
    let after: String = tx.query_one("SHOW statement_timeout", &[]).await?.get(0);
    tx.batch_execute("SET LOCAL statement_timeout = '7s'")
        .await?;
    duplicates(&tx, &manager, &named("Acme Trading")).await?;
    let own: String = tx.query_one("SHOW statement_timeout", &[]).await?.get(0);
    tx.rollback().await?;
    prove(
        "the caller's statement timeout is unchanged after matching",
        after == "0" && own == "7s",
    )?;

    // Another transaction holds `companies` exclusively. A bounded match gives
    // up at its timeout; an unbounded one waits for as long as the lock lasts.
    let mut holder = db.get().await?;
    let lock = holder.transaction().await?;
    lock.batch_execute("LOCK TABLE companies IN ACCESS EXCLUSIVE MODE")
        .await?;
    let tx = raw.transaction().await?;
    let started = Instant::now();
    let waited = tokio::time::timeout(
        Duration::from_secs(5),
        duplicates(&tx, &manager, &named("Acme Trading")),
    )
    .await;
    let elapsed = started.elapsed();
    lock.rollback().await?;
    drop(holder);
    drop(tx);
    match &waited {
        Ok(Err(error)) => println!("  refused after {elapsed:?}: {error}"),
        Ok(Ok(found)) => println!("  answered after {elapsed:?}: {}", found.len()),
        Err(_) => println!("  still waiting after {elapsed:?}"),
    }
    prove(
        "matching blocked on a locked table ends at its statement timeout, not when the lock ends",
        matches!(waited, Ok(Err(_))) && elapsed < Duration::from_secs(3),
    )?;

    // End to end: a create under `require_reason` in the large tenant, where
    // matching runs inside the tenant-wide create lock.
    let matching = NewCompany {
        name: "Acme Trading Bhd".into(),
        code: Some("acme_trading_2".into()),
        ..NewCompany::default()
    };
    let mut tx = mutations
        .begin(&mut client, manager.mutation_context("company.create"))
        .await?;
    settings
        .put(
            &mut tx,
            &manager,
            bilibi_settings::Target::Tenant,
            "company.duplicates.on_create",
            serde_json::json!("require_reason"),
        )
        .await?;
    let candidates = create_matches(tx.sql(), &manager, &matching)
        .await?
        .candidates
        .ok_or("the manager may list companies")?;
    prove(
        "the create preview names the real duplicate",
        candidates.first().is_some_and(|c| c.id == ACME),
    )?;
    prove(
        "a create without a reason is refused under require_reason",
        matches!(create(&mut tx, &manager, &matching).await?, CreateOutcome::Refused(errors)
            if errors.iter().any(|e| e.field == "create_anyway")),
    )?;
    let before: i64 = tx.sql().query_one(ROWS_READ, &[]).await?.get(0);
    let reviewed = NewCompany {
        create_anyway: Some(CreateAnyway {
            candidate_ids: candidates.iter().map(|c| c.id).collect(),
            reason: "A separate branch, reviewed".into(),
        }),
        ..matching.clone()
    };
    let started = Instant::now();
    let outcome = create(&mut tx, &manager, &reviewed).await?;
    let elapsed = started.elapsed();
    let after: i64 = tx.sql().query_one(ROWS_READ, &[]).await?.get(0);
    tx.rollback().await?;
    println!(
        "  reviewed create read {} company rows and index entries in {elapsed:?}",
        after - before
    );
    prove(
        "the reviewed create succeeds, reading a bounded number of company rows inside the lock",
        matches!(outcome, CreateOutcome::Created(_)) && after - before < 5_000,
    )?;
    println!("company-duplicate-bounds: all checks passed");
    Ok(())
}
