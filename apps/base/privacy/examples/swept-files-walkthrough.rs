//! Manual running-service walkthrough of a retention sweep that reaches a
//! record's files (#515): a walkthrough Domain's closed cases hold private
//! `base/media` attachments; its record class sweeps a case past the
//! company's period, erases the case's files in the sweep's transaction and
//! removes their bytes after it commits, through `SubjectOwner::swept_subject`,
//! `Media::remove_swept_bytes` and `Sweeper::after_sweep`. A hold on a file
//! keeps it until released; an open case is never reached. Explicit
//! disposable DB and root only. The checks fail the executable when the
//! stored state breaks the contract. Run once with `seed` (production seeds
//! build their own app), then without.
#[path = "support/case_files.rs"]
mod case_files;

use bilibi_authz::grants::Grants;
use bilibi_base::media::{AttachmentSubject, Media, Rules, Upload};
use bilibi_database::{pool, MutationBoundary};
use bilibi_party_ids::OrganisationId;
use bilibi_privacy::{setting_keys, HoldTarget, Privacy, SERVICE, SWEEP_SCHEDULE};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

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
fn check(label: &str, ok: bool) -> Result<()> {
    println!("{} {label}", if ok { "ok  " } else { "FAIL" });
    if ok {
        Ok(())
    } else {
        Err(format!("contract broken: {label}").into())
    }
}
/// Saves one company setting the way System Settings does.
async fn put(
    boundary: &MutationBoundary,
    settings: &Settings,
    client: &mut Client,
    scope: &Scope,
    key: &str,
    value: Value,
) -> Result<()> {
    let company = OrganisationId::new(1)?;
    let mut tx = boundary
        .begin(client, scope.mutation_context("settings.company.save"))
        .await?;
    settings
        .put(&mut tx, scope, Target::Company(company), key, value)
        .await?;
    tx.commit().await?;
    Ok(())
}
/// Queues a sweep now and waits for the queue to finish it.
async fn sweep(
    app: &bilibi_web::App,
    client: &mut Client,
    admin: &Scope,
    label: &str,
) -> Result<()> {
    let schedule = app
        .services
        .get::<bilibi_base::schedule::Schedule>("base/schedule.store")?;
    let queued = schedule
        .run_now(client, admin, SWEEP_SCHEDULE, jiff::Timestamp::now())
        .await?;
    println!("{label}: run queued {}", queued.is_ok());
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
    let state: String = client
        .query_one(
            "SELECT state||' '||coalesce(failure_code,'') FROM base_queue_jobs WHERE worker_id='base/privacy-retention-sweep' ORDER BY id DESC LIMIT 1",
            &[],
        )
        .await?
        .get(0);
    println!("{label}: job {state}");
    check(
        &format!("{label}: the sweep job completes"),
        state.starts_with("completed"),
    )
}
/// A case's one file: (state, bytes removed, asset row present, bytes on disk).
async fn file(client: &Client, root: &str, case: &str) -> Result<(String, bool, bool, bool)> {
    let row = client
        .query_one(
            "SELECT a.state,a.bytes_removed_at IS NOT NULL,m.id IS NOT NULL,coalesce(m.storage_key,a.erased_key) \
             FROM base_media_attachments a LEFT JOIN base_media_assets m ON m.id=a.media_asset_id \
             WHERE a.tenant_id=1 AND a.subject_type=$1 AND a.subject_id=$2",
            &[&case_files::CASE, &case],
        )
        .await?;
    let key: Option<String> = row.get(3);
    let on_disk = key.is_some_and(|key| Path::new(root).join(key).exists());
    Ok((row.get(0), row.get(1), row.get(2), on_disk))
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
    // File handlers need a real desk (ADR 19).
    db.get().await?.batch_execute("INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1) ON CONFLICT DO NOTHING; \
        SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35576")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin = app.security.guard(&login.token).await?;
    let privacy = app.services.get::<Privacy>(SERVICE)?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let media = app.services.get::<Media>("base/media.store")?;
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
    {
        let mut tx = boundary
            .begin(
                &mut client,
                admin.mutation_context("privacy.walkthrough.configure"),
            )
            .await?;
        for (key, value) in [
            ("media.disks", json!({"local": root})),
            ("media.attachment_disk", json!("local")),
        ] {
            settings
                .put(&mut tx, &admin, Target::Global, key, value)
                .await?;
        }
        tx.commit().await?;
    }

    println!("== three cases with a file each: two closed 60 days ago, one open");
    case_files::install(&client, media.clone()).await?;
    client
        .batch_execute(
            "INSERT INTO walkthrough_cases(tenant_id,id,person_id,closed_at) VALUES \
             (1,301,1,now()-interval '60 days'),(1,302,2,now()-interval '60 days'),(1,303,1,NULL) ON CONFLICT DO NOTHING",
        )
        .await?;
    let mut held_file = 0;
    for case in ["301", "302", "303"] {
        let subject = AttachmentSubject {
            kind: case_files::CASE,
            id: case,
        };
        let upload = media
            .upload_attachment(
                &mut client,
                &admin,
                subject,
                Upload {
                    filename: format!("case-{case}.txt"),
                    bytes: format!("Case {case} note\n").into_bytes(),
                },
                Rules::default(),
            )
            .await?;
        let mut tx = boundary
            .begin(
                &mut client,
                admin.mutation_context("privacy.walkthrough.attach"),
            )
            .await?;
        media
            .submit_attachments(&mut tx, &admin, subject, &[upload.public_id.as_str()])
            .await?;
        tx.commit().await?;
        if case == "302" {
            held_file = client
                .query_one(
                    "SELECT id FROM base_media_attachments WHERE tenant_id=1 AND public_id=$1",
                    &[&upload.public_id],
                )
                .await?
                .get(0);
        }
    }
    for case in ["301", "302", "303"] {
        let (state, _, asset, on_disk) = file(&client, &root, case).await?;
        check(
            &format!("case {case}: its file is live with its bytes on disk"),
            state == "submitted" && asset && on_disk,
        )?;
    }
    let hold = privacy
        .place_hold(
            &mut client,
            &admin,
            HoldTarget::Record {
                kind: bilibi_base::media::ATTACHMENT_KIND.into(),
                id: held_file,
            },
            "Walkthrough hold on case 302's file",
        )
        .await?;
    println!("hold {} on case 302's file", hold.id);

    println!("== the company turns closed cases on, 30 days; an operator enables the sweep");
    let [enabled, period, _] = setting_keys("wt_cases/cases.closed_cases");
    put(
        &boundary,
        &settings,
        &mut client,
        &admin,
        &enabled,
        json!(true),
    )
    .await?;
    put(
        &boundary,
        &settings,
        &mut client,
        &admin,
        &period,
        json!(30),
    )
    .await?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));
    let schedule = app
        .services
        .get::<bilibi_base::schedule::Schedule>("base/schedule.store")?;
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

    let result: Result<()> = async {
        sweep(&app, &mut client, &admin, "run 1").await?;
        let (state, removed, asset, on_disk) = file(&client, &root, "301").await?;
        check(
            "the sweep erases closed case 301's file: a tombstone, its asset row gone, its bytes removed from disk",
            state == "erased" && removed && !asset && !on_disk,
        )?;
        let (state, removed, asset, on_disk) = file(&client, &root, "302").await?;
        check(
            "a hold on case 302's file keeps the file, its asset and its bytes",
            state == "submitted" && !removed && asset && on_disk,
        )?;
        let (state, _, asset, on_disk) = file(&client, &root, "303").await?;
        check(
            "the open case 303 is not reached: the sweep grant is refused for a case its owner did not sweep",
            state == "submitted"
                && asset
                && on_disk
                && case_files::refused()
                    .iter()
                    .any(|r| r == "sweep erase case 303: Forbidden"),
        )?;
        let audited: i64 = client
            .query_one(
                "SELECT count(*) FROM base_audit_mutations WHERE auditable_type=$1 AND event='updated' AND new_values->>'bytes'='removed'",
                &[&bilibi_base::media::ATTACHMENT_KIND],
            )
            .await?
            .get(0);
        check("the byte removal is audited once", audited == 1)?;
        let summary: (i64, i64) = client
            .query_one(
                "SELECT acted,kept FROM base_privacy_sweeps WHERE class='wt_cases/cases.closed_cases' ORDER BY id DESC LIMIT 1",
                &[],
            )
            .await
            .map(|row| (row.get(0), row.get(1)))?;
        check(
            "the run's summary counts one case acted on and one kept",
            summary == (1, 1),
        )?;

        println!("== the hold is released; the next run finishes case 302");
        privacy
            .release_hold(&mut client, &admin, hold.id, "Walkthrough hold released")
            .await?;
        sweep(&app, &mut client, &admin, "run 2").await?;
        let (state, removed, asset, on_disk) = file(&client, &root, "302").await?;
        check(
            "after release, case 302's file is erased and its bytes removed",
            state == "erased" && removed && !asset && !on_disk,
        )?;
        let (state, ..) = file(&client, &root, "301").await?;
        let audited: i64 = client
            .query_one(
                "SELECT count(*) FROM base_audit_mutations WHERE auditable_type=$1 AND event='updated' AND new_values->>'bytes'='removed'",
                &[&bilibi_base::media::ATTACHMENT_KIND],
            )
            .await?
            .get(0);
        check(
            "case 301 is not erased again: two byte removals in all",
            state == "erased" && audited == 2,
        )?;
        let (state, ..) = file(&client, &root, "303").await?;
        check("case 303 is still untouched", state == "submitted")?;
        Ok(())
    }
    .await;
    let _ = stop.send(true);
    let _ = workers.await;
    result?;
    println!("Swept-files walkthrough passed.");
    Ok(())
}
