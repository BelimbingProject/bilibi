//! Companies reach privacy requests (ADR 05, #595), on a disposable database,
//! through `base/privacy` as an operator runs them.
//!
//! Creates three companies through the Company facade: A, Ada's sole
//! proprietorship, with her email, tax ID, legal name and a custom "Contact
//! name"; B, a private limited company with her phone beside its own shared
//! email, tax ID, registration number, legal name, website and custom value;
//! E, a company whose email only contains hers, which the operator does not
//! confirm. Requests name person 1 with Ada's email and phone. Locate must
//! list A, B and E as candidates, each naming its legal form and which
//! identifier matched; an access request confirming A and B must export A
//! whole and only B's name, legal form and matching phone; an approved
//! erasure must then clear Ada's values from A and B and from their audit
//! history, erase A's name and code (a sole proprietorship is its owner's),
//! leave B every business value but her phone, keep the history of fields
//! that are not personal (status), and leave E and its history alone. Exits
//! non-zero when a check fails.
use bilibi_authz::grants::Grants;
use bilibi_company::{
    CompanyFieldValues, CompanyService, CreateOutcome, NewCompany, SERVICE as COMPANIES,
};
use bilibi_custom_fields::{CustomFields, NewField};
use bilibi_database::{pool, MutationBoundary};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_personal_data::Identifier;
use bilibi_privacy::{ConfirmedRecord, NewRequest, Privacy, RequestKind, RequestView, SERVICE};
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

const STORE: &str = "core/company.companies";
const EMAIL: &str = "ada@lovelace.test";
const PHONE: &str = "+60 12-345 6789";

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
/// Rows of `companies` and of their audit trail, among `ids`, holding `needle`.
async fn holding(client: &Client, ids: &[i64], needle: &str) -> Result<i64> {
    Ok(client.query_one(
        "SELECT (SELECT count(*) FROM companies c WHERE c.tenant_id=1 AND c.id=ANY($2) AND c::text ILIKE $1)
              + (SELECT count(*) FROM base_audit_mutations m WHERE m.tenant_id=1 AND m.auditable_type='company' AND m.auditable_id=ANY($2::bigint[]::text[])
                 AND (coalesce(m.old_values::text,'') || coalesce(m.new_values::text,'')) ILIKE $1)",
        &[&format!("%{needle}%"), &ids],
    ).await?.get(0))
}
fn confirm(ids: &[i64]) -> Vec<ConfirmedRecord> {
    ids.iter()
        .map(|id| ConfirmedRecord {
            store: STORE.into(),
            kind: "company".into(),
            id: id.to_string(),
        })
        .collect()
}
fn request(kind: RequestKind) -> Result<NewRequest> {
    Ok(NewRequest {
        kind,
        person: PersonId::new(1)?,
        identifiers: vec![
            Identifier::Email(EMAIL.into()),
            Identifier::Phone(PHONE.into()),
        ],
        company: OrganisationId::new(1)?,
    })
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
    // Background work and the export's file need real desks (ADR 19).
    db.get().await?.batch_execute("INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,3,1) ON CONFLICT DO NOTHING; \
        SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35578")?;
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
    let companies = app.services.get::<CompanyService>(COMPANIES)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let fields = app
        .services
        .get::<CustomFields>(bilibi_custom_fields::SERVICE)?;
    let second_key = enroll(&app, &second).await?;
    erasure_approval::publish_rules(&app, &mut client, &admin, 1).await?;
    {
        let mut tx = boundary
            .begin(
                &mut client,
                admin.mutation_context("walkthrough.company.configure"),
            )
            .await?;
        let chromium =
            std::env::var("BILIBI_PRIVACY_DEMO_CHROMIUM").unwrap_or_else(|_| "chromium".into());
        for (key, value) in [
            ("artifacts.storage_root", json!(root)),
            ("pdf.chromium.binary", json!(chromium)),
        ] {
            settings
                .put(&mut tx, &admin, Target::Global, key, value.clone())
                .await
                .map_err(|e| format!("{key}={value}: {e:?}"))?;
        }
        let defined = fields
            .define(
                &mut tx,
                &admin,
                &NewField {
                    record_type: "company".into(),
                    key: "contact_name".into(),
                    label: "Contact name".into(),
                    kind: "text".into(),
                    ..NewField::default()
                },
            )
            .await?;
        if let Err(errors) = defined {
            return Err(format!("define contact_name: {errors:?}").into());
        }
        tx.commit().await?;
    }
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));

    println!("== three companies: A (Ada's sole proprietorship), B (a business with her phone), E (an email that only contains hers)");
    let legal_form = |code: &'static str| {
        let client = &client;
        async move {
            Ok::<i64, Box<dyn std::error::Error>>(
                client
                    .query_one(
                        "SELECT id FROM company_legal_entity_types WHERE code=$1",
                        &[&code],
                    )
                    .await?
                    .get(0),
            )
        }
    };
    let sole = legal_form("sole_proprietorship").await?;
    let private_limited = legal_form("private_limited").await?;
    let mut created = Vec::new();
    for (name, new, custom) in [
        (
            "Ada Lovelace Consulting",
            NewCompany {
                legal_name: Some("Ada Lovelace".into()),
                legal_entity_type_id: Some(sole),
                tax_id: Some("TX-ADA-1815".into()),
                email: Some(EMAIL.into()),
                ..NewCompany::default()
            },
            Some("Ada Lovelace"),
        ),
        (
            "Analytical Engines Sdn Bhd",
            NewCompany {
                legal_name: Some("Analytical Engines Sdn. Bhd.".into()),
                legal_entity_type_id: Some(private_limited),
                tax_id: Some("TX-ENG-1842".into()),
                registration_number: Some("202601234567".into()),
                website: Some("https://engines.test".into()),
                email: Some("accounts@engines.test".into()),
                phone: Some(PHONE.into()),
                ..NewCompany::default()
            },
            Some("Accounts desk"),
        ),
        (
            "Lovelace Look-alike Trading",
            NewCompany {
                email: Some("ada@lovelace.test.my".into()),
                ..NewCompany::default()
            },
            Some("Someone else"),
        ),
    ] {
        let mut values = CompanyFieldValues::new();
        if let Some(contact) = custom {
            values.insert("custom".into(), json!({ "contact_name": contact }));
        }
        let new = NewCompany {
            name: name.into(),
            ..new
        };
        match companies
            .create_company_with_fields(&admin, &new, &values)
            .await?
        {
            CreateOutcome::Created(company) => created.push(company.id),
            CreateOutcome::Refused(errors) => return Err(format!("{name}: {errors:?}").into()),
        }
    }
    let [a, b, e] = <[i64; 3]>::try_from(created).map_err(|_| "three companies")?;
    ensure(
        holding(&client, &[a], "Ada Lovelace").await? > 0
            && holding(&client, &[a], "TX-ADA-1815").await? > 0,
        "A's values and their audit copies hold Ada's details before any request",
    )?;

    println!("== access request: person 1, {EMAIL}, {PHONE}");
    let access = privacy
        .receive(&mut client, &admin, request(RequestKind::Access)?)
        .await?;
    let mut candidates = None;
    let mut summaries = std::collections::HashMap::new();
    for (store, _, found) in privacy.locate(&client, &admin, access.id).await? {
        if store == STORE {
            for candidate in &found.candidates {
                println!("  candidate {}: {}", candidate.id, candidate.summary);
                summaries.insert(candidate.id.clone(), candidate.summary.clone());
            }
            let mut ids: Vec<String> = found.candidates.iter().map(|c| c.id.clone()).collect();
            ids.sort();
            println!(
                "  located: {:?}, candidates {ids:?}",
                found
                    .kinds
                    .iter()
                    .map(|k| (k.by_person, k.by_identifier))
                    .collect::<Vec<_>>()
            );
            candidates = Some(ids);
        }
    }
    let mut expected = vec![a.to_string(), b.to_string(), e.to_string()];
    expected.sort();
    ensure(
        candidates.as_ref() == Some(&expected),
        "locate lists A, B and E as candidates found by email or phone",
    )?;
    let summary = |id: i64| summaries.get(&id.to_string()).cloned().unwrap_or_default();
    ensure(
        summary(a).contains("Sole Proprietorship")
            && summary(a).contains("matched email")
            && summary(b).contains("Private Limited Company")
            && summary(b).contains("matched phone")
            && summaries
                .values()
                .all(|s| !s.contains(EMAIL) && !s.contains("6789")),
        "each candidate names its legal form and which identifier matched, with no values",
    )?;
    let access = privacy
        .verify(
            &mut client,
            &admin,
            access.id,
            access.version,
            "Passport checked at counter",
            &confirm(&[a, b]),
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
        .ok_or("export has no companies store")?;
    let exported: Vec<i64> = records["companies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_i64())
        .collect();
    println!("  exported companies: {exported:?}");
    ensure(
        exported == vec![a, b],
        "the confirmed A and B are exported, E is not",
    )?;
    ensure(
        records["companies"][0]["custom_fields"]["contact_name"] == "Ada Lovelace"
            && records["companies"][0]["tax_id"] == "TX-ADA-1815",
        "A is exported with its tax ID and custom fields",
    )?;
    let exported_b = &records["companies"][1];
    println!("  exported B: {exported_b}");
    ensure(
        exported_b["phone"] == PHONE
            && exported_b["legal_entity_type"] == "private_limited"
            && exported_b["email"].is_null()
            && !exported_b.to_string().contains("TX-ENG-1842")
            && !exported_b.to_string().contains("Accounts desk"),
        "B, a business, is exported with only her matching phone, not its own email, tax ID or custom fields",
    )?;

    println!("== erasure request: person 1, A and B confirmed");
    let erasure = privacy
        .receive(&mut client, &admin, request(RequestKind::Erasure)?)
        .await?;
    let erasure = privacy
        .verify(
            &mut client,
            &admin,
            erasure.id,
            erasure.version,
            "Signed request, ID checked",
            &confirm(&[a, b]),
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
    for needle in [EMAIL, "Ada Lovelace", "TX-ADA-1815", "123456789", "12-345"] {
        ensure(
            holding(&client, &[a, b], needle).await? == 0,
            &format!("no row or audit value of A or B holds {needle:?}"),
        )?;
    }
    let row = |id: i64| {
        let client = &client;
        async move {
            let row = client
                .query_one(
                    "SELECT name,code,email FROM companies WHERE tenant_id=1 AND id=$1",
                    &[&id],
                )
                .await?;
            Ok::<(String, String, String), Box<dyn std::error::Error>>((
                row.get(0),
                row.get(1),
                row.get(2),
            ))
        }
    };
    let (name, code_a, _) = row(a).await?;
    ensure(
        name == "[erased]" && code_a == format!("erased_{a}"),
        "the sole proprietorship's name and code are erased",
    )?;
    ensure(
        row(b).await?.0 == "Analytical Engines Sdn Bhd",
        "B keeps its business name",
    )?;
    let kept: Value = client
        .query_one(
            "SELECT to_jsonb(c) FROM companies c WHERE tenant_id=1 AND id=$1",
            &[&b],
        )
        .await?
        .get(0);
    println!(
        "  B after erasure: email={} phone={} tax_id={} legal_name={} registration_number={} website={} custom_fields={}",
        kept["email"], kept["phone"], kept["tax_id"], kept["legal_name"], kept["registration_number"], kept["website"], kept["custom_fields"]
    );
    ensure(
        kept["email"] == "accounts@engines.test"
            && kept["phone"] == ""
            && kept["tax_id"] == "TX-ENG-1842"
            && kept["legal_name"] == "Analytical Engines Sdn. Bhd."
            && kept["registration_number"] == "202601234567"
            && kept["website"] == "https://engines.test"
            && kept["custom_fields"]["contact_name"] == "Accounts desk",
        "B keeps its own email, tax ID, legal name, registration number, website and custom fields; only her phone is cleared",
    )?;
    let status_history: i64 = client
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type='company' AND auditable_id=$1::bigint::text AND new_values->>'status'='active'",
            &[&b],
        )
        .await?
        .get(0);
    ensure(
        status_history > 0,
        "B's audit history keeps its status, a field that is not personal",
    )?;
    ensure(
        row(e).await?.2 == "ada@lovelace.test.my"
            && holding(&client, &[e], "Someone else").await? > 0,
        "the unconfirmed E and its history are untouched",
    )?;

    stop.send(true)?;
    let _ = workers.await;
    println!("company privacy walkthrough passed");
    Ok(())
}
