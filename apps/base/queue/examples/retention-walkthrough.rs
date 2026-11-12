//! Manual retention and status walkthrough on a disposable database; no test
//! harness. It deletes finished queue jobs, so never point it at real data.
use bilibi_base::settings::Settings;
use bilibi_database::{pool, Error, Result};
use bilibi_module_registry::Services;
use bilibi_queue::Queue;
use std::sync::Arc;
use std::time::SystemTime;
use tokio_postgres::Client;

/// A production worker whose owner is told when its job ends.
const TOLD: &str = "base/privacy-request-store";
/// A production worker with no ending hook.
const PLAIN: &str = "base/schedule-occurrence-retention";
const TRACE: &str = "queue-retention-walkthrough";

async fn seed(
    client: &Client,
    case: &str,
    worker: &str,
    state: &str,
    ending: Option<&str>,
    days: i32,
) -> Result<()> {
    let lease = (state == "executing").then_some("walkthrough-lease");
    client.execute("INSERT INTO base_queue_jobs(tenant_id,worker_id,worker_module,args,actor_kind,actor_id,trace_id,max_attempts,state,ending,lease_token,lease_expires_at,updated_at) VALUES(1,$1,'base/queue',jsonb_build_object('case',$2::text),'user',1,$3,3,$4,$5,$6,CASE WHEN $6::text IS NULL THEN NULL ELSE clock_timestamp()+interval '1 hour' END,clock_timestamp()-make_interval(days=>$7))", &[&worker,&case,&TRACE,&state,&ending,&lease,&days]).await?;
    Ok(())
}
async fn cases(client: &Client) -> Result<Vec<String>> {
    Ok(client
        .query(
            "SELECT args->>'case' FROM base_queue_jobs WHERE trace_id=$1 AND args ? 'case' ORDER BY 1",
            &[&TRACE],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect())
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
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    let queue = services.get::<Queue>("base/queue.store")?;
    let settings = services.get::<Settings>("base/settings.store")?;
    queue.workers().get(TOLD)?;
    queue.workers().get(PLAIN)?;
    let client = db.get().await?;
    let client: &Client = &client;
    client
        .execute("DELETE FROM base_queue_jobs WHERE trace_id=$1", &[&TRACE])
        .await?;

    let days = settings
        .get_global(client, "queue.jobs.keep_days")
        .await?
        .as_i64()
        .ok_or(Error::Internal)?;
    assert_eq!(days, 30);
    println!("PASS setting: queue.jobs.keep_days defaults to {days} days");

    seed(client, "completed-old", PLAIN, "completed", None, 40).await?;
    seed(client, "completed-recent", PLAIN, "completed", None, 5).await?;
    seed(client, "cancelled-plain-old", PLAIN, "cancelled", None, 40).await?;
    seed(
        client,
        "cancelled-told-old",
        TOLD,
        "cancelled",
        Some("told"),
        40,
    )
    .await?;
    seed(client, "cancelled-untold-old", TOLD, "cancelled", None, 40).await?;
    seed(
        client,
        "discarded-unrecorded-old",
        TOLD,
        "discarded",
        Some("failed"),
        40,
    )
    .await?;
    seed(client, "discarded-recent", PLAIN, "discarded", None, 5).await?;
    seed(client, "scheduled-old", PLAIN, "scheduled", None, 40).await?;
    seed(client, "retryable-old", PLAIN, "retryable", None, 40).await?;
    seed(client, "executing-old", PLAIN, "executing", None, 40).await?;
    let before = cases(client).await?;
    assert_eq!(before.len(), 10);

    let now = SystemTime::now();
    assert_eq!(queue.prune(client, 0, now).await?, 0);
    assert_eq!(cases(client).await?, before);
    println!("PASS zero keeps every job");
    for refused in [-1, 3651] {
        assert!(matches!(
            queue.prune(client, refused, now).await,
            Err(Error::Invalid)
        ));
    }
    assert_eq!(cases(client).await?, before);
    println!("PASS a number of days outside 0 to 3650 is refused and deletes nothing");

    // The live counts, read the slow way over the whole table, must equal the
    // line System Info builds from the partial indexes.
    let counted = client.query_one("SELECT count(*) FILTER (WHERE state IN ('available','scheduled')), count(*) FILTER (WHERE state='retryable'), count(*) FILTER (WHERE state='discarded'), count(*) FILTER (WHERE ending='failed') FROM base_queue_jobs", &[]).await?;
    let (pending, retryable, discarded, unrecorded): (i64, i64, i64, i64) = (
        counted.get(0),
        counted.get(1),
        counted.get(2),
        counted.get(3),
    );
    assert!(pending >= 1 && retryable >= 1 && discarded >= 2 && unrecorded >= 1);
    let status = bilibi_queue::health_status(client).await?;
    assert_eq!(
        status,
        format!("Available ({pending} pending, {retryable} retryable, {discarded} discarded, {unrecorded} ended without their owner recording it)")
    );
    println!("PASS status line equals a full count: {status}");

    assert_eq!(queue.prune(client, days, now).await?, 3);
    assert_eq!(
        cases(client).await?,
        [
            "cancelled-untold-old",
            "completed-recent",
            "discarded-recent",
            "discarded-unrecorded-old",
            "executing-old",
            "retryable-old",
            "scheduled-old",
        ]
    );
    println!("PASS 30 days: the old completed job, the old cancelled job of a worker without a hook and the old told job are deleted; recent, unfinished, untold and unrecorded jobs stay");
    assert_eq!(queue.prune(client, days, now).await?, 0);
    println!("PASS a second pass deletes nothing");

    // More than one statement's batch, so the pass has to repeat.
    client.execute("INSERT INTO base_queue_jobs(tenant_id,worker_id,worker_module,args,actor_kind,actor_id,trace_id,max_attempts,state,updated_at) SELECT 1,$1,'base/queue','{}'::jsonb,'user',1,$2,3,'completed',clock_timestamp()-interval '40 days' FROM generate_series(1,25000)", &[&PLAIN,&TRACE]).await?;
    assert_eq!(queue.prune(client, days, now).await?, 25_000);
    assert_eq!(cases(client).await?.len(), 7);
    println!(
        "PASS 25,000 old finished jobs are deleted in batches and the kept jobs are untouched"
    );

    let indexes: Vec<String> = client
        .query(
            "SELECT indexname::text FROM pg_indexes WHERE tablename='base_queue_jobs' ORDER BY 1",
            &[],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for index in [
        "base_queue_claim",
        "base_queue_expired_leases",
        "base_queue_jobs_discarded",
        "base_queue_jobs_ending_failed",
        "base_queue_jobs_endings",
    ] {
        assert!(indexes.iter().any(|name| name == index));
    }
    println!("PASS indexes: {}", indexes.join(", "));

    client
        .execute("DELETE FROM base_queue_jobs WHERE trace_id=$1", &[&TRACE])
        .await?;
    Ok(())
}
