//! A delete survives a restart, on a disposable migrated database (#580).
//!
//! Deletes two file documents while no Queue executor runs, as if the process
//! stopped right after the delete committed, and checks that each delete left
//! its durable `core/document-delete` job and kept its files. A legal hold is
//! then placed on the second document's file. Starting the executor must
//! remove the first document with its file and every row, as the
//! `document-deleter` principal, and show the second again with its file
//! kept. Exits non-zero when a check fails.
//!
//! On a migrated database that holds the walkthrough fixture and that
//! `bilibi-web seed-production` then seeded:
//!
//! `BILIBI_DOCUMENT_DELETE_PROOF=disposable BILIBI_DOCUMENT_DELETE_ROOT=/an/empty/folder
//! DATABASE_URL=… cargo run -p bilibi-document --example delete-job-proof --locked`
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
        "delete-job-proof"
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
    if std::env::var("BILIBI_DOCUMENT_DELETE_PROOF").as_deref() != Ok("disposable") {
        return Err("set BILIBI_DOCUMENT_DELETE_PROOF=disposable and DATABASE_URL to a disposable migrated database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let root = std::path::PathBuf::from(std::env::var("BILIBI_DOCUMENT_DELETE_ROOT")?);
    bilibi_walkthrough_fixture::install(&db).await?;
    // A workplace for the administrator: files can only be kept by someone
    // signed in to a company.
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'delete_job_admin','Delete Job Admin','human' WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
              u AS (UPDATE users SET person_id=p.id FROM p, user_identities i WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
         INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35564")?;
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

    // Two clean file documents (Media's scan runs on the Queue), then the
    // Queue stops, as if the process did, before they are deleted.
    let (stop, worker) = executor(&app);
    let mut filed = Vec::new();
    for name in ["delete-job-gone.txt", "delete-job-held.txt"] {
        let stored = documents
            .store_file(
                &scope,
                Upload {
                    bytes: b"Delete job proof\n",
                    file_name: name,
                },
            )
            .await?;
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
                    artifact_id: Some(stored.artifact_id.clone()),
                    ..Draft::default()
                },
            )
            .await?;
        tx.commit().await?;
        filed.push((document.id, stored.artifact_id));
    }
    let _ = stop.send(true);
    let _ = worker.await;
    let mut deleted = Vec::new();
    for (id, file) in filed {
        let document = documents.get(&scope, id).await?;
        let mut client = db.get().await?;
        let mut tx = app
            .mutations
            .begin(&mut client, scope.mutation_context("core.document.delete"))
            .await?;
        documents
            .delete(&mut tx, &scope, id, document.version)
            .await?;
        tx.commit().await?;
        deleted.push((id, file));
    }
    let (gone, gone_file) = deleted[0].clone();
    let (held, held_file) = deleted[1].clone();
    let client = db.get().await?;
    for (id, file) in &deleted {
        let jobs: i64 = client.query_one(
            "SELECT count(*) FROM base_queue_jobs WHERE tenant_id=$1 AND worker_id='core/document-delete' AND args->>'document'=$2::text AND state='available' AND system_principal='document-deleter'",
            &[&tenant, &id.to_string()],
        ).await?.get(0);
        check(
            jobs == 1,
            &format!("document {id}: the delete committed its durable job"),
        )?;
        let deleting: bool = client
            .query_one(
                "SELECT deleting_at IS NOT NULL FROM core_documents WHERE tenant_id=$1 AND id=$2",
                &[&tenant, id],
            )
            .await?
            .get(0);
        check(
            deleting,
            &format!("document {id}: marked as deleting and hidden"),
        )?;
        check(
            documents.get(&scope, *id).await.is_err(),
            &format!("document {id}: unreadable while deleting"),
        )?;
        let kept: bool = client
            .query_one(
                "SELECT deleted_at IS NULL FROM base_artifacts WHERE public_id=$1",
                &[file],
            )
            .await?
            .get(0);
        check(
            kept,
            &format!("document {id}: its file is kept until the job runs"),
        )?;
    }
    // A hold placed after the delete committed, before the job runs.
    client.execute(
        "UPDATE base_artifacts SET retain_until=now()+interval '1 day',legal_hold_at=now(),expires_at=NULL WHERE public_id=$1",
        &[&held_file],
    ).await?;

    // The process is back: the executor finishes what the deletes started.
    let (stop, worker) = executor(&app);
    let mut settled = false;
    for _ in 0..150 {
        let open: i64 = client.query_one(
            "SELECT count(*) FROM base_queue_jobs WHERE tenant_id=$1 AND worker_id='core/document-delete' AND state NOT IN ('completed','cancelled','discarded')",
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
    check(settled, "every delete job finished")?;
    let completed: i64 = client.query_one(
        "SELECT count(*) FROM base_queue_jobs WHERE tenant_id=$1 AND worker_id='core/document-delete' AND state='completed'", &[&tenant],
    ).await?.get(0);
    check(completed == 2, "both delete jobs completed")?;

    let rows: i64 = client.query_one(
        "SELECT (SELECT count(*) FROM core_documents WHERE tenant_id=$1 AND id=$2) + (SELECT count(*) FROM core_document_revisions WHERE tenant_id=$1 AND document_id=$2) + (SELECT count(*) FROM core_document_access WHERE tenant_id=$1 AND document_id=$2) + (SELECT count(*) FROM core_document_files WHERE tenant_id=$1 AND artifact_id=$3)",
        &[&tenant, &gone, &gone_file],
    ).await?.get(0);
    check(
        rows == 0,
        &format!("document {gone}: the document, its revision, grant and ledger row are gone"),
    )?;
    let purged: bool = client.query_one(
        "SELECT deleted_at IS NOT NULL AND purged_at IS NOT NULL FROM base_artifacts WHERE public_id=$1", &[&gone_file],
    ).await?.get(0);
    check(
        purged,
        &format!("document {gone}: its file is deleted and its bytes purged"),
    )?;
    let by_deleter: i64 = client.query_one(
        "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type='core.document.document' AND auditable_id=$2::text AND event='deleted' AND actor_type='system' AND system_principal='document-deleter'",
        &[&tenant, &gone.to_string()],
    ).await?.get(0);
    check(
        by_deleter == 1,
        &format!("document {gone}: its removal is audited as System · document-deleter"),
    )?;

    let document = documents.get(&scope, held).await;
    check(
        document.as_ref().is_ok_and(|d| d.deleting_at.is_none()),
        &format!("document {held}: shown again, because a hold keeps its file"),
    )?;
    let kept: bool = client.query_one(
        "SELECT deleted_at IS NULL AND legal_hold_at IS NOT NULL FROM base_artifacts WHERE public_id=$1", &[&held_file],
    ).await?.get(0);
    check(kept, &format!("document {held}: its held file is kept"))?;
    let restored: i64 = client.query_one(
        "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type='core.document.document' AND auditable_id=$2::text AND event='updated' AND system_principal='document-deleter' AND (old_values->>'deleting')::boolean AND NOT (new_values->>'deleting')::boolean",
        &[&tenant, &held.to_string()],
    ).await?.get(0);
    check(
        restored == 1,
        &format!("document {held}: clearing the deleting mark is audited"),
    )?;
    println!("\na delete interrupted after its commit finishes once the Queue runs, and a hold placed meanwhile keeps the document");
    Ok(())
}
