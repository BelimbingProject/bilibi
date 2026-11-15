//! A request that cannot finish names why when the operator can fix it in
//! Settings (#715), on a disposable database (`seed` first, same variables as
//! `requests-walkthrough`). One approved erasure of person 1 runs while its
//! erasure certificate cannot be printed: first with no Chromium at the
//! configured path, then with a browser that never answers (1 s start limit),
//! then with the real browser. Each failed finish must be recorded as
//! `pdf_engine_unavailable` and then `pdf_render_timed_out` on the request,
//! its audit row and its finish job, never the generic `finish_failed`; the
//! real browser must then complete it and clear the reason. Exits non-zero
//! when a check fails. `BILIBI_FINISH_REASONS_KEEP_FAILING=yes` stops after
//! the timeout, leaving the request failing for a look at its page.
use bilibi_authz::grants::Grants;
use bilibi_database::{pool, MutationBoundary};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_privacy::{NewRequest, Privacy, RequestKind, RequestView, FINISH_WORKER, SERVICE};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[allow(dead_code)]
#[path = "support/erasure.rs"]
mod erasure_approval;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        println!("  ok: {message}");
        Ok(())
    } else {
        Err(format!("contract violated: {message}").into())
    }
}

/// The authenticator code for `key` (base32) at `offset` 30-second steps.
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
async fn enroll(app: &bilibi_web::App, scope: &Scope) -> Result<String> {
    let enrollment = app
        .user
        .begin_two_factor(scope, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|e| format!("begin two-factor: {e:?}"))?;
    app.user
        .confirm_two_factor(scope, &code(&enrollment.key, -1))
        .await
        .map_err(|e| format!("confirm two-factor: {e:?}"))?;
    Ok(enrollment.key)
}
/// The request once `done` holds for it, within 90 seconds.
async fn wait(
    privacy: &Privacy,
    client: &Client,
    scope: &Scope,
    id: i64,
    done: impl Fn(&RequestView) -> bool,
) -> Result<RequestView> {
    for _ in 0..180 {
        let request = privacy.request(client, scope, id).await?;
        if done(&request) {
            return Ok(request);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(privacy.request(client, scope, id).await?)
}
/// The finish job's failure code and the request's audit rows recording `code`.
async fn recorded(client: &Client, id: i64, code: &str) -> Result<(Option<String>, i64)> {
    let job: Option<String> = client
        .query_one(
            "SELECT failure_code FROM base_queue_jobs WHERE worker_id=$1 AND (args->>'request_id')::bigint=$2 ORDER BY id DESC LIMIT 1",
            &[&FINISH_WORKER, &id],
        )
        .await?
        .get(0);
    let audit: i64 = client
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type='base/privacy.request' \
             AND auditable_id=$1::bigint::text AND new_values->>'finish_failed'=$2",
            &[&id, &code],
        )
        .await?
        .get(0);
    Ok((job, audit))
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_PRIVACY_DEMO").as_deref() != Ok("yes") {
        return Err("explicit disposable DB opt-in required".into());
    }
    let root = std::env::var("BILIBI_PRIVACY_DEMO_ROOT")?;
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    if std::env::args().nth(1).as_deref() == Some("seed") {
        return Ok(bilibi_web::seed::production(&db).await?);
    }
    // Background work needs real desks (ADR 19).
    db.get().await?.batch_execute("INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,3,1) ON CONFLICT DO NOTHING; \
        SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35579")?;
    app.services
        .get::<Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    async fn login(app: &bilibi_web::App, email: &str) -> Result<Scope> {
        let login = app
            .user
            .login(email, bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        Ok(app.security.guard(&login.token).await?)
    }
    let mut client = db.get().await?;
    let admin = login(&app, "admin@example.test").await?;
    let second = login(&app, "ai@agent.my").await?;
    let privacy = app.services.get::<Privacy>(SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let second_key = enroll(&app, &second).await?;
    erasure_approval::publish_rules(&app, &mut client, &admin, 1).await?;
    let chromium =
        std::env::var("BILIBI_PRIVACY_DEMO_CHROMIUM").unwrap_or_else(|_| "chromium".into());
    let hang = std::path::Path::new(&root).join("never-answers.sh");
    std::fs::write(&hang, "#!/bin/sh\nsleep 60\n")?;
    std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o700))?;
    let configure = |pairs: Vec<(&'static str, Value)>| {
        let (db, boundary, settings, admin) = (
            db.clone(),
            boundary.clone(),
            settings.clone(),
            admin.clone(),
        );
        async move {
            let mut client = db.get().await?;
            let mut tx = boundary
                .begin(
                    &mut client,
                    admin.mutation_context("privacy.walkthrough.configure"),
                )
                .await?;
            for (key, value) in pairs {
                println!("  setting {key} = {value}");
                settings
                    .put(&mut tx, &admin, Target::Global, key, value.clone())
                    .await
                    .map_err(|e| format!("{key}={value}: {e:?}"))?;
            }
            tx.commit().await?;
            Ok::<(), Box<dyn std::error::Error>>(())
        }
    };
    println!("== no Chromium at the configured path");
    configure(vec![
        ("artifacts.storage_root", json!(root)),
        ("pdf.chromium.binary", json!("/nonexistent/chromium")),
    ])
    .await?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));

    let received = privacy
        .receive(
            &mut client,
            &admin,
            NewRequest {
                kind: RequestKind::Erasure,
                person: PersonId::new(1)?,
                identifiers: Vec::new(),
                company: OrganisationId::new(1)?,
            },
        )
        .await?;
    let verified = privacy
        .verify(
            &mut client,
            &admin,
            received.id,
            received.version,
            "Signed request, ID checked",
            &[],
        )
        .await?;
    let decided = erasure_approval::decide(
        &app,
        &privacy,
        &mut client,
        &admin,
        &second.clone().presenting_step_up(&code(&second_key, 0)),
        &verified,
        true,
    )
    .await?;
    let id = decided.id;
    let missing = wait(&privacy, &client, &admin, id, |r| {
        r.finish_error.is_some() || r.status != "running"
    })
    .await?;
    println!(
        "  request {id} status={} finish_attempts={} finish_error={:?}",
        missing.status, missing.finish_attempts, missing.finish_error
    );
    let (job, audit) = recorded(&client, id, "pdf_engine_unavailable").await?;
    println!("  finish job failure_code={job:?}, audit rows naming it: {audit}");
    ensure(
        missing.status == "running"
            && missing.finish_error.as_deref() == Some("pdf_engine_unavailable"),
        "a missing Chromium is recorded on the request as pdf_engine_unavailable",
    )?;
    ensure(
        job.as_deref() == Some("pdf_engine_unavailable") && audit == 1,
        "the finish job and the request's audit row name pdf_engine_unavailable",
    )?;

    println!("== a browser that never answers (1 s start limit)");
    configure(vec![
        ("pdf.chromium.binary", json!(hang.display().to_string())),
        ("pdf.launch_timeout_seconds", json!(1)),
    ])
    .await?;
    let timed_out = wait(&privacy, &client, &admin, id, |r| {
        r.finish_error.as_deref() != Some("pdf_engine_unavailable") || r.status != "running"
    })
    .await?;
    println!(
        "  request {id} status={} finish_attempts={} finish_error={:?}",
        timed_out.status, timed_out.finish_attempts, timed_out.finish_error
    );
    let (job, audit) = recorded(&client, id, "pdf_render_timed_out").await?;
    println!("  finish job failure_code={job:?}, audit rows naming it: {audit}");
    ensure(
        timed_out.status == "running"
            && timed_out.finish_error.as_deref() == Some("pdf_render_timed_out")
            && timed_out.finish_attempts >= 2,
        "a timed-out render is recorded on the request as pdf_render_timed_out",
    )?;
    ensure(
        job.as_deref() == Some("pdf_render_timed_out") && audit == 1,
        "the finish job and the request's audit row name pdf_render_timed_out",
    )?;
    if std::env::var("BILIBI_FINISH_REASONS_KEEP_FAILING").as_deref() == Ok("yes") {
        println!("request {id} left failing: open /privacy/requests/{id}");
        stop.send(true)?;
        let _ = workers.await;
        return Ok(());
    }

    println!("== the real browser");
    configure(vec![
        ("pdf.chromium.binary", json!(chromium)),
        ("pdf.launch_timeout_seconds", json!(60)),
    ])
    .await?;
    let done = wait(&privacy, &client, &admin, id, |r| r.status != "running").await?;
    println!(
        "  request {id} status={} finish_error={:?}",
        done.status, done.finish_error
    );
    ensure(
        done.status == "completed" && done.finish_error.is_none(),
        "with a working browser the erasure completes and the reason is cleared",
    )?;

    stop.send(true)?;
    let _ = workers.await;
    println!("finish reasons walkthrough passed");
    Ok(())
}
