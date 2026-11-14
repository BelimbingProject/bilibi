//! Documents related to a person reach privacy requests (ADR 05, #583), on a
//! disposable database, through `base/privacy` as an operator runs them.
//!
//! Files seven documents: A related only to person 1, B related to person 1
//! and company 1, C related only to person 1 with its file under legal hold,
//! D related only to company 1, E related to person 1 whose upload failed
//! its malware check (held: its bytes stay a Media file, never an Artifact),
//! F related to person 1 and already being deleted, and G related to person
//! 1 with a privacy hold on its revision. An access request for person 1 must
//! export A, B, C, E and G with their revisions and their clean files,
//! never D or F. An approved erasure must then delete A, E and F (their names
//! and file names gone from the tables and the audit trail, A's Artifact and
//! E's Media bytes gone from disk) and count them deleted, keep B as shared
//! with only its person link removed, keep C under its file's hold and G
//! under its revision's hold, and leave D alone (#583, #659). Exits non-zero
//! when a check fails.
use bilibi_authz::grants::Grants;
use bilibi_database::{pool, MutationBoundary};
use bilibi_document::{Documents, Draft, Reference, RevisionInput, Upload, SERVICE as DOCUMENTS};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_privacy::{HoldTarget, NewRequest, Privacy, RequestKind, RequestView, SERVICE};
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

/// Fixture scanner, not a production adapter: a "medical" file fails the
/// check, so its upload is held as a Media file; every other file is clean.
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
        bytes: &'a [u8],
        _facts: bilibi_media::ScanFacts<'a>,
    ) -> bilibi_media::ScanFuture<'a> {
        Box::pin(async move {
            if String::from_utf8_lossy(bytes).contains("medical") {
                bilibi_media::Verdict::Infected {
                    signature: "fixture".into(),
                }
            } else {
                bilibi_media::Verdict::Clean
            }
        })
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
/// Rows of the document tables, the document audit trail and document
/// notices holding `needle`.
async fn holding(client: &Client, needle: &str) -> Result<i64> {
    Ok(client.query_one(
        "SELECT (SELECT count(*) FROM core_documents d WHERE d.tenant_id=1 AND d::text LIKE $1)
              + (SELECT count(*) FROM core_document_revisions r WHERE r.tenant_id=1 AND r::text LIKE $1)
              + (SELECT count(*) FROM core_document_files f WHERE f.tenant_id=1 AND f::text LIKE $1)
              + (SELECT count(*) FROM base_audit_mutations m WHERE m.tenant_id=1 AND m.auditable_type LIKE 'core.document.%' AND (coalesce(m.old_values::text,'') || coalesce(m.new_values::text,'')) LIKE $1)
              + (SELECT count(*) FROM notifications n WHERE n.type LIKE 'core.document.%' AND n::text LIKE $1)",
        &[&format!("%{needle}%")],
    ).await?.get(0))
}
/// Whether a file under `dir` holds `needle`.
fn on_disk(dir: &std::path::Path, needle: &[u8]) -> Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if on_disk(&path, needle)? {
                return Ok(true);
            }
        } else if std::fs::read(&path)?
            .windows(needle.len())
            .any(|w| w == needle)
        {
            return Ok(true);
        }
    }
    Ok(false)
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

    println!("== seven documents: A, B (also company 1), C (file held), D (company 1 only), E (upload held), F (deleting), G (revision held)");
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
        ("Ada medical note", "ada-medical.txt", vec![person.clone()]),
        ("Ada old payslip", "ada-payslip.txt", vec![person.clone()]),
        ("Ada bank letter", "ada-bank.txt", vec![person.clone()]),
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
    let [(a, a_file), (b, _), (c, c_file), (d, _), (e, e_file), (f, _), (g, _)] =
        <[_; 7]>::try_from(filed).map_err(|_| "seven documents")?;
    let e_subject = e_file
        .strip_prefix("upload:")
        .ok_or("E's upload is held, not an Artifact")?
        .to_owned();
    // F's delete was confirmed and its job has not finished yet.
    client
        .execute(
            "UPDATE core_documents SET deleting_at=now() WHERE tenant_id=1 AND id=$1",
            &[&f],
        )
        .await?;
    // A follower of A is told of a new revision: the notice must not carry
    // A's name or the change log, which no erasure reaches.
    for sql in [
        "INSERT INTO core_document_access(tenant_id,document_id,user_id,can_view,granted_by) VALUES(1,$1,$2,true,$3)",
        "INSERT INTO core_document_marks(tenant_id,user_id,document_id,following) SELECT 1,$2,$1,true WHERE $3::bigint IS NOT NULL",
    ] {
        client.execute(sql, &[&a, &second.user(), &admin.user()]).await?;
    }
    {
        let renewed = documents
            .store_file(
                &admin,
                Upload {
                    bytes: b"Ada passport scan, renewed\n",
                    file_name: "ada-passport-renewed.txt",
                },
            )
            .await?;
        let version = documents.get(&admin, a).await?.version;
        let mut tx = boundary
            .begin(
                &mut client,
                admin.mutation_context("core.document.add_revision"),
            )
            .await?;
        documents
            .add_revision(
                &mut tx,
                &admin,
                a,
                version,
                &RevisionInput {
                    artifact_id: Some(renewed.artifact_id),
                    change_log: "Ada renewed passport".into(),
                    ..RevisionInput::default()
                },
            )
            .await?;
        tx.commit().await?;
    }
    let notices: i64 = client
        .query_one(
            "SELECT count(*) FROM notifications WHERE tenant_id=1 AND user_id=$1 AND type='core.document.revision'",
            &[&second.user()],
        )
        .await?
        .get(0);
    ensure(notices == 1, "A's follower is told of its new revision")?;
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
        located == Some(6),
        "locate counts the six documents related to person 1, F too",
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
        exported == vec![a, b, c, e, g],
        "A, B, C, E and G are exported, D and F (deleting) are not",
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
        files.len() == 5
            && [
                "Ada passport scan",
                "Ada supplier contract",
                "Ada disputed invoice",
                "Ada bank letter",
            ]
            .iter()
            .all(|name| files.iter().any(|f| f.contains(name))),
        "the five clean files (two of A) are in the export, read through the owner",
    )?;

    let g_revision: i64 = client
        .query_one(
            "SELECT id FROM core_document_revisions WHERE tenant_id=1 AND document_id=$1",
            &[&g],
        )
        .await?
        .get(0);
    privacy
        .place_hold(
            &mut client,
            &admin,
            HoldTarget::Record {
                kind: "core.document.revision".into(),
                id: g_revision,
            },
            "Bank dispute pending",
        )
        .await?;

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
    let deleted: i64 = erased
        .stores
        .iter()
        .filter(|s| s.store == STORE)
        .flat_map(|s| {
            s.result
                .as_ref()
                .and_then(|r| r["actions"].as_array().cloned())
                .unwrap_or_default()
        })
        .filter(|a| a["kind"] == "core.document.document" && a["action"] == "deleted")
        .filter_map(|a| a["count"].as_i64())
        .sum();
    ensure(deleted == 3, "the certificate counts A, E and F deleted")?;
    let left: i64 = client
        .query_one(
            "SELECT count(*) FROM core_documents WHERE tenant_id=1 AND id = ANY($1)",
            &[&vec![a, e, f]],
        )
        .await?
        .get(0);
    ensure(
        left == 0,
        "A, E and F are removed when the erasure finishes",
    )?;
    let deleted: bool = client
        .query_one(
            "SELECT deleted_at IS NOT NULL FROM base_artifacts WHERE public_id=$1",
            &[&a_file],
        )
        .await?
        .get(0);
    ensure(deleted, "A's file is deleted")?;
    let held_file = client
        .query_opt(
            "SELECT state, bytes_removed_at IS NOT NULL FROM base_media_attachments WHERE tenant_id=1 AND subject_type='core.document.upload' AND subject_id=$1",
            &[&e_subject],
        )
        .await?
        .map(|row| (row.get::<_, String>(0), row.get::<_, bool>(1)));
    ensure(
        held_file == Some(("erased".into(), true)),
        "E's held upload is an erased Media file whose bytes are removed",
    )?;
    let media_names: i64 = client
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type LIKE 'base/media/%' AND (coalesce(old_values::text,'') || coalesce(new_values::text,'')) LIKE '%ada-medical.txt%'",
            &[],
        )
        .await?
        .get(0);
    ensure(media_names == 0, "no Media audit value holds E's file name")?;
    for needle in ["Ada passport scan", "Ada medical note"] {
        ensure(
            !on_disk(std::path::Path::new(&root), needle.as_bytes())?,
            &format!("no stored file holds {needle:?}"),
        )?;
    }
    for needle in [
        "Ada passport scan",
        "ada-passport.txt",
        "Ada medical note",
        "ada-medical.txt",
        "Ada old payslip",
        "ada-payslip.txt",
        "Ada renewed passport",
    ] {
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
    ensure(
        documents
            .get(&admin, g)
            .await
            .is_ok_and(|d| d.name == "Ada bank letter")
            && links(g).await? == vec!["person".to_owned()],
        "G is kept under its revision's privacy hold",
    )?;

    stop.send(true)?;
    let _ = workers.await;
    println!("document privacy walkthrough passed");
    Ok(())
}
