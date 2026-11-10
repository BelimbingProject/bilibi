//! Record History with a contributed kind (`HISTORY_KINDS`), on a disposable,
//! migrated database; no test harness. This example declares the kind
//! `walkthrough.company_profile` as extending Core Company's `company`
//! records, writes two changes of a "profile" kept beside company 1 through
//! the audited mutation boundary, and serves the app: company 1's History
//! disclosure then lists those changes beside the company's own, with no
//! change to Company's page. It also prints the trail `bilibi_audit` reads
//! for the company's kinds with and without the contributed one.
//!
//!   BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes DATABASE_URL=... APP_ORIGIN=... BIND_ADDR=... \
//!     cargo run -p bilibi-audit-views --example history-kinds
//!
//! Sign in as admin@example.test (the fixture password) and open
//! `/companies/1`, then its History.
use bilibi_audit::subject_history;
use bilibi_audit_views::{HistoryKind, HISTORY_KINDS};
use bilibi_database::{pool, Change, Error, MutationBoundary};
use bilibi_module_registry::{Contribution, Provenance};
use serde_json::json;

/// The walkthrough's own row kind beside a company.
const KIND: &str = "walkthrough.company_profile";

inventory::submit! { Contribution::new(HISTORY_KINDS, KIND, Provenance { module: "base/audit_views", source: file!(), line: line!() }, &HistoryKind {
    record: "company", kind: KIND,
}) }
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Outcome<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> Outcome {
    if std::env::var("BILIBI_DISPOSABLE_AUDIT_FIXTURE").as_deref() != Ok("yes") {
        return Err(
            "Set BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes and point DATABASE_URL at a disposable database."
                .into(),
        );
    }
    let env = |key: &str| std::env::var(key).map_err(|_| Error::Invalid);
    let db = pool(&env("DATABASE_URL")?)?;
    let origin = env("APP_ORIGIN")?;
    let bind = env("BIND_ADDR")?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin = app.security.guard(&login.token).await?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let mut client = db.get().await?;
    let written: i64 = client
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type=$1",
            &[&KIND],
        )
        .await?
        .get(0);
    if written == 0 {
        // Two changes of the profile kept beside company 1, through the
        // audited boundary as its owner would write them.
        for (old, new) in [
            (json!(null), json!({"segment": "Retail", "rating": "B"})),
            (
                json!({"segment": "Retail", "rating": "B"}),
                json!({"segment": "Retail", "rating": "A"}),
            ),
        ] {
            let mut tx = boundary
                .begin(
                    &mut client,
                    admin.mutation_context("walkthrough.history-kinds"),
                )
                .await?;
            tx.mutate(move |_| {
                Box::pin(async move {
                    Ok((
                        (),
                        vec![Change {
                            kind: KIND.into(),
                            id: 1,
                            old,
                            new,
                        }],
                    ))
                })
            })
            .await?;
            tx.commit().await?;
        }
    }
    let own = subject_history(&**client, &admin, &["company".into()], "1", 5).await?;
    let both = subject_history(&**client, &admin, &["company".into(), KIND.into()], "1", 5).await?;
    println!(
        "company 1's own kind: {} changes; with {KIND}: {} changes ({} of them {KIND})",
        own.len(),
        both.len(),
        both.iter()
            .filter(|entry| entry.auditable_type == KIND)
            .count()
    );
    drop(client);
    println!(
        "Sign in at {origin} as admin@example.test and open {origin}/companies/1, then History."
    );
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, bilibi_web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
