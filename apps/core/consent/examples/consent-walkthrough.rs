//! Manual running-service walkthrough of Core Consent's personal-data store
//! (ADR 05, issue #467): an access request exports every decision version,
//! suppression and dispatch check of the person and of the addresses the
//! operator confirmed; an erasure clears their addresses, evidence and
//! correlation keys, keeps the bare facts, and keeps an active suppression as
//! a keyed hash the dispatch check still refuses. Another person's address and
//! tenant 2's rows for the same address are left alone. Explicit disposable DB
//! only; fails the executable when the observed state breaks the contract.
use bilibi_authz::grants::Grants;
use bilibi_consent::{
    Answer, Capture, Channel, Consent, Contact, DecisionInput, Question, Reason, Refusal, Source,
    SuppressionInput, SERVICE as CONSENT,
};
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

const STORE: &str = "core/consent.consent";
const EMAIL: &str = "ada.private@example.net";
const PHONE: &str = "+60123456789";
const OTHER: &str = "kim.unrelated@example.net";
/// Every free text written about person 1 or their two addresses.
const EVIDENCE: [&str; 8] = [
    "Ticked Send me offers on form v3",
    "Replied STOP to the June newsletter",
    "Signed up at the trade fair stand",
    "Keyword JOIN from the phone",
    "Complained on a phone call",
    "List-Unsubscribe from the private mailbox",
    "Carrier said unreachable",
    "Ada confirmed the number on a call",
];

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
fn summary(request: &RequestView) {
    println!(
        "request {} {:?} status={}",
        request.id, request.kind, request.status
    );
    for store in request.stores.iter().filter(|s| s.store == STORE) {
        println!(
            "  store {} {} attempts={} error={:?} result={}",
            store.store,
            store.state,
            store.attempts,
            store.last_error,
            store.result.as_ref().map_or("-".into(), Value::to_string)
        );
    }
}

fn contact(person: Option<i64>, address: Option<&str>) -> Contact {
    Contact {
        person: person.map(|p| PersonId::new(p).expect("person")),
        address: address.map(str::to_owned),
    }
}
fn question(company: i64, who: Contact, channel: Channel) -> Question {
    Question {
        company_id: company,
        contact: who,
        channel,
        purpose: "marketing".into(),
    }
}
/// One consent write in its own transaction, as a sender or a staff page makes it.
enum Write<'a> {
    Decide(DecisionInput),
    Suppress(SuppressionInput),
    Lift(i64, &'a str),
    Check(Question, &'a str),
}
async fn write(
    consent: &Consent,
    boundary: &MutationBoundary,
    client: &mut Client,
    scope: &Scope,
    write: Write<'_>,
) -> Result<(i64, Option<Answer>)> {
    let mut tx = boundary
        .begin(client, scope.mutation_context("walkthrough.consent"))
        .await?;
    let refused = |e| format!("consent write: {e:?}");
    let done = match write {
        Write::Decide(input) => (
            consent
                .record(&mut tx, scope, &input)
                .await
                .map_err(refused)?
                .id,
            None,
        ),
        Write::Suppress(input) => (
            consent
                .suppress(&mut tx, scope, &input)
                .await
                .map_err(refused)?
                .id,
            None,
        ),
        Write::Lift(id, reason) => (
            consent
                .lift(&mut tx, scope, id, reason)
                .await
                .map_err(refused)?
                .id,
            None,
        ),
        Write::Check(q, key) => (
            0,
            Some(
                consent
                    .check(&mut tx, scope, &q, key, "walkthrough/campaigns")
                    .await
                    .map_err(refused)?,
            ),
        ),
    };
    tx.commit().await?;
    Ok(done)
}

/// Prints the four tables' rows of `tenant`.
async fn tables(client: &Client, tenant: i64) -> Result<()> {
    for (table, columns) in [
        ("core_consent_decisions", "id,company_id,person_id,address,evidence,granted,version,erased_at IS NOT NULL"),
        ("core_consent_current", "id,company_id,person_id,address,granted,version"),
        ("core_consent_suppressions", "id,company_id,person_id,address,evidence,reason,lifted_at IS NOT NULL,lift_reason,address_hash IS NOT NULL,erased_at IS NOT NULL"),
        ("core_consent_log", "id,company_id,person_id,address,key,outcome,erased_at IS NOT NULL"),
    ] {
        let cells: Vec<String> = columns.split(',').map(|c| format!("coalesce(({c})::text,'null')")).collect();
        let rows = client
            .query(
                &format!("SELECT concat_ws(' | ',{}) FROM {table} WHERE tenant_id=$1 ORDER BY id", cells.join(",")),
                &[&tenant],
            )
            .await?;
        println!("  {table} (tenant {tenant}; {columns}): {} rows", rows.len());
        for row in rows {
            println!("    {}", row.get::<_, String>(0));
        }
    }
    Ok(())
}
/// Rows of the four tables in `tenant` holding `needle` in any column, with
/// punctuation and spaces ignored (so `+60 12-345 6789` is the same number).
async fn holding(client: &Client, tenant: i64, needle: &str) -> Result<i64> {
    let mut found = 0;
    for table in [
        "core_consent_decisions",
        "core_consent_current",
        "core_consent_suppressions",
        "core_consent_log",
    ] {
        found += client
            .query_one(
                &format!("SELECT count(*) FROM {table} t WHERE tenant_id=$1 AND regexp_replace(row_to_json(t)::text,'[^0-9a-zA-Z@.]','','g') ILIKE '%'||regexp_replace($2,'[^0-9a-zA-Z@.]','','g')||'%'"),
                &[&tenant, &needle],
            )
            .await?
            .get::<_, i64>(0);
    }
    Ok(found)
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
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35574")?;
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
    // Tenant 2's manager records consent for its own company.
    client
        .batch_execute("INSERT INTO role_capabilities(tenant_id,role_id,capability) VALUES(2,5,'core.consent.view'),(2,5,'core.consent.record'),(2,5,'core.consent.check') ON CONFLICT DO NOTHING")
        .await?;
    let admin = login(&app, "admin@example.test").await?;
    let second = login(&app, "ai@agent.my").await?;
    let other = login(&app, "manager@other.example.test").await?;
    let privacy = app.services.get::<Privacy>(SERVICE)?;
    let consent = app.services.get::<Consent>(CONSENT)?;
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
                admin.mutation_context("walkthrough.consent.configure"),
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
                .put(&mut tx, &admin, Target::Global, key, value)
                .await?;
        }
        tx.commit().await?;
    }
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));

    println!(
        "== consent for person 1, their two addresses, another person's address, and tenant 2"
    );
    use Capture::{InPerson, Web};
    use Channel::{Email, Sms};
    use Source::{Agent, InboundKeyword, Portal};
    let ada = Some(1);
    let (t1, t2) = (&admin, &other);
    let decisions = [
        (t1, 1, ada, None, Email, true, Portal, Web, EVIDENCE[0]),
        (
            t1,
            1,
            ada,
            None,
            Email,
            false,
            InboundKeyword,
            Capture::Email,
            EVIDENCE[1],
        ),
        (
            t1,
            1,
            None,
            Some(EMAIL),
            Email,
            true,
            Agent,
            InPerson,
            EVIDENCE[2],
        ),
        (
            t1,
            1,
            None,
            Some("+60 12-345 6789"),
            Sms,
            true,
            InboundKeyword,
            Capture::Sms,
            EVIDENCE[3],
        ),
        (
            t1,
            1,
            None,
            Some(OTHER),
            Email,
            true,
            Agent,
            Web,
            "Kim's own sign-up",
        ),
        (
            t2,
            100,
            None,
            Some(EMAIL),
            Email,
            true,
            Agent,
            Web,
            "Tenant 2 sign-up",
        ),
    ];
    for (scope, company, person, address, channel, granted, source, captured_on, evidence) in
        decisions
    {
        let input = DecisionInput {
            company_id: company,
            contact: contact(person, address),
            purpose: "marketing".into(),
            channel,
            granted,
            source,
            captured_on,
            evidence: Some(evidence.into()),
        };
        write(
            &consent,
            &boundary,
            &mut client,
            scope,
            Write::Decide(input),
        )
        .await?;
    }
    let suppressions = [
        (t1, 1, ada, None, Sms, Reason::Complaint, EVIDENCE[4]),
        (
            t1,
            1,
            None,
            Some(EMAIL),
            Email,
            Reason::Unsubscribe,
            EVIDENCE[5],
        ),
        (
            t1,
            1,
            None,
            Some(PHONE),
            Sms,
            Reason::HardBounce,
            EVIDENCE[6],
        ),
        (
            t1,
            1,
            None,
            Some(PHONE),
            Sms,
            Reason::Complaint,
            "Complaint by SMS reply",
        ),
        (
            t1,
            1,
            None,
            Some(OTHER),
            Email,
            Reason::HardBounce,
            "Kim's mailbox bounced",
        ),
        (
            t2,
            100,
            None,
            Some(EMAIL),
            Email,
            Reason::Unsubscribe,
            "Tenant 2 unsubscribe",
        ),
    ];
    let mut ids = Vec::new();
    for (scope, company, person, address, channel, reason, evidence) in suppressions {
        let input = SuppressionInput {
            company_id: company,
            contact: contact(person, address),
            channel,
            reason,
            evidence: Some(evidence.into()),
        };
        ids.push(
            write(
                &consent,
                &boundary,
                &mut client,
                scope,
                Write::Suppress(input),
            )
            .await?
            .0,
        );
    }
    // The hard bounce on the phone is lifted; its complaint stays active.
    write(
        &consent,
        &boundary,
        &mut client,
        t1,
        Write::Lift(ids[2], EVIDENCE[7]),
    )
    .await?;
    for (person, address, channel, key) in [
        (ada, None, Email, "newsletter-7:person-1"),
        (None, Some(EMAIL), Email, "newsletter-7:ada-private"),
        (None, Some(PHONE), Sms, "sms-3:ada-phone"),
        (None, Some(OTHER), Email, "newsletter-7:kim"),
    ] {
        let q = question(1, contact(person, address), channel);
        let (_, answer) = write(&consent, &boundary, &mut client, t1, Write::Check(q, key)).await?;
        println!("check {key}: {answer:?}");
    }
    tables(&client, 1).await?;
    let suppressed = Answer::Refused(Refusal::Suppressed);

    println!("== access request: person 1, giving {EMAIL} and their phone as 012-345 6789");
    let identifiers = vec![
        Identifier::Email(EMAIL.to_uppercase()),
        Identifier::Phone("012-345 6789".into()),
    ];
    let access = privacy
        .receive(
            &mut client,
            &admin,
            NewRequest {
                kind: RequestKind::Access,
                person: PersonId::new(1)?,
                identifiers: identifiers.clone(),
                company: OrganisationId::new(1)?,
            },
        )
        .await?;
    let mut confirm = Vec::new();
    for (store, _, located) in privacy.locate(&client, &admin, access.id).await? {
        if store == STORE {
            println!("  located kinds: {:?}", located.kinds);
            for candidate in &located.candidates {
                println!(
                    "  candidate {} {}: {}",
                    candidate.kind, candidate.id, candidate.summary
                );
                confirm.push(ConfirmedRecord {
                    store: STORE.into(),
                    kind: candidate.kind.clone(),
                    id: candidate.id.clone(),
                });
            }
            ensure(
                located.candidates.len() == 2,
                "one candidate per matched address, none for the other person's",
            )?;
            ensure(
                located
                    .candidates
                    .iter()
                    .all(|c| !c.summary.contains(EMAIL) && !c.summary.contains("6789")),
                "candidate summaries show no address",
            )?;
        }
    }
    let access = privacy
        .verify(
            &mut client,
            &admin,
            access.id,
            access.version,
            "Passport checked at counter",
            &confirm,
        )
        .await?;
    let access = privacy
        .approve(&mut client, &admin, access.id, access.version)
        .await?;
    let access = settle(&privacy, &client, &admin, access.id).await?;
    summary(&access);
    let (_, bytes, _) = privacy
        .request_document(&mut client, &admin, access.id, None)
        .await?;
    let bundle: Value = {
        use std::io::Read;
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
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
        .ok_or("export has no consent store")?;
    for (section, rows) in records.as_object().into_iter().flatten() {
        println!(
            "  export {section}: {} rows",
            rows.as_array().map_or(0, Vec::len)
        );
        for row in rows.as_array().into_iter().flatten() {
            println!("    {row}");
        }
    }
    let exported = records.to_string();
    ensure(
        EVIDENCE.iter().all(|e| exported.contains(e)),
        "every evidence text and lift reason is exported",
    )?;
    ensure(
        exported.contains(EMAIL) && exported.contains(PHONE),
        "both confirmed addresses are exported",
    )?;
    ensure(
        records["consent_decisions"].as_array().map_or(0, Vec::len) == 4,
        "all four decision versions (two of the person, one per address)",
    )?;
    ensure(
        records["consent_suppressions"]
            .as_array()
            .map_or(0, Vec::len)
            == 4,
        "the person's suppression and three of the addresses (one lifted)",
    )?;
    ensure(
        records["consent_checks"].as_array().map_or(0, Vec::len) == 3,
        "three dispatch checks",
    )?;
    ensure(
        !exported.contains(OTHER) && !exported.contains("Tenant 2"),
        "no other person's address and nothing of tenant 2",
    )?;

    println!("== erasure request: person 1, both addresses confirmed");
    let erasure = privacy
        .receive(
            &mut client,
            &admin,
            NewRequest {
                kind: RequestKind::Erasure,
                person: PersonId::new(1)?,
                identifiers,
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
            &confirm,
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
    summary(&erased);
    ensure(erased.status == "completed", "the erasure completed")?;
    tables(&client, 1).await?;
    for needle in EVIDENCE.iter().copied().chain([
        EMAIL,
        PHONE,
        "newsletter-7:ada-private",
        "sms-3:ada-phone",
        "newsletter-7:person-1",
    ]) {
        ensure(
            holding(&client, 1, needle).await? == 0,
            &format!("no row of the four tables holds {needle:?}"),
        )?;
    }
    let audit: i64 = client
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type LIKE 'core.consent.%' AND (old_values::text||new_values::text) ILIKE ANY($1)",
            &[&vec![format!("%{EMAIL}%"), format!("%{PHONE}%"), format!("%{}%", EVIDENCE[2])]],
        )
        .await?
        .get(0);
    ensure(
        audit == 0,
        "audit evidence of the erased consent records holds neither address nor evidence",
    )?;
    let kept: i64 = client
        .query_one("SELECT count(*) FROM core_consent_suppressions WHERE tenant_id=1 AND address_hash IS NOT NULL AND lifted_at IS NULL", &[])
        .await?
        .get(0);
    ensure(
        kept == 2,
        "both active address suppressions are kept as keyed hashes",
    )?;
    let lifted: i64 = client
        .query_one("SELECT count(*) FROM core_consent_suppressions WHERE tenant_id=1 AND lifted_at IS NOT NULL AND address IS NULL AND address_hash IS NULL AND lift_reason='[erased]'", &[])
        .await?
        .get(0);
    ensure(
        lifted == 1,
        "the lifted suppression keeps no address or hash and its reason is erased",
    )?;
    let envelope: Vec<String> = client
        .query(
            "SELECT jsonb_object_keys(sealed) FROM core_consent_keys WHERE tenant_id=1",
            &[],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    println!("  tenant 1 address key envelope fields: {envelope:?}");
    ensure(
        !envelope.is_empty(),
        "tenant 1's address key is stored as a sealed envelope",
    )?;

    println!("== dispatch checks after the erasure");
    let answer = consent
        .preview(&admin, &question(1, contact(None, Some(EMAIL)), Email))
        .await
        .map_err(|e| format!("{e:?}"))?;
    println!("  preview {EMAIL}: {answer:?}");
    ensure(
        answer == suppressed,
        "the erased email address is still suppressed",
    )?;
    let suppressed = Some(suppressed);
    for (address, channel, key, expect, message) in [
        (
            "Ada.Private@Example.NET ",
            Email,
            "newsletter-8:ada-private",
            true,
            "a logged check of the erased address, typed differently, is refused as suppressed",
        ),
        (
            "+60 12 345 6789",
            Sms,
            "sms-4:ada-phone",
            true,
            "the erased phone number's active complaint still suppresses it",
        ),
        (
            EMAIL,
            Channel::Messaging,
            "chat-1:ada-private",
            false,
            "the hash suppresses only its own channel",
        ),
        (
            OTHER,
            Email,
            "newsletter-8:kim",
            true,
            "the other person's address keeps its own suppression",
        ),
    ] {
        let q = question(1, contact(None, Some(address)), channel);
        let (_, answer) = write(&consent, &boundary, &mut client, t1, Write::Check(q, key)).await?;
        println!("  check {key}: {answer:?}");
        ensure((answer == suppressed) == expect, message)?;
    }
    ensure(
        holding(&client, 1, OTHER).await? == 5,
        "the other person's decision, current state, suppression and checks are untouched",
    )?;
    let answer = consent
        .preview(&other, &question(100, contact(None, Some(EMAIL)), Email))
        .await
        .map_err(|e| format!("{e:?}"))?;
    println!("  tenant 2 preview {EMAIL}: {answer:?}");
    ensure(
        Some(answer) == suppressed,
        "tenant 2 still answers from its own row",
    )?;
    ensure(
        holding(&client, 2, EMAIL).await? == 3
            && holding(&client, 2, "Tenant 2 unsubscribe").await? == 1,
        "tenant 2's rows for the same address are untouched",
    )?;
    ensure(
        client
            .query_opt("SELECT 1 FROM core_consent_keys WHERE tenant_id=2", &[])
            .await?
            .is_none(),
        "tenant 2 has no key: nothing of it was hashed",
    )?;
    let log: i64 = client
        .query_one("SELECT count(*) FROM core_consent_log WHERE tenant_id=1 AND erased_at IS NOT NULL AND address IS NULL AND key='[erased]'", &[])
        .await?
        .get(0);
    ensure(log == 3, "the three erased checks keep their bare fact")?;

    stop.send(true)?;
    let _ = workers.await;
    println!("consent walkthrough passed");
    Ok(())
}
