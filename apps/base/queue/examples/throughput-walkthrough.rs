//! Manual running-service walkthrough of the executor's claim loop (#542): a
//! slot goes straight to the next waiting job after one ran, waits a poll
//! interval only when nothing was claimed, and a failing job still waits its
//! backoff. One slot and the default one-second poll interval, so the numbers
//! are the loop's own: before the fix a slot slept a full interval after every
//! job and ran about one job a second. Explicit disposable DB only. The checks
//! fail the executable when the observed state breaks the contract.
use bilibi_database::{pool, Error, MutationBoundary, Result};
use bilibi_module_registry::{Contribution, Provenance, Services};
use bilibi_queue::{
    Config, Execution, Executor, Handler, HandlerBinding, HandlerFuture, HandlerService, Outcome,
    Queue, Worker, HANDLERS, WORKERS,
};
use bilibi_tenancy::Authentication;
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();

const WORKER: &str = "queue/throughput";
/// Jobs the quick run enqueues. At one job a second they would take 30 s.
const JOBS: i64 = 30;

fn allowed(args: Value) -> Result<Value> {
    let mode = args
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("mode"))
        .and_then(Value::as_str);
    if matches!(mode, Some("quick" | "fail")) {
        Ok(args)
    } else {
        Err(Error::Invalid)
    }
}
inventory::submit! { Contribution::new(WORKERS, WORKER, Provenance { module: "base/queue", source: file!(), line: line!() }, &Worker { max_attempts: 3, validate: allowed }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, Provenance { module: "base/queue", source: file!(), line: line!() }, &HandlerBinding { service: "base/queue.throughput", capability: "companies.update" }) }

/// Finishes at once, or fails with a retryable code.
struct Quick;
impl Handler for Quick {
    fn perform<'a>(&'a self, _: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            match args["mode"].as_str() {
                Some("fail") => Outcome::Retry("walkthrough_failure"),
                _ => Outcome::Completed,
            }
        })
    }
}

/// Transactions the database has finished so far, commits and rollbacks.
async fn finished(client: &tokio_postgres::Client) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT (xact_commit+xact_rollback)::bigint FROM pg_stat_database WHERE datname=current_database()",
            &[],
        )
        .await?
        .get(0))
}
/// A job's state and attempt.
async fn state(client: &tokio_postgres::Client, job: i64) -> Result<(String, i32)> {
    let row = client
        .query_one(
            "SELECT state,attempt FROM base_queue_jobs WHERE id=$1",
            &[&job],
        )
        .await?;
    Ok((row.get(0), row.get(1)))
}

fn check(label: &str, ok: bool) -> Result<()> {
    println!("{} {label}", if ok { "ok  " } else { "FAIL" });
    if ok {
        Ok(())
    } else {
        Err(Error::Internal)
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
    services.provide(
        "base/queue.throughput",
        Arc::new(HandlerService(Arc::new(Quick))),
    )?;
    let queue = services.get::<Queue>("base/queue.store")?;
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    let boundary = services.get::<MutationBoundary>("base/database.mutations")?;
    let mut client = db.get().await?;
    let actor = authentication
        .authenticate(&**client, 1, 1, String::new())
        .await?;

    println!("== {JOBS} quick jobs, one slot, a one-second poll interval");
    let mut tx = boundary
        .begin(
            &mut client,
            actor.mutation_context("queue.throughput.enqueue"),
        )
        .await?;
    for _ in 0..JOBS {
        queue
            .enqueue_for(&mut tx, &actor, WORKER, json!({"mode":"quick"}))
            .await?;
    }
    tx.commit().await?;
    let executor = Executor::from_services(
        &services,
        Config {
            concurrency: 1,
            poll_interval: Duration::from_secs(1),
            lease_seconds: 30,
            max_runtime: Duration::from_secs(30),
        },
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let started = Instant::now();
    let running = tokio::spawn(executor.run(stopped));
    let mut done = 0_i64;
    while started.elapsed() < Duration::from_secs(60) {
        done = client
            .query_one(
                "SELECT count(*) FROM base_queue_jobs WHERE worker_id=$1 AND state='completed'",
                &[&WORKER],
            )
            .await?
            .get(0);
        if done == JOBS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let took = started.elapsed();
    println!("{done} of {JOBS} jobs completed in {took:.2?}");
    check(
        "every job completes, each exactly once",
        done == JOBS
            && client
                .query_one(
                    "SELECT count(*) FROM base_queue_jobs WHERE worker_id=$1 AND attempt<>1",
                    &[&WORKER],
                )
                .await?
                .get::<_, i64>(0)
                == 0,
    )?;
    check(
        "a slot goes straight to the next waiting job: the batch takes a fifth of the time one job a second would",
        took < Duration::from_secs((JOBS / 5) as u64),
    )?;

    println!("== an empty queue is polled, not spun on");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let before = finished(&client).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after = finished(&client).await?;
    println!("{} transactions in 3 idle seconds", after - before);
    check(
        "idle, a slot claims about once a second, not in a loop",
        after - before < 100,
    )?;

    println!("== a failing job still waits its backoff");
    let mut tx = boundary
        .begin(
            &mut client,
            actor.mutation_context("queue.throughput.enqueue"),
        )
        .await?;
    let failing = queue
        .enqueue_for(&mut tx, &actor, WORKER, json!({"mode":"fail"}))
        .await?
        .id();
    tx.commit().await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (first, attempt) = state(&client, failing).await?;
    println!("after 1.5 s: {first}, attempt {attempt}");
    check(
        "it ran once and is retryable: not claimed again before its backoff ends",
        first == "retryable" && attempt == 1,
    )?;
    let mut last = (first, attempt);
    for _ in 0..200 {
        last = state(&client, failing).await?;
        if last.0 == "discarded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("at the end: {}, attempt {}", last.0, last.1);
    check(
        "retries still run, and the job is discarded after its third attempt",
        last == ("discarded".to_owned(), 3),
    )?;

    let _ = stop.send(true);
    let _ = running.await;
    println!("Throughput walkthrough passed.");
    Ok(())
}
