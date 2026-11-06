//! Native proof of survivor-equivalent consumer counts and pages. Run only
//! against an empty disposable DATABASE_URL, after real-Git discovery --pin:
//! `cargo run --locked -p bilibi-web --example records-equivalence`.
//! Uses the complete Platform app, walkthrough fixture, sealed scopes, real
//! Records owner checks/redirects and audited holder writes. No UI is changed
//! or served. Output contains assertions and public composition identity only.
mod holder;

use bilibi_authz::grants::Grants;
use bilibi_database::{pool, Db, Error, MutationBoundary};
use bilibi_grid::{query::MAX_ROWS, BOUNDS, MAX_ESTIMATED_COST, MAX_OFFSET};
use bilibi_module_registry::{Composition, PinState};
use bilibi_records::{Records, ReferenceMatch};
use bilibi_tenancy::{Authentication, Scope};
use serde_json::Value;
use std::time::Duration;
use tokio_postgres::{error::SqlState, types::ToSql, IsolationLevel, Transaction};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Outcome<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Apply the established Grid cost bound to the complete consumer statement.
async fn admit(tx: &Transaction<'_>, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Outcome {
    let plan: Value = tx
        .query_one(&format!("EXPLAIN (FORMAT JSON) {sql}"), params)
        .await?
        .get(0);
    let cost = plan[0]["Plan"]["Total Cost"]
        .as_f64()
        .ok_or(Error::Invalid)?;
    assert!(
        cost <= MAX_ESTIMATED_COST,
        "consumer query exceeds Grid cost bound"
    );
    Ok(())
}

/// The holder owns its authorization, conditional aggregate and stable page.
/// Membership is the exact same owner subquery in both complete statements.
async fn read(
    tx: &Transaction<'_>,
    scope: &Scope,
    matched: &ReferenceMatch,
    offset: usize,
) -> Outcome<((i64, i64, i64), Vec<i64>)> {
    bilibi_authz::authorize_key(tx, scope, holder::VIEW).await?;
    assert!(offset <= MAX_OFFSET);
    let limit = 2_i64;
    assert!(limit as usize <= MAX_ROWS);
    let (tenant, kind, survivor) = matched.parameters();
    assert_eq!(tenant, scope.tenant());
    let condition = format!(
        "FROM records_equivalence_notes WHERE (tenant_id,record_kind,record_id) IN ({})",
        matched.sql()
    );
    let count = format!("SELECT count(*),count(*) FILTER (WHERE owner_id IS NOT NULL),count(*) FILTER (WHERE owner_id=$4) {condition}");
    let params: &[&(dyn ToSql + Sync)] = &[&tenant, &kind, &survivor, &scope.user()];
    admit(tx, &count, params).await?;
    let counts = tx.query_one(&count, params).await?;
    let page = format!("SELECT id {condition} ORDER BY id LIMIT $4 OFFSET $5");
    let offset = offset as i64;
    let params: &[&(dyn ToSql + Sync)] = &[&tenant, &kind, &survivor, &limit, &offset];
    admit(tx, &page, params).await?;
    let ids = tx
        .query(&page, params)
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    Ok(((counts.get(0), counts.get(1), counts.get(2)), ids))
}

/// Whole-row membership carries the holder's tenant and kind boundary. An
/// old ID-only holder query cannot consume this relation at all.
async fn membership_scope(db: &Db, records: &Records, admin: &Scope, other: &Scope) -> Outcome {
    let mut client = db.get().await?;
    let tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .start()
        .await?;
    tx.batch_execute(BOUNDS).await?;
    // Keep a same-ID row from another tenant visible only in this disposable
    // transaction. Row 8 already supplies the same ID under another kind.
    tx.execute(
        "INSERT INTO records_equivalence_notes(id,tenant_id,record_kind,record_id) VALUES(11,$1,'company','1')",
        &[&other.tenant()],
    )
    .await?;
    let overlaps: i64 = tx
        .query_one(
            "SELECT count(*) FROM records_equivalence_notes WHERE record_id='1' AND (tenant_id<>$1 OR record_kind<>$2)",
            &[&admin.tenant(), &"company"],
        )
        .await?
        .get(0);
    assert_eq!(overlaps, 2);
    let matched = records
        .matching(&tx, admin, "company", "1")
        .await?
        .ok_or(Error::Forbidden)?;
    assert_eq!(
        read(&tx, admin, &matched, 0).await?,
        ((5, 3, 2), vec![1, 2])
    );
    let (tenant, kind, survivor) = matched.parameters();
    let old_id_only = format!(
        "SELECT count(*) FROM records_equivalence_notes WHERE record_id IN ({})",
        matched.sql()
    );
    let error = tx
        .query_one(&old_id_only, &[&tenant, &kind, &survivor])
        .await
        .expect_err("three-column membership must reject an ID-only holder query");
    assert_eq!(error.code(), Some(&SqlState::SYNTAX_ERROR));
    tx.rollback().await?;
    println!(
        "PASS whole-row membership excludes same-ID rows in another tenant and kind; ID-only use is invalid"
    );
    Ok(())
}

async fn expect(
    db: &Db,
    records: &Records,
    scope: &Scope,
    source: &str,
    counts: (i64, i64, i64),
    pages: &[&[i64]],
) -> Outcome {
    let mut client = db.get().await?;
    // Owner permission decisions log on this same connection, so READ WRITE.
    let tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .start()
        .await?;
    tx.batch_execute(BOUNDS).await?;
    let matched = records
        .matching(&tx, scope, "company", source)
        .await?
        .ok_or(Error::Forbidden)?;
    for (n, expected) in pages.iter().enumerate() {
        assert_eq!(
            read(&tx, scope, &matched, n * 2).await?,
            (counts, expected.to_vec())
        );
    }
    tx.commit().await?;
    Ok(())
}

async fn merge(
    db: &Db,
    boundary: &MutationBoundary,
    records: &Records,
    scope: &Scope,
    sources: &[String],
    to: &str,
) -> Outcome {
    let mut client = db.get().await?;
    let mut tx = boundary
        .begin(
            &mut client,
            scope.mutation_context("records_equivalence.merge"),
        )
        .await?;
    for from in sources {
        records
            .redirect(&mut tx, scope, "company", from, to)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn denial(db: &Db, records: &Records, admin: &Scope, viewer: &Scope) -> Outcome {
    let mut client = db.get().await?;
    let tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .start()
        .await?;
    tx.batch_execute(BOUNDS).await?;
    for (scope, kind, id) in [
        (admin, "company", "100"), // another tenant
        (admin, "company", "999999"),
        (admin, "company", "malformed"),
        (admin, "absent.kind", "1"),
        (admin, "company", "62"),  // removed
        (viewer, "company", "61"), // unreadable merged source
        (viewer, "company", "1"),
    ] {
        assert!(records.matching(&tx, scope, kind, id).await?.is_none());
    }
    tx.commit().await?;
    println!(
        "PASS uniform denial: tenant, missing, malformed, unmounted, removed, unreadable survivor"
    );
    Ok(())
}

async fn snapshots(
    db: &Db,
    boundary: &MutationBoundary,
    records: &Records,
    admin: &Scope,
) -> Outcome {
    let mut client = db.get().await?;
    let tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .start()
        .await?;
    tx.batch_execute(BOUNDS).await?;
    let matched = records
        .matching(&tx, admin, "company", "61")
        .await?
        .ok_or(Error::Forbidden)?;
    merge(db, boundary, records, admin, &["58".into()], "1").await?;
    assert_eq!(read(&tx, admin, &matched, 4).await?, ((5, 3, 2), vec![5]));
    tx.commit().await?;
    expect(
        db,
        records,
        admin,
        "1",
        (6, 3, 2),
        &[&[1, 2], &[3, 4], &[5, 6], &[]],
    )
    .await?;
    println!("PASS concurrent merge: one caller snapshot across count/pages; fresh transaction sees commit");

    let redirects = "SELECT count(*) FROM base_records_redirects WHERE tenant_id=$1 AND kind='company' AND from_id='2'";
    for isolation in ["REPEATABLE READ", "SERIALIZABLE"] {
        let mut tx = boundary
            .begin(
                &mut client,
                admin.mutation_context("records_equivalence.rollback"),
            )
            .await?;
        tx.sql()
            .batch_execute(&format!("SET TRANSACTION ISOLATION LEVEL {isolation}"))
            .await?;
        assert!(matches!(
            records.redirect(&mut tx, admin, "company", "2", "1").await,
            Err(Error::Invalid)
        ));
        let written: i64 = tx
            .sql()
            .query_one(redirects, &[&admin.tenant()])
            .await?
            .get(0);
        assert_eq!(written, 0);
        drop(tx);
    }
    expect(db, records, admin, "2", (1, 0, 0), &[&[7], &[]]).await?;
    println!("PASS redirect outside READ COMMITTED refused before any write");

    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("records_equivalence.rollback"),
        )
        .await?;
    records
        .redirect(&mut tx, admin, "company", "2", "1")
        .await?;
    let written: i64 = tx
        .sql()
        .query_one(redirects, &[&admin.tenant()])
        .await?
        .get(0);
    assert_eq!(written, 1);
    let checked = records
        .check(tx.sql(), admin, "company", "2")
        .await?
        .map_err(|_| Error::Forbidden)?;
    assert_eq!(checked.id, "1");
    drop(tx); // owner neither commits nor rolls back the caller's transaction
    expect(db, records, admin, "2", (1, 0, 0), &[&[7], &[]]).await?;
    println!("PASS READ COMMITTED caller transaction: own uncommitted redirect followed; dropping rolls it back");
    Ok(())
}

async fn connections(db: &Db, records: &Records, admin: &Scope) -> Outcome {
    let mut held = Vec::new();
    for _ in 0..db.status().max_size {
        held.push(db.get().await?);
    }
    let client = held.first_mut().ok_or(Error::Internal)?;
    let tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .start()
        .await?;
    tx.batch_execute(BOUNDS).await?;
    let matched = tokio::time::timeout(
        Duration::from_secs(3),
        records.matching(&tx, admin, "company", "1"),
    )
    .await??
    .ok_or(Error::Forbidden)?;
    assert_eq!(read(&tx, admin, &matched, 0).await?.0, (7, 3, 2));
    tx.commit().await?;
    println!("PASS exhausted pool: checks and consumer queries finish on caller connection");
    let tx = client.transaction().await?;
    assert!(matches!(
        records.matching(&tx, admin, "company", "1").await,
        Err(Error::Invalid)
    ));
    tx.rollback().await?;
    println!("PASS READ COMMITTED refused explicitly");
    Ok(())
}

#[tokio::main]
async fn main() -> Outcome {
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_web::modules()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:3000")?;
    let composition = app.services.get::<Composition>("bootstrap.composition")?;
    assert_eq!(
        composition.pin,
        PinState::Pinned,
        "proof requires a genuine current composition pin"
    );
    println!(
        "Platform composition: {}",
        composition.commit.ok_or(Error::Invalid)?
    );
    app.services
        .get::<Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let records = app.services.get::<Records>(bilibi_records::SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let client = db.get().await?;
    let admin = authentication
        .authenticate(&**client, 1, 4, "records-equivalence".into())
        .await?;
    let viewer = authentication
        .authenticate(&**client, 1, 2, "records-equivalence".into())
        .await?;
    let other = authentication
        .authenticate(&**client, 2, 5, "records-equivalence".into())
        .await?;
    drop(client);
    holder::insert(
        &db,
        &boundary,
        &records,
        &admin,
        &[
            (1, "company", "61", None),
            (2, "company", "61", Some(4)),
            (3, "company", "60", Some(1)),
            (4, "company", "1", Some(4)),
            (5, "company", "60", None),
            (6, "company", "58", None),
            (7, "company", "2", None),
            (8, "person", "1", None),
        ],
    )
    .await?;
    holder::insert(
        &db,
        &boundary,
        &records,
        &other,
        &[(9, "company", "100", None)],
    )
    .await?;
    let original = holder::snapshot(&db, &admin).await?;
    let other_original = holder::snapshot(&db, &other).await?;
    expect(&db, &records, &admin, "61", (2, 1, 1), &[&[1, 2], &[]]).await?;
    println!("PASS unmerged source: conditional counts and ordered pagination");
    merge(&db, &boundary, &records, &admin, &["61".into()], "60").await?;
    merge(&db, &boundary, &records, &admin, &["60".into()], "1").await?;
    // Executable reproduction of canonicalization followed by stored equality.
    let client = db.get().await?;
    let checked = records
        .check(&**client, &admin, "company", "61")
        .await?
        .map_err(|_| Error::Forbidden)?;
    let equality: i64 = client.query_one("SELECT count(*) FROM records_equivalence_notes WHERE tenant_id=$1 AND record_kind=$2 AND record_id=$3", &[&admin.tenant(),&checked.kind,&checked.id]).await?.get(0);
    assert_eq!(equality, 1);
    drop(client);
    println!("REPRODUCED canonical equality returns 1; immutable merged references require 5");
    for source in ["61", "60", "1", "01"] {
        expect(
            &db,
            &records,
            &admin,
            source,
            (5, 3, 2),
            &[&[1, 2], &[3, 4], &[5], &[]],
        )
        .await?;
    }
    println!("PASS merged/chain/canonical source: exact total/taken/owned counts and every page");
    membership_scope(&db, &records, &admin, &other).await?;
    snapshots(&db, &boundary, &records, &admin).await?;
    assert_eq!(holder::snapshot(&db, &admin).await?, original);
    assert_eq!(holder::snapshot(&db, &other).await?, other_original);
    println!(
        "PASS holder rows and complete holder audit history unchanged across merges and reads"
    );

    // Additional historical sources exceed Grid's one-of bound. Membership
    // remains a SQL relation, with no client-side aliases or partial sums.
    db.get().await?.execute("INSERT INTO companies(id,tenant_id,name,code,status) SELECT n,$1,'Merge fixture '||n,'merge_fixture_'||n,'active' FROM generate_series(1000,1104) n", &[&admin.tenant()]).await?;
    holder::insert(
        &db,
        &boundary,
        &records,
        &admin,
        &[(10, "company", "1000", None)],
    )
    .await?;
    let before_many = holder::snapshot(&db, &admin).await?;
    let sources: Vec<_> = (1000..=1104).map(|n| n.to_string()).collect();
    merge(&db, &boundary, &records, &admin, &sources, "1").await?;
    expect(
        &db,
        &records,
        &admin,
        "1000",
        (7, 3, 2),
        &[&[1, 2], &[3, 4], &[5, 6], &[10], &[]],
    )
    .await?;
    assert_eq!(holder::snapshot(&db, &admin).await?, before_many);
    let count = records
        .referrers(&admin, "company", "1000")
        .await?
        .into_iter()
        .find(|r| r.table == holder::TABLE)
        .ok_or(Error::Internal)?
        .count;
    assert_eq!(count, 7);
    println!("PASS more than 100 predecessors: complete membership and existing referrers agree");

    db.get().await?.execute("INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES($1,$2,'admin.company.view',false) ON CONFLICT (tenant_id,user_id,capability) DO UPDATE SET is_allowed=false", &[&admin.tenant(),&viewer.user()]).await?;
    db.get()
        .await?
        .execute(
            "UPDATE companies SET deleted_at=now() WHERE tenant_id=$1 AND id=62",
            &[&admin.tenant()],
        )
        .await?;
    denial(&db, &records, &admin, &viewer).await?;
    connections(&db, &records, &admin).await?;
    println!("PASS all native Records equivalence assertions");
    Ok(())
}
