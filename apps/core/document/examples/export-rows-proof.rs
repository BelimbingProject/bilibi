//! Disposable proof that the Documents CSV export writes every matching
//! document, not only the first list page (issue 585).
//!
//! Files link documents through the real `Documents` facade as the
//! administrator, then downloads `/documents/export.csv` through the module's
//! download service, as the route does, and compares the file with the list's
//! own total. Fails the executable when a row is missing, repeated, or when a
//! partial file does not say that it is partial, or when an export leaves no
//! `core.document.exported` action with its filter and row count (issue 602).
//!
//! On a migrated database that holds the walkthrough fixture and that
//! `bilibi-web seed-production` then seeded (the pick-lists a document is
//! filed under):
//!
//! `BILIBI_DOCUMENT_EXPORT_PROOF=disposable DATABASE_URL=… cargo run -p
//! bilibi-document --example export-rows-proof --locked`
//!
//! `BILIBI_DOCUMENT_EXPORT_DOCUMENTS` (310) is how many documents to file: more
//! than one list page (300). Set it above 5,000 to see the export's own limit
//! and its closing line.
use bilibi_database::{pool, Error};
use bilibi_document::{Documents, Draft, Filter, Source, SERVICE};
use bilibi_tenancy::Scope;
use bilibi_ui::download::{DownloadRequest, DownloadService};
use std::collections::{HashMap, HashSet};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The module's download service, which serves the export route.
const FILES: &str = "core/document.files";
/// The most rows one export writes (`EXPORT_ROWS` in the module).
const EXPORT_ROWS: usize = 5_000;
/// Documents filed in one transaction.
const BATCH: usize = 250;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        println!("  ok: {message}");
        Ok(())
    } else {
        Err(format!("contract violated: {message}").into())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DOCUMENT_EXPORT_PROOF").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_DOCUMENT_EXPORT_PROOF=disposable and DATABASE_URL to a disposable migrated database"
                .into(),
        );
    }
    let size: usize = match std::env::var("BILIBI_DOCUMENT_EXPORT_DOCUMENTS") {
        Ok(value) => value.parse()?,
        Err(_) => 310,
    };
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    // The administrator files every document, so needs a workplace.
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'export_proof_admin','Export Proof Admin','human' WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
              u AS (UPDATE users SET person_id=p.id FROM p, user_identities i WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
         INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35585")?;
    let documents = app.services.get::<Documents>(SERVICE)?;
    let files = app.services.get::<DownloadService<Scope, Error>>(FILES)?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin: Scope = app.security.guard(&login.token).await?;

    println!("== filing {size} link documents");
    let mut filed = 0;
    while filed < size {
        let mut client = app.db.get().await?;
        let mut tx = app
            .mutations
            .begin(&mut client, admin.mutation_context("core.document.create"))
            .await?;
        for i in filed..size.min(filed + BATCH) {
            let draft = Draft {
                name: Some(format!("Export proof {i:05}")),
                source: Source::External,
                external_url: Some(format!("https://example.test/export-proof/{i}")),
                ..Draft::default()
            };
            documents
                .create(&mut tx, &admin, &draft)
                .await
                .map_err(|e| format!("create {i}: {e:?}"))?;
        }
        tx.commit().await?;
        filed = size.min(filed + BATCH);
    }

    let filter = Filter {
        page_size: 1,
        ..Filter::default()
    };
    let total = usize::try_from(documents.list(&admin, &filter).await?.total)?;
    println!("== the list counts {total} documents for the administrator");
    ensure(
        total >= size,
        "the administrator lists every filed document",
    )?;

    let nothing = HashMap::new();
    let file = files
        .0
        .serve(DownloadRequest {
            operation: "export",
            scope: &admin,
            path: &nothing,
            query: &nothing,
        })
        .await?;
    let text = String::from_utf8(file.bytes)?;
    // No name here holds a line break, so every line is one row.
    let mut lines: Vec<&str> = text.split("\r\n").filter(|l| !l.is_empty()).collect();
    ensure(
        lines.first().is_some_and(|l| l.starts_with("id,name,")),
        "the first line is the header",
    )?;
    lines.remove(0);
    let expected = total.min(EXPORT_ROWS);
    println!(
        "== export: {} line(s) after the header, saved as {:?}",
        lines.len(),
        file.filename
    );
    if total > EXPORT_ROWS {
        let last = lines.pop().unwrap_or_default();
        ensure(
            last.starts_with(&format!(
                "Not complete: the first {expected} of {total} documents."
            )),
            "a partial file ends with a line that says how many of how many it holds",
        )?;
        ensure(
            file.filename == "documents-not-complete.csv",
            "a partial file is saved under a name that says so",
        )?;
    } else {
        ensure(
            file.filename == "documents.csv",
            "a complete file is saved as documents.csv",
        )?;
    }
    ensure(
        lines.len() == expected,
        &format!("the file holds {expected} document rows"),
    )?;
    let ids: HashSet<&str> = lines
        .iter()
        .filter_map(|line| line.split(',').next())
        .collect();
    ensure(ids.len() == expected, "every row is a different document")?;

    let search = HashMap::from([("search".to_owned(), "Export proof 00001".to_owned())]);
    let narrow = files
        .0
        .serve(DownloadRequest {
            operation: "export",
            scope: &admin,
            path: &nothing,
            query: &search,
        })
        .await?;
    let narrow_rows = String::from_utf8(narrow.bytes)?
        .split("\r\n")
        .filter(|l| !l.is_empty())
        .count()
        - 1;
    let actions = app
        .db
        .get()
        .await?
        .query(
            "SELECT actor_id,payload FROM base_audit_actions WHERE tenant_id=$1 AND event='core.document.exported' ORDER BY id",
            &[&admin.tenant()],
        )
        .await?;
    println!("== {} core.document.exported action(s)", actions.len());
    ensure(actions.len() == 2, "each export leaves one exported action")?;
    let payload = |i: usize| actions[i].get::<_, serde_json::Value>(1);
    ensure(
        actions.iter().all(|a| a.get::<_, i64>(0) == admin.user()),
        "the actions name the administrator who exported",
    )?;
    ensure(
        payload(0)["rows"] == expected && payload(0)["total"] == total,
        &format!("the first action records {expected} of {total} rows"),
    )?;
    ensure(
        payload(1)["rows"] == narrow_rows && payload(1)["filter"]["search"] == "Export proof 00001",
        &format!("the searched export records its search and its {narrow_rows} row(s)"),
    )?;
    Ok(())
}
