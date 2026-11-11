//! Disposable proof that a pending scan blocks merge preview and commit.
use bilibi_database::pool;
use bilibi_document::{
    Documents, Draft, MergeSource, ScanStatus, Source, Upload, WriteError, SERVICE,
};
use bilibi_media::{Media, ScanFacts, ScanFuture, Scanner, Verdict};
use bilibi_queue::{Config, Executor};
use bilibi_tenancy::Scope;
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct SlowScanner {
    release: std::path::PathBuf,
}
impl Scanner for SlowScanner {
    fn name(&self) -> &str {
        "document-pending-merge-proof"
    }
    fn version(&self) -> String {
        "1".into()
    }
    fn scan<'a>(&'a self, bytes: &'a [u8], _facts: ScanFacts<'a>) -> ScanFuture<'a> {
        Box::pin(async move {
            if bytes.starts_with(b"slow") {
                while !self.release.exists() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            Verdict::Clean
        })
    }
}
macro_rules! create {
    ($app:expr,$docs:expr,$scope:expr,$name:expr,$token:expr) => {{
        let mut client = $app.db.get().await?;
        let mut tx = $app
            .mutations
            .begin(&mut client, $scope.mutation_context("core.document.create"))
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
    if std::env::var("BILIBI_DOCUMENT_MERGE_PROOF").as_deref() != Ok("disposable") {
        return Err("disposable only".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    db.get().await?.batch_execute(
  "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'merge_proof_admin','Merge Proof Admin','human'
   WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id
    WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
   u AS (UPDATE users SET person_id=p.id FROM p,user_identities i
    WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
   INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING"
 ).await?;
    let root = std::path::PathBuf::from(std::env::var("BILIBI_DOCUMENT_MERGE_ROOT")?);
    for (key, folder) in [
        ("artifacts.storage_root", "artifacts"),
        ("media.disks", "media"),
    ] {
        let path = root.join(folder);
        std::fs::create_dir_all(&path)?;
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
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:4475")?;
    let release = root.join("release");
    if release.exists() {
        std::fs::remove_file(&release)?;
    }
    app.services
        .get::<Media>("base/media.store")?
        .bind_scanner(Arc::new(SlowScanner {
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
    let docs = app.services.get::<Documents>(SERVICE)?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let scope: Scope = app.security.guard(&login.token).await?;
    let clean = docs
        .store_file(
            &scope,
            Upload {
                bytes: b"clean merge survivor",
                file_name: "merge-survivor.txt",
            },
        )
        .await?;
    assert_eq!(clean.scan_status, ScanStatus::Clean);
    let survivor = create!(app, docs, scope, "Merge survivor", clean.artifact_id);
    let bytes = format!("slow pending merge proof {}", jiff::Timestamp::now());
    let stored = docs
        .store_file(
            &scope,
            Upload {
                bytes: bytes.as_bytes(),
                file_name: "pending-merge.txt",
            },
        )
        .await?;
    assert_eq!(stored.scan_status, ScanStatus::Pending);
    let source = create!(app, docs, scope, "Pending merge source", stored.artifact_id);
    let held = docs.revisions(&scope, source.id).await?.remove(0);
    assert_eq!(held.scan_status, ScanStatus::Pending);
    assert!(held.artifact_id.is_none());
    let src = MergeSource {
        id: source.id,
        version: source.version,
    };
    let dst = MergeSource {
        id: survivor.id,
        version: survivor.version,
    };
    let client = db.get().await?;
    let preview = docs.preview_merge(&**client, &scope, dst, &[src]).await;
    let preview_refused = matches!(preview, Err(WriteError::ScanPending));
    let mut client = app.db.get().await?;
    let mut tx = app
        .mutations
        .begin(&mut client, scope.mutation_context("core.document.merge"))
        .await?;
    let commit = docs.merge(&mut tx, &scope, dst, &[src]).await;
    let commit_refused = matches!(commit, Err(WriteError::ScanPending));
    drop(tx);
    let reverse_src = MergeSource {
        id: survivor.id,
        version: survivor.version,
    };
    let reverse_dst = MergeSource {
        id: source.id,
        version: source.version,
    };
    let client = db.get().await?;
    let reverse_preview = docs
        .preview_merge(&**client, &scope, reverse_dst, &[reverse_src])
        .await;
    let reverse_preview_refused = matches!(reverse_preview, Err(WriteError::ScanPending));
    let mut other_client = app.db.get().await?;
    let mut other_tx = app
        .mutations
        .begin(
            &mut other_client,
            scope.mutation_context("core.document.merge"),
        )
        .await?;
    let reverse_commit = docs
        .merge(&mut other_tx, &scope, reverse_dst, &[reverse_src])
        .await;
    let reverse_commit_refused = matches!(reverse_commit, Err(WriteError::ScanPending));
    drop(other_tx);
    std::fs::write(&release, "release scanner")?;
    stop.send(true)?;
    worker.await??;
    println!(
        "pending source {}, preview_refused={preview_refused}, commit_refused={commit_refused}",
        source.id
    );
    if !preview_refused || !commit_refused || !reverse_preview_refused || !reverse_commit_refused {
        return Err("pending merge participant was accepted".into());
    }
    println!("pending survivor preview_refused={reverse_preview_refused}, commit_refused={reverse_commit_refused}");
    assert_eq!(docs.get(&scope, source.id).await?.id, source.id);
    Ok(())
}
