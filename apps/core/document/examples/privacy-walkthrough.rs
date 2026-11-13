//! Documents related to a person reach privacy requests (ADR 05, #583), on a
//! disposable database, through `base/privacy` as an operator runs them.
//!
//! Files four documents: A related only to person 1, B related to person 1
//! and company 1, C related only to person 1 with its file under legal hold,
//! and D related only to company 1. An access request for person 1 must
//! export A, B and C with their revisions and their three files, never D. An
//! approved erasure must then erase A (its name and file name gone from the
//! tables and the audit trail, its file deleted by the delete job), keep B as
//! shared with only its person link removed, keep C under the hold, and leave
//! D alone. Exits non-zero when a check fails.
use bilibi_authz::grants::Grants;
use bilibi_database::{pool, MutationBoundary};
use bilibi_document::{Documents, Draft, Reference, Upload, SERVICE as DOCUMENTS};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_privacy::{NewRequest, Privacy, RequestKind, RequestView, SERVICE};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[allow(dead_code)]
#[path = "../../../base/privacy/examples/support/erasure.rs"]
mod erasure_approval;

const STORE: &str = "core/document.documents";

/// Fixture scanner, not a production adapter.
struct Clean;
impl bilibi_media::Scanner for Clean {
    fn name(&self) -> &str {
        "document-privacy"
    }
    fn version(&self) -> String {
        "1.0 (example)".into()
    }
    fn scan<'a>(
        &'a self,
        _bytes: &'a [u8],
        _facts: bilibi_media::ScanFacts<'a>,
    ) -> bilibi_media::ScanFuture<'a> {
        Box::pin(async { bilibi_media::Verdict::Clean })
    }
}

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        println!("  ok: {message}");
        Ok(())
    } else {
        Err(format!("contract violated: {message}").into())
    }
}

/// The authenticator code for `key` (base32) at `offset` 30-second steps.
fn code(key: &str, offset: i64) -> String {
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let step = (now / 30 + offset) as u64;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).expect("key");
    mac.update(&step.to_be_bytes());
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
async fn enroll(app: &bilibi_web::App, scope: &Scope) -> Result<String> {
    let enrollment = app
        .user
        .begin_two_factor(scope, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|e| format!("begin two-factor: {e:?}"))?;
    app.user
        .confirm_two_factor(scope, &code(&enrollment.key, -1))
        .await
        .map_err(|e| format!("confirm two-factor: {e:?}"))?;
    Ok(enrollment.key)
}
async fn settle(privacy: &Privacy, client: &Client, scope: &Scope, id: i64) -> Result<RequestView> {
    for _ in 0..120 {
        let request = privacy.request(client, scope, id).await?;
        if request.status != "running" {
            return Ok(request);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(privacy.request(client, scope, id).await?)
}
/// Rows of the document tables and the document audit trail holding `needle`.
async fn holding(client: &Client, needle: &str) -> Result<i64> {
    Ok(client.query_one(
        "SELECT (SELECT count(*) FROM core_documents d WHERE d.tenant_id=1 AND d::text LIKE $1)
              + (SELECT count(*) FROM core_document_revisions r WHERE r.tenant_id=1 AND r::text LIKE $1)
              + (SELECT count(*) FROM core_document_files f WHERE f.tenant_id=1 AND f::text LIKE $1)
              + (SELECT count(*) FROM base_audit_mutations m WHERE m.tenant_id=1 AND m.auditable_type LIKE 'core.document.%' AND (coalesce(m.old_values::text,'') || coalesce(m.new_values::text,'')) LIKE $1)",
        &[&format!("%{needle}%")],
    ).await?.get(0))
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
    // Background work, the export's file and consent writes need real desks
    // (ADR 19); tenant 2's manager gets one in its company 100.
    db.get().await?.batch_execute("INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,3,1) ON CONFLICT DO NOTHING; \
        INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(5,2,'other_manager','Other Manager','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=5 WHERE tenant_id=2 AND id=5; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(2,5,100) ON CONFLICT DO NOTHING; \
        SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35577")?;
    app.services
        .get::<Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    async fn login(app: &bilibi_web::App, email: &str) -> Result<Scope> {
        let login = app
            .user
            .login(email, bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        Ok(app.security.guard(&login.token).await?)
    }
    let mut client = db.get().await?;
    let admin = login(&app, "admin@example.test").await?;
    let second = login(&app, "ai@agent.my").await?;
    let privacy = app.services.get::<Privacy>(SERVICE)?;
    let documents = app.services.get::<Documents>(DOCUMENTS)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let second_key = enroll(&app, &second).await?;
    erasure_approval::publish_rules(&app, &mut client, &admin, 1).await?;
    {
        let mut tx = boundary
            .begin(
                &mut client,
                admin.mutation_context("walkthrough.document.configure"),
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
                .put(&mut tx, &admin, Target::Global, key, value.clone())
                .await
                .map_err(|e| format!("{key}={value}: {e:?}"))?;
        }
        tx.commit().await?;
    }
    app.services
        .get::<bilibi_media::Media>("base/media.store")?
        .bind_scanner(std::sync::Arc::new(Clean))?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));

    println!("== four documents: A (person 1), B (person 1 and company 1), C (person 1, held), D (company 1)");
    let person = Reference::new("person", "1");
    let company = Reference::new("company", "1");
    let mut filed = Vec::new();
    for (name, file, related) in [
        (
            "Ada passport scan",
            "ada-passport.txt",
            vec![person.clone()],
        ),
        (
            "Ada supplier contract",
            "ada-contract.txt",
            vec![person.clone(), company.clone()],
        ),
        (
            "Ada disputed invoice",
            "ada-dispute.txt",
            vec![person.clone()],
        ),
        (
            "Company price list",
            "price-list.txt",
            vec![company.clone()],
        ),
    ] {
        let stored = documents
            .store_file(
                &admin,
                Upload {
                    bytes: format!("{name}\n").as_bytes(),
                    file_name: file,
                },
            )
            .await?;
        let mut tx = boundary
            .begin(&mut client, admin.mutation_context("core.document.create"))
            .await?;
        let document = documents
            .create(
                &mut tx,
                &admin,
                &Draft {
                    name: Some(name.into()),
                    artifact_id: Some(stored.artifact_id.clone()),
                    related,
                    ..Draft::default()
                },
            )
            .await?;
        tx.commit().await?;
        filed.push((document.id, stored.artifact_id));
    }
    let [(a, a_file), (b, _), (c, c_file), (d, _)] =
        <[_; 4]>::try_from(filed).map_err(|_| "four documents")?;
    client.execute(
        "UPDATE base_artifacts SET retain_until=now()+interval '1 day',legal_hold_at=now(),expires_at=NULL WHERE public_id=$1",
        &[&c_file],
    ).await?;

    println!("== access request: person 1");
    let access = privacy
        .receive(
            &mut client,
            &admin,
            NewRequest {
                kind: RequestKind::Access,
                person: PersonId::new(1)?,
                identifiers: Vec::new(),
                company: OrganisationId::new(1)?,
            },
        )
        .await?;
    let mut located = None;
    for (store, _, found) in privacy.locate(&client, &admin, access.id).await? {
        if store == STORE {
            located = Some(found.kinds.iter().map(|k| k.by_person).sum::<i64>());
        }
    }
    ensure(
        located == Some(3),
        "locate counts the three documents related to person 1",
    )?;
    let access = privacy
        .verify(
            &mut client,
            &admin,
            access.id,
            access.version,
            "Passport checked at counter",
            &[],
        )
        .await?;
    let access = privacy
        .approve(&mut client, &admin, access.id, access.version)
        .await?;
    let access = settle(&privacy, &client, &admin, access.id).await?;
    ensure(access.status == "completed", "the access request completed")?;
    let (_, bytes, _) = privacy
        .request_document(&mut client, &admin, access.id, None)
        .await?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let bundle: Value = {
        use std::io::Read;
        let mut text = String::new();
        zip.by_name("export.json")?.read_to_string(&mut text)?;
        serde_json::from_str(&text)?
    };
    let records = bundle["stores"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["store"] == STORE)
        .map(|s| s["records"].clone())
        .ok_or("export has no documents store")?;
    let exported: Vec<i64> = records["documents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["id"].as_i64())
        .collect();
    println!("  exported documents: {exported:?}");
    ensure(
        exported == vec![a, b, c],
        "A, B and C are exported, D is not",
    )?;
    ensure(
        records["documents"][0]["revisions"][0]["file_name"] == "ada-passport.txt",
        "each document is exported with its revisions",
    )?;
    let mut files = Vec::new();
    for index in 0..zip.len() {
        use std::io::Read;
        let mut entry = zip.by_index(index)?;
        if entry.name()?.contains("document-") {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            files.push(text);
        }
    }
    println!("  exported files: {files:?}");
    ensure(
        files.len() == 3
            && [
                "Ada passport scan",
                "Ada supplier contract",
                "Ada disputed invoice",
            ]
            .iter()
            .all(|name| files.iter().any(|f| f.contains(name))),
        "the three files are in the export, read through the owner",
    )?;

    println!("== erasure request: person 1");
    let erasure = privacy
        .receive(
            &mut client,
            &admin,
            NewRequest {
                kind: RequestKind::Erasure,
                person: PersonId::new(1)?,
                identifiers: Vec::new(),
                company: OrganisationId::new(1)?,
            },
        )
        .await?;
    let erasure = privacy
        .verify(
            &mut client,
            &admin,
            erasure.id,
            erasure.version,
            "Signed request, ID checked",
            &[],
        )
        .await?;
    let decided = erasure_approval::decide(
        &app,
        &privacy,
        &mut client,
        &admin,
        &second.clone().presenting_step_up(&code(&second_key, 0)),
        &erasure,
        true,
    )
    .await?;
    let erased = settle(&privacy, &client, &admin, decided.id).await?;
    for store in erased.stores.iter().filter(|s| s.store == STORE) {
        println!(
            "  store {} result={}",
            store.store,
            serde_json::to_string(&store.result)?
        );
    }
    ensure(erased.status == "completed", "the erasure completed")?;
    let mut gone = false;
    for _ in 0..150 {
        gone = client
            .query_opt(
                "SELECT 1 FROM core_documents WHERE tenant_id=1 AND id=$1",
                &[&a],
            )
            .await?
            .is_none();
        if gone {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    ensure(gone, "A is removed by its delete job")?;
    let deleted: bool = client
        .query_one(
            "SELECT deleted_at IS NOT NULL FROM base_artifacts WHERE public_id=$1",
            &[&a_file],
        )
        .await?
        .get(0);
    ensure(deleted, "A's file is deleted")?;
    for needle in ["Ada passport scan", "ada-passport.txt"] {
        ensure(
            holding(&client, needle).await? == 0,
            &format!("no document row or document audit value holds {needle:?}"),
        )?;
    }
    let links = |id: i64| {
        let client = &client;
        async move {
            Ok::<Vec<String>, Box<dyn std::error::Error>>(client
                .query("SELECT record_kind FROM core_document_links WHERE tenant_id=1 AND document_id=$1 ORDER BY record_kind", &[&id])
                .await?
                .iter()
                .map(|row| row.get(0))
                .collect())
        }
    };
    ensure(
        links(b).await? == vec!["company".to_owned()],
        "B keeps its company link and loses the person link",
    )?;
    ensure(
        documents.get(&admin, b).await?.name == "Ada supplier contract",
        "B is kept as shared",
    )?;
    ensure(
        documents
            .get(&admin, c)
            .await
            .is_ok_and(|d| d.name == "Ada disputed invoice")
            && links(c).await? == vec!["person".to_owned()],
        "C is kept under its file's legal hold",
    )?;
    ensure(
        documents.get(&admin, d).await?.name == "Company price list",
        "D is untouched",
    )?;

    stop.send(true)?;
    let _ = workers.await;
    println!("document privacy walkthrough passed");
    Ok(())
}
