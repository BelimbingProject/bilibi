//! Manual running-service walkthrough of #557: a fenced owner transaction
//! that stays open longer than the lease no longer ends its attempt (the
//! heartbeat does not wait behind the job row's lock), and an owner's ending
//! hook that stalls delays the next job's ending by about one lease, not by
//! the whole attempt bound. A three-second lease (one-second beats) and a
//! sixty-second attempt bound keep the two apart. Explicit disposable DB
//! only. The checks fail the executable when the observed state breaks the
//! contract.
use bilibi_authz::{grants::Grants, SystemPrincipalDefinition, VocabularyContribution, VOCABULARY};
use bilibi_database::{pool, Db, Error, MutationBoundary, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance, Services};
use bilibi_queue::{
    Config, Ended, Ending, EndingBinding, EndingFuture, EndingService, Execution, Executor,
    Handler, HandlerBinding, HandlerFuture, HandlerService, Outcome, Queue, Worker, ENDINGS,
    HANDLERS, WORKERS,
};
use bilibi_tenancy::{Authentication, Scope};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();

const WORKER: &str = "queue/lease";
/// The system principal the ending hook runs as.
const PRINCIPAL: &str = "queue-lease";
/// The lease; heartbeats come every third of it.
const LEASE: i32 = 3;
/// How long the fenced transaction stays open: longer than the lease.
const HOLD: Duration = Duration::from_secs(5);

fn allowed(args: Value) -> Result<Value> {
    let mode = args
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("mode"))
        .and_then(Value::as_str);
    if matches!(mode, Some("hold" | "quick" | "stall")) {
        Ok(args)
    } else {
        Err(Error::Invalid)
    }
}
inventory::submit! { Contribution::new(WORKERS, WORKER, Provenance { module: "base/queue", source: file!(), line: line!() }, &Worker { max_attempts: 3, validate: allowed }) }
inventory::submit! { Contribution::new(VOCABULARY, "queue.lease-walkthrough", Provenance { module: "base/queue", source: file!(), line: line!() }, &VocabularyContribution {
    domains: &[], verbs: &[], capabilities: &[], roles: &[],
    system_principals: &[SystemPrincipalDefinition { name: PRINCIPAL, capabilities: &["companies.update"] }],
}) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, Provenance { module: "base/queue", source: file!(), line: line!() }, &HandlerBinding { service: "base/queue.lease", capability: "companies.update" }) }
inventory::submit! { Contribution::new(ENDINGS, WORKER, Provenance { module: "base/queue", source: file!(), line: line!() }, &EndingBinding { service: "base/queue.lease_ended", principal: PRINCIPAL, capability: "companies.update" }) }

/// `hold` keeps a fenced transaction open past the lease, then commits.
struct Hold {
    db: Db,
}
impl Handler for Hold {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            if args["mode"].as_str() != Some("hold") {
                return Outcome::Completed;
            }
            let fenced = async {
                let mut client = self.db.get().await?;
                let tx = execution.begin(&mut client, "queue.lease.hold").await?;
                tokio::time::sleep(HOLD).await;
                tx.commit().await
            };
            match fenced.await {
                Ok(()) => Outcome::Completed,
                Err(_) => Outcome::Retry("walkthrough_fence_failed"),
            }
        })
    }
}

/// The owner's ending hook: a `stall` job's hook never answers.
struct Told;
impl Ending for Told {
    fn ended<'a>(
        &'a self,
        _: &'a mut MutationTransaction<'_>,
        _: &'a Scope,
        job: &'a Ended,
    ) -> EndingFuture<'a> {
        Box::pin(async move {
            if job.args["mode"].as_str() == Some("stall") {
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
            Ok(())
        })
    }
}

fn check(label: &str, ok: bool) -> Result<()> {
    println!("{} {label}", if ok { "ok  " } else { "FAIL" });
    if ok {
        Ok(())
    } else {
        Err(Error::Internal)
    }
}

fn config() -> Config {
    Config {
        concurrency: 1,
        poll_interval: Duration::from_secs(1),
        lease_seconds: LEASE,
        max_runtime: Duration::from_secs(60),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_QUEUE_DEMO").as_deref() != Ok("yes") {
        return Err(Error::Invalid);
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    db.get().await?.batch_execute("INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES(1,1,'companies.update',true) ON CONFLICT DO NOTHING").await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide(
        "bootstrap.composition",
        std::sync::Arc::new(bilibi_web::composition()),
    )?;
    services.install(&modules)?;
    // This fixture binary enrols no authenticator (README of base/step_up).
    services
        .get::<bilibi_base::step_up::StepUp>(bilibi_base::step_up::SERVICE)?
        .set_enforced(false);
    services.provide(
        "base/queue.lease",
        Arc::new(HandlerService(Arc::new(Hold { db: db.clone() }))),
    )?;
    services.provide(
        "base/queue.lease_ended",
        Arc::new(EndingService(Arc::new(Told))),
    )?;
    let queue = services.get::<Queue>("base/queue.store")?;
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    let boundary = services.get::<MutationBoundary>("base/database.mutations")?;
    let mut client = db.get().await?;
    let actor = authentication
        .authenticate(&**client, 1, 1, String::new())
        .await?;
    let admin = authentication
        .authenticate(&**client, 1, 4, String::new())
        .await?;
    services
        .get::<Grants>("base/authz.grants")?
        .set_system(&admin, PRINCIPAL, "companies.update", Some(true))
        .await?;

    println!("== a fenced transaction open {HOLD:?}, longer than the {LEASE}-second lease");
    let mut tx = boundary
        .begin(&mut client, actor.mutation_context("queue.lease.enqueue"))
        .await?;
    let held = queue
        .enqueue_for(&mut tx, &actor, WORKER, json!({"mode":"hold"}))
        .await?
        .id();
    tx.commit().await?;
    let executor = Executor::from_services(&services, config())?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let started = Instant::now();
    let running = tokio::spawn(executor.run(stopped));
    let mut seen = (String::new(), 0);
    let mut code: Option<String> = None;
    while started.elapsed() < Duration::from_secs(30) {
        let row = client
            .query_one(
                "SELECT state,attempt,failure_code FROM base_queue_jobs WHERE id=$1",
                &[&held],
            )
            .await?;
        seen = (row.get(0), row.get(1));
        code = row.get(2);
        if seen.0 != "available" && seen.0 != "executing" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = stop.send(true);
    let _ = running.await;
    println!(
        "after {:.2?}: {}, attempt {}, failure {code:?}",
        started.elapsed(),
        seen.0,
        seen.1
    );
    check(
        "it completes on its first attempt: a heartbeat behind the fence does not end it",
        seen == ("completed".to_owned(), 1),
    )?;

    println!("== an ending hook that stalls holds up the next ending by about one lease");
    // No executor runs: enqueue and cancel them through the owner command.
    let mut tx = boundary
        .begin(&mut client, actor.mutation_context("queue.lease.enqueue"))
        .await?;
    let mut ended = Vec::new();
    for mode in ["stall", "quick"] {
        let job = queue
            .enqueue_for(&mut tx, &actor, WORKER, json!({ "mode": mode }))
            .await?
            .id();
        bilibi_queue::cancel_for(&mut tx, &actor, job).await?;
        ended.push(job);
    }
    tx.commit().await?;
    let executor = Executor::from_services(&services, config())?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let started = Instant::now();
    let running = tokio::spawn(executor.run(stopped));
    let mut told = false;
    while started.elapsed() < Duration::from_secs(90) && !told {
        tokio::time::sleep(Duration::from_millis(100)).await;
        told = client
            .query_one(
                "SELECT ending='told' FROM base_queue_jobs WHERE id=$1",
                &[&ended[1]],
            )
            .await?
            .get::<_, Option<bool>>(0)
            .unwrap_or(false);
    }
    let took = started.elapsed();
    let _ = stop.send(true);
    let _ = running.await;
    let stalled: (Option<String>, i32) = {
        let row = client
            .query_one(
                "SELECT ending,ending_attempts FROM base_queue_jobs WHERE id=$1",
                &[&ended[0]],
            )
            .await?;
        (row.get(0), row.get(1))
    };
    println!(
        "the next job told after {took:.2?}; the stalled one: ending {:?}, {} failed attempt(s)",
        stalled.0, stalled.1
    );
    check(
        "the next ending waits about one lease, not the sixty-second attempt bound",
        told && took < Duration::from_secs(15),
    )?;
    check(
        "the stalled ending is not told, and counts one failed attempt to try again later",
        stalled.0.is_none() && stalled.1 >= 1,
    )?;
    println!("Lease walkthrough passed.");
    Ok(())
}
