//! Bounded audit evidence on a disposable, migrated database; no test
//! harness. It shows, and fails when it does not hold, that
//!
//! - each tenant has its own hash chain, a transaction's rows are linked in
//!   one pass as it commits, and a commit in one tenant does not wait for
//!   another tenant's chain (#576);
//! - the Actions and Data Mutations lists read one period, count their
//!   matches only to a cap and stop at their statement timeout (#577);
//! - "Verify audit log" is a queued job that walks the chains in bounded
//!   steps from a stored place, one verification at a time, and names a
//!   removed row (#578).
//!
//!   BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes DATABASE_URL=... \
//!     cargo run -p bilibi-audit-views --example bounded-evidence
//!
//! It writes about 12,000 evidence rows and removes one behind the guard's
//! back (`session_replication_role`, so the database role must be a
//! superuser, as a disposable database's is). Run it once per database.
use bilibi_audit::{
    advance_verification, latest_verification, list_actions, list_mutations, request_verification,
    ActionQuery, Attribution, MutationQuery, MAX_COUNT, STEP_LINKS,
};
use bilibi_audit_views::VERIFY_WORKER;
use bilibi_database::{pool, Error, MutationBoundary};
use bilibi_queue::Queue;
use bilibi_tenancy::Scope;
use serde_json::json;
use std::time::{Duration, Instant, SystemTime};
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Outcome<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn check(what: &str, holds: bool) -> Outcome {
    println!("{what}: {}", if holds { "ok" } else { "FAILED" });
    if holds {
        Ok(())
    } else {
        Err(format!("{what}: did not hold").into())
    }
}

async fn text(client: &Client, sql: &str) -> Outcome<String> {
    Ok(client.query_one(sql, &[]).await?.get(0))
}

/// Starts a verification the way the page does: the run and its job in one
/// audited transaction.
async fn start(
    boundary: &MutationBoundary,
    queue: &Queue,
    client: &mut Client,
    scope: &Scope,
) -> Result<i64, Error> {
    let mut tx = boundary
        .begin(client, scope.mutation_context("audit.chain.verify"))
        .await?;
    let id = request_verification(&mut tx, scope).await?;
    queue
        .enqueue_for(&mut tx, scope, VERIFY_WORKER, json!({"verification": id}))
        .await?;
    tx.commit().await?;
    Ok(id)
}

const MUTATION: &str = "INSERT INTO base_audit_mutations(tenant_id,actor_type,actor_id,auditable_type,auditable_id,event,old_values,new_values)";
const ACTION: &str =
    "INSERT INTO base_audit_actions(tenant_id,actor_type,actor_id,event,payload,occurred_at)";

#[tokio::main]
async fn main() -> Outcome {
    if std::env::var("BILIBI_DISPOSABLE_AUDIT_FIXTURE").as_deref() != Ok("yes") {
        return Err(
            "Set BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes and point DATABASE_URL at a disposable database."
                .into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let origin = std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost:35576".into());
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin = app.security.guard(&login.token).await?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let queue = app.services.get::<Queue>(bilibi_queue::SERVICE)?;
    let mut client = db.get().await?;
    let mut second = db.get().await?;

    println!("== #576: one chain per tenant, linked in one pass as the transaction commits");
    let writing = client.transaction().await?;
    writing
        .batch_execute(&format!(
            "{MUTATION} SELECT 1,'user',1,'walkthrough.bounded',g::text,'updated','{{\"n\":0}}'::jsonb,jsonb_build_object('n',g) FROM generate_series(1,2000) g; \
             {MUTATION} SELECT 2,'user',5,'walkthrough.bounded',g::text,'created','{{}}'::jsonb,jsonb_build_object('n',g) FROM generate_series(1,3) g; \
             {ACTION} VALUES (1,'user',1,'walkthrough.bounded','{{\"result\":\"committed\"}}',NULL),(2,'user',5,'walkthrough.bounded','{{\"result\":\"committed\"}}',NULL)"
        ))
        .await?;
    const UNLINKED: &str = "SELECT count(*)::text FROM base_audit_mutations WHERE auditable_type='walkthrough.bounded' AND chain_seq IS NULL";
    let before = writing.query_one(UNLINKED, &[]).await?.get::<_, String>(0);
    let committing = Instant::now();
    writing.commit().await?;
    println!(
        "2,003 changes of two tenants in one transaction: {before} unlinked before commit, {} after; commit took {} ms",
        text(&client, UNLINKED).await?,
        committing.elapsed().as_millis()
    );
    check(
        "no row is left unlinked",
        text(&client, UNLINKED).await? == "0",
    )?;
    check(
        "each tenant's rows took consecutive links of that tenant's own chain",
        text(&client, "SELECT bool_and(links = last - first + 1)::text FROM (SELECT count(*) AS links, min(chain_seq) AS first, max(chain_seq) AS last \
             FROM base_audit_mutations WHERE auditable_type='walkthrough.bounded' GROUP BY tenant_id) t").await? == "true",
    )?;
    for row in client
        .query(
            "SELECT chain||' tenant '||tenant_id||': '||seq||' links' FROM base_audit_chain_heads WHERE tenant_id IN (1,2) ORDER BY chain,tenant_id",
            &[],
        )
        .await?
    {
        println!("head {}", row.get::<_, String>(0));
    }
    // Another session holds tenant 1's chain, as a commit in flight does.
    let held = second.transaction().await?;
    held.execute(
        "SELECT pg_advisory_xact_lock(841332, base_audit_chain_key('mutations', 1))",
        &[],
    )
    .await?;
    let commit_in = |tenant: i64, wait: &str| {
        format!(
            "BEGIN; SET LOCAL lock_timeout='{wait}'; {MUTATION} VALUES ({tenant},'user',1,'walkthrough.lock','1','created','{{}}','{{\"n\":1}}'); COMMIT"
        )
    };
    let other_tenant = client.batch_execute(&commit_in(2, "2s")).await;
    let same_tenant = client.batch_execute(&commit_in(1, "500ms")).await;
    client.batch_execute("ROLLBACK").await?;
    held.rollback().await?;
    check(
        "while tenant 1's chain is held, a commit in tenant 2 goes through",
        other_tenant.is_ok(),
    )?;
    check(
        "and a commit in tenant 1 waits for it",
        same_tenant.is_err(),
    )?;

    println!("== #577: the lists read one period, count to a cap and stop at a timeout");
    client
        .batch_execute(&format!(
            "{ACTION} SELECT 1,'user',1,'walkthrough.period','{{}}'::jsonb,now() - interval '40 days' FROM generate_series(1,30); \
             {ACTION} SELECT 1,'user',1,'walkthrough.period','{{}}'::jsonb,now() - interval '1 day' FROM generate_series(1,12); \
             {ACTION} SELECT 1,'user',1,'walkthrough.many','{{}}'::jsonb,now() - interval '2 hours' FROM generate_series(1,10050)"
        ))
        .await?;
    let search = |text: &str| ActionQuery {
        search: text.into(),
        ..ActionQuery::default()
    };
    let recent = list_actions(&mut client, &admin, &search("walkthrough.period")).await?;
    check(
        "the default period (last 30 days) lists the 12 recent of 42 actions",
        recent.total_entries == 12 && !recent.more,
    )?;
    let wider = ActionQuery {
        since: SystemTime::now() - Duration::from_secs(60 * 86_400),
        ..search("walkthrough.period")
    };
    check(
        "a 60-day period lists all 42",
        list_actions(&mut client, &admin, &wider)
            .await?
            .total_entries
            == 42,
    )?;
    let many = list_actions(&mut client, &admin, &search("walkthrough.many")).await?;
    println!(
        "10,050 matches: counted {} (more: {}), page 1 has {} rows",
        many.total_entries,
        many.more,
        many.entries.len()
    );
    check(
        "the count stops at the cap and says there are more",
        many.total_entries == MAX_COUNT && many.more && many.entries.len() == 25,
    )?;
    let beyond = ActionQuery {
        page: 100_000,
        ..search("walkthrough.many")
    };
    check(
        "a page beyond the cap reads nothing",
        list_actions(&mut client, &admin, &beyond)
            .await?
            .entries
            .is_empty(),
    )?;
    let changes = list_mutations(&mut client, &admin, &MutationQuery::default()).await?;
    println!(
        "data mutations, default period: {} counted, page 1 has {} rows",
        changes.total_entries,
        changes.entries.len()
    );
    // A read that cannot finish: another session holds the table.
    let blocking = second.transaction().await?;
    blocking
        .batch_execute("LOCK TABLE base_audit_actions IN ACCESS EXCLUSIVE MODE")
        .await?;
    let reading = Instant::now();
    let stopped = list_actions(&mut client, &admin, &ActionQuery::default()).await?;
    blocking.rollback().await?;
    println!(
        "a blocked read came back after {:.1} s",
        reading.elapsed().as_secs_f32()
    );
    check(
        "a read that outlasts its statement timeout stops and says so, without an error",
        stopped.timed_out && stopped.entries.is_empty(),
    )?;

    println!("== #578: verification is a queued job that walks the chains in bounded steps");
    let first = start(&boundary, &queue, &mut client, &admin).await?;
    let again = start(&boundary, &queue, &mut client, &admin).await;
    check(
        "a second verification is refused while one runs",
        matches!(again, Err(Error::Conflict)),
    )?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped_signal) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped_signal));
    let mut run = None;
    for _ in 0..240 {
        let latest = latest_verification(&**client)
            .await?
            .ok_or("the verification row is missing")?;
        if !latest.running() {
            run = Some(latest);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = stop.send(true);
    let _ = workers.await;
    println!(
        "job: {}",
        text(&client, "SELECT state||' '||coalesce(failure_code,'') FROM base_queue_jobs WHERE worker_id='base/audit-views-verify' ORDER BY id DESC LIMIT 1").await?
    );
    let run = run.ok_or("the queued verification did not finish in two minutes")?;
    for report in &run.reports {
        println!(
            "verification {} by the job: {} {} links in {} tenant chains, unsealed {}, break {:?}",
            run.id, report.chain, report.checked, report.tenants, report.unsealed, report.break_at
        );
    }
    check(
        "the job finished the verification it was started with, whole",
        run.id == first && run.state == "whole" && run.reports.len() == 2,
    )?;
    check(
        "its result is recorded as one audit.chain.verified action",
        client
            .query_one(
                "SELECT count(*) FROM base_audit_actions WHERE event='audit.chain.verified' AND payload->>'verification'=$1",
                &[&first.to_string()],
            )
            .await?
            .get::<_, i64>(0)
            == 1,
    )?;
    // Remove one row of tenant 2 behind the guard's back, then verify again,
    // this time stepping by hand as the job steps.
    client
        .batch_execute(
            "SET session_replication_role = replica; \
             DELETE FROM base_audit_mutations WHERE id = (SELECT id FROM base_audit_mutations WHERE tenant_id=2 AND auditable_type='walkthrough.bounded' ORDER BY id LIMIT 1 OFFSET 1); \
             SET session_replication_role = origin;",
        )
        .await?;
    let mut tx = boundary
        .begin(&mut client, admin.mutation_context("audit.chain.verify"))
        .await?;
    let id = request_verification(&mut tx, &admin).await?;
    tx.commit().await?;
    let who = Attribution::from_scope(&admin);
    let mut steps = 0;
    let broken = loop {
        let run = advance_verification(&mut client, id, &who).await?;
        steps += 1;
        if !run.running() {
            break run;
        }
    };
    println!(
        "after a row was removed: {} in {steps} steps of at most {STEP_LINKS} links each",
        broken.state
    );
    for report in &broken.reports {
        println!(
            "  {} {} links, break {:?}",
            report.chain, report.checked, report.break_at
        );
    }
    let found = broken.reports.iter().any(|report| {
        report.chain == "mutations"
            && report
                .break_at
                .as_ref()
                .is_some_and(|b| b.tenant == Some(2) && b.reason == "missing")
    });
    check(
        "the removed row is found in tenant 2's chain, and no other chain is blamed",
        broken.state == "broken"
            && found
            && broken
                .reports
                .iter()
                .all(|r| r.chain == "mutations" || r.break_at.is_none()),
    )?;
    check("the walk took several bounded steps", steps > 4)?;
    Ok(())
}
