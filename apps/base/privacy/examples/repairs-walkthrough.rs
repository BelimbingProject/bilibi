//! Running-service walkthrough of the request repairs (#561, #562, #564,
//! #565, #598). A walkthrough store (`wt_probe/probe.records`) counts its
//! locates and lists 250 records matched only by email. It shows:
//!
//! - the request page locates once per mounted page, again only on Find
//!   again (#565);
//! - matches left unconfirmed past the candidate cap are counted on the
//!   store's answer and in `export.json` (#564);
//! - a finish attempt that stored its export and failed to file it leaves no
//!   second copy, and an erasure deletes an export no request references;
//!   a certificate rendered by a failed attempt is filed, not rendered again
//!   (#562);
//! - a failed access request drops its staged copy; a store mounted while it
//!   was failed is asked on Run again, a store no longer mounted is answered
//!   `unmounted`, and the request completes (#561, #598);
//! - placing a hold while an erasure holds the tenant's guard gives up after
//!   five seconds with "try again" (#598).
//!
//! Explicit disposable DB only (`seed` first, same variables as
//! `requests-walkthrough`). Each check fails the executable when the stored
//! state breaks the contract.
use bilibi_authz::grants::Grants;
use bilibi_database::{pool, Error, MutationBoundary, Result as DbResult};
use bilibi_module_registry::{Contribution, Module, Provenance};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_personal_data::{
    Candidate, Disclosure, Erased, Erasure, Exported, Handler, HandlerFuture, Holds, Identifier,
    KindCount, Located, PersonalData, Subject, PERSONAL_DATA,
};
use bilibi_privacy::{ConfirmedRecord, HoldTarget, NewRequest, Privacy, RequestKind, RequestView};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use bilibi_ui::page::{LiveInput, LiveRequest, PageRequest, PageService};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[allow(dead_code)]
#[path = "support/erasure.rs"]
mod erasure_approval;

// ------------------------------------------------------------ probe store

const MODULE: &str = "wt_probe/probe";
const STORE: &str = "wt_probe/probe.records";
const KIND: &str = "wt_probe.record";
/// Records the probe matches only by an identifier: more than the 200 a
/// request lists.
const MATCHES: usize = 250;
const PROVENANCE: Provenance = Provenance {
    module: MODULE,
    source: file!(),
    line: line!(),
};
inventory::submit! { Module {
    id: MODULE, layer: bilibi_module_registry::Layer::Domains,
    dependencies: &["base/personal_data", "base/privacy"], migrations: &[], services: &[],
} }
inventory::submit! { Contribution::new(PERSONAL_DATA, STORE, PROVENANCE,
&PersonalData { label: "Probe records (walkthrough)", kinds: &[], person_subjects: &[], handler: &PROBE }) }

/// How many times any request's locate asked the probe.
static LOCATES: AtomicUsize = AtomicUsize::new(0);
struct Probe;
static PROBE: Probe = Probe;
impl Handler for Probe {
    fn locate<'a>(
        &'a self,
        _client: &'a Client,
        _scope: &'a Scope,
        subject: &'a Subject,
    ) -> HandlerFuture<'a, Located> {
        Box::pin(async move {
            LOCATES.fetch_add(1, Ordering::SeqCst);
            let matches = if subject.identifiers.is_empty() {
                0
            } else {
                MATCHES
            };
            Ok(Located {
                kinds: vec![KindCount {
                    kind: KIND.into(),
                    by_person: 0,
                    by_identifier: matches as i64,
                }],
                candidates: (1..=matches)
                    .map(|n| Candidate {
                        kind: KIND.into(),
                        id: n.to_string(),
                        summary: format!("Probe record {n}"),
                    })
                    .collect(),
                truncated: false,
            })
        })
    }
    fn export<'a>(
        &'a self,
        _client: &'a Client,
        _scope: &'a Scope,
        subject: &'a Subject,
        _grant: &'a Disclosure,
    ) -> HandlerFuture<'a, Exported> {
        Box::pin(async move {
            Ok(Exported {
                records: json!({"confirmed": subject.confirmed.len()}),
                files: Vec::new(),
            })
        })
    }
    fn erase<'a>(
        &'a self,
        _tx: &'a mut bilibi_database::MutationTransaction<'_>,
        _scope: &'a Scope,
        _subject: &'a Subject,
        _grant: &'a Erasure,
        _holds: &'a Holds,
    ) -> HandlerFuture<'a, Erased> {
        Box::pin(async { Ok(Erased::default()) })
    }
}

// ----------------------------------------------------------------- helpers

fn now_step() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        / 30
}
/// The authenticator code for `key` (base32) at the current 30-second step.
fn totp(key: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut secret = Vec::new();
    let (mut buffer, mut bits) = (0u64, 0u32);
    for c in key.chars().filter(|c| c.is_ascii_alphanumeric()) {
        let value = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .find(c.to_ascii_uppercase())
            .expect("base32") as u64;
        buffer = buffer << 5 | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            secret.push((buffer >> bits) as u8);
        }
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).expect("key");
    mac.update(&(now_step() as u64).to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let at = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[at] & 0x7f,
        digest[at + 1],
        digest[at + 2],
        digest[at + 3],
    ]);
    format!("{:06}", binary % 1_000_000)
}

/// One signed-in account, its session token and its authenticator.
struct Account {
    scope: Scope,
    token: String,
    key: String,
    used: i64,
}
impl Account {
    async fn enroll(app: &bilibi_web::App, email: &str) -> Result<Self> {
        let login = app
            .user
            .login(email, bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        let scope = app.security.guard(&login.token).await?;
        let enrollment = app
            .user
            .begin_two_factor(&scope, bilibi_walkthrough_fixture::PASSWORD)
            .await
            .map_err(|e| format!("begin two-factor: {e:?}"))?;
        let mut account = Self {
            scope,
            token: login.token,
            key: enrollment.key,
            used: 0,
        };
        let first = account.code().await;
        app.user
            .confirm_two_factor(&account.scope, &first)
            .await
            .map_err(|e| format!("confirm two-factor: {e:?}"))?;
        Ok(account)
    }
    async fn code(&mut self) -> String {
        while now_step() <= self.used {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.used = now_step();
        totp(&self.key)
    }
    async fn step_up(&mut self) -> Scope {
        let code = self.code().await;
        self.scope.clone().presenting_step_up(&code)
    }
}

/// Fails the walkthrough unless `ok`, after printing the check.
fn check(ok: bool, what: &str) -> Result<()> {
    println!("  [{}] {what}", if ok { "ok" } else { "FAILED" });
    if ok {
        Ok(())
    } else {
        Err(format!("check failed: {what}").into())
    }
}
async fn count(client: &Client, sql: &str) -> Result<i64> {
    Ok(client.query_one(sql, &[]).await?.get(0))
}
/// Waits up to `seconds` for `sql` (one bigint) to reach at least `want`.
async fn reach(client: &Client, sql: &str, want: i64, seconds: u64) -> Result<i64> {
    let mut seen = 0;
    for _ in 0..seconds {
        seen = count(client, sql).await?;
        if seen >= want {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(seen)
}
/// Waits up to `seconds` while the request is `approved` or `running`.
async fn settle(
    privacy: &Privacy,
    client: &Client,
    scope: &Scope,
    id: i64,
    seconds: u64,
) -> Result<RequestView> {
    for _ in 0..seconds {
        let request = privacy.request(client, scope, id).await?;
        if !matches!(request.status.as_str(), "running" | "approved") {
            return Ok(request);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(privacy.request(client, scope, id).await?)
}
fn summary(request: &RequestView) {
    println!(
        "  request {} {:?} status={} export={} certificate={} finish_attempts={}",
        request.id,
        request.kind,
        request.status,
        request.export_artifact.is_some(),
        request.certificate_artifact.is_some(),
        request.finish_attempts
    );
    for store in &request.stores {
        println!(
            "    store {:<32} {:<10} attempts={} error={:?}",
            store.store, store.state, store.attempts, store.last_error
        );
    }
}
async fn new_person(client: &Client, name: &str, email: &str) -> Result<PersonId> {
    let id: i64 = client
        .query_one(
            "INSERT INTO persons(tenant_id,code,name,kind,email) \
             SELECT 1, trim(both '_' from lower(regexp_replace($1,'[^A-Za-z0-9]+','_','g'))) || '_' || (coalesce(max(id),0)+1), $1, 'human', $2 FROM persons RETURNING id",
            &[&name, &email],
        )
        .await?
        .get(0);
    Ok(PersonId::new(id)?)
}
async fn receive(
    privacy: &Privacy,
    client: &mut Client,
    receiver: &Scope,
    kind: RequestKind,
    person: PersonId,
    email: &str,
) -> Result<RequestView> {
    Ok(privacy
        .receive(
            client,
            receiver,
            NewRequest {
                kind,
                person,
                identifiers: vec![Identifier::Email(email.into())],
                company: OrganisationId::new(1)?,
            },
        )
        .await?)
}
async fn verify(
    privacy: &Privacy,
    client: &mut Client,
    receiver: &Scope,
    request: &RequestView,
    confirm: &[ConfirmedRecord],
) -> Result<RequestView> {
    Ok(privacy
        .verify(
            client,
            receiver,
            request.id,
            request.version,
            "Identity checked at the counter",
            confirm,
        )
        .await?)
}
/// Live request documents of `kind` filed under `request`'s Artifacts subject.
async fn documents(client: &Client, request: i64, kind: &str) -> Result<Vec<String>> {
    Ok(client
        .query(
            "SELECT public_id FROM base_artifacts WHERE owner_id='base.privacy' AND subject=$1 AND kind=$2 \
             AND ready_at IS NOT NULL AND deleted_at IS NULL ORDER BY id",
            &[&format!("request:{request}"), &kind],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect())
}
/// Makes a retrying job of `request` on `worker` run now (disposable DB only).
async fn hurry(client: &Client, worker: &str, request: i64, last: bool) -> Result<()> {
    client
        .execute(
            &format!(
                "UPDATE base_queue_jobs SET available_at=clock_timestamp(){} WHERE worker_id=$1 AND args->>'request_id'=$2 AND state='retryable'",
                if last { ",max_attempts=attempt+1" } else { "" }
            ),
            &[&worker, &request.to_string()],
        )
        .await?;
    Ok(())
}
/// `export.json` of a completed access request, read as the operator does.
async fn export_json(
    privacy: &Privacy,
    client: &mut Client,
    scope: &Scope,
    id: i64,
) -> Result<Value> {
    let (_, bytes, _) = privacy
        .request_document(client, scope, id, Some("Walkthrough check"))
        .await?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut text = String::new();
    archive.by_name("export.json")?.read_to_string(&mut text)?;
    Ok(serde_json::from_str(&text)?)
}
fn store_entry<'a>(export: &'a Value, store: &str) -> Option<&'a Value> {
    export["stores"]
        .as_array()?
        .iter()
        .find(|s| s["store"] == store)
}

/// Mounts the request page as `account` and checks how many locates its
/// first render, four more inputs and Find again cost.
async fn page_locates(app: &bilibi_web::App, account: &Account, id: i64) -> Result<()> {
    let pages = app
        .services
        .get::<PageService<Scope, Error>>("base/privacy.pages")?;
    let path = HashMap::from([("id".to_owned(), id.to_string())]);
    let query = HashMap::new();
    let Some(mut live) = pages
        .0
        .mount(PageRequest {
            operation: "request",
            scope: Some(&account.scope),
            path: &path,
            query: &query,
            body: &[],
            framed: false,
        })
        .await?
    else {
        return Err("the request page is not live".into());
    };
    let start = LOCATES.load(Ordering::SeqCst);
    let refresh = live
        .update(LiveRequest {
            scope: &account.scope,
            credential: &account.token,
            input: LiveInput::Refresh,
        })
        .await?;
    let mounted = LOCATES.load(Ordering::SeqCst) - start;
    check(
        mounted == 1,
        &format!("the mounted page located once (asked the probe {mounted} time(s))"),
    )?;
    check(
        refresh.html.contains("privacy-locate-again"),
        "the open request offers Find again",
    )?;
    for event in [
        json!({"type": "open_refuse"}),
        json!({"type": "close_modal"}),
        json!({"type": "open_verify"}),
    ] {
        live.update(LiveRequest {
            scope: &account.scope,
            credential: &account.token,
            input: LiveInput::Event(&event),
        })
        .await?;
    }
    live.update(LiveRequest {
        scope: &account.scope,
        credential: &account.token,
        input: LiveInput::Refresh,
    })
    .await?;
    let inputs = LOCATES.load(Ordering::SeqCst) - start;
    check(
        inputs == 1,
        &format!("a refresh, a dialog opened and closed and the verify dialog located nothing more (total {inputs})"),
    )?;
    let again = json!({"type": "locate_again"});
    live.update(LiveRequest {
        scope: &account.scope,
        credential: &account.token,
        input: LiveInput::Event(&again),
    })
    .await?;
    let found = LOCATES.load(Ordering::SeqCst) - start;
    check(
        found == 2,
        &format!("Find again located once more (total {found})"),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_PRIVACY_DEMO").as_deref() != Ok("yes") {
        return Err("explicit disposable DB opt-in required".into());
    }
    let root = std::env::var("BILIBI_PRIVACY_DEMO_ROOT")?;
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    if std::env::args().nth(1).as_deref() == Some("seed") {
        return Ok(bilibi_web::seed::production(&db).await?);
    }
    // ADR 19: the file handlers need explicit desks, not an unlinked admin.
    db.get().await?.batch_execute("INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,3,1),(1,4,2),(1,3,2) ON CONFLICT DO NOTHING; \
        SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35575")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let mut admin = Account::enroll(&app, "admin@example.test").await?;
    let second = Account::enroll(&app, "ai@agent.my").await?;
    let privacy = app.services.get::<Privacy>(bilibi_privacy::SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let mut client = db.get().await?;
    erasure_approval::publish_rules(&app, &mut client, &admin.scope, 1).await?;
    {
        let mut tx = boundary
            .begin(
                &mut client,
                admin
                    .scope
                    .mutation_context("privacy.walkthrough.configure"),
            )
            .await?;
        let chromium =
            std::env::var("BILIBI_PRIVACY_DEMO_CHROMIUM").unwrap_or_else(|_| "chromium".into());
        for (key, value) in [
            ("media.disks", json!({"local": root})),
            ("media.attachment_disk", json!("local")),
            ("artifacts.storage_root", json!(root)),
            ("pdf.chromium.binary", json!(chromium)),
        ] {
            settings
                .put(&mut tx, &admin.scope, Target::Global, key, value)
                .await?;
        }
        tx.commit().await?;
    }
    // A request document's filing fails while its request is listed here.
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS walkthrough_unfiled(request_id bigint PRIMARY KEY); \
             CREATE OR REPLACE FUNCTION walkthrough_unfiled() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
               IF EXISTS (SELECT 1 FROM walkthrough_unfiled WHERE request_id = NEW.id) \
                  AND ((NEW.export_artifact IS NOT NULL AND OLD.export_artifact IS NULL) \
                    OR (NEW.certificate_artifact IS NOT NULL AND OLD.certificate_artifact IS NULL)) THEN \
                 RAISE EXCEPTION 'walkthrough: filing the document fails'; END IF; RETURN NEW; END $$; \
             DROP TRIGGER IF EXISTS walkthrough_unfiled ON base_privacy_requests; \
             CREATE TRIGGER walkthrough_unfiled BEFORE UPDATE ON base_privacy_requests \
               FOR EACH ROW EXECUTE FUNCTION walkthrough_unfiled();",
        )
        .await?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));

    println!("== #565: the request page locates once per mounted page");
    let pat = new_person(&client, "Pat Page", "pat-page@example.test").await?;
    let paged = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        pat,
        "pat-page@example.test",
    )
    .await?;
    page_locates(&app, &admin, paged.id).await?;

    println!("== #564: matches past the candidate cap are counted, not lost");
    let located = privacy.locate(&client, &admin.scope, paged.id).await?;
    let probe = located
        .iter()
        .find(|(store, _, _)| store == STORE)
        .map(|(_, _, found)| found)
        .ok_or("the probe store did not answer locate")?;
    check(
        probe.candidates.len() == 200 && probe.truncated,
        &format!(
            "{MATCHES} matches are listed as the first 200 (listed {}, truncated {})",
            probe.candidates.len(),
            probe.truncated
        ),
    )?;
    let confirm: Vec<ConfirmedRecord> = ["1", "2", "3"]
        .iter()
        .map(|id| ConfirmedRecord {
            store: STORE.into(),
            kind: KIND.into(),
            id: (*id).into(),
        })
        .collect();
    let paged = verify(&privacy, &mut client, &second.scope, &paged, &confirm).await?;
    let paged = privacy
        .approve(&mut client, &admin.scope, paged.id, paged.version)
        .await?;
    let paged = settle(&privacy, &client, &second.scope, paged.id, 90).await?;
    summary(&paged);
    check(paged.status == "completed", "the access request completed")?;
    let left = paged
        .stores
        .iter()
        .find(|s| s.store == STORE)
        .and_then(|s| s.result.as_ref())
        .and_then(|r| r["unconfirmed"].as_i64());
    check(
        left == Some(MATCHES as i64 - 3),
        &format!(
            "the probe's answer counts {} left unconfirmed: {left:?}",
            MATCHES - 3
        ),
    )?;
    let export = export_json(&privacy, &mut client, &admin.scope, paged.id).await?;
    let entry = store_entry(&export, STORE).ok_or("export.json has no probe store")?;
    check(
        entry["unconfirmed_matches"].as_i64() == Some(MATCHES as i64 - 3),
        &format!(
            "export.json names {} matches left unconfirmed: {}",
            MATCHES - 3,
            entry["unconfirmed_matches"]
        ),
    )?;

    println!("== #562: a finish attempt that fails to file its export leaves no second copy");
    let eve = new_person(&client, "Eve Unfiled", "eve-unfiled@example.test").await?;
    let first = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        eve,
        "eve-unfiled@example.test",
    )
    .await?;
    let first = verify(&privacy, &mut client, &second.scope, &first, &[]).await?;
    client
        .execute(
            "INSERT INTO walkthrough_unfiled(request_id) VALUES($1)",
            &[&first.id],
        )
        .await?;
    privacy
        .approve(&mut client, &admin.scope, first.id, first.version)
        .await?;
    let attempts = format!(
        "SELECT finish_attempts::bigint FROM base_privacy_requests WHERE id={}",
        first.id
    );
    check(
        reach(&client, &attempts, 1, 90).await? >= 1,
        "the first finish attempt failed to file its export",
    )?;
    let orphans = documents(&client, first.id, "export").await?;
    check(
        orphans.len() == 1,
        &format!("one unfiled export is stored ({})", orphans.len()),
    )?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, first.id, false).await?;
    check(
        reach(&client, &attempts, 2, 90).await? >= 2,
        "the second finish attempt failed too",
    )?;
    let after = documents(&client, first.id, "export").await?;
    check(
        after.len() == 1 && after != orphans,
        &format!(
            "the retry deleted the unfiled export before building its own: {} live, first gone {}",
            after.len(),
            !after.contains(&orphans[0])
        ),
    )?;
    client
        .execute(
            "DELETE FROM walkthrough_unfiled WHERE request_id=$1",
            &[&first.id],
        )
        .await?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, first.id, false).await?;
    let first = settle(&privacy, &client, &second.scope, first.id, 90).await?;
    summary(&first);
    let live = documents(&client, first.id, "export").await?;
    check(
        first.status == "completed"
            && live.len() == 1
            && first.export_artifact.as_ref() == live.first(),
        &format!(
            "completed with exactly its filed export live ({} live)",
            live.len()
        ),
    )?;

    println!("== #598: a failed access request keeps no staged copy");
    let unfiled = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        eve,
        "eve-unfiled@example.test",
    )
    .await?;
    let unfiled = verify(&privacy, &mut client, &second.scope, &unfiled, &[]).await?;
    client
        .execute(
            "INSERT INTO walkthrough_unfiled(request_id) VALUES($1)",
            &[&unfiled.id],
        )
        .await?;
    privacy
        .approve(&mut client, &admin.scope, unfiled.id, unfiled.version)
        .await?;
    let attempts = format!(
        "SELECT finish_attempts::bigint FROM base_privacy_requests WHERE id={}",
        unfiled.id
    );
    reach(&client, &attempts, 1, 90).await?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, unfiled.id, true).await?;
    let failed = format!(
        "SELECT (status='failed')::int::bigint FROM base_privacy_requests WHERE id={}",
        unfiled.id
    );
    check(
        reach(&client, &failed, 1, 90).await? == 1,
        "the access request whose finish ran out of attempts is failed",
    )?;
    let staged = count(
        &client,
        &format!(
            "SELECT count(*) FROM base_privacy_request_exports WHERE request_id={}",
            unfiled.id
        ),
    )
    .await?;
    check(
        staged == 0,
        &format!("its staged copy is dropped ({staged} staged rows)"),
    )?;
    check(
        documents(&client, unfiled.id, "export").await?.len() == 1,
        "its last attempt left one export no request references",
    )?;

    println!("== #562: the person's erasure deletes every export, filed or not, and files one certificate");
    let erasure = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Erasure,
        eve,
        "eve-unfiled@example.test",
    )
    .await?;
    let erasure = verify(&privacy, &mut client, &second.scope, &erasure, &[]).await?;
    client
        .execute(
            "INSERT INTO walkthrough_unfiled(request_id) VALUES($1)",
            &[&erasure.id],
        )
        .await?;
    let erasure = erasure_approval::decide(
        &app,
        &privacy,
        &mut client,
        &second.scope,
        &admin.step_up().await,
        &erasure,
        true,
    )
    .await?;
    println!("  erasure {} approved: {}", erasure.id, erasure.status);
    let attempts = format!(
        "SELECT finish_attempts::bigint FROM base_privacy_requests WHERE id={}",
        erasure.id
    );
    check(
        reach(&client, &attempts, 1, 120).await? >= 1,
        "the erasure's first finish attempt failed to file its certificate",
    )?;
    let rendered = documents(&client, erasure.id, "certificate").await?;
    check(
        rendered.len() == 1,
        &format!("one unfiled certificate is stored ({})", rendered.len()),
    )?;
    client
        .execute(
            "DELETE FROM walkthrough_unfiled WHERE request_id=$1",
            &[&erasure.id],
        )
        .await?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, erasure.id, false).await?;
    let erasure = settle(&privacy, &client, &second.scope, erasure.id, 120).await?;
    summary(&erasure);
    let certificates = documents(&client, erasure.id, "certificate").await?;
    check(
        erasure.status == "completed"
            && certificates == rendered
            && erasure.certificate_artifact.as_ref() == certificates.first(),
        &format!(
            "completed, filing the certificate already rendered ({} certificate(s))",
            certificates.len()
        ),
    )?;
    let left: usize = documents(&client, first.id, "export").await?.len()
        + documents(&client, unfiled.id, "export").await?.len();
    check(
        left == 0,
        &format!("no export of the person is left, filed or unfiled ({left})"),
    )?;

    println!("== #561: a store mounted and a store removed while a request is failed");
    let mo = new_person(&client, "Mo Mounted", "mo-mounted@example.test").await?;
    let moved = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        mo,
        "mo-mounted@example.test",
    )
    .await?;
    let moved = verify(&privacy, &mut client, &second.scope, &moved, &[]).await?;
    client
        .execute(
            "INSERT INTO walkthrough_unfiled(request_id) VALUES($1)",
            &[&moved.id],
        )
        .await?;
    privacy
        .approve(&mut client, &admin.scope, moved.id, moved.version)
        .await?;
    let attempts = format!(
        "SELECT finish_attempts::bigint FROM base_privacy_requests WHERE id={}",
        moved.id
    );
    reach(&client, &attempts, 1, 90).await?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, moved.id, true).await?;
    let failed = format!(
        "SELECT (status='failed')::int::bigint FROM base_privacy_requests WHERE id={}",
        moved.id
    );
    check(
        reach(&client, &failed, 1, 90).await? == 1,
        "the access request is failed",
    )?;
    client
        .execute(
            "DELETE FROM walkthrough_unfiled WHERE request_id=$1",
            &[&moved.id],
        )
        .await?;
    // As if the probe's module were mounted after the request ran, and a
    // store's module removed since (disposable database only).
    client
        .execute(
            "DELETE FROM base_privacy_request_stores WHERE request_id=$1 AND store=$2",
            &[&moved.id, &STORE],
        )
        .await?;
    client
        .execute(
            "INSERT INTO base_privacy_request_stores(request_id,tenant_id,store) VALUES($1,1,'wt_gone/gone.records')",
            &[&moved.id],
        )
        .await?;
    println!("  the probe store's row is gone and a row of a removed store waits");
    let current = privacy.request(&**client, &second.scope, moved.id).await?;
    let again = privacy
        .run_again(&mut client, &second.scope, moved.id, current.version)
        .await;
    check(
        again.is_ok(),
        &format!(
            "Run again is accepted: {:?}",
            again.as_ref().map(|r| &r.status)
        ),
    )?;
    let moved = settle(&privacy, &client, &second.scope, moved.id, 120).await?;
    summary(&moved);
    let state = |store: &str| {
        moved
            .stores
            .iter()
            .find(|s| s.store == store)
            .map(|s| s.state.clone())
    };
    check(moved.status == "completed", "the request completed")?;
    check(
        state(STORE).as_deref() == Some("answered"),
        &format!("the store mounted since was asked: {:?}", state(STORE)),
    )?;
    check(
        state("wt_gone/gone.records").as_deref() == Some("unmounted"),
        &format!(
            "the removed store is answered unmounted: {:?}",
            state("wt_gone/gone.records")
        ),
    )?;
    let export = export_json(&privacy, &mut client, &admin.scope, moved.id).await?;
    check(
        export["stores_not_installed"] == json!(["wt_gone/gone.records"])
            && store_entry(&export, STORE).is_some(),
        &format!(
            "export.json has the probe store and names the removed one: {}",
            export["stores_not_installed"]
        ),
    )?;

    println!("== #598: a hold placed while an erasure holds the guard gives up with try again");
    let guard = db.get().await?;
    guard
        .batch_execute("BEGIN; SELECT pg_advisory_xact_lock_shared(841330, 1)")
        .await?;
    let started = std::time::Instant::now();
    let placed = {
        let mut client = db.get().await?;
        privacy
            .place_hold(
                &mut client,
                &admin.scope,
                HoldTarget::Person { person: mo },
                "Placed during an erasure",
            )
            .await
    };
    let waited = started.elapsed();
    guard.batch_execute("COMMIT").await?;
    let refused = match &placed {
        Err(Error::Refused(message)) => message.contains("Try again"),
        _ => false,
    };
    check(
        refused && waited >= Duration::from_secs(4) && waited < Duration::from_secs(30),
        &format!(
            "refused after {} ms: {:?}",
            waited.as_millis(),
            placed
                .as_ref()
                .map(|hold| hold.id)
                .map_err(|e| e.to_string())
        ),
    )?;
    let placed: DbResult<_> = privacy
        .place_hold(
            &mut client,
            &admin.scope,
            HoldTarget::Person { person: mo },
            "Placed once the erasure committed",
        )
        .await;
    check(
        placed.is_ok(),
        "the same hold is placed once the guard is free",
    )?;

    stop.send(true)?;
    workers.await??;
    println!("all checks passed");
    Ok(())
}
