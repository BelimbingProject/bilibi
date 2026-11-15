//! Disposable proof for two upload findings of issue 602, on a migrated,
//! fixture-installed and seeded database.
//!
//! 1. While `store_file` waits on the scanner (up to 30 seconds), it holds no
//!    pooled connection: two uploads wait at once with no Queue executor
//!    running, so Media's scan never finishes, while the program samples how
//!    many of the pool's connections are checked out.
//! 2. A held upload's Media draft is retained when its file is attached, so
//!    Media's cleanup of drafts older than a day spares it during a scanner
//!    outage. The drafts are aged in SQL, the only writes outside the facades
//!    (with Media's guard trigger off), then `Media::cleanup_drafts` runs as
//!    the hourly schedule would. Then the scanner comes back, and settling
//!    each scan releases its draft to that cleanup. (A document is neither
//!    deleted nor merged while a scan is pending, so nothing else must
//!    release one.)
//!
//! `BILIBI_DOCUMENT_SCAN_WAIT_PROOF=disposable BILIBI_DOCUMENT_SCAN_WAIT_ROOT=…
//! DATABASE_URL=… cargo run -p bilibi-document --example scan-wait-proof --locked`
use bilibi_database::pool;
use bilibi_document::{Documents, Draft, ScanStatus, Source, Upload, SERVICE};
use bilibi_media::{Media, ScanFacts, ScanFuture, Scanner, Verdict};
use bilibi_queue::{Config, Executor};
use bilibi_tenancy::Scope;
use serde_json::json;
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Calls every file clean, once a Queue executor runs Media's scan job.
struct FixtureScanner;
impl Scanner for FixtureScanner {
    fn name(&self) -> &str {
        "document-scan-wait-proof"
    }
    fn version(&self) -> String {
        "1".into()
    }
    fn scan<'a>(&'a self, _bytes: &'a [u8], _facts: ScanFacts<'a>) -> ScanFuture<'a> {
        Box::pin(async { Verdict::Clean })
    }
}

struct Proof {
    failures: usize,
}
impl Proof {
    fn check(&mut self, ok: bool, what: &str) {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            self.failures += 1;
        }
    }
}

/// The held upload's draft: (exists, retained), by document.
async fn draft(db: &bilibi_database::Db, tenant: i64, document: i64) -> Result<(bool, bool)> {
    let row = db
        .get()
        .await?
        .query_opt(
            "SELECT a.retained_at IS NOT NULL FROM base_media_attachments a
             JOIN core_document_files f ON f.tenant_id=a.tenant_id AND f.media_public_id=a.public_id
             JOIN core_document_revisions r ON r.tenant_id=f.tenant_id AND r.file_id=f.id
             WHERE r.tenant_id=$1 AND r.document_id=$2",
            &[&tenant, &document],
        )
        .await?;
    Ok((row.is_some(), row.is_some_and(|row| row.get(0))))
}

async fn draft_by_id(db: &bilibi_database::Db, public_id: &str) -> Result<Option<bool>> {
    Ok(db
        .get()
        .await?
        .query_opt(
            "SELECT retained_at IS NOT NULL FROM base_media_attachments WHERE public_id=$1",
            &[&public_id],
        )
        .await?
        .map(|row| row.get(0)))
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DOCUMENT_SCAN_WAIT_PROOF").as_deref() != Ok("disposable") {
        return Err("use a disposable migrated and seeded database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'scan_wait_admin','Scan Wait Admin','human'
            WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id
                WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
          u AS (UPDATE users SET person_id=p.id FROM p,user_identities i
            WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
          INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING",
    ).await?;
    let root = PathBuf::from(std::env::var("BILIBI_DOCUMENT_SCAN_WAIT_ROOT")?);
    for (key, folder) in [
        ("artifacts.storage_root", "artifacts"),
        ("media.disks", "media"),
    ] {
        let path = root.join(folder);
        std::fs::DirBuilder::new().recursive(true).create(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        let value = if key == "media.disks" {
            json!({"local":path})
        } else {
            json!(path)
        };
        db.get()
            .await?
            .execute(
                "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted)
             VALUES($1,0,'global',0,$2,false) ON CONFLICT (key,tenant_id,scope_type,scope_id)
             DO UPDATE SET value=excluded.value",
                &[&key, &value],
            )
            .await?;
    }
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:4476")?;
    let media = app.services.get::<Media>("base/media.store")?;
    media.bind_scanner(Arc::new(FixtureScanner))?;
    let documents = app.services.get::<Documents>(SERVICE)?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let scope: Scope = app.security.guard(&login.token).await?;
    let tenant = scope.tenant();
    let mut proof = Proof { failures: 0 };

    // 1. Two uploads wait on a scan that cannot finish: no executor runs.
    let in_use = |db: &bilibi_database::Db| {
        let status = db.status();
        status.size.saturating_sub(status.available)
    };
    let baseline = in_use(&db);
    let mut uploads = Vec::new();
    for (name, bytes) in [
        ("first.txt", b"first held upload".as_slice()),
        ("second.txt", b"second held upload".as_slice()),
    ] {
        let (documents, scope) = (documents.clone(), scope.clone());
        uploads.push(tokio::spawn(async move {
            documents
                .store_file(
                    &scope,
                    Upload {
                        file_name: name,
                        bytes,
                    },
                )
                .await
                .map_err(|e| format!("store {name}: {e:?}"))
        }));
    }
    // Sample while both wait: they reach the scanner wait within a second
    // or two and stay there for 30 seconds.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let (mut samples, mut holding) = (0, 0);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        samples += 1;
        if in_use(&db) > baseline {
            holding += 1;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!(
        "connections checked out beyond the {baseline} before: {holding} of {samples} samples"
    );
    proof.check(
        holding * 10 <= samples,
        "two waiting uploads hold no pooled connection between their looks (at most 1 in 10 samples)",
    );
    let mut stored = Vec::new();
    for upload in uploads {
        stored.push(upload.await??);
    }
    proof.check(
        stored.iter().all(|s| s.scan_status == ScanStatus::Pending),
        "both uploads end their wait still pending",
    );

    // 2. Filing them attaches held files and retains their drafts.
    let mut ids = Vec::new();
    for (name, file) in ["Scan wait: first", "Scan wait: second"]
        .iter()
        .zip(&stored)
    {
        let mut client = app.db.get().await?;
        let mut tx = app
            .mutations
            .begin(&mut client, scope.mutation_context("core.document.create"))
            .await?;
        let document = documents
            .create(
                &mut tx,
                &scope,
                &Draft {
                    name: Some((*name).into()),
                    source: Source::Internal,
                    artifact_id: Some(file.artifact_id.clone()),
                    ..Draft::default()
                },
            )
            .await
            .map_err(|e| format!("create {name}: {e:?}"))?;
        tx.commit().await?;
        ids.push(document.id);
    }
    let mut drafts = Vec::new();
    for &id in &ids {
        let public_id: String = db
            .get()
            .await?
            .query_one(
                "SELECT f.media_public_id FROM core_document_files f JOIN core_document_revisions r
                 ON r.tenant_id=f.tenant_id AND r.file_id=f.id WHERE r.tenant_id=$1 AND r.document_id=$2",
                &[&tenant, &id],
            )
            .await?
            .get(0);
        drafts.push(public_id);
    }
    proof.check(
        draft(&db, tenant, ids[0]).await? == (true, true)
            && draft(&db, tenant, ids[1]).await? == (true, true),
        "attaching a held upload retains its Media draft",
    );

    // Two days into a scanner outage, the hourly draft cleanup runs.
    // The guard refuses to rewrite a draft's creation time, so it is off for
    // this one statement.
    {
        let mut client = db.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute(
            "ALTER TABLE base_media_attachments DISABLE TRIGGER base_media_attachments_guard",
        )
        .await?;
        tx.execute(
            "UPDATE base_media_attachments SET created_at=now()-interval '2 days' WHERE public_id=ANY($1)",
            &[&drafts],
        )
        .await?;
        tx.batch_execute(
            "ALTER TABLE base_media_attachments ENABLE TRIGGER base_media_attachments_guard",
        )
        .await?;
        tx.commit().await?;
    }
    let swept = media
        .cleanup_drafts(
            &mut *db.get().await?,
            tenant,
            Duration::from_secs(24 * 3600),
        )
        .await?;
    println!("draft cleanup removed {} draft(s)", swept.removed);
    proof.check(
        draft_by_id(&db, &drafts[0]).await?.is_some()
            && draft_by_id(&db, &drafts[1]).await?.is_some(),
        "Media's cleanup of day-old drafts spares both waiting uploads",
    );

    // The scanner is back: both scans settle clean.
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(
        Executor::from_services(
            &app.services,
            Config {
                concurrency: 4,
                poll_interval: Duration::from_millis(200),
                ..Config::default()
            },
        )?
        .run(stopped),
    );
    let mut settled = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        settled = true;
        for &id in &ids {
            settled &= documents.revisions(&scope, id).await?[0].scan_status == ScanStatus::Clean;
        }
        if settled {
            break;
        }
    }
    stop.send(true)?;
    worker.await??;
    proof.check(settled, "both scans settle clean once the scanner is back");
    proof.check(
        draft_by_id(&db, &drafts[0]).await? == Some(false)
            && draft_by_id(&db, &drafts[1]).await? == Some(false),
        "settling each scan releases its draft to Media's cleanup",
    );
    let swept = media
        .cleanup_drafts(
            &mut *db.get().await?,
            tenant,
            Duration::from_secs(24 * 3600),
        )
        .await?;
    proof.check(
        swept.removed >= 2
            && draft_by_id(&db, &drafts[0]).await?.is_none()
            && draft_by_id(&db, &drafts[1]).await?.is_none(),
        "the next cleanup removes both released drafts",
    );
    if proof.failures > 0 {
        return Err(format!("{} checks failed", proof.failures).into());
    }
    println!("all checks passed");
    Ok(())
}
