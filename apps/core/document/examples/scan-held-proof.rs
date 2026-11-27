//! Disposable end-to-end proof for the held-revision scan lifecycle.
//! A fixture scanner is bound only in this executable, never in production.
//! Run after migrate, fixture and seed-production with a dedicated database.
use bilibi_database::pool;
use bilibi_document::{Documents, Draft, ScanStatus, Source, Upload, WriteError, SERVICE};
use bilibi_media::{Media, ScanFacts, ScanFuture, Scanner, Verdict};
use bilibi_queue::{Config, Executor};
use bilibi_tenancy::Scope;
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct FixtureScanner {
    release: std::path::PathBuf,
}
impl Scanner for FixtureScanner {
    fn name(&self) -> &str {
        "document-held-proof"
    }
    fn version(&self) -> String {
        "1".into()
    }
    fn scan<'a>(&'a self, bytes: &'a [u8], _facts: ScanFacts<'a>) -> ScanFuture<'a> {
        Box::pin(async move {
            if bytes.starts_with(b"slow") {
                while !self.release.exists() {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            if bytes.starts_with(b"infected") {
                Verdict::Infected {
                    signature: "fixture".into(),
                }
            } else {
                Verdict::Clean
            }
        })
    }
}
macro_rules! create {
    ($app:expr, $docs:expr, $scope:expr, $name:expr, $token:expr) => {{
        let mut client = $app.db.get().await?;
        let mut tx = $app
            .mutations
            .begin(
                &mut client,
                $scope.mutation_context("core.document.scan_proof"),
            )
            .await?;
        let document = $docs
            .create(
                &mut tx,
                &$scope,
                &Draft {
                    name: Some($name.into()),
                    source: Source::Internal,
                    artifact_id: Some($token),
                    ..Draft::default()
                },
            )
            .await?;
        tx.commit().await?;
        document
    }};
}
#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DOCUMENT_SCAN_PROOF").as_deref() != Ok("disposable") {
        return Err("use a disposable migrated and seeded database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'scan_proof_admin','Scan Proof Admin','human'
            WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id
                WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
          u AS (UPDATE users SET person_id=p.id FROM p,user_identities i
            WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
          INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING"
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:4474")?;
    let root = std::path::PathBuf::from(std::env::var("BILIBI_DOCUMENT_SCAN_ROOT")?);
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
    let release = root.join("scan-release");
    if release.exists() {
        std::fs::remove_file(&release)?;
    }
    app.services
        .get::<Media>("base/media.store")?
        .bind_scanner(Arc::new(FixtureScanner {
            release: release.clone(),
        }))?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(
        Executor::from_services(
            &app.services,
            Config {
                concurrency: 2,
                poll_interval: Duration::from_millis(200),
                ..Config::default()
            },
        )?
        .run(stopped),
    );
    let documents = app.services.get::<Documents>(SERVICE)?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let scope: Scope = app.security.guard(&login.token).await?;

    let infected = documents
        .store_file(
            &scope,
            Upload {
                file_name: "infected.txt",
                bytes: b"infected fixture bytes",
            },
        )
        .await?;
    assert_eq!(infected.scan_status, ScanStatus::Quarantined);
    let held = create!(
        app,
        documents,
        scope,
        "Quarantined document",
        infected.artifact_id
    );
    let revision = documents.revisions(&scope, held.id).await?.remove(0);
    assert_eq!(revision.scan_status, ScanStatus::Quarantined);
    assert!(revision.artifact_id.is_none());
    assert!(matches!(
        documents.read_file(&scope, held.id, 1).await,
        Err(WriteError::Quarantined)
    ));
    let no_artifact: bool = db.get().await?.query_one(
        "SELECT r.artifact_id IS NULL AND f.media_public_id IS NOT NULL FROM core_document_revisions r
         JOIN core_document_files f ON f.tenant_id=r.tenant_id AND f.id=r.file_id
         WHERE r.tenant_id=$1 AND r.document_id=$2", &[&scope.tenant(),&held.id]
    ).await?.get(0);
    assert!(no_artifact);
    println!(
        "infected revision held without Artifact: document {}",
        held.id
    );

    db.get()
        .await?
        .execute(
            "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted)
         VALUES('core.document.retention_days',$1,'tenant',$1,'1'::jsonb,false)
         ON CONFLICT (key,tenant_id,scope_type,scope_id) DO UPDATE SET value='1'::jsonb",
            &[&scope.tenant()],
        )
        .await?;
    let slow_bytes = b"slow clean fixture bytes";
    let slow = documents
        .store_file(
            &scope,
            Upload {
                file_name: "slow.txt",
                bytes: slow_bytes,
            },
        )
        .await?;
    assert_eq!(slow.scan_status, ScanStatus::Pending);
    let pending = create!(
        app,
        documents,
        scope,
        "Pending scan document",
        slow.artifact_id
    );
    assert!(matches!(
        documents.read_file(&scope, pending.id, 1).await,
        Err(WriteError::Quarantined)
    ));
    let queued: bool = db
        .get()
        .await?
        .query_one(
            "SELECT scan_job_id IS NOT NULL AND artifact_id IS NULL FROM core_document_revisions
         WHERE tenant_id=$1 AND document_id=$2",
            &[&scope.tenant(), &pending.id],
        )
        .await?
        .get(0);
    assert!(queued);
    println!(
        "pending revision queued without Artifact: document {}",
        pending.id
    );
    std::fs::write(&release, "scan may complete")?;

    let mut promoted = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let revision = documents.revisions(&scope, pending.id).await?.remove(0);
        if revision.scan_status == ScanStatus::Clean {
            assert!(revision.artifact_id.is_some());
            let file = documents.read_file(&scope, pending.id, 1).await?;
            assert_eq!(file.bytes, slow_bytes);
            promoted = true;
            break;
        }
        assert_eq!(revision.scan_status, ScanStatus::Pending);
    }
    assert!(promoted, "Queue did not promote the clean scan");
    let published: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM base_artifacts a JOIN core_document_files f
         ON f.tenant_id=a.tenant_id AND f.subject::text=a.subject
         JOIN core_document_revisions r ON r.tenant_id=f.tenant_id AND r.file_id=f.id
         WHERE r.tenant_id=$1 AND r.document_id=$2 AND a.owner_id='core.document'
           AND a.ready_at IS NOT NULL AND a.deleted_at IS NULL",
            &[&scope.tenant(), &pending.id],
        )
        .await?
        .get(0);
    assert_eq!(
        published, 1,
        "retries must recover the one published Artifact"
    );
    let other_login = app
        .user
        .login(
            "manager@other.example.test",
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await?;
    let other_scope = app.security.guard(&other_login.token).await?;
    assert!(documents.revisions(&other_scope, pending.id).await.is_err());
    let protection = documents.retention_statuses(&scope, pending.id).await?;
    assert!(protection
        .get(&1)
        .is_some_and(|state| state.retain_until.is_some()));
    let notices: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM notifications WHERE tenant_id=$1 AND user_id=$2
         AND type='core.document.scan' AND url IN ($3,$4)",
            &[
                &scope.tenant(),
                &scope.user(),
                &format!("/documents/{}", held.id),
                &format!("/documents/{}", pending.id),
            ],
        )
        .await?
        .get(0);
    assert!(
        notices >= 3,
        "expected held and settled notices, got {notices}"
    );
    println!("clean promotion, file read, and {notices} scan notices passed");
    stop.send(true)?;
    worker.await??;
    Ok(())
}
