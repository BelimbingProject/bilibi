//! Proof for #698: the scheduled retention sweep reads the settings of the
//! companies that turned a class on, not of every company. A tenant gets 2,000
//! more companies; only company 1 turns customer invitations on. One run must
//! read the settings table a few dozen times (on main: once per company and
//! class), still erase the due invitation of no company, keep company 2's
//! invitation while company 2 has the class off, and erase it once company 2
//! turns the class on. Explicit disposable DB only. Run once with `seed`
//! (production seeds build their own app), then without; exits non-zero on a
//! failed check.
use bilibi_authz::grants::Grants;
use bilibi_database::{pool, MutationBoundary};
use bilibi_party_ids::OrganisationId;
use bilibi_privacy::{setting_keys, SWEEP_SCHEDULE};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const COMPANIES: i64 = 2_000;
/// Settings reads one run may make: a few per class and per company that
/// turned a class on. Main makes one per company and class (about 12,000 here).
const READ_BOUND: i64 = 200;

/// The authenticator code for `key` at `offset` 30-second steps from now.
fn code(key: &str, offset: i64) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut secret = Vec::new();
    let (mut buffer, mut bits) = (0u64, 0u32);
    for c in key.chars().filter(|c| c.is_ascii_alphanumeric()) {
        let value = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .find(c.to_ascii_uppercase())
            .expect("base32") as u64;
        buffer = buffer << 5 | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            secret.push((buffer >> bits) as u8);
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let step = (now / 30 + offset) as u64;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).expect("key");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let at = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[at] & 0x7f,
        digest[at + 1],
        digest[at + 2],
        digest[at + 3],
    ]);
    format!("{:06}", binary % 1_000_000)
}
/// Saves one company setting the way System Settings does.
async fn put(
    boundary: &MutationBoundary,
    settings: &Settings,
    client: &mut Client,
    scope: &Scope,
    company: i64,
    key: &str,
    value: Value,
) -> Result<()> {
    let mut tx = boundary
        .begin(client, scope.mutation_context("settings.company.save"))
        .await?;
    settings
        .put(
            &mut tx,
            scope,
            Target::Company(OrganisationId::new(company)?),
            key,
            value,
        )
        .await?;
    Ok(tx.commit().await?)
}
/// Every read of the settings table PostgreSQL has counted so far. A pooled
/// connection reports its counts once idle, so this waits until they settle.
async fn settings_reads(client: &Client) -> Result<i64> {
    let mut last = -1;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let now: i64 = client
            .query_one(
                "SELECT (coalesce(seq_scan,0)+coalesce(idx_scan,0))::bigint FROM pg_stat_user_tables WHERE relname='base_settings'",
                &[],
            )
            .await?
            .get(0);
        if now == last {
            return Ok(now);
        }
        last = now;
    }
    Ok(last)
}
/// Queues a run now and waits for the queue to finish it; returns its seconds.
async fn sweep(app: &bilibi_web::App, client: &mut Client, admin: &Scope) -> Result<f64> {
    let schedule = app
        .services
        .get::<bilibi_base::schedule::Schedule>("base/schedule.store")?;
    schedule
        .run_now(client, admin, SWEEP_SCHEDULE, jiff::Timestamp::now())
        .await?
        .map_err(|e| format!("run now: {e:?}"))?;
    for _ in 0..120 {
        let open: i64 = client
            .query_one(
                "SELECT count(*) FROM base_queue_jobs WHERE worker_id='base/privacy-retention-sweep' AND state NOT IN ('completed','cancelled','discarded')",
                &[],
            )
            .await?
            .get(0);
        if open == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let row = client
        .query_one(
            "SELECT state, extract(epoch FROM completed_at-attempted_at)::float8 FROM base_queue_jobs WHERE worker_id='base/privacy-retention-sweep' ORDER BY id DESC LIMIT 1",
            &[],
        )
        .await?;
    let state: String = row.get(0);
    if state != "completed" {
        return Err(format!("the sweep job ended {state}").into());
    }
    Ok(row.get::<_, Option<f64>>(1).unwrap_or(0.0))
}
async fn erased(client: &Client, id: i64) -> Result<bool> {
    Ok(client
        .query_one(
            "SELECT name='[erased]' FROM customer_invitations WHERE id=$1",
            &[&id],
        )
        .await?
        .get(0))
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_PRIVACY_DEMO").as_deref() != Ok("yes") {
        return Err("explicit disposable DB opt-in required".into());
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    if std::env::args().nth(1).as_deref() == Some("seed") {
        return Ok(bilibi_web::seed::production(&db).await?);
    }
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35575")?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin = app.security.guard(&login.token).await?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let mut client = db.get().await?;
    // Granting the scheduler is a Step-up operation; enrolling counts as a fresh check.
    let enrollment = app
        .user
        .begin_two_factor(&admin, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|e| format!("begin two-factor: {e:?}"))?;
    app.user
        .confirm_two_factor(&admin, &code(&enrollment.key, -1))
        .await
        .map_err(|e| format!("confirm two-factor: {e:?}"))?;

    client
        .execute(
            "INSERT INTO companies(tenant_id,name,status,code) SELECT 1,'Sweep Co '||lpad(n::text,4,'0'),'active','sweep_'||n FROM generate_series(1,$1::bigint) n",
            &[&COMPANIES],
        )
        .await?;
    // 46001: an expired invitation of no company (the primary company's run);
    // 46002: an expired invitation of company 2 (company 2's own run).
    client.batch_execute("INSERT INTO customer_invitations(id,tenant_id,person_id,name,email,token_hash,invited_by,expires_at,affiliation_id,organisation_id,created_at) OVERRIDING SYSTEM VALUE VALUES \
        (46001,1,1,'Sweep Invitee','sweep-1@example.test',decode('0000b3a1','hex'),4,now()-interval '40 days',NULL,NULL,now()-interval '70 days'), \
        (46002,1,1,'Sweep Invitee','sweep-2@example.test',decode('0000b3a2','hex'),4,now()-interval '40 days',2,2,now()-interval '70 days')").await?;
    let [on, days, _] = setting_keys("core/portal.invitations");
    put(
        &boundary,
        &settings,
        &mut client,
        &admin,
        1,
        &days,
        json!(30),
    )
    .await?;
    put(
        &boundary,
        &settings,
        &mut client,
        &admin,
        1,
        &on,
        json!(true),
    )
    .await?;
    put(
        &boundary,
        &settings,
        &mut client,
        &admin,
        2,
        &days,
        json!(30),
    )
    .await?;

    let schedule = app
        .services
        .get::<bilibi_base::schedule::Schedule>("base/schedule.store")?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));
    schedule
        .review(&mut client, &admin, SWEEP_SCHEDULE, true)
        .await?;
    grants
        .set_system(
            &admin,
            bilibi_base::schedule::PRINCIPAL,
            bilibi_base::schedule::EXECUTE,
            Some(true),
        )
        .await?;

    let companies: i64 = client
        .query_one("SELECT count(*) FROM companies WHERE tenant_id=1", &[])
        .await?
        .get(0);
    let before = settings_reads(&client).await?;
    let seconds = sweep(&app, &mut client, &admin).await?;
    let reads = settings_reads(&client).await? - before;
    println!("run 1 over {companies} companies: {reads} settings reads, {seconds:.2} s");
    let first = (erased(&client, 46001).await?, erased(&client, 46002).await?);
    put(
        &boundary,
        &settings,
        &mut client,
        &admin,
        2,
        &on,
        json!(true),
    )
    .await?;
    sweep(&app, &mut client, &admin).await?;
    let second = erased(&client, 46002).await?;
    stop.send(true)?;
    let _ = workers.await;

    let checks = [
        (
            "one run reads the settings of the companies that turned a class on, not of every company",
            reads <= READ_BOUND,
            format!("{reads} reads over {companies} companies"),
        ),
        (
            "the due invitation of no company is still erased by the primary company's run",
            first.0,
            format!("{first:?}"),
        ),
        (
            "company 2's invitation is kept while company 2 has the class off",
            !first.1,
            format!("{first:?}"),
        ),
        (
            "once company 2 turns the class on, its invitation is erased",
            second,
            format!("{second}"),
        ),
    ];
    let mut failed = 0;
    for (label, ok, detail) in checks {
        if ok {
            println!("PASS {label}");
        } else {
            failed += 1;
            println!("FAIL {label}: {detail}");
        }
    }
    if failed > 0 {
        return Err(format!("{failed} check(s) failed").into());
    }
    Ok(())
}
