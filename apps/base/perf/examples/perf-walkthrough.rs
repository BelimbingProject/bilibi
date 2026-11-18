//! Walkthrough against an explicitly supplied disposable DB. It shows what
//! the recorder accepts, drops, stores and prunes, and checks lock recovery.
use bilibi_database::{pool, pool_with, timing, Db, Error, PoolLimits, Result};
use bilibi_module_registry::Services;
use bilibi_perf::{
    Config, Filter, Finish, Kind, Outcome, Recorder, Sample, Store, Submit, Windows,
};
use bilibi_settings::Settings;
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::{sync::watch, time::sleep};
use tokio_postgres::Transaction;

async fn reader_waiting(lock: &Transaction<'_>) -> Result<bool> {
    Ok(lock.query_one(
        "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype='relation' AND NOT granted \
         AND mode='AccessShareLock' AND database=(SELECT oid FROM pg_database WHERE datname=current_database()) \
         AND relation='base_perf_samples'::regclass)",
        &[],
    ).await?.get(0))
}

async fn backend_pid(db: &Db) -> Result<i32> {
    let client = db.get().await?;
    Ok(client
        .query_one("SELECT pg_backend_pid()", &[])
        .await?
        .get(0))
}

async fn blocked_read<T>(
    lock: &Transaction<'_>,
    db: &Db,
    expected_pid: i32,
    name: &str,
    read: impl Future<Output = T>,
) -> Result<T> {
    tokio::pin!(read);
    let wait = async {
        while !reader_waiting(lock).await? {
            sleep(Duration::from_millis(25)).await;
        }
        Ok::<_, Error>(())
    };
    tokio::select! {
        result = wait => result?,
        _ = &mut read => panic!("{name} finished before reaching the PostgreSQL lock"),
    }
    let result = read.await;
    assert!(
        !reader_waiting(lock).await?,
        "{name} left a PostgreSQL reader blocked"
    );
    assert_eq!(
        backend_pid(db).await?,
        expected_pid,
        "{name} discarded a connection after PostgreSQL recovered it"
    );
    Ok(result)
}

async fn flush(recorder: &Recorder) {
    for _ in 0..100 {
        if recorder.stats().pending == 0 {
            sleep(Duration::from_millis(200)).await;
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
}
async fn show(recorder: &Recorder, label: &str) -> Result<()> {
    let page = recorder
        .list_samples(&Filter {
            page_size: 100,
            ..Filter::default()
        })
        .await?;
    println!("-- {label}: {} stored row(s), newest first", page.total);
    for row in page
        .entries
        .iter()
        .filter(|row| row.sample.kind != Kind::Runtime)
        .take(8)
    {
        let sample = &row.sample;
        println!(
            "   {:<8} {:<26} {:<9} {:>4} ms  db {}x/{} ms  size {:?}",
            sample.kind.as_str(),
            sample.identity,
            sample.outcome.as_str(),
            sample.duration_ms,
            sample.db_count,
            sample.db_duration_ms,
            sample.response_size_class.map(|class| class.as_str()),
        );
    }
    Ok(())
}
async fn work(millis: u64) -> u32 {
    sleep(Duration::from_millis(millis)).await;
    42
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_PERF_DEMO").as_deref() != Ok("yes") {
        return Err(Error::Invalid);
    }
    bilibi_base::link();
    bilibi_core::link();
    let url = std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?;
    let db = pool(&url)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide(
        "bootstrap.composition",
        std::sync::Arc::new(bilibi_web::composition()),
    )?;
    services.install(&modules)?;
    let settings = services.get::<Settings>("base/settings.store")?;
    let mut client = db
        .get_detached("the walkthrough's own checking connection")
        .await?;
    let config = Config {
        policy_refresh: Duration::from_secs(1),
        ..Config::default()
    };
    let recorder = Arc::new(Recorder::new(db.clone(), settings.clone(), config)?);
    if std::env::args().nth(1).as_deref() == Some("inspect") {
        let rows = client
            .query("SELECT kind, count(*), max(observed_at) FROM base_perf_samples GROUP BY kind ORDER BY kind", &[])
            .await?;
        println!("separate-process view of the persisted history:");
        for row in rows {
            println!(
                "   {:<8} {:>5} rows, newest {:?}",
                row.get::<_, &str>(0),
                row.get::<_, i64>(1),
                row.get::<_, SystemTime>(2)
            );
        }
        return Ok(());
    }
    client
        .batch_execute(
            "TRUNCATE base_perf_samples; DELETE FROM base_settings WHERE key LIKE 'perf.%'",
        )
        .await?;
    let set = |key: &'static str, value: &'static str| {
        let client = &client;
        async move {
            client
                .execute("INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES($1,0,'global',0,$2::text::jsonb,false) ON CONFLICT(key,tenant_id,scope_type,scope_id) DO UPDATE SET value=excluded.value", &[&key, &value])
                .await
        }
    };

    println!("== 1. Observation shape and redaction (minimum duration 0 ms)");
    set("perf.minimum_duration_ms", "0").await?;
    let (stop, stopped) = watch::channel(false);
    let writer = tokio::spawn(recorder.clone().run(stopped));
    sleep(Duration::from_millis(1500)).await;
    let started = Instant::now();
    let value = recorder
        .observe(Kind::Request, Some("/companies/{id}"), work(30), |_| {
            Finish {
                outcome: Outcome::Ok,
                response_bytes: Some(12_000),
            }
        })
        .await;
    println!(
        "observed work returned {value} after {} ms",
        started.elapsed().as_millis()
    );
    let value = recorder
        .observe(
            Kind::Request,
            Some("/companies/7?token=secret"),
            work(5),
            |_| Finish::outcome(Outcome::Ok),
        )
        .await;
    println!("work with a query-string identity ran unobserved and returned {value}");
    let value = recorder
        .observe(
            Kind::Job,
            Some("base/walkthrough-job"),
            async {
                let _ = timing::timed(client.query_one("SELECT 1", &[])).await;
                let _ = timing::timed(client.query_one("SELECT pg_sleep(0.02)", &[])).await;
                work(5).await
            },
            |_| Finish::outcome(Outcome::Error),
        )
        .await;
    println!("job work with two timed queries returned {value}");
    let abandoned = tokio::time::timeout(
        Duration::from_millis(10),
        recorder.observe(Kind::Event, Some("event:/companies"), work(2_000), |_| {
            Finish::outcome(Outcome::Ok)
        }),
    )
    .await;
    println!(
        "abandoned work (client went away): finished={}",
        abandoned.is_ok()
    );
    flush(&recorder).await;
    show(&recorder, "after four observations").await?;

    println!(
        "\n== 2. Settings policy (reads happen in the background, never on the business path)"
    );
    set("perf.minimum_duration_ms", "100").await?;
    sleep(Duration::from_millis(1500)).await;
    recorder
        .observe(Kind::Request, Some("/fast"), work(20), |_| {
            Finish::outcome(Outcome::Ok)
        })
        .await;
    recorder
        .observe(Kind::Request, Some("/slow"), work(150), |_| {
            Finish::outcome(Outcome::Ok)
        })
        .await;
    flush(&recorder).await;
    show(&recorder, "minimum 100 ms: only /slow should be new").await?;
    set("perf.enabled", "false").await?;
    sleep(Duration::from_millis(1500)).await;
    let submit = recorder.submit(Sample::runtime(Default::default()));
    println!("recording disabled: submit says {submit:?}");
    set("perf.enabled", "true").await?;
    sleep(Duration::from_millis(1500)).await;

    println!("\n== 3. Process facts replace BEAM facts");
    println!("{:#?}", recorder.process().await);

    println!("\n== 4. A full queue drops samples and never blocks the caller");
    let saturated = Recorder::new(
        db.clone(),
        settings.clone(),
        Config {
            max_pending: 5,
            ..Config::default()
        },
    )?;
    let started = Instant::now();
    let outcomes: Vec<_> = (0..20)
        .map(|i| {
            let mut sample = Sample::runtime(Default::default());
            sample.duration_ms = i;
            saturated.submit(sample)
        })
        .collect();
    let accepted = outcomes.iter().filter(|o| **o == Submit::Accepted).count();
    let dropped = outcomes.iter().filter(|o| **o == Submit::Dropped).count();
    println!(
        "20 submits with no writer and room for 5: accepted={accepted} dropped={dropped} in {} us",
        started.elapsed().as_micros()
    );
    println!("stats: {:?}", saturated.stats());

    println!("\n== 5. An unreachable store cannot slow business work");
    let dead = pool("postgres://bilibi:bilibi@10.255.255.1:5432/bilibi?connect_timeout=1")?;
    let broken = Arc::new(Recorder::new(
        dead,
        settings.clone(),
        Config {
            write_timeout: Duration::from_millis(500),
            ..Config::default()
        },
    )?);
    let (broken_stop, broken_stopped) = watch::channel(false);
    let broken_writer = tokio::spawn(broken.clone().run(broken_stopped));
    // 105 ms clears the default 100 ms minimum, so every call yields a sample
    // that the writer then fails to store. Without a store, settings cannot be
    // read either, and the recorder keeps its defaults.
    let started = Instant::now();
    let mut slowest = 0;
    for _ in 0..10 {
        let call = Instant::now();
        broken
            .observe(Kind::Request, Some("/walkthrough"), work(105), |_| {
                Finish::outcome(Outcome::Ok)
            })
            .await;
        slowest = slowest.max(call.elapsed().as_millis());
    }
    println!(
        "10 observed 105 ms operations took {} ms in total (1050 ms of work), slowest {slowest} ms",
        started.elapsed().as_millis()
    );
    sleep(Duration::from_millis(2500)).await;
    println!(
        "broken recorder stats after its writes failed: {:?}",
        broken.stats()
    );
    println!("broken recorder health: {}", broken.health_status().await);
    let _ = broken_stop.send(true);
    broken_writer.await.map_err(|_| Error::Internal)??;
    println!(
        "healthy recorder health: {}",
        recorder.health_status().await
    );

    println!("\n== 6. Regressions between two windows");
    client.batch_execute(
        "INSERT INTO base_perf_samples(kind,identity,outcome,duration_ms,observed_at) \
         SELECT 'request','/companies/{id}','ok',50,now()-interval '2 hours'+n*interval '1 second' FROM generate_series(1,30) n; \
         INSERT INTO base_perf_samples(kind,identity,outcome,duration_ms,observed_at) \
         SELECT 'request','/companies/{id}','ok',2500,now()-interval '30 minutes'+n*interval '1 second' FROM generate_series(1,30) n; \
         INSERT INTO base_perf_samples(kind,identity,outcome,duration_ms,observed_at) \
         SELECT 'request','/steady','ok',40,now()-interval '2 hours'+n*interval '1 second' FROM generate_series(1,30) n; \
         INSERT INTO base_perf_samples(kind,identity,outcome,duration_ms,observed_at) \
         SELECT 'request','/steady','ok',40,now()-interval '30 minutes'+n*interval '1 second' FROM generate_series(1,30) n").await?;
    let now = SystemTime::now();
    let ago = |minutes: u64| now - Duration::from_secs(minutes * 60);
    let windows = Windows {
        baseline_from: ago(180),
        baseline_to: ago(60),
        current_from: ago(60),
        current_to: now + Duration::from_secs(60),
        min_samples: 20,
        limit: 10,
    };
    for row in recorder.regressions(&windows).await? {
        println!(
            "   {} {}: {:.0} ms mean -> p95 {:.0} ms ({:+.1}%)",
            row.kind.as_str(),
            row.identity,
            row.baseline_average_ms,
            row.current_p95_ms,
            row.delta_percent
        );
    }

    println!("\n== 7. Retention by age and by row cap");
    client.batch_execute(
        "INSERT INTO base_perf_samples(kind,identity,outcome,duration_ms,observed_at) \
         SELECT 'request','/old','ok',10,now()-interval '45 days'+n*interval '1 second' FROM generate_series(1,40) n; \
         INSERT INTO base_perf_samples(kind,identity,outcome,duration_ms,observed_at) \
         SELECT 'request','/bulk','ok',10,now()-interval '1 day'+n*interval '1 millisecond' FROM generate_series(1,1100) n").await?;
    set("perf.history.max_rows", "1000").await?;
    let before: i64 = client
        .query_one("SELECT count(*) FROM base_perf_samples", &[])
        .await?
        .get(0);
    let removed = recorder.prune().await?;
    let after: i64 = client
        .query_one("SELECT count(*) FROM base_perf_samples", &[])
        .await?
        .get(0);
    println!("rows before={before} removed={removed} after={after} (cap 1000, keep 30 days)");
    let old: i64 = client
        .query_one(
            "SELECT count(*) FROM base_perf_samples WHERE identity='/old'",
            &[],
        )
        .await?
        .get(0);
    println!("rows older than the cutoff left: {old}");
    println!(
        "second prune is idempotent: removed={}",
        recorder.prune().await?
    );

    let _ = stop.send(true);
    writer.await.map_err(|_| Error::Internal)??;

    println!("\n== 8. PostgreSQL stops every read while the history lock is held");
    let read_pool = pool_with(
        &url,
        PoolLimits {
            size: 1,
            wait: Duration::from_secs(1),
        },
    )?;
    let recorder = Recorder::new(read_pool.clone(), settings, Config::default())?;
    let pid = backend_pid(&read_pool).await?;
    let filter = Filter::default();
    let expected_total = recorder.list_samples(&filter).await?.total;
    let lock = client.transaction().await?;
    lock.batch_execute("LOCK TABLE base_perf_samples IN ACCESS EXCLUSIVE MODE")
        .await?;
    let diagnostics = blocked_read(
        &lock,
        &read_pool,
        pid,
        "diagnostics",
        recorder.diagnostics(),
    )
    .await?;
    assert_eq!(diagnostics.store, Store::Unavailable);
    assert!(blocked_read(
        &lock,
        &read_pool,
        pid,
        "sample list",
        recorder.list_samples(&filter)
    )
    .await?
    .is_err());
    assert!(blocked_read(
        &lock,
        &read_pool,
        pid,
        "regressions",
        recorder.regressions(&windows)
    )
    .await?
    .is_err());
    assert!(blocked_read(
        &lock,
        &read_pool,
        pid,
        "slowest",
        recorder.slowest(ago(180), now, 10)
    )
    .await?
    .is_err());
    lock.rollback().await?;
    assert_eq!(recorder.list_samples(&filter).await?.total, expected_total);
    println!("PASS Performance reads stopped and reused the recovered PostgreSQL connection");

    println!("\nRun again with `-- inspect` in another process to read the persisted rows.");
    Ok(())
}
