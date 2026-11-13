//! Files stored but never attached are discarded, on a disposable migrated
//! database (#581).
//!
//! Stores two files and attaches only the second. Each store must queue its
//! delayed `core/document-file-discard` job, a day out, as the
//! `document-file-cleaner` principal. The jobs are then brought forward and
//! Queue's executor runs them: the unattached file's Artifact is deleted and
//! purged and its ledger row removed (audited as the principal), so it can no
//! longer be attached; the attached file and its document are untouched.
//! Exits non-zero when a check fails.
//!
//! On a migrated database that holds the walkthrough fixture and that
//! `bilibi-web seed-production` then seeded:
//!
//! `BILIBI_DOCUMENT_DISCARD_PROOF=disposable BILIBI_DOCUMENT_DISCARD_ROOT=/an/empty/folder
//! DATABASE_URL=… cargo run -p bilibi-document --example discard-file-proof --locked`
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
        "discard-file-proof"
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
    if std::env::var("BILIBI_DOCUMENT_DISCARD_PROOF").as_deref() != Ok("disposable") {
        return Err("set BILIBI_DOCUMENT_DISCARD_PROOF=disposable and DATABASE_URL to a disposable migrated database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let root = std::path::PathBuf::from(std::env::var("BILIBI_DOCUMENT_DISCARD_ROOT")?);
    bilibi_walkthrough_fixture::install(&db).await?;
    // A workplace for the administrator: files can only be kept by someone
    // signed in to a company.
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'discard_proof_admin','Discard Proof Admin','human' WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
              u AS (UPDATE users SET person_id=p.id FROM p, user_identities i WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
         INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35565")?;
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
    for name in ["discard-abandoned.txt", "discard-attached.txt"] {
        stored.push(
            documents
                .store_file(
                    &scope,
                    Upload {
                        bytes: b"Discard proof\n",
                        file_name: name,
                    },
                )
                .await?,
        );
    }
    let (abandoned, attached) = (stored[0].artifact_id.clone(), stored[1].artifact_id.clone());
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

    let ledger = |artifact: &str| {
        let artifact = artifact.to_owned();
        let db = db.clone();
        async move {
            Ok::<_, Box<dyn std::error::Error>>(
                db.get()
                    .await?
                    .query_opt(
                        "SELECT id,state FROM core_document_files WHERE tenant_id=$1 AND artifact_id=$2",
                        &[&tenant, &artifact],
                    )
                    .await?
                    .map(|row| (row.get::<_, i64>(0), row.get::<_, String>(1))),
            )
        }
    };
    let (abandoned_id, _) = ledger(&abandoned).await?.ok_or("no ledger row")?;
    let (attached_id, _) = ledger(&attached).await?.ok_or("no ledger row")?;
    for id in [abandoned_id, attached_id] {
        let jobs: i64 = client.query_one(
            "SELECT count(*) FROM base_queue_jobs WHERE tenant_id=$1 AND worker_id='core/document-file-discard' AND args->>'file'=$2::text AND state='scheduled' AND system_principal='document-file-cleaner' AND available_at > now() + interval '23 hours'",
            &[&tenant, &id.to_string()],
        ).await?.get(0);
        check(
            jobs == 1,
            &format!("file {id}: storing it queued its discard a day out"),
        )?;
    }

    // A day passes.
    client.execute(
        "UPDATE base_queue_jobs SET available_at=now() WHERE tenant_id=$1 AND worker_id='core/document-file-discard'",
        &[&tenant],
    ).await?;
    let mut settled = false;
    for _ in 0..150 {
        let open: i64 = client.query_one(
            "SELECT count(*) FROM base_queue_jobs WHERE tenant_id=$1 AND worker_id='core/document-file-discard' AND state NOT IN ('completed','cancelled','discarded')",
            &[&tenant],
        ).await?.get(0);
        if open == 0 {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = stop.send(true);
    let _ = worker.await;
    check(settled, "every discard job finished")?;
    let completed: i64 = client.query_one(
        "SELECT count(*) FROM base_queue_jobs WHERE tenant_id=$1 AND worker_id='core/document-file-discard' AND state='completed'",
        &[&tenant],
    ).await?.get(0);
    check(completed == 2, "both discard jobs completed")?;

    check(
        ledger(&abandoned).await?.is_none(),
        &format!("file {abandoned_id}: the unattached ledger row is gone"),
    )?;
    let purged: bool = client.query_one(
        "SELECT deleted_at IS NOT NULL AND purged_at IS NOT NULL FROM base_artifacts WHERE public_id=$1",
        &[&abandoned],
    ).await?.get(0);
    check(
        purged,
        &format!("file {abandoned_id}: its Artifact is deleted and its bytes purged"),
    )?;
    let audited: i64 = client.query_one(
        "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type='core.document.file' AND auditable_id=$2::text AND event='deleted' AND system_principal='document-file-cleaner'",
        &[&tenant, &abandoned_id.to_string()],
    ).await?.get(0);
    check(
        audited == 1,
        &format!("file {abandoned_id}: its removal is audited as System · document-file-cleaner"),
    )?;
    let mut client = db.get().await?;
    let mut tx = app
        .mutations
        .begin(&mut client, scope.mutation_context("core.document.create"))
        .await?;
    let late = documents
        .create(
            &mut tx,
            &scope,
            &Draft {
                artifact_id: Some(abandoned.clone()),
                ..Draft::default()
            },
        )
        .await;
    drop(tx);
    check(
        late.is_err(),
        &format!("file {abandoned_id}: it can no longer be attached"),
    )?;

    check(
        ledger(&attached)
            .await?
            .is_some_and(|(_, state)| state == "attached"),
        &format!("file {attached_id}: the attached ledger row is kept"),
    )?;
    let live: bool = client.query_one(
        "SELECT deleted_at IS NULL AND ready_at IS NOT NULL FROM base_artifacts WHERE public_id=$1",
        &[&attached],
    ).await?.get(0);
    check(live, &format!("file {attached_id}: its Artifact is kept"))?;
    check(
        documents.get(&scope, document.id).await.is_ok(),
        &format!("document {}: still readable", document.id),
    )?;
    println!("\na file never attached is discarded a day after it was stored, and an attached one is kept");
    Ok(())
}
