//! A library file outlives the installation's Artifact expiry, on a
//! disposable migrated database (#582).
//!
//! With `artifacts.retention_days` at its minimum (1) and the tenant's
//! `core.document.retention_days` at its default (0), stores two files and
//! attaches one. The attached file's Artifact must have no expiry and no
//! retain-until (an audited `artifacts.kept`), the unattached one keeps the
//! installation expiry, and after that expiry passes the attached file still
//! downloads while the unattached one is unreadable. Deleting the document
//! still removes its file. Exits non-zero when a check fails.
//!
//! On a migrated database that holds the walkthrough fixture and that
//! `bilibi-web seed-production` then seeded:
//!
//! `BILIBI_DOCUMENT_KEEP_PROOF=disposable BILIBI_DOCUMENT_KEEP_ROOT=/an/empty/folder
//! DATABASE_URL=… cargo run -p bilibi-document --example keep-file-proof --locked`
use bilibi_database::pool;
use bilibi_document::{Documents, Draft, Upload, SERVICE};
use bilibi_media::{Media, ScanFacts, ScanFuture, Scanner, Verdict};
use bilibi_queue::{Config, Executor};
use bilibi_tenancy::Scope;
use serde_json::json;
use std::time::Duration;
use std::{os::unix::fs::PermissionsExt, sync::Arc};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Fixture scanner, not a production adapter.
struct Clean;
impl Scanner for Clean {
    fn name(&self) -> &str {
        "keep-file-proof"
    }
    fn version(&self) -> String {
        "1.0 (example)".into()
    }
    fn scan<'a>(&'a self, _bytes: &'a [u8], _facts: ScanFacts<'a>) -> ScanFuture<'a> {
        Box::pin(async { Verdict::Clean })
    }
}

/// Queue's executor in process, as a deployed host runs it.
fn executor(
    app: &bilibi_web::App,
) -> (
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let executor = Executor::from_services(
        &app.services,
        Config {
            concurrency: 2,
            poll_interval: Duration::from_millis(200),
            ..Config::default()
        },
    )
    .expect("the Queue executor");
    (
        stop,
        tokio::spawn(async move {
            let _ = executor.run(stopped).await;
        }),
    )
}

fn check(ok: bool, what: &str) -> Result<()> {
    if ok {
        println!("ok   {what}");
        Ok(())
    } else {
        Err(format!("FAIL {what}").into())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DOCUMENT_KEEP_PROOF").as_deref() != Ok("disposable") {
        return Err("set BILIBI_DOCUMENT_KEEP_PROOF=disposable and DATABASE_URL to a disposable migrated database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let root = std::path::PathBuf::from(std::env::var("BILIBI_DOCUMENT_KEEP_ROOT")?);
    bilibi_walkthrough_fixture::install(&db).await?;
    // A workplace for the administrator: files can only be kept by someone
    // signed in to a company.
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'keep_proof_admin','Keep Proof Admin','human' WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
              u AS (UPDATE users SET person_id=p.id FROM p, user_identities i WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
         INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35566")?;
    // The installation's Artifact expiry at its minimum: one day, as an
    // operator might set it for exports.
    db.get().await?.execute(
        "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES('artifacts.retention_days',0,'global',0,'1'::jsonb,false) ON CONFLICT (key,tenant_id,scope_type,scope_id) DO UPDATE SET value = excluded.value",
        &[],
    ).await?;
    for (key, folder) in [
        ("artifacts.storage_root", "artifacts"),
        ("media.disks", "media"),
    ] {
        let path = root.join(folder);
        std::fs::DirBuilder::new().recursive(true).create(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        let value = if key == "media.disks" {
            json!({"local": path})
        } else {
            json!(path)
        };
        db.get().await?.execute(
            "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES($1,0,'global',0,$2,false) ON CONFLICT (key,tenant_id,scope_type,scope_id) DO UPDATE SET value = excluded.value",
            &[&key, &value],
        ).await?;
    }
    app.services
        .get::<Media>("base/media.store")?
        .bind_scanner(Arc::new(Clean))?;
    let documents = app.services.get::<Documents>(SERVICE)?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let scope: Scope = app.security.guard(&login.token).await?;
    let tenant = scope.tenant();

    let (stop, worker) = executor(&app);
    let mut stored = Vec::new();
    for name in ["keep-attached.txt", "keep-unattached.txt"] {
        stored.push(
            documents
                .store_file(
                    &scope,
                    Upload {
                        bytes: b"Keep proof\n",
                        file_name: name,
                    },
                )
                .await?,
        );
    }
    let (attached, unattached) = (stored[0].artifact_id.clone(), stored[1].artifact_id.clone());
    let mut client = db.get().await?;
    let mut tx = app
        .mutations
        .begin(&mut client, scope.mutation_context("core.document.create"))
        .await?;
    let document = documents
        .create(
            &mut tx,
            &scope,
            &Draft {
                artifact_id: Some(attached.clone()),
                ..Draft::default()
            },
        )
        .await?;
    tx.commit().await?;

    let kept: bool = client.query_one(
        "SELECT expires_at IS NULL AND retain_until IS NULL AND legal_hold_at IS NULL FROM base_artifacts WHERE public_id=$1",
        &[&attached],
    ).await?.get(0);
    check(kept, "the attached file has no expiry and no retain-until")?;
    let audited: i64 = client.query_one(
        "SELECT count(*) FROM base_audit_actions WHERE tenant_id=$1 AND event='artifacts.kept' AND payload->>'artifact_id'=$2",
        &[&tenant, &attached],
    ).await?.get(0);
    check(audited == 1, "keeping it is audited as artifacts.kept")?;
    let expiring: bool = client.query_one(
        "SELECT expires_at BETWEEN now()+interval '23 hours' AND now()+interval '25 hours' FROM base_artifacts WHERE public_id=$1",
        &[&unattached],
    ).await?.get(0);
    check(
        expiring,
        "the unattached file keeps the installation's one-day expiry",
    )?;

    // The day passes.
    client.execute(
        "UPDATE base_artifacts SET expires_at=created_at+interval '1 microsecond' WHERE expires_at IS NOT NULL AND owner_id='core.document' AND tenant_id=$1",
        &[&tenant],
    ).await?;
    let file = documents.read_file(&scope, document.id, 1).await;
    check(
        file.as_ref().is_ok_and(|f| f.bytes == b"Keep proof\n"),
        "after the installation expiry the attached file still downloads",
    )?;
    let expired: bool = client
        .query_one(
            "SELECT expires_at <= now() FROM base_artifacts WHERE public_id=$1",
            &[&unattached],
        )
        .await?
        .get(0);
    check(expired, "the unattached file has expired")?;

    let document = documents.get(&scope, document.id).await?;
    let mut tx = app
        .mutations
        .begin(&mut client, scope.mutation_context("core.document.delete"))
        .await?;
    documents
        .delete(&mut tx, &scope, document.id, document.version)
        .await?;
    tx.commit().await?;
    let mut gone = false;
    for _ in 0..150 {
        gone = client
            .query_one(
                "SELECT deleted_at IS NOT NULL FROM base_artifacts WHERE public_id=$1",
                &[&attached],
            )
            .await?
            .get(0);
        if gone {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = stop.send(true);
    let _ = worker.await;
    check(gone, "deleting the document still deletes its kept file")?;
    println!("\na library file is kept until its document is deleted, whatever the installation's Artifact expiry");
    Ok(())
}
