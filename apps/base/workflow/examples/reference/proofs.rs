//! Checks that exit 1 when a delivery, maintenance or bell bound is broken.
//! Each runs real transactions on the disposable specimen database after
//! `init`; `proof-bell` also needs `approval-init`.
use bilibi_database::{Db, Error, MutationBoundary, Result};
use bilibi_module_registry::Services;
use bilibi_tenancy::{Authentication, Scope};
use bilibi_workflow::{ClaimOptions, Engine, Request, StartOptions};
use std::time::{Duration, Instant};

fn check(holds: bool, what: &str) -> Result<()> {
    println!("{} {what}", if holds { "ok  " } else { "FAIL" });
    if holds {
        Ok(())
    } else {
        Err(Error::Internal)
    }
}
async fn count(db: &Db, sql: &str) -> Result<i64> {
    Ok(db.get().await?.query_one(sql, &[]).await?.get(0))
}

/// The immediate delivery job must not complete over an undelivered event.
/// Record `id` is approved, its event is left leased as by an attempt whose
/// process died, and the real Queue executor runs the job: it has to stay
/// retryable while the lease lasts and deliver once the lease has run out.
pub async fn delivery(
    db: &Db,
    services: &Services,
    engine: &Engine,
    scope: &Scope,
    id: i64,
) -> Result<()> {
    let client = db.get().await?;
    client
        .execute(
            "UPDATE workflow_reference_listener_gate SET fail_second=false,wait_seconds=0 WHERE singleton",
            &[],
        )
        .await?;
    let request = Request {
        expected_version: Some(0),
        ..Request::default()
    };
    let history = engine
        .transition(scope, "ReferenceRecord", id, "approved", request)
        .await?
        .id;
    client.execute("UPDATE base_workflow_transition_outbox SET attempts=attempts+1,lease_token='dead-attempt',lease_expires_at=clock_timestamp()+interval '5 minutes' WHERE history_id=$1", &[&history]).await?;
    let job: i64 = client.query_one("SELECT id FROM base_queue_jobs WHERE worker_id='base/workflow-transition-delivery' AND (args->>'history_id')::bigint=$1", &[&history]).await?.get(0);
    let config = bilibi_queue::Config {
        concurrency: 2,
        poll_interval: Duration::from_millis(100),
        lease_seconds: 30,
        max_runtime: Duration::from_secs(60),
    };
    let executor = bilibi_queue::Executor::from_services(services, config)?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let running = tokio::spawn(executor.run(stopped));
    let reader = &client;
    let wait_for = move |wanted: &'static str, seconds: u64| async move {
        let started = Instant::now();
        loop {
            let row = reader
                .query_one(
                    "SELECT state,failure_code FROM base_queue_jobs WHERE id=$1",
                    &[&job],
                )
                .await?;
            let (state, code): (String, Option<String>) = (row.get(0), row.get(1));
            if state == wanted
                || state == "completed"
                || started.elapsed() > Duration::from_secs(seconds)
            {
                return Ok::<_, Error>((state, code));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let delivered =
        "SELECT delivered_at IS NOT NULL FROM base_workflow_transition_outbox WHERE history_id=$1";
    let leased = async {
        let (state, code) = wait_for("retryable", 30).await?;
        check(
            state == "retryable" && code.as_deref() == Some("transition_delivery_deferred"),
            &format!("job {job} over a leased event: state={state} code={code:?}"),
        )?;
        let done: bool = client.query_one(delivered, &[&history]).await?.get(0);
        check(!done, "the leased event is still undelivered")?;
        // The dead attempt's lease runs out; the retried job now delivers.
        client.execute("UPDATE base_workflow_transition_outbox SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE history_id=$1", &[&history]).await?;
        let (state, code) = wait_for("completed", 60).await?;
        check(
            state == "completed",
            &format!("job {job} after the lease ran out: state={state} code={code:?}"),
        )?;
        let done: bool = client.query_one(delivered, &[&history]).await?.get(0);
        check(done, "the event is delivered")
    }
    .await;
    let _ = stop.send(true);
    running.await.map_err(|_| Error::Internal)??;
    leased
}

/// A maintenance pass leaves no Audit row for a run it did not change,
/// records a repair, and visits a bounded number of running runs, continuing
/// where the previous pass stopped.
pub async fn maintenance(db: &Db, engine: &Engine, scope: &Scope) -> Result<()> {
    const AUDITED: &str =
        "SELECT count(*) FROM base_audit_mutations WHERE auditable_type='workflow.process_maintenance'";
    const RUNNING: &str =
        "SELECT id FROM base_workflow_process_runs WHERE status='running' ORDER BY id";
    let mut started = Vec::new();
    for record in [18_i64, 19, 20] {
        let run = engine
            .start_run(
                scope,
                "reference.worker",
                "ReferenceRecord",
                record,
                StartOptions::new(format!("proof:maintenance:{record}")),
            )
            .await?;
        started.push(run.id);
    }
    let before = count(db, AUDITED).await?;
    for _ in 0..2 {
        let summary = engine.maintain_workflows(scope, 100, 30).await?;
        check(
            summary.reconciled >= started.len(),
            &format!(
                "a pass visited the running runs: reconciled={} skipped={}",
                summary.reconciled, summary.skipped
            ),
        )?;
    }
    let idle = count(db, AUDITED).await?;
    check(
        idle == before,
        &format!(
            "two passes over unchanged runs wrote {} audit rows",
            idle - before
        ),
    )?;
    // A worker claims a step and dies: maintenance requeues it and says so.
    let claimed = engine
        .claim_work(
            scope,
            "proof-worker",
            ClaimOptions {
                lease_seconds: 1,
                definition_key: Some("reference.worker".into()),
                run_ids: Some(vec![started[0]]),
                executor_keys: None,
            },
        )
        .await?
        .ok_or(Error::NotFound)?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    engine.maintain_workflows(scope, 100, 30).await?;
    let repaired = count(db, AUDITED).await?;
    check(
        repaired == idle + 1,
        &format!(
            "a pass that requeued one expired lease wrote {} audit row",
            repaired - idle
        ),
    )?;
    let status: String = db
        .get()
        .await?
        .query_one(
            "SELECT status FROM base_workflow_process_work_items WHERE id=$1",
            &[&claimed.work_item.id],
        )
        .await?
        .get(0);
    check(
        status != "leased",
        &format!("the expired step is {status} again"),
    )?;
    // One run per pass: each pass continues after the last one and the lap
    // starts again from the first running run.
    let client = db.get().await?;
    let running: Vec<i64> = client
        .query(RUNNING, &[])
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    client
        .execute(
            "UPDATE base_workflow_process_maintenance SET last_run=0",
            &[],
        )
        .await?;
    let mut visited = Vec::new();
    for _ in 0..=running.len() {
        let summary = engine.maintain_workflows(scope, 1, 30).await?;
        check(
            summary.reconciled + summary.skipped == 1,
            &format!(
                "a pass bounded to one run visited {}",
                summary.reconciled + summary.skipped
            ),
        )?;
        let place: i64 = client
            .query_one(
                "SELECT last_run FROM base_workflow_process_maintenance",
                &[],
            )
            .await?
            .get(0);
        visited.push(place);
    }
    let mut lap = running.clone();
    lap.push(running[0]);
    check(
        visited == lap,
        &format!("passes visited {visited:?} of running runs {running:?}"),
    )
}

/// One bell render reads one bounded page of approvals: with twelve waiting
/// for this reader it counts ten, says there are more and lists five.
pub async fn bell(db: &Db, services: &Services, engine: &Engine, scope: &Scope) -> Result<()> {
    let indexes = count(db, "SELECT count(*) FROM pg_indexes WHERE indexname IN ('base_workflow_approval_requests_run','base_workflow_approval_events_request','base_workflow_approval_delegations_delegate','base_workflow_process_runs_running')").await?;
    check(indexes == 4, &format!("{indexes} of 4 indexes exist"))?;
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    let boundary = services.get::<MutationBoundary>("base/database.mutations")?;
    let mut client = db.get().await?;
    // The fixture's manager prepared every reference record (`approval-init`).
    let preparer = authentication
        .authenticate_session(&**client, scope.tenant(), 1, None, None, String::new())
        .await?;
    for record in 3_i64..=14 {
        let mut tx = boundary
            .begin(
                &mut client,
                preparer.mutation_context("reference.approval.submitted"),
            )
            .await?;
        engine
            .submit_approval_in(
                &mut tx,
                &preparer,
                super::APPROVAL_CASE.key,
                record,
                &format!("reference:{record}:1"),
                None,
            )
            .await?;
        tx.commit().await?;
    }
    drop(client);
    let waiting = engine
        .my_approvals(
            scope,
            &bilibi_workflow::ApprovalQuery {
                limit: 100,
                ..Default::default()
            },
        )
        .await?
        .entries
        .len();
    check(
        waiting == 12,
        &format!("{waiting} approvals wait for this reader"),
    )?;
    let notifications =
        services.get::<bilibi_notifications::NotificationBoundary>("base/notifications.sink")?;
    let lists = notifications.todos(scope).await?;
    let list = lists
        .iter()
        .find(|list| list.see_all.as_deref() == Some("/workflow/approvals"))
        .ok_or(Error::NotFound)?;
    check(
        list.count == 10 && list.more && list.items.len() == 5,
        &format!(
            "the bell counts {}{} and lists {}",
            list.count,
            if list.more { "+" } else { "" },
            list.items.len()
        ),
    )
}
