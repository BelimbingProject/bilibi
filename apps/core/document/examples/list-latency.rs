//! Documents list, filter and audit-read latency over a large fixture, on a
//! disposable migrated database.
//!
//! Seeds (once) tens of thousands of link documents through the real
//! `Documents` facade, each filed by the administrator, related to one of the
//! walkthrough companies and shared with a mix of grants, then times the
//! Documents list, its filters and the module README's audit queries. No mocks
//! and no test runner; prints one Markdown table row per case.
//!
//! Grants for `viewer@example.test`, the broad reader, by document number `i`
//! (`i % 10`): 0-3 view and download, 4 view without download, 5 ended, 6-9
//! none. The manager has view on every third document, staff view and download
//! on every fifth, and the contractor, the narrow reader, on every hundredth.
//! Every twentieth document has expired, the next one expires within 30 days,
//! and the viewer favourites every fiftieth. An ended grant cannot be made
//! through Share (it refuses a past day), so the fixture shares those until
//! today and then moves their end two days back in SQL, the one write outside
//! the facade.
//!
//! On a migrated database that holds the walkthrough fixture and that
//! `bilibi-web seed-production` then seeded (the pick-lists a document is
//! filed under):
//!
//! `BILIBI_DOCUMENT_LATENCY=disposable DATABASE_URL=… cargo run -p
//! bilibi-document --example list-latency --release --locked`
//!
//! `BILIBI_DOCUMENT_LATENCY_DOCUMENTS` (50000) is the fixture size and
//! `BILIBI_DOCUMENT_LATENCY_RUNS` (50) the timed repetitions of each case. A
//! rerun on the same database only adds missing documents, then measures.
use bilibi_database::{pool, Db};
use bilibi_document::{
    Documents, Draft, ExpiryFilter, Filter, MarkFilter, MarkPatch, Reference, ShareInput, Source,
    SERVICE,
};
use bilibi_tenancy::Scope;
use jiff::{civil::Date, Timestamp, ToSpan};
use std::time::{Duration, Instant};
use tokio_postgres::types::ToSql;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// The fixture's documents are named with this prefix and their number.
const PREFIX: &str = "Latency fixture";
/// One word of each name, so a search for one matches about one in 23.
const WORDS: [&str; 23] = [
    "agreement",
    "certificate",
    "statement",
    "report",
    "policy",
    "notice",
    "form",
    "schedule",
    "plan",
    "minutes",
    "letter",
    "memo",
    "register",
    "licence",
    "permit",
    "manual",
    "checklist",
    "summary",
    "proposal",
    "budget",
    "review",
    "receipt",
    "drawing",
];
/// Walkthrough accounts (tenant 1).
const MANAGER: i64 = 1;
const VIEWER: i64 = 2;
const STAFF: i64 = 3;
const CONTRACTOR: i64 = 7;

/// The module README's per-document audit queries, in its order.
const DOCUMENT_MUTATIONS: &str =
    "SELECT id, occurred_at, auditable_type, event, actor_type, actor_id
FROM base_audit_mutations
WHERE tenant_id = $1 AND auditable_type LIKE 'core.document.%'
  AND ((auditable_type = 'core.document.document' AND auditable_id = $2::text)
    OR new_values ->> 'document_id' = $2::text OR old_values ->> 'document_id' = $2::text)
ORDER BY id";
const DOCUMENT_FILE_READS: &str = "SELECT id, occurred_at, event, actor_type, actor_id
FROM base_audit_actions
WHERE tenant_id = $1 AND event LIKE 'artifacts.%' AND payload ->> 'owner_id' = 'core.document'
  AND payload ->> 'artifact_id' IN (SELECT artifact_id FROM core_document_revisions WHERE tenant_id = $1 AND document_id = $2::text::bigint)
ORDER BY id";
const DOCUMENT_REFUSALS: &str = "SELECT id, occurred_at, actor_type, actor_id, payload ->> 'revision' AS revision, payload ->> 'reason' AS reason
FROM base_audit_actions
WHERE tenant_id = $1 AND event = 'core.document.download_refused' AND payload ->> 'document_id' = $2::text
ORDER BY id";
/// The module README's per-actor audit queries: the newest 100 of each.
const ACTOR_MUTATIONS: &str = "SELECT id, occurred_at, auditable_type, auditable_id, event
FROM base_audit_mutations
WHERE tenant_id = $1 AND actor_type = 'user' AND actor_id = $2 AND auditable_type LIKE 'core.document.%'
ORDER BY occurred_at DESC, id DESC
LIMIT 100";
const ACTOR_ACTIONS: &str = "SELECT id, occurred_at, event, payload
FROM base_audit_actions
WHERE tenant_id = $1 AND actor_type = 'user' AND actor_id = $2
  AND (event LIKE 'core.document.%' OR (event LIKE 'artifacts.%' AND payload ->> 'owner_id' = 'core.document'))
ORDER BY occurred_at DESC, id DESC
LIMIT 100";

/// The transaction every write runs in, as a page or an agent would.
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

fn number(name: &str, default: usize) -> Result<usize> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse()?),
        Err(_) => Ok(default),
    }
}

/// Median, 95th percentile (nearest rank) and maximum, in milliseconds.
fn summary(mut runs: Vec<Duration>) -> (f64, f64, f64) {
    runs.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let rank = |p: f64| ((p * runs.len() as f64).ceil() as usize).clamp(1, runs.len()) - 1;
    (
        ms(runs[rank(0.5)]),
        ms(runs[rank(0.95)]),
        ms(runs[runs.len() - 1]),
    )
}

fn row(case: &str, reader: &str, rows: String, runs: Vec<Duration>) {
    let (median, p95, max) = summary(runs);
    println!("| {case} | {reader} | {rows} | {median:.1} | {p95:.1} | {max:.1} |");
}

struct Fixture {
    app: bilibi_web::App,
    documents: std::sync::Arc<Documents>,
    admin: Scope,
    viewer: Scope,
    today: Date,
}

impl Fixture {
    /// Creates document `i` with its link, expiry, grants and marks.
    async fn seed(&self, i: usize) -> Result<()> {
        let (app, documents, admin) = (&self.app, &self.documents, &self.admin);
        let company = (i * 7) % 62 + 1;
        let expires_on = match i % 20 {
            0 => Some(self.today - 30.days()),
            1 => Some(self.today + 10.days()),
            _ => None,
        };
        let draft = Draft {
            name: Some(format!("{PREFIX} {i:05} {}", WORDS[i % WORDS.len()])),
            source: Source::External,
            external_url: Some(format!("https://example.test/latency/{i}")),
            related: vec![Reference::new("company", company.to_string())],
            expires_on,
            ..Draft::default()
        };
        let document = write!(app, admin, "core.document.create", |tx| documents
            .create(&mut tx, admin, &draft)
            .await
            .map_err(|e| format!("create {i}: {e:?}"))?);
        let mut shares = Vec::new();
        match i % 10 {
            0..=3 => shares.push((VIEWER, true, None)),
            4 => shares.push((VIEWER, false, None)),
            5 => shares.push((VIEWER, true, Some(self.today))),
            _ => {}
        }
        if i.is_multiple_of(3) {
            shares.push((MANAGER, false, None));
        }
        if i.is_multiple_of(5) {
            shares.push((STAFF, true, None));
        }
        if i.is_multiple_of(100) {
            shares.push((CONTRACTOR, true, None));
        }
        for (user_id, can_download, until) in shares {
            let input = ShareInput {
                user_id,
                can_download,
                can_manage: false,
                can_share: false,
                until,
                email: false,
            };
            write!(app, admin, "core.document.share", |tx| documents
                .share(&mut tx, admin, document.id, &input)
                .await
                .map_err(|e| format!("share {i}: {e:?}"))?);
        }
        if i.is_multiple_of(50) {
            let viewer = &self.viewer;
            let patch = MarkPatch {
                favourite: Some(true),
                following: Some(true),
            };
            write!(app, viewer, "core.document.mark", |tx| documents
                .mark(&mut tx, viewer, document.id, patch)
                .await
                .map_err(|e| format!("mark {i}: {e:?}"))?);
        }
        Ok(())
    }
}

async fn login(app: &bilibi_web::App, email: &str) -> Result<Scope> {
    let login = app
        .user
        .login(email, bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    Ok(app.security.guard(&login.token).await?)
}

/// Times `sql` as the facade and Database Queries run a statement: unnamed,
/// so each run is planned with its values.
async fn time_sql(
    db: &Db,
    runs: usize,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
) -> Result<(usize, Vec<Duration>)> {
    let client = db.get().await?;
    let mut rows = 0;
    for _ in 0..3 {
        rows = client.query(sql, params).await?.len();
    }
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        client.query(sql, params).await?;
        times.push(start.elapsed());
    }
    Ok((rows, times))
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DOCUMENT_LATENCY").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_DOCUMENT_LATENCY=disposable and DATABASE_URL to a disposable migrated database"
                .into(),
        );
    }
    let size = number("BILIBI_DOCUMENT_LATENCY_DOCUMENTS", 50_000)?;
    let runs = number("BILIBI_DOCUMENT_LATENCY_RUNS", 50)?.max(1);
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    // The administrator files every document, so needs a workplace; the
    // readers hold the Documents capabilities directly.
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'list_latency_admin','List Latency Admin','human' WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
              u AS (UPDATE users SET person_id=p.id FROM p, user_identities i WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
         INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING;
         INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed)
         SELECT 1, u, c, true FROM unnest(ARRAY[1,2,3,7]) u, unnest(ARRAY['core.document.view','core.document.download']) c
         ON CONFLICT DO NOTHING;",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35563")?;
    let documents = app.services.get::<Documents>(SERVICE)?;
    let admin = login(&app, "admin@example.test").await?;
    let viewer = login(&app, "viewer@example.test").await?;
    let contractor = login(&app, "contractor@example.test").await?;
    let today = Timestamp::now().in_tz("UTC")?.date();
    let fixture = std::sync::Arc::new(Fixture {
        app,
        documents: documents.clone(),
        admin: admin.clone(),
        viewer: viewer.clone(),
        today,
    });

    // Seed what is missing, four writers at a time.
    let existing: Vec<i64> = db
        .get()
        .await?
        .query(
            "SELECT substr(name, length($2) + 2, 5)::bigint FROM core_documents WHERE tenant_id = 1 AND name LIKE $1",
            &[&format!("{PREFIX} %"), &PREFIX],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let existing: std::collections::HashSet<usize> =
        existing.into_iter().map(|i| i as usize).collect();
    let missing: Vec<usize> = (0..size).filter(|i| !existing.contains(i)).collect();
    if !missing.is_empty() {
        let started = Instant::now();
        eprintln!("seeding {} document(s)", missing.len());
        let missing = std::sync::Arc::new(missing);
        let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut writers = Vec::new();
        for _ in 0..4 {
            let (fixture, missing, next) = (fixture.clone(), missing.clone(), next.clone());
            writers.push(tokio::spawn(async move {
                loop {
                    let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&i) = missing.get(n) else {
                        return Ok::<(), Box<dyn std::error::Error + Send + Sync>>(());
                    };
                    fixture.seed(i).await?;
                    if n % 1000 == 999 {
                        eprintln!(
                            "  {} of {} in {:.0?}",
                            n + 1,
                            missing.len(),
                            started.elapsed()
                        );
                    }
                }
            }));
        }
        for writer in writers {
            writer.await??;
        }
        eprintln!("seeded in {:.0?}", started.elapsed());
    }
    // Ended grants: shared until today, then moved into the past.
    db.get().await?.execute(
        "UPDATE core_document_access a SET expires_at = now() - interval '2 days' FROM core_documents d
         WHERE a.tenant_id = 1 AND d.tenant_id = 1 AND d.id = a.document_id AND a.user_id = $1
           AND a.expires_at > now() AND d.name LIKE $2",
        &[&VIEWER, &format!("{PREFIX} %")],
    ).await?;
    db.get().await?.batch_execute("ANALYZE").await?;

    // The fixture's shape, so a reader can check what was measured.
    let client = db.get().await?;
    let facts = client.query_one(
        "SELECT (SELECT count(*) FROM core_documents WHERE tenant_id=1),
                (SELECT count(*) FROM core_document_access WHERE tenant_id=1),
                (SELECT count(*) FROM core_document_access WHERE tenant_id=1 AND expires_at <= now()),
                (SELECT count(*) FROM core_document_access WHERE tenant_id=1 AND NOT can_download),
                (SELECT count(*) FROM core_document_links WHERE tenant_id=1),
                (SELECT count(*) FROM core_document_marks WHERE tenant_id=1),
                (SELECT count(*) FROM base_audit_mutations),
                (SELECT count(*) FROM base_audit_actions)",
        &[],
    ).await?;
    let fact = |i: usize| facts.get::<_, i64>(i);
    println!(
        "Fixture: {} documents, {} grants ({} ended, {} without download), {} links, {} marks; {} audit mutations, {} audit actions. {runs} timed runs per case after 3 warm-up runs.\n",
        fact(0), fact(1), fact(2), fact(3), fact(4), fact(5), fact(6), fact(7)
    );
    drop(client);

    println!("| Case | Reader | Rows (page / total) | Median ms | p95 ms | Max ms |");
    println!("| --- | --- | --- | --- | --- | --- |");
    let related = Reference::new("company", "8");
    let cases: Vec<(&str, Filter)> = vec![
        ("List, newest revision first", Filter::default()),
        (
            "List, by name, page 40",
            Filter {
                sort: bilibi_document::Sort::Name,
                descending: false,
                page: 40,
                ..Filter::default()
            },
        ),
        (
            "Related record (company 8)",
            Filter {
                related: Some(related.clone()),
                ..Filter::default()
            },
        ),
        (
            "Expired",
            Filter {
                expiry: Some(ExpiryFilter::Expired),
                ..Filter::default()
            },
        ),
        (
            "Favourites",
            Filter {
                marked: Some(MarkFilter::Favourites),
                ..Filter::default()
            },
        ),
        (
            "Search, one word (\"permit\")",
            Filter {
                search: "permit".into(),
                ..Filter::default()
            },
        ),
        (
            "Search, no match",
            Filter {
                search: "nothing-matches-this".into(),
                ..Filter::default()
            },
        ),
    ];
    let readers = [
        ("viewer (broad)", &viewer),
        ("contractor (narrow)", &contractor),
        ("admin (owner of all)", &admin),
    ];
    for (case, filter) in &cases {
        for (label, scope) in readers {
            let mut page = documents.list(scope, filter).await?;
            for _ in 0..2 {
                page = documents.list(scope, filter).await?;
            }
            let mut times = Vec::with_capacity(runs);
            for _ in 0..runs {
                let start = Instant::now();
                documents.list(scope, filter).await?;
                times.push(start.elapsed());
            }
            row(
                case,
                label,
                format!("{} / {}", page.entries.len(), page.total),
                times,
            );
        }
    }

    // Audit reads: the README's queries, bound as Database Queries binds them.
    let document: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT id FROM core_documents WHERE tenant_id = 1 AND name LIKE $1 ORDER BY id LIMIT 1",
            &[&format!("{PREFIX} 00000 %")],
        )
        .await?
        .get(0);
    let tenant: i64 = 1;
    let id = document.to_string();
    for (case, sql) in [
        ("Audit: one document, mutations", DOCUMENT_MUTATIONS),
        ("Audit: one document, file reads", DOCUMENT_FILE_READS),
        ("Audit: one document, refusals", DOCUMENT_REFUSALS),
    ] {
        let (rows, times) = time_sql(&db, runs, sql, &[&tenant, &id]).await?;
        row(
            case,
            &format!("document {document}"),
            rows.to_string(),
            times,
        );
    }
    for (case, sql) in [
        ("Audit: one actor, newest 100 mutations", ACTOR_MUTATIONS),
        ("Audit: one actor, newest 100 actions", ACTOR_ACTIONS),
    ] {
        for (label, scope) in [("actor: admin", &admin), ("actor: viewer", &viewer)] {
            let (rows, times) = time_sql(&db, runs, sql, &[&tenant, &scope.user()]).await?;
            row(case, label, rows.to_string(), times);
        }
    }
    Ok(())
}
