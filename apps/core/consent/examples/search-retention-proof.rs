//! Running-database proof for #604 finding 2: the privacy locate's address
//! match and the Suppressions search read indexes instead of whole tables,
//! and the dispatch log has a retention class that keeps every check and
//! clears its address and correlation key once the company's period has
//! passed, skipping held checks, recent checks and companies that did not
//! turn the class on. Explicit disposable DB only. Every check runs and is
//! reported; the executable fails when any did not hold.
use bilibi_authz::grants::Grants;
use bilibi_consent::{
    Channel, Consent, Contact, Question, Reason, State, SuppressionFilter, SuppressionInput,
    SERVICE as CONSENT,
};
use bilibi_database::{pool, MutationBoundary};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_privacy::{setting_keys, HoldTarget, Privacy, SERVICE as PRIVACY, SWEEP_SCHEDULE};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const CLASS: &str = "core/consent.dispatch_log";
/// A log row: id, person, address, key, outcome, anonymised.
type LogRow = (i64, Option<i64>, Option<String>, String, String, bool);

#[derive(Default)]
struct Checks(usize);
impl Checks {
    fn ensure(&mut self, ok: bool, message: &str) {
        if ok {
            println!("  ok: {message}");
        } else {
            println!("  FAIL {message}");
            self.0 += 1;
        }
    }
}

fn address(value: &str) -> Contact {
    Contact {
        person: None,
        address: Some(value.into()),
    }
}

/// The plan of `sql` with `params`, as text.
async fn plan(
    client: &Client,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<String> {
    let rows = client.query(&format!("EXPLAIN {sql}"), params).await?;
    Ok(rows
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// One dispatch check in its own transaction; returns the log row's id.
async fn check(
    consent: &Consent,
    boundary: &MutationBoundary,
    client: &mut Client,
    scope: &Scope,
    company: i64,
    contact: Contact,
    key: &str,
) -> Result<i64> {
    let mut tx = boundary
        .begin(client, scope.mutation_context("walkthrough.consent"))
        .await?;
    let question = Question {
        company_id: company,
        contact,
        channel: Channel::Email,
        purpose: "marketing".into(),
    };
    consent
        .check(&mut tx, scope, &question, key, "walkthrough/campaigns")
        .await
        .map_err(|e| format!("check: {e:?}"))?;
    tx.commit().await?;
    Ok(client
        .query_one("SELECT id FROM core_consent_log WHERE key=$1", &[&key])
        .await?
        .get(0))
}

/// Saves one company 1 setting as System Settings does.
async fn put(
    boundary: &MutationBoundary,
    settings: &Settings,
    client: &mut Client,
    scope: &Scope,
    key: &str,
    value: Value,
) -> Result<()> {
    let mut tx = boundary
        .begin(client, scope.mutation_context("settings.company.save"))
        .await?;
    settings
        .put(
            &mut tx,
            scope,
            Target::Company(OrganisationId::new(1)?),
            key,
            value,
        )
        .await?;
    tx.commit().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_CONSENT_DEMO").as_deref() != Ok("yes") {
        return Err("explicit disposable DB opt-in required".into());
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    if std::env::args().nth(1).as_deref() == Some("seed") {
        return Ok(bilibi_web::seed::production(&db).await?);
    }
    // A Dev instance: granting the scheduler asks for no second factor.
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    // Consent writes need a desk in their company (ADR 19).
    db.get().await?.batch_execute("INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,4,2) ON CONFLICT DO NOTHING; \
        SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35576")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin = app.security.guard(&login.token).await?;
    let consent = app.services.get::<Consent>(CONSENT)?;
    let privacy = app.services.get::<Privacy>(PRIVACY)?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let mut client = db.get().await?;
    let mut checks = Checks::default();

    println!("== the dispatch log keeps its checks and drops their addresses after the period");
    let old_address = check(
        &consent,
        &boundary,
        &mut client,
        &admin,
        1,
        address("old.reader@example.net"),
        "retention-old-address",
    )
    .await?;
    let old_both = check(
        &consent,
        &boundary,
        &mut client,
        &admin,
        1,
        Contact {
            person: Some(PersonId::new(1)?),
            address: Some("ada.private@example.net".into()),
        },
        "retention-old-person",
    )
    .await?;
    let held = check(
        &consent,
        &boundary,
        &mut client,
        &admin,
        1,
        address("held.reader@example.net"),
        "retention-held",
    )
    .await?;
    let recent = check(
        &consent,
        &boundary,
        &mut client,
        &admin,
        1,
        address("recent.reader@example.net"),
        "retention-recent",
    )
    .await?;
    let other_company = check(
        &consent,
        &boundary,
        &mut client,
        &admin,
        2,
        address("other.reader@example.net"),
        "retention-company-2",
    )
    .await?;
    // Four checks made 40 days ago; the recent one stays today's.
    client
        .execute(
            "UPDATE core_consent_log SET at=now()-interval '40 days' WHERE id=ANY($1)",
            &[&vec![old_address, old_both, held, other_company]],
        )
        .await?;
    privacy
        .place_hold(
            &mut client,
            &admin,
            HoldTarget::Record {
                kind: bilibi_consent::LOG_RECORD.into(),
                id: held,
            },
            "Complaint under review",
        )
        .await?;
    let [enabled, period, _] = setting_keys(CLASS);
    let configured = async {
        put(
            &boundary,
            &settings,
            &mut client,
            &admin,
            &period,
            json!(30),
        )
        .await?;
        put(
            &boundary,
            &settings,
            &mut client,
            &admin,
            &enabled,
            json!(true),
        )
        .await
    }
    .await;
    checks.ensure(
        configured.is_ok(),
        &format!("company 1 turns on the dispatch log class with a 30-day period ({configured:?})"),
    );
    let schedule = app
        .services
        .get::<bilibi_base::schedule::Schedule>("base/schedule.store")?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));
    schedule
        .review(&mut client, &admin, SWEEP_SCHEDULE, true)
        .await?;
    grants
        .set_system(
            &admin,
            bilibi_base::schedule::PRINCIPAL,
            bilibi_base::schedule::EXECUTE,
            Some(true),
        )
        .await?;
    let _ = schedule
        .run_now(&mut client, &admin, SWEEP_SCHEDULE, jiff::Timestamp::now())
        .await?;
    for _ in 0..90 {
        let open: i64 = client
            .query_one(
                "SELECT count(*) FROM base_queue_jobs WHERE worker_id='base/privacy-retention-sweep' AND state NOT IN ('completed','cancelled','discarded')",
                &[],
            )
            .await?
            .get(0);
        if open == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let _ = stop.send(true);
    let _ = workers.await;
    let summary = client
        .query_opt(
            "SELECT acted,kept FROM base_privacy_sweeps WHERE tenant_id=1 AND class=$1 AND company_id=1 AND finished_at IS NOT NULL ORDER BY id DESC LIMIT 1",
            &[&CLASS],
        )
        .await?
        .map(|row| (row.get::<_, i64>(0), row.get::<_, i64>(1)));
    checks.ensure(
        summary == Some((2, 1)),
        &format!("company 1's sweep summary: 2 anonymised, 1 kept for its hold ({summary:?})"),
    );
    let rows: Vec<LogRow> = client
        .query(
            "SELECT id,person_id,address,key,outcome,erased_at IS NOT NULL FROM core_consent_log WHERE id=ANY($1) ORDER BY id",
            &[&vec![old_address, old_both, held, recent, other_company]],
        )
        .await?
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
        .collect();
    let row = |id: i64| rows.iter().find(|r| r.0 == id).cloned();
    checks.ensure(
        row(old_address)
            .is_some_and(|r| r.2.is_none() && r.3 == "[erased]" && r.4 == "no_consent" && r.5),
        "an old address check is kept with its outcome, its address and key cleared",
    );
    checks.ensure(
        row(old_both).is_some_and(|r| r.1 == Some(1) && r.2.is_none() && r.3 == "[erased]" && r.5),
        "an old check of a person keeps the person, its address and key cleared",
    );
    checks.ensure(
        row(held).is_some_and(|r| r.2.as_deref() == Some("held.reader@example.net") && !r.5),
        "a held old check keeps its address",
    );
    checks.ensure(
        row(recent).is_some_and(|r| {
            r.2.as_deref() == Some("recent.reader@example.net") && r.3 == "retention-recent"
        }),
        "a check inside the period is untouched",
    );
    checks.ensure(
        row(other_company).is_some_and(|r| r.2.as_deref() == Some("other.reader@example.net")),
        "company 2, which did not turn the class on, keeps its old check",
    );
    let leaked: i64 = client
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE auditable_type=$1 AND auditable_id=ANY($2) \
             AND (coalesce(old_values::text,'')||coalesce(new_values::text,'')) ~ '(old\\.reader|ada\\.private)@example\\.net|retention-old-'",
            &[&bilibi_consent::LOG_RECORD, &vec![old_address.to_string(), old_both.to_string()]],
        )
        .await?
        .get(0);
    checks.ensure(
        leaked == 0,
        &format!("no audit evidence of the anonymised checks still holds their address or key ({leaked} rows)"),
    );

    println!("== the address searches read indexes, not whole tables");
    client
        .batch_execute(
            "INSERT INTO core_consent_log(tenant_id,company_id,address,channel,purpose,key,at,outcome,caller) \
             SELECT 1,1,CASE WHEN n%2=0 THEN 'reader'||n||'@example.net' ELSE '+601'||lpad(n::text,8,'0') END,'email','marketing','bulk-'||n,now(),'allowed','walkthrough/bulk' \
             FROM generate_series(1,60000) n; \
             INSERT INTO core_consent_suppressions(tenant_id,company_id,address,channel,reason) \
             SELECT 1,1,substr(md5(n::text),1,10)||'@mail'||(n%97)||'.example','email','hard_bounce' FROM generate_series(1,60000) n; \
             INSERT INTO core_consent_suppressions(tenant_id,company_id,address,channel,reason) VALUES(1,1,'bounce1234@example.net','email','hard_bounce'); \
             ANALYZE core_consent_log; ANALYZE core_consent_suppressions;",
        )
        .await?;
    // `privacy.rs`'s address-only match, as the locate binds it.
    let emails = vec!["reader12@example.net".to_owned()];
    let phones = vec!["%60100012345%".to_owned()];
    let locate = plan(
        &client,
        "SELECT count(*) FROM core_consent_log WHERE tenant_id=$1 AND person_id IS NULL AND address IS NOT NULL AND (address=ANY($2) OR regexp_replace(address,'[^0-9]','','g') ILIKE ANY($3))",
        &[&1i64, &emails, &phones],
    )
    .await?;
    checks.ensure(
        locate.contains("core_consent_log_identifier_idx")
            && locate.contains("core_consent_log_digits_idx")
            && !locate.contains("Seq Scan"),
        &format!("the privacy locate's email and phone match reads indexes:\n{locate}"),
    );
    let page = consent
        .suppressions(
            &admin,
            1,
            &SuppressionFilter {
                search: "bounce1234@".into(),
                state: State::All,
                ..SuppressionFilter::default()
            },
        )
        .await?;
    checks.ensure(
        page.total == 1
            && page.entries[0].contact.address.as_deref() == Some("bounce1234@example.net"),
        &format!(
            "the Suppressions search finds one address by part of it ({} found)",
            page.total
        ),
    );
    let search = plan(
        &client,
        "SELECT count(*) FROM core_consent_suppressions WHERE tenant_id=$1 AND company_id=$2 AND (lower(address) LIKE $3 OR person_id=$4)",
        &[&1i64, &1i64, &"%bounce1234@%", &None::<i64>],
    )
    .await?;
    checks.ensure(
        search.contains("core_consent_suppressions_search_idx") && !search.contains("Seq Scan"),
        &format!("the Suppressions search reads the trigram index:\n{search}"),
    );
    {
        let mut tx = boundary
            .begin(&mut client, admin.mutation_context("walkthrough.consent"))
            .await?;
        consent
            .suppress(
                &mut tx,
                &admin,
                &SuppressionInput {
                    company_id: 1,
                    contact: Contact {
                        person: Some(PersonId::new(2)?),
                        address: None,
                    },
                    channel: Channel::Sms,
                    reason: Reason::Complaint,
                    evidence: None,
                },
            )
            .await
            .map_err(|e| format!("suppress: {e:?}"))?;
        tx.commit().await?;
    }
    let by_person = consent
        .suppressions(
            &admin,
            1,
            &SuppressionFilter {
                search: "2".into(),
                ..SuppressionFilter::default()
            },
        )
        .await?;
    checks.ensure(
        by_person
            .entries
            .iter()
            .any(|s| s.contact.person.map(|p| p.get()) == Some(2)),
        "searching a person's id still finds that person's suppression",
    );

    if checks.0 > 0 {
        return Err(format!("{} check(s) failed", checks.0).into());
    }
    println!("search and retention proof: every check held");
    Ok(())
}
