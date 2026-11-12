//! Audit coverage of every document action, on a disposable migrated database.
//!
//! Performs each action through the real `Documents` facade, then fails unless
//! the action wrote at least one audited row (a `base/audit` mutation of a
//! `core.document.*` kind, or an Artifacts action for a file read) and the
//! per-document query in the module README finds every document row the action
//! wrote. No mocks and no test runner; prints one line per action.
//!
//! Files need private storage and a scanner, which a plain build carries
//! neither of: the example makes a private folder under
//! `BILIBI_DOCUMENT_AUDIT_ROOT`, points the settings at it, binds a scanner
//! that calls everything clean (it exists only in this executable) and runs
//! Queue's executor in process, as a deployed host would.
//!
//! On a migrated database that holds the walkthrough fixture and that
//! `bilibi-web seed-production` then seeded (the pick-lists a document is filed
//! under):
//!
//! `BILIBI_DOCUMENT_AUDIT=disposable BILIBI_DOCUMENT_AUDIT_ROOT=/an/empty/folder
//! DATABASE_URL=… cargo run -p bilibi-document --example audit-coverage --locked`
use bilibi_database::{pool, Db};
use bilibi_document::{
    CustomerShareInput, Documents, Draft, MarkPatch, MergeSource, Patch, Recipient, Reference,
    RevisionInput, ShareInput, Source, Upload, SERVICE,
};
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
        "audit-coverage"
    }
    fn version(&self) -> String {
        "1.0 (example)".into()
    }
    fn scan<'a>(&'a self, _bytes: &'a [u8], _facts: ScanFacts<'a>) -> ScanFuture<'a> {
        Box::pin(async { Verdict::Clean })
    }
}

/// The rows the per-document query in the README finds for `document`.
const FOR_DOCUMENT: &str = "SELECT id FROM base_audit_mutations
    WHERE tenant_id = $1 AND auditable_type LIKE 'core.document.%'
      AND ((auditable_type = 'core.document.document' AND auditable_id = $2::text)
        OR new_values ->> 'document_id' = $2::text OR old_values ->> 'document_id' = $2::text)";

/// What one action left behind, read from the audit tables.
struct Trail<'a> {
    db: &'a Db,
    tenant: i64,
    /// The highest audit ids before the action: `(mutation, action)`.
    mark: (i64, i64),
    gaps: Vec<String>,
}
impl Trail<'_> {
    async fn start(&mut self) -> Result<()> {
        let client = self.db.get().await?;
        let row = client
            .query_one(
                "SELECT coalesce((SELECT max(id) FROM base_audit_mutations), 0), coalesce((SELECT max(id) FROM base_audit_actions), 0)",
                &[],
            )
            .await?;
        self.mark = (row.get(0), row.get(1));
        Ok(())
    }

    /// Reports one action. `documents` are the documents it touched; a
    /// `core.document.file` row (a ledger entry for bytes not yet attached)
    /// names no document and is not held to the query.
    async fn finish(&mut self, action: &str, documents: &[i64]) -> Result<()> {
        let client = self.db.get().await?;
        let mutations = client
            .query(
                "SELECT id, auditable_type, event FROM base_audit_mutations WHERE tenant_id = $1 AND id > $2 AND auditable_type LIKE 'core.document.%' ORDER BY id",
                &[&self.tenant, &self.mark.0],
            )
            .await?;
        let ids: Vec<String> = documents.iter().map(i64::to_string).collect();
        let actions: i64 = client
            .query_one(
                "SELECT count(*) FROM base_audit_actions WHERE tenant_id = $1 AND id > $2 AND ((event LIKE 'artifacts.%' AND payload ->> 'owner_id' = 'core.document') OR (event = 'core.document.download_refused' AND payload ->> 'document_id' = ANY($3)))",
                &[&self.tenant, &self.mark.1, &ids],
            )
            .await?
            .get(0);
        let mut found = std::collections::BTreeSet::new();
        for document in documents {
            for row in client
                .query(FOR_DOCUMENT, &[&self.tenant, &document.to_string()])
                .await?
            {
                found.insert(row.get::<_, i64>(0));
            }
        }
        let mut kinds = Vec::new();
        let mut missed = Vec::new();
        for row in &mutations {
            let (id, kind, event): (i64, String, String) = (row.get(0), row.get(1), row.get(2));
            kinds.push(format!(
                "{} {event}",
                kind.trim_start_matches("core.document.")
            ));
            if kind != "core.document.file" && !found.contains(&id) {
                missed.push(format!("{kind} {event} (#{id})"));
            }
        }
        let evidence = mutations.len() as i64 + actions;
        println!(
            "{action:<22} {} audited change(s), {actions} file action(s): {}",
            mutations.len(),
            kinds.join(", ")
        );
        if evidence == 0 {
            self.gaps
                .push(format!("{action}: left no audited change or event"));
        }
        if !missed.is_empty() {
            self.gaps.push(format!(
                "{action}: the per-document query misses {}",
                missed.join(", ")
            ));
        }
        Ok(())
    }
}

/// The transaction every action runs in, as a page or an agent would.
macro_rules! write {
    ($app:expr, $scope:expr, $event:expr, |$tx:ident| $body:expr) => {{
        let mut client = $app.db.get().await?;
        let mut $tx = $app
            .mutations
            .begin(&mut client, $scope.mutation_context($event))
            .await?;
        let out = $body;
        $tx.commit().await?;
        out
    }};
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DOCUMENT_AUDIT").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_DOCUMENT_AUDIT=disposable and DATABASE_URL to a disposable migrated database"
                .into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let root = std::path::PathBuf::from(std::env::var("BILIBI_DOCUMENT_AUDIT_ROOT")?);
    bilibi_walkthrough_fixture::install(&db).await?;
    // A workplace for the administrator: files can only be kept by someone
    // signed in to a company.
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'audit_coverage_admin','Audit Coverage Admin','human' WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
              u AS (UPDATE users SET person_id=p.id FROM p, user_identities i WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
         INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35563")?;
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
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(
        Executor::from_services(
            &app.services,
            Config {
                concurrency: 2,
                poll_interval: std::time::Duration::from_millis(200),
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
    let viewer = db
        .get()
        .await?
        .query_one(
            "SELECT u.id FROM users u JOIN user_identities i ON i.id = u.identity_id WHERE u.tenant_id = $1 AND i.email = 'viewer@example.test'",
            &[&scope.tenant()],
        )
        .await?
        .get::<_, i64>(0);
    let mut trail = Trail {
        db: &db,
        tenant: scope.tenant(),
        mark: (0, 0),
        gaps: Vec::new(),
    };
    let link = |name: &str, url: &str| Draft {
        name: Some(name.into()),
        source: Source::External,
        external_url: Some(url.into()),
        related: vec![Reference::new("company", "1")],
        ..Draft::default()
    };

    // Create, which Quick add and Copy also call.
    trail.start().await?;
    let a = write!(app, scope, "core.document.create", |tx| documents
        .create(
            &mut tx,
            &scope,
            &link("Audit coverage A", "https://example.test/a")
        )
        .await?);
    trail.finish("create / quick add", &[a.id]).await?;

    trail.start().await?;
    let a = write!(app, scope, "core.document.update", |tx| documents
        .update(
            &mut tx,
            &scope,
            a.id,
            a.version,
            &Patch {
                name: Some("Audit coverage A, renamed".into()),
                ..Patch::default()
            },
        )
        .await?);
    trail.finish("update", &[a.id]).await?;

    trail.start().await?;
    write!(app, scope, "core.document.add_revision", |tx| documents
        .add_revision(
            &mut tx,
            &scope,
            a.id,
            a.version,
            &RevisionInput {
                external_url: Some("https://example.test/a2".into()),
                change_log: "Second link".into(),
                ..RevisionInput::default()
            },
        )
        .await?);
    trail.finish("add revision", &[a.id]).await?;
    let a = documents.get(&scope, a.id).await?;

    let other = Reference::new("company", "2");
    trail.start().await?;
    write!(app, scope, "core.document.link", |tx| documents
        .link(&mut tx, &scope, a.id, &other)
        .await?);
    trail.finish("link", &[a.id]).await?;
    trail.start().await?;
    write!(app, scope, "core.document.unlink", |tx| documents
        .unlink(&mut tx, &scope, a.id, &other)
        .await?);
    trail.finish("unlink", &[a.id]).await?;

    trail.start().await?;
    write!(app, scope, "core.document.share", |tx| documents
        .share(
            &mut tx,
            &scope,
            a.id,
            &ShareInput {
                user_id: viewer,
                can_download: true,
                can_manage: false,
                can_share: false,
                until: None,
                email: true,
            },
        )
        .await?);
    trail.finish("share, email a link", &[a.id]).await?;
    // The queued delivery settles in the background (sent, failed or
    // suppressed) and is audited on its own.
    trail.start().await?;
    for _ in 0..100 {
        let queued: i64 = db
            .get()
            .await?
            .query_one(
                "SELECT count(*) FROM core_document_emails WHERE tenant_id = $1 AND state = 'queued'",
                &[&scope.tenant()],
            )
            .await?
            .get(0);
        if queued == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    trail.finish("email delivery", &[a.id]).await?;
    let grant = documents
        .access(&scope, a.id)
        .await?
        .into_iter()
        .find(|grant| grant.user_id == viewer)
        .ok_or("the grant was not saved")?;
    trail.start().await?;
    write!(app, scope, "core.document.unshare", |tx| documents
        .unshare(&mut tx, &scope, a.id, grant.id)
        .await?);
    trail.finish("unshare", &[a.id]).await?;

    trail.start().await?;
    write!(app, scope, "core.document.mark", |tx| documents
        .mark(
            &mut tx,
            &scope,
            a.id,
            MarkPatch {
                favourite: Some(true),
                following: Some(true),
            },
        )
        .await?);
    trail.finish("favourite, follow", &[a.id]).await?;

    trail.start().await?;
    let copy = write!(app, scope, "core.document.copy", |tx| documents
        .copy(&mut tx, &scope, a.id, None)
        .await?);
    trail.finish("copy", &[copy.id]).await?;

    let b = write!(app, scope, "core.document.create", |tx| documents
        .create(
            &mut tx,
            &scope,
            &link("Audit coverage B", "https://example.test/b")
        )
        .await?);
    let a = documents.get(&scope, a.id).await?;
    trail.start().await?;
    write!(app, scope, "core.document.merge", |tx| documents
        .merge(
            &mut tx,
            &scope,
            MergeSource {
                id: a.id,
                version: a.version,
            },
            &[MergeSource {
                id: b.id,
                version: b.version,
            }],
        )
        .await?);
    trail.finish("merge", &[a.id, b.id]).await?;

    // A file: store, create, download (which Preview also is), then delete.
    trail.start().await?;
    let stored = documents
        .store_file(
            &scope,
            Upload {
                bytes: b"Audit coverage file\n",
                file_name: "audit-coverage.txt",
            },
        )
        .await?;
    trail.finish("upload", &[]).await?;
    trail.start().await?;
    let filed = write!(app, scope, "core.document.create", |tx| documents
        .create(
            &mut tx,
            &scope,
            &Draft {
                artifact_id: Some(stored.artifact_id.clone()),
                ..Draft::default()
            },
        )
        .await?);
    trail.finish("create with a file", &[filed.id]).await?;
    trail.start().await?;
    documents.read_file(&scope, filed.id, 1).await?;
    trail.finish("download / preview", &[filed.id]).await?;
    let outsider = app
        .user
        .login("viewer@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let outsider: Scope = app.security.guard(&outsider.token).await?;
    trail.start().await?;
    if documents.read_file(&outsider, filed.id, 1).await.is_ok() {
        return Err("a reader with no grant got the file".into());
    }
    trail.finish("refused download", &[filed.id]).await?;

    // Sharing with a portal customer (a company's business customers). The
    // customer's own download and refusals are proved, with their audit, by
    // `customer-share-walkthrough`; the share kept here goes with the delete.
    let company = CustomerShareInput {
        recipient: Recipient::Company(2),
        until: None,
    };
    trail.start().await?;
    let shared = write!(app, scope, "core.document.customer_share", |tx| documents
        .share_customer(&mut tx, &scope, filed.id, &company)
        .await?);
    trail.finish("share with a customer", &[filed.id]).await?;
    trail.start().await?;
    write!(app, scope, "core.document.customer_unshare", |tx| documents
        .unshare_customer(&mut tx, &scope, filed.id, shared.id)
        .await?);
    trail.finish("stop sharing", &[filed.id]).await?;
    write!(app, scope, "core.document.customer_share", |tx| documents
        .share_customer(&mut tx, &scope, filed.id, &company)
        .await?);

    for (action, id) in [("delete (link)", copy.id), ("delete (file)", filed.id)] {
        let document = documents.get(&scope, id).await?;
        trail.start().await?;
        write!(app, scope, "core.document.delete", |tx| documents
            .delete(&mut tx, &scope, id, document.version)
            .await?);
        // A deleting document is already unreadable; its metadata goes once
        // the files are, so wait for the row itself.
        for _ in 0..100 {
            let left: i64 = db
                .get()
                .await?
                .query_one(
                    "SELECT count(*) FROM core_documents WHERE tenant_id = $1 AND id = $2",
                    &[&scope.tenant(), &id],
                )
                .await?
                .get(0);
            if left == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        trail.finish(action, &[id]).await?;
    }

    let _ = stop.send(true);
    let _ = worker.await;
    if trail.gaps.is_empty() {
        println!("\nevery document action left audited evidence the per-document query finds");
        Ok(())
    } else {
        for gap in &trail.gaps {
            eprintln!("GAP {gap}");
        }
        Err(format!("{} audit gap(s)", trail.gaps.len()).into())
    }
}
