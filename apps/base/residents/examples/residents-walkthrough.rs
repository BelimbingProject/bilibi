//! Running proof for resident readers, not a unit-test harness. With no
//! argument it migrates a disposable database, starts private server
//! processes of itself (`worker LABEL CAPACITY`) and checks one owner per key,
//! takeover after a kill, fenced writes, restart with backoff, release,
//! withdrawn authority and a clean stop. A failed check exits non-zero.
//! `serve` runs the whole app with the specimen reader on a database the
//! launcher prepared, for a person to look at the Resident readers page.
//! The specimen module and its reader are linked only into this example.
use bilibi_authz::{
    grants::Grants, CapabilityDefinition, SystemPrincipalDefinition, VocabularyContribution,
    VOCABULARY,
};
use bilibi_base::step_up::StepUp;
use bilibi_database::{pool, Change, Db, Error, MutationBoundary, Result};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance, Service, Services};
use bilibi_residents::{
    Config, Ended, Filter, ReadFuture, Reader, ReaderService, Reading, Resident, Residents,
    Supervisor, READERS, SERVICE,
};
use bilibi_tenancy::{Authentication, Scope};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::Instant,
};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();

/// A reference module, as a Domain would declare its reader: the module, its
/// system principal, the reader and the service that implements it.
const MODULE: &str = "extensions/residents_walkthrough";
const READER: &str = "residents_walkthrough/line";
const SPECIMEN: &str = "extensions/residents_walkthrough.reader";
/// The specimen's named system principal. Declaration grants nothing: an
/// administrator gives it the capability, tenant by tenant.
const PRINCIPAL: &str = "residents-specimen";
const CAPABILITY: &str = "residents.specimen.read";
/// Seconds a private worker's lease lasts; a killed owner is taken over
/// after at most this long, a cleanly stopped one at once.
const LEASE: u64 = 6;
macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! { Contribution::new(VOCABULARY, "residents.walkthrough", provenance!(), &VocabularyContribution {
    domains: &["residents"], verbs: &["read"],
    capabilities: &[CapabilityDefinition::tenant(CAPABILITY)],
    roles: &[],
    system_principals: &[SystemPrincipalDefinition { name: PRINCIPAL, capabilities: &[CAPABILITY] }],
}) }
inventory::submit! { Contribution::new(READERS, READER, provenance!(), &Reader {
    service: SPECIMEN, principal: PRINCIPAL, capability: CAPABILITY,
}) }
inventory::submit! {
    Module {
        id: MODULE, layer: Layer::Extensions,
        dependencies: &["base/database", "base/authz", "base/residents"],
        migrations: &[],
        services: &[Service { id: SPECIMEN, factory: |services| {
            Ok(Arc::new(ReaderService(Arc::new(Specimen {
                db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
            }))))
        } }],
    }
}
/// The label of the process the specimen runs in, as its supervisor writes it.
static OWNER: OnceLock<String> = OnceLock::new();
fn owner() -> &'static str {
    OWNER.get().map_or("unlabelled", String::as_str)
}

/// Stands in for a connection: every 200 ms it "handles an event" by writing
/// one fenced, audited row naming the process it runs in.
struct Specimen {
    db: Db,
}
impl Specimen {
    /// A diagnostic line of the walkthrough's own, outside any lease.
    async fn note(&self, reading: &Reading, what: &str) {
        if let Ok(client) = self.db.get().await {
            let _ = client
                .execute(
                    "INSERT INTO resident_walkthrough_events(tenant_id,key,owner,what) VALUES($1,$2,$3,$4)",
                    &[&reading.tenant(), &reading.key(), &owner(), &what],
                )
                .await;
        }
    }
    async fn starts(&self, reading: &Reading) -> i64 {
        let Ok(client) = self.db.get().await else {
            return 0;
        };
        client
            .query_one(
                "SELECT count(*) FROM resident_walkthrough_events WHERE tenant_id=$1 AND key=$2 AND what='started'",
                &[&reading.tenant(), &reading.key()],
            )
            .await
            .map_or(0, |row| row.get(0))
    }
    /// One handled event: the reading's fence, then the owner's write.
    async fn beat(&self, reading: &Reading) -> Result<()> {
        let mut client = self.db.get().await?;
        let mut tx = reading
            .begin(&mut client, "residents.specimen.beat")
            .await?;
        let (tenant, key, owner) = (reading.tenant(), reading.key().to_owned(), owner());
        tx.mutate(move |sql| Box::pin(async move {
            let id: i64 = sql.query_one("INSERT INTO resident_walkthrough_beats(tenant_id,key,owner) VALUES($1,$2,$3) RETURNING id", &[&tenant, &key, &owner]).await?.get(0);
            Ok(((), vec![Change { kind: "residents.specimen.beat".into(), id, old: Value::Null, new: json!({"key": key, "owner": owner}) }]))
        })).await?;
        tx.commit().await
    }
}
impl Resident for Specimen {
    fn read<'a>(&'a self, reading: &'a Reading) -> ReadFuture<'a> {
        Box::pin(async move {
            let starts = self.starts(reading).await;
            self.note(reading, "started").await;
            if reading.scope().principal() != Some(PRINCIPAL) || reading.scope().user() != 0 {
                return Ended::Failed("specimen_scope_wrong");
            }
            match reading.key() {
                "flaky-1" if starts == 0 => return Ended::Failed("specimen_failed"),
                "flaky-1" if starts == 1 => panic!("private specimen panic"),
                "failing-1" => return Ended::Failed("specimen_failed"),
                "closing-1" => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    return Ended::Closed;
                }
                _ => {}
            }
            let mut refused = false;
            loop {
                tokio::select! {
                    _ = reading.cancelled() => {
                        self.note(reading, "cancelled").await;
                        return Ended::Closed;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
                match self.beat(reading).await {
                    Ok(()) => {}
                    // The lease is no longer this reading's: nothing was
                    // written. Wait to be cancelled.
                    Err(Error::Conflict) if !refused => {
                        refused = true;
                        self.note(reading, "refused_write").await;
                    }
                    Err(Error::Conflict | Error::Unauthorized | Error::Forbidden) => {}
                    Err(_) => return Ended::Failed("specimen_unavailable"),
                }
            }
        })
    }
}

fn check(passed: bool, what: &str) -> Result<()> {
    if passed {
        println!("ok: {what}");
        Ok(())
    } else {
        println!("FAILED: {what}");
        Err(Error::Internal)
    }
}
/// Polls `sql` (one boolean) until it is true, or fails the walkthrough with
/// the readings as they stand.
async fn until(db: &Db, seconds: u64, what: &str, sql: &str) -> Result<Duration> {
    let started = Instant::now();
    loop {
        let met: bool = db.get().await?.query_one(sql, &[]).await?.get(0);
        if met {
            println!("ok: {what} ({} ms)", started.elapsed().as_millis());
            return Ok(started.elapsed());
        }
        if started.elapsed() > Duration::from_secs(seconds) {
            println!("FAILED: {what} (waited {seconds} s)");
            show(db).await?;
            return Err(Error::Internal);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn show(db: &Db) -> Result<()> {
    for row in db.get().await?.query("SELECT tenant_id,key,state,owner,starts,failures,last_error,lease_token IS NOT NULL FROM base_resident_readings ORDER BY tenant_id,key", &[]).await? {
        println!(
            "   tenant={} key={} state={} owner={:?} starts={} failures={} last_error={:?} leased={}",
            row.get::<_, i64>(0),
            row.get::<_, String>(1),
            row.get::<_, String>(2),
            row.get::<_, Option<String>>(3),
            row.get::<_, i64>(4),
            row.get::<_, i32>(5),
            row.get::<_, Option<String>>(6),
            row.get::<_, bool>(7)
        );
    }
    Ok(())
}
async fn truth(db: &Db, sql: &str) -> Result<bool> {
    Ok(db.get().await?.query_one(sql, &[]).await?.get(0))
}
/// Starts a private server process of this example. Closing its input stops
/// it cleanly; `kill` ends it the hard way. Only processes started here are
/// ever stopped, by the handle that started them.
fn worker(label: &str, capacity: usize) -> Result<Child> {
    let child = Command::new(std::env::current_exe().map_err(|_| Error::Internal)?)
        .args(["worker", label, &capacity.to_string()])
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::Internal)?;
    println!(
        "started worker {label} pid={:?} capacity={capacity}",
        child.id()
    );
    Ok(child)
}
async fn stop(mut child: Child, label: &str) -> Result<()> {
    drop(child.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .map_err(|_| Error::Internal)?
        .map_err(|_| Error::Internal)?;
    check(status.success(), &format!("worker {label} stopped cleanly"))
}

/// The walkthrough's own evidence, beside the Platform's tables.
const TABLES: &str = "CREATE TABLE IF NOT EXISTS resident_walkthrough_beats(id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, tenant_id bigint NOT NULL, key text NOT NULL, owner text NOT NULL, at timestamptz NOT NULL DEFAULT clock_timestamp()); CREATE TABLE IF NOT EXISTS resident_walkthrough_events(id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, tenant_id bigint NOT NULL, key text NOT NULL, owner text NOT NULL, what text NOT NULL, at timestamptz NOT NULL DEFAULT clock_timestamp())";

/// The whole app with the specimen reader, on a database `./bilibi start`
/// migrated and seeded, at `APP_ORIGIN` and `BIND_ADDR`: two readings run,
/// one waits to retry and one waits for a free place (this process holds two).
async fn serve(db: Db) -> Result<()> {
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let bind = std::env::var("BIND_ADDR").map_err(|_| Error::Invalid)?;
    db.get().await?.batch_execute(TABLES).await?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    let services = app.services.clone();
    let supervisor = Supervisor::from_services(
        &services,
        Config {
            capacity: 2,
            ..Config::default()
        },
    )?;
    let _ = OWNER.set(supervisor.owner().to_owned());
    // The launcher's administrator of the platform operator's tenant grants
    // the principal and keeps the keys, as a Domain's own command would.
    let admin = {
        let client = db.get().await?;
        let row = client.query_one("SELECT u.tenant_id,u.id FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE i.email='ai@agent.my'", &[]).await?;
        services
            .get::<Authentication>("base/tenancy.authentication")?
            .authenticate(&**client, row.get(0), row.get(1), String::new())
            .await?
    };
    services
        .get::<Grants>("base/authz.grants")?
        .set_system(&admin, PRINCIPAL, CAPABILITY, Some(true))
        .await?;
    let residents = services.get::<Residents>(SERVICE)?;
    let boundary = services.get::<MutationBoundary>("base/database.mutations")?;
    for key in ["failing-1", "steady-1", "steady-2", "steady-3"] {
        let mut client = db.get().await?;
        let mut tx = boundary
            .begin(
                &mut client,
                admin.mutation_context("residents.specimen.keep"),
            )
            .await?;
        residents.keep(&mut tx, &admin, READER, key).await?;
        tx.commit().await?;
    }
    // The failing reading has already failed often: its next wait is the longest.
    db.get()
        .await?
        .execute(
            "UPDATE base_resident_readings SET failures=8 WHERE key='failing-1' AND failures<8",
            &[],
        )
        .await?;
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|_| Error::Internal)?;
    let (halt, halted) = tokio::sync::watch::channel(false);
    let readers = tokio::spawn(supervisor.run(halted));
    println!(
        "Residents walkthrough serving at {origin}/system/readers pid={}",
        std::process::id()
    );
    let served = axum::serve(
        listener,
        bilibi_web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|_| Error::Internal);
    let _ = halt.send(true);
    readers.await.map_err(|_| Error::Internal)??;
    served
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_RESIDENTS_DEMO").as_deref() != Ok("yes") {
        eprintln!(
            "Set BILIBI_DISPOSABLE_RESIDENTS_DEMO=yes and DATABASE_URL to a disposable database."
        );
        return Err(Error::Invalid);
    }
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map_or("", String::as_str);
    if mode == "serve" {
        return serve(db).await;
    }
    let mut services = Services::default();
    let modules = bilibi_module_registry::discover()?;
    if mode != "worker" {
        bilibi_database::migrate(&db, &modules).await?;
        bilibi_walkthrough_fixture::install(&db).await?;
        db.get().await?.batch_execute(TABLES).await?;
    }
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    // This fixture binary enrols no authenticator (README of base/step_up).
    services
        .get::<StepUp>(bilibi_base::step_up::SERVICE)?
        .set_enforced(false);
    if mode == "worker" {
        let label = args.get(1).ok_or(Error::Invalid)?.clone();
        let capacity = args
            .get(2)
            .and_then(|capacity| capacity.parse().ok())
            .ok_or(Error::Invalid)?;
        let supervisor = Supervisor::from_services(
            &services,
            Config {
                capacity,
                poll_interval: Duration::from_millis(100),
                lease_seconds: LEASE as i32,
                grace: Duration::from_secs(1),
                restart_delay: Duration::from_millis(200),
                max_backoff: Duration::from_secs(2),
                healthy_after: Duration::from_secs(60),
                reproof: Duration::from_secs(1),
                owner: Some(label),
            },
        )?;
        let _ = OWNER.set(supervisor.owner().to_owned());
        let (halt, halted) = tokio::sync::watch::channel(false);
        let running = tokio::spawn(supervisor.run(halted));
        // Runs until its input closes (the walkthrough's clean stop) or Ctrl-C.
        let mut line = String::new();
        let mut input = BufReader::new(tokio::io::stdin());
        tokio::select! {
            _ = input.read_line(&mut line) => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        let _ = halt.send(true);
        let ended = running.await.map_err(|_| Error::Internal)?;
        // A read of the terminal still pending after Ctrl-C must not hold the exit.
        std::process::exit(i32::from(ended.is_err()));
    }
    let residents = services.get::<Residents>(SERVICE)?;
    let boundary = services.get::<MutationBoundary>("base/database.mutations")?;
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    let grants = services.get::<Grants>("base/authz.grants")?;
    // The fixture's manager (tenant 1) keeps and releases keys; its grant-all
    // administrator grants the principal and is the platform operator;
    // tenant 2's manager stands for another tenant.
    let (manager, admin, other) = {
        let client = db.get().await?;
        (
            authentication
                .authenticate(&**client, 1, 1, String::new())
                .await?,
            authentication
                .authenticate(&**client, 1, 4, String::new())
                .await?,
            authentication
                .authenticate(&**client, 2, 5, String::new())
                .await?,
        )
    };
    let keep = |scope: &Scope, key: &'static str| {
        let (db, boundary, residents, scope) = (
            db.clone(),
            boundary.clone(),
            residents.clone(),
            scope.clone(),
        );
        async move {
            let mut client = db.get().await?;
            let mut tx = boundary
                .begin(
                    &mut client,
                    scope.mutation_context("residents.specimen.keep"),
                )
                .await?;
            let added = residents.keep(&mut tx, &scope, READER, key).await?;
            tx.commit().await?;
            Ok::<bool, Error>(added)
        }
    };

    println!("== The facade: keep is idempotent, bounded and transactional");
    {
        let mut client = db.get().await?;
        let mut tx = boundary
            .begin(
                &mut client,
                manager.mutation_context("residents.specimen.keep"),
            )
            .await?;
        check(
            matches!(
                residents.keep(&mut tx, &manager, READER, "Not A Key").await,
                Err(Error::Invalid)
            ),
            "a key that is not a reference is refused",
        )?;
        check(
            matches!(
                residents
                    .keep(&mut tx, &manager, "residents/unknown", "steady-1")
                    .await,
                Err(Error::Invalid)
            ),
            "an undeclared reader is refused",
        )?;
        check(
            matches!(
                residents.keep(&mut tx, &other, READER, "steady-1").await,
                Err(Error::Forbidden)
            ),
            "a scope of another tenant than the transaction's is refused",
        )?;
        check(
            residents
                .keep(&mut tx, &manager, READER, "rolled-back")
                .await?,
            "a key is kept inside the owner's transaction",
        )?;
        tx.rollback().await?;
    }
    check(
        truth(&db, "SELECT count(*)=0 FROM base_resident_readings").await?,
        "the owner's rollback leaves no reading",
    )?;

    println!("== A refused principal: no grant, no reading");
    let a = worker("A", 2)?;
    check(keep(&manager, "steady-1").await?, "steady-1 kept")?;
    check(
        !keep(&manager, "steady-1").await?,
        "keeping steady-1 again changes nothing",
    )?;
    check(keep(&manager, "steady-2").await?, "steady-2 kept")?;
    until(&db, 20, "both readings were refused with actor_refused and wait to retry", "SELECT count(*)=2 FROM base_resident_readings WHERE last_error='actor_refused' AND failures>0").await?;
    check(
        truth(&db, "SELECT count(*)=0 FROM resident_walkthrough_events").await?,
        "the refused reader never ran",
    )?;
    grants
        .set_system(&admin, PRINCIPAL, CAPABILITY, Some(true))
        .await?;
    println!("granted {PRINCIPAL}: {CAPABILITY} in tenant 1");
    until(&db, 20, "after the grant both readings run in worker A", "SELECT count(*)=2 FROM base_resident_readings WHERE state='running' AND owner='A' AND lease_expires_at>clock_timestamp()").await?;

    println!("== One owner per key across two processes");
    let b = worker("B", 8)?;
    keep(&manager, "steady-3").await?;
    keep(&manager, "steady-4").await?;
    until(&db, 20, "worker A is full, so worker B owns steady-3 and steady-4", "SELECT count(*)=2 FROM base_resident_readings WHERE state='running' AND owner='B' AND key IN ('steady-3','steady-4')").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    check(
        truth(&db, "SELECT count(*)=4 AND bool_and(owners=1 AND beats>=3) FROM (SELECT key, count(DISTINCT owner) AS owners, count(*) AS beats FROM resident_walkthrough_beats WHERE at>clock_timestamp()-interval '1500 milliseconds' GROUP BY key) recent").await?,
        "in the last 1.5 s every key was written by exactly one process",
    )?;
    check(
        truth(&db, "SELECT bool_and(r.owner=w.owner) FROM base_resident_readings r JOIN (SELECT DISTINCT key, owner FROM resident_walkthrough_beats WHERE at>clock_timestamp()-interval '1500 milliseconds') w USING (key)").await?,
        "and that process is the lease's owner",
    )?;
    check(
        truth(&db, "SELECT count(*)>0 AND bool_and(actor_type='system' AND system_principal='residents-specimen') FROM base_audit_mutations WHERE auditable_type='residents.specimen.beat'").await?,
        "the reader's writes are audited as System · residents-specimen",
    )?;

    println!("== Fenced write: a reading whose lease moved on writes nothing and is cancelled");
    db.get().await?.execute("UPDATE base_resident_readings SET lease_token=gen_random_uuid()::text WHERE key='steady-3'", &[]).await?;
    until(&db, 20, "Reading::begin refused the old owner's write", "SELECT count(*)=1 FROM resident_walkthrough_events WHERE key='steady-3' AND what='refused_write'").await?;
    until(&db, 20, "the heartbeat cancelled its reader", "SELECT count(*)=1 FROM resident_walkthrough_events WHERE key='steady-3' AND what='cancelled'").await?;
    until(&db, 20, "the unowned lease ran out and steady-3 was started again", "SELECT count(*)=1 FROM base_resident_readings WHERE key='steady-3' AND state='running' AND starts=2 AND last_error='owner_lost' AND lease_expires_at>clock_timestamp()").await?;

    println!("== Takeover: the owner is killed");
    let mut a = a;
    a.kill().await.map_err(|_| Error::Internal)?;
    println!("killed worker A");
    let took = until(&db, 30, "worker B took over steady-1 and steady-2", "SELECT count(*)=2 FROM base_resident_readings WHERE state='running' AND owner='B' AND key IN ('steady-1','steady-2') AND last_error='owner_lost' AND lease_expires_at>clock_timestamp()").await?;
    check(
        took <= Duration::from_secs(LEASE + 3),
        "within the lease and a poll",
    )?;
    until(&db, 20, "worker B writes for the keys it took over", "SELECT count(DISTINCT key)=2 FROM resident_walkthrough_beats WHERE owner='B' AND key IN ('steady-1','steady-2')").await?;
    check(
        truth(&db, "SELECT bool_and(a.last<b.first) FROM (SELECT key, max(at) AS last FROM resident_walkthrough_beats WHERE owner='A' GROUP BY key) a JOIN (SELECT key, min(at) AS first FROM resident_walkthrough_beats WHERE owner='B' GROUP BY key) b USING (key)").await?,
        "no key was written by both processes at once",
    )?;

    println!("== Failure and backoff: fail, panic, then run");
    keep(&manager, "flaky-1").await?;
    until(&db, 20, "the first run failed with the reader's code and backs off", "SELECT count(*)=1 FROM base_resident_readings WHERE key='flaky-1' AND last_error='specimen_failed' AND failures=1").await?;
    until(&db, 20, "the second run panicked and backs off longer", "SELECT count(*)=1 FROM base_resident_readings WHERE key='flaky-1' AND last_error='reader_panicked' AND failures=2").await?;
    until(&db, 20, "the third run reads", "SELECT count(*)=1 FROM base_resident_readings WHERE key='flaky-1' AND state='running' AND starts=3 AND lease_expires_at>clock_timestamp()").await?;
    check(
        truth(&db, "SELECT (max(at) FILTER (WHERE n=2) - max(at) FILTER (WHERE n=1)) >= interval '2 seconds' FROM (SELECT at, row_number() OVER (ORDER BY id) AS n FROM resident_walkthrough_events WHERE key='flaky-1' AND what='started') s").await?,
        "the second start waited out the backoff",
    )?;
    keep(&manager, "closing-1").await?;
    until(&db, 20, "a reader whose connection closes is started again without a failure", "SELECT count(*)=1 FROM base_resident_readings WHERE key='closing-1' AND starts>=3 AND failures=0 AND last_error IS NULL").await?;

    println!("== Release: the key stops being read");
    for key in ["closing-1", "steady-4"] {
        let mut client = db.get().await?;
        let mut tx = boundary
            .begin(
                &mut client,
                manager.mutation_context("residents.specimen.release"),
            )
            .await?;
        check(
            residents.release(&mut tx, &manager, READER, key).await?,
            &format!("{key} released"),
        )?;
        check(
            !residents.release(&mut tx, &manager, READER, key).await?,
            "releasing it again changes nothing",
        )?;
        tx.commit().await?;
    }
    until(&db, 20, "the released reader was cancelled", "SELECT count(*)=1 FROM resident_walkthrough_events WHERE key='steady-4' AND what='cancelled'").await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    check(
        truth(&db, "SELECT (SELECT count(*)=0 FROM base_resident_readings WHERE key IN ('steady-4','closing-1')) AND (SELECT count(*)=0 FROM resident_walkthrough_beats WHERE key='steady-4' AND at>clock_timestamp()-interval '700 milliseconds')").await?,
        "no reading is left and nothing is written for it",
    )?;
    check(
        truth(&db, "SELECT count(*) FILTER (WHERE event='created')=6 AND count(*) FILTER (WHERE event='deleted')=2 FROM base_audit_mutations WHERE auditable_type='base/residents.reader'").await?,
        "every keep and release is in the audit trail",
    )?;

    println!("== Withdrawn grant: the readers stop until it is given again");
    grants
        .set_system(&admin, PRINCIPAL, CAPABILITY, None)
        .await?;
    until(
        &db,
        20,
        "every reading stopped with actor_refused",
        "SELECT count(*)=4 AND bool_and(last_error='actor_refused') FROM base_resident_readings",
    )
    .await?;
    grants
        .set_system(&admin, PRINCIPAL, CAPABILITY, Some(true))
        .await?;
    until(&db, 20, "after the grant every reading runs again", "SELECT count(*)=4 AND bool_and(state='running' AND lease_expires_at>clock_timestamp()) FROM base_resident_readings").await?;

    println!("== Clean stop: leases are handed back, not left to expire");
    let before: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM resident_walkthrough_events WHERE what='cancelled'",
            &[],
        )
        .await?
        .get(0);
    let stopped = Instant::now();
    stop(b, "B").await?;
    check(
        truth(&db, "SELECT count(*)=4 AND bool_and(state='waiting' AND lease_token IS NULL AND available_at<=clock_timestamp() AND last_error='actor_refused') FROM base_resident_readings").await?,
        "every reading waits, due now, with no lease and no new error",
    )?;
    let after: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM resident_walkthrough_events WHERE what='cancelled'",
            &[],
        )
        .await?
        .get(0);
    check(
        after - before == 4,
        "all four readers were told to stop and returned",
    )?;
    let c = worker("C", 8)?;
    until(&db, 20, "worker C runs every reading", "SELECT count(*)=4 AND bool_and(state='running' AND owner='C' AND lease_expires_at>clock_timestamp()) FROM base_resident_readings").await?;
    until(
        &db,
        20,
        "worker C writes for every key",
        "SELECT count(DISTINCT key)=4 FROM resident_walkthrough_beats WHERE owner='C'",
    )
    .await?;
    check(
        stopped.elapsed() < Duration::from_secs(LEASE),
        "sooner than a lease could have expired",
    )?;
    check(
        truth(&db, "SELECT bool_and(b.last<c.first) FROM (SELECT key, max(at) AS last FROM resident_walkthrough_beats WHERE owner='B' GROUP BY key) b JOIN (SELECT key, min(at) AS first FROM resident_walkthrough_beats WHERE owner='C' GROUP BY key) c USING (key)").await?,
        "worker B's last write precedes worker C's first for every key",
    )?;

    println!("== Tenancy: a key in another tenant is its own reading under that tenant's grant");
    check(
        keep(&other, "steady-1").await?,
        "tenant 2 keeps its own steady-1",
    )?;
    until(&db, 20, "tenant 2's reading is refused: the principal holds no grant there", "SELECT count(*)=1 FROM base_resident_readings WHERE tenant_id=2 AND key='steady-1' AND last_error='actor_refused' AND failures>0").await?;
    check(
        truth(&db, "SELECT (SELECT count(*)=0 FROM resident_walkthrough_beats WHERE tenant_id=2) AND (SELECT count(*)=1 FROM base_resident_readings WHERE tenant_id=1 AND key='steady-1' AND state='running')").await?,
        "nothing was read in tenant 2, and tenant 1's steady-1 still runs",
    )?;

    println!("== What operators see");
    {
        let client = db.get().await?;
        let page = residents
            .list(&**client, &admin, &Filter::default(), 1, 25)
            .await?;
        for status in &page.entries {
            println!(
                "   reader={} key={} tenant={} state={:?} mounted={} owner={:?} starts={} failures={} last_error={:?} heartbeat={}",
                status.reader, status.key, status.tenant, status.state, status.mounted, status.owner,
                status.starts, status.failures, status.last_error, status.heartbeat_at.is_some()
            );
        }
        check(
            page.total == 5 && page.entries.len() == 5,
            "the platform operator lists every tenant's readings",
        )?;
        let running = residents
            .list(
                &**client,
                &admin,
                &Filter {
                    search: "steady".into(),
                    state: Some(bilibi_residents::State::Running),
                },
                1,
                25,
            )
            .await?;
        check(running.total == 3, "filtered by status and search")?;
        check(
            matches!(
                residents
                    .list(&**client, &other, &Filter::default(), 1, 25)
                    .await,
                Err(Error::Forbidden)
            ),
            "another tenant's manager is refused the list",
        )?;
        println!(
            "   System Info: {}",
            bilibi_residents::health_status(&**client).await?
        );
    }
    stop(c, "C").await?;
    println!("residents walkthrough passed");
    Ok(())
}
