//! Disposable proof that the Retention due view costs the same few statements
//! however many managed files it checks (#586). Before, Base authorized every
//! file separately and the Documents owner answered each with about eight
//! statements, so a records manager's view grew by that much per file.
//!
//! The program talks to PostgreSQL through a small relay of its own that counts
//! the statements the client sends (each Execute and simple Query message). It
//! files clean, retained file documents through the facade, then moves their
//! retain-until into the past and places one legal hold in SQL with the
//! retention guard switched off, the only writes outside the facade (Base
//! refuses to shorten retention). Run after migrate, fixture and
//! seed-production on a dedicated database:
//!
//! `BILIBI_DOCUMENT_RETENTION_PROOF=disposable BILIBI_DOCUMENT_RETENTION_ROOT=… DATABASE_URL=…
//! cargo run -p bilibi-document --example retention-due-proof --locked`
use bilibi_database::pool;
use bilibi_document::{Documents, Draft, Filter, ScanStatus, Source, Upload, SERVICE};
use bilibi_media::{Media, ScanFacts, ScanFuture, Scanner, Verdict};
use bilibi_queue::{Config, Executor};
use bilibi_tenancy::Scope;
use serde_json::json;
use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Due documents filed in each round; the second round doubles the set.
const ROUND: usize = 40;

struct CleanScanner;
impl Scanner for CleanScanner {
    fn name(&self) -> &str {
        "document-retention-proof"
    }
    fn version(&self) -> String {
        "1".into()
    }
    fn scan<'a>(&'a self, _bytes: &'a [u8], _facts: ScanFacts<'a>) -> ScanFuture<'a> {
        Box::pin(async { Verdict::Clean })
    }
}

/// Copies the client's messages to the server, counting each statement.
async fn relay(
    mut from: impl AsyncRead + Unpin,
    mut to: impl AsyncWrite + Unpin,
    statements: &AtomicU64,
) -> std::io::Result<()> {
    // The startup message has a length and no type byte.
    let mut length = [0u8; 4];
    from.read_exact(&mut length).await?;
    let mut body = vec![0; (u32::from_be_bytes(length) as usize).saturating_sub(4)];
    from.read_exact(&mut body).await?;
    to.write_all(&length).await?;
    to.write_all(&body).await?;
    loop {
        let mut head = [0u8; 5];
        from.read_exact(&mut head).await?;
        if matches!(head[0], b'E' | b'Q') {
            statements.fetch_add(1, Ordering::Relaxed);
        }
        let size = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let mut message = head.to_vec();
        message.resize(1 + size.max(4), 0);
        from.read_exact(&mut message[5..]).await?;
        to.write_all(&message).await?;
    }
}

/// A local port that forwards to the database and counts what is sent.
async fn counting_relay(url: &str, statements: Arc<AtomicU64>) -> Result<String> {
    let config: tokio_postgres::Config = url.parse()?;
    let host = match config.get_hosts().first() {
        Some(tokio_postgres::config::Host::Tcp(host)) => host.clone(),
        _ => return Err("DATABASE_URL must name a TCP host".into()),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let (host, statements) = (host.clone(), statements.clone());
            tokio::spawn(async move {
                let Ok(server) = TcpStream::connect((host.as_str(), port)).await else {
                    return;
                };
                // Small messages relayed one by one must not wait for an ACK.
                if client.set_nodelay(true).is_err() || server.set_nodelay(true).is_err() {
                    return;
                }
                let (client_read, mut client_write) = client.into_split();
                let (mut server_read, server_write) = server.into_split();
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
                });
                let _ = relay(client_read, server_write, &statements).await;
            });
        }
    });
    let (credentials, rest) = url.split_once('@').ok_or("DATABASE_URL needs user@host")?;
    let (_, database) = rest
        .split_once('/')
        .ok_or("DATABASE_URL needs a database")?;
    Ok(format!("{credentials}@127.0.0.1:{local}/{database}"))
}

struct Proof {
    app: bilibi_web::App,
    documents: Arc<Documents>,
    scope: Scope,
    statements: Arc<AtomicU64>,
    failures: usize,
}
impl Proof {
    fn check(&mut self, ok: bool, what: &str) {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            self.failures += 1;
        }
    }
    /// Clean file documents filed by the administrator, retained one day.
    /// Media scans each upload in a Queue job, so an executor runs meanwhile;
    /// it stops before anything is counted, since the relay sees its polls.
    async fn file_all(&self, names: &[String]) -> Result<Vec<i64>> {
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(
            Executor::from_services(
                &self.app.services,
                Config {
                    concurrency: 2,
                    poll_interval: Duration::from_millis(100),
                    ..Config::default()
                },
            )?
            .run(stopped),
        );
        let mut ids = Vec::new();
        for name in names {
            ids.push(self.file(name).await?);
        }
        stop.send(true)?;
        worker.await??;
        Ok(ids)
    }
    async fn file(&self, name: &str) -> Result<i64> {
        let stored = self
            .documents
            .store_file(
                &self.scope,
                Upload {
                    file_name: "record.txt",
                    bytes: format!("retention proof {name}").as_bytes(),
                },
            )
            .await
            .map_err(|e| format!("store {name}: {e:?}"))?;
        if stored.scan_status != ScanStatus::Clean {
            return Err(format!("{name} is not clean").into());
        }
        let mut client = self.app.db.get().await?;
        let mut tx = self
            .app
            .mutations
            .begin(
                &mut client,
                self.scope.mutation_context("core.document.retention_proof"),
            )
            .await?;
        let document = self
            .documents
            .create(
                &mut tx,
                &self.scope,
                &Draft {
                    name: Some(name.into()),
                    source: Source::Internal,
                    artifact_id: Some(stored.artifact_id),
                    ..Draft::default()
                },
            )
            .await
            .map_err(|e| format!("create {name}: {e:?}"))?;
        tx.commit().await?;
        Ok(document.id)
    }
    /// Moves the documents' files past retain-until (and holds them) in SQL.
    async fn age(&self, ids: &[i64], hold: bool) -> Result<()> {
        let mut client = self.app.db.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute(
            "ALTER TABLE base_artifacts DISABLE TRIGGER base_artifacts_retention_guard",
        )
        .await?;
        let changed = tx.execute(
            "UPDATE base_artifacts a SET retain_until=clock_timestamp()-interval '1 day',
                legal_hold_at=CASE WHEN $3 THEN clock_timestamp() END
             FROM core_document_revisions r
             WHERE r.tenant_id=$1 AND r.document_id=ANY($2) AND a.public_id=r.artifact_id AND a.retain_until IS NOT NULL",
            &[&self.scope.tenant(), &ids, &hold],
        ).await?;
        tx.batch_execute(
            "ALTER TABLE base_artifacts ENABLE TRIGGER base_artifacts_retention_guard",
        )
        .await?;
        tx.commit().await?;
        if changed != ids.len() as u64 {
            return Err(format!("aged {changed} files for {} documents", ids.len()).into());
        }
        Ok(())
    }
    /// One Retention due page and the statements it sent.
    async fn due(&self, page: i64, page_size: i64) -> Result<(bilibi_document::Page, u64)> {
        let filter = Filter {
            retention_due: true,
            page,
            page_size,
            ..Filter::default()
        };
        let before = self.statements.load(Ordering::Relaxed);
        let result = self
            .documents
            .list(&self.scope, &filter)
            .await
            .map_err(|e| format!("due list: {e:?}"))?;
        Ok((result, self.statements.load(Ordering::Relaxed) - before))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DOCUMENT_RETENTION_PROOF").as_deref() != Ok("disposable") {
        return Err("use a disposable migrated and seeded database".into());
    }
    let statements = Arc::new(AtomicU64::new(0));
    let url = counting_relay(&std::env::var("DATABASE_URL")?, statements.clone()).await?;
    let db = pool(&url)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'retention_proof_admin','Retention Proof Admin','human'
            WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id
                WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
          u AS (UPDATE users SET person_id=p.id FROM p,user_identities i
            WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
          INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING
        ; INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted)
          VALUES('core.document.retention_days',1,'tenant',1,'1'::jsonb,false)
          ON CONFLICT (key,tenant_id,scope_type,scope_id) DO UPDATE SET value='1'::jsonb",
    ).await?;
    let root = std::path::PathBuf::from(std::env::var("BILIBI_DOCUMENT_RETENTION_ROOT")?);
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
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:4475")?;
    app.services
        .get::<Media>("base/media.store")?
        .bind_scanner(Arc::new(CleanScanner))?;
    let documents = app.services.get::<Documents>(SERVICE)?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let scope: Scope = app.security.guard(&login.token).await?;
    let mut proof = Proof {
        app,
        documents,
        scope,
        statements,
        failures: 0,
    };

    // Controls: still retained, and past retain-until but on legal hold.
    let controls = proof
        .file_all(&[
            "Retention proof: still retained".into(),
            "Retention proof: on legal hold".into(),
        ])
        .await?;
    let (retained, held) = (controls[0], controls[1]);
    proof.age(&[held], true).await?;
    let mut due = BTreeSet::new();
    let mut counts = Vec::new();
    for round in 0..2 {
        let names: Vec<String> = (0..ROUND)
            .map(|i| format!("Retention proof: due {round}-{i}"))
            .collect();
        let ids = proof.file_all(&names).await?;
        proof.age(&ids, false).await?;
        due.extend(ids);
        let (page, sent) = proof.due(1, 300).await?;
        let listed: BTreeSet<i64> = page.entries.iter().map(|d| d.id).collect();
        println!(
            "Retention due over {} due files: {} statements",
            due.len(),
            sent
        );
        proof.check(
            listed == due && page.total == due.len() as i64,
            &format!(
                "the view lists exactly the {} due documents (listed {}, total {})",
                due.len(),
                listed.len(),
                page.total
            ),
        );
        proof.check(
            !listed.contains(&retained) && !listed.contains(&held),
            "a still-retained and a held document are not due",
        );
        proof.check(
            due.iter().all(|id| page.retention_due_at.contains_key(id)),
            "every due document has its retain-until",
        );
        counts.push(sent);
    }
    proof.check(
        counts[1] <= counts[0] + 2,
        &format!(
            "doubling the due files ({} -> {}) does not add statements ({} -> {})",
            ROUND,
            2 * ROUND,
            counts[0],
            counts[1]
        ),
    );
    proof.check(
        counts[1] < 2 * ROUND as u64,
        &format!(
            "one render sends fewer statements ({}) than it checks files ({})",
            counts[1],
            2 * ROUND
        ),
    );
    let (second, _) = proof.due(2, 25).await?;
    proof.check(
        second.entries.len() == 25
            && second.entries.iter().all(|d| due.contains(&d.id))
            && second.total == due.len() as i64,
        "the second page of 25 holds due documents only",
    );
    if proof.failures > 0 {
        return Err(format!("{} checks failed", proof.failures).into());
    }
    println!("all checks passed");
    Ok(())
}
