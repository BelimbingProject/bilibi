//! Disposable browser proof for the New document dialog's upload progress and
//! resume. It runs the real host with a fixture scanner and Queue's executor;
//! the scanner lives only in this executable, never in production.
//!
//! The scanner keeps every file pending while `<root>/scan-hold` exists (the
//! proof creates it at start; remove it to let the verdicts land) and then
//! calls the file clean. It accepts any size, so a large upload is scanned
//! like any other. The settings raise the upload limits to 256 MiB (Media,
//! Artifacts and Documents) and the scan time limit to 600 seconds, and
//! resumable uploads stage on the `local` Media disk under `<root>/media`.
//!
//! Run after `migrate` against a dedicated disposable database:
//! `BILIBI_DOCUMENT_UPLOAD_PROOF=disposable DATABASE_URL=… BILIBI_DOCUMENT_UPLOAD_ROOT=…
//! APP_ORIGIN=http://127.0.0.1:4476 BIND_ADDR=127.0.0.1:4476
//! cargo run -p bilibi-document --example upload-resume-proof`, then sign in
//! as `admin@example.test` at `APP_ORIGIN`.
use bilibi_database::pool;
use bilibi_media::{Media, ScanFacts, ScanFuture, Scanner, Verdict};
use bilibi_queue::{Config, Executor};
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The largest file each layer accepts here.
const LIMIT: u64 = 256 * 1024 * 1024;

struct HoldingScanner {
    hold: PathBuf,
}
impl Scanner for HoldingScanner {
    fn name(&self) -> &str {
        "document-upload-proof"
    }
    fn version(&self) -> String {
        "1".into()
    }
    fn scan<'a>(&'a self, _bytes: &'a [u8], _facts: ScanFacts<'a>) -> ScanFuture<'a> {
        Box::pin(async move {
            while self.hold.exists() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Verdict::Clean
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DOCUMENT_UPLOAD_PROOF").as_deref() != Ok("disposable") {
        return Err("use a disposable migrated database".into());
    }
    let origin = std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:4476".into());
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:4476".into());
    let root = PathBuf::from(std::env::var("BILIBI_DOCUMENT_UPLOAD_ROOT")?);
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    // The administrator files documents under company 1.
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'upload_proof_admin','Upload Proof Admin','human'
            WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id
                WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
          u AS (UPDATE users SET person_id=p.id FROM p,user_identities i
            WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
          INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING"
    ).await?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    let mut settings = vec![
        ("media.max_bytes", json!(LIMIT)),
        ("artifacts.max_bytes", json!(LIMIT)),
        ("core.document.max_upload_bytes", json!(LIMIT)),
        ("media.scan.timeout_seconds", json!(600)),
    ];
    for (key, folder) in [
        ("artifacts.storage_root", "artifacts"),
        ("media.disks", "media"),
    ] {
        let path = root.join(folder);
        std::fs::DirBuilder::new().recursive(true).create(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        settings.push(if key == "media.disks" {
            (key, json!({"local": path}))
        } else {
            (key, json!(path))
        });
    }
    let client = db.get().await?;
    for (key, value) in &settings {
        client
            .execute(
                "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted)
                 VALUES($1,0,'global',0,$2,false) ON CONFLICT (key,tenant_id,scope_type,scope_id)
                 DO UPDATE SET value=excluded.value",
                &[key, value],
            )
            .await?;
    }
    drop(client);
    let hold = root.join("scan-hold");
    std::fs::write(&hold, b"")?;
    app.services
        .get::<Media>("base/media.store")?
        .bind_scanner(Arc::new(HoldingScanner { hold: hold.clone() }))?;
    println!(
        "Fixture scanner bound; files stay pending until {} is removed",
        hold.display()
    );
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
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("Bilibi at {origin}");
    let served = axum::serve(listener, bilibi_web::router(app)).await;
    let _ = stop.send(true);
    let _ = worker.await;
    Ok(served?)
}
