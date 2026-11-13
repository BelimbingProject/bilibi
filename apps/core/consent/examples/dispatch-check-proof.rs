//! Running-database proof of the dispatch check's cost and order (#587,
//! #604): a check holds its company shared, so concurrent checks and other
//! writes into the company go on while archiving still waits for it; a later
//! any-channel withdrawal ends an earlier channel grant; `check_many` gives
//! the answers `check` gives, logs one row per recipient, and refuses a batch
//! with an invalid recipient as a whole. Explicit disposable DB only; fails
//! the executable when a check does not hold.
use bilibi_authz::grants::Grants;
use bilibi_consent::{
    Answer, Batch, Capture, Channel, Consent, Contact, DecisionInput, Question, Reason, Recipient,
    Refusal, Source, SuppressionInput, WriteError, BATCH_MAX, SERVICE as CONSENT,
};
use bilibi_database::{pool, MutationBoundary};
use bilibi_party_ids::PersonId;
use bilibi_tenancy::Scope;
use std::time::Instant;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        println!("  ok: {message}");
        Ok(())
    } else {
        Err(format!("FAIL {message}").into())
    }
}
fn person(id: i64) -> Contact {
    Contact {
        person: Some(PersonId::new(id).expect("person")),
        address: None,
    }
}
fn address(value: &str) -> Contact {
    Contact {
        person: None,
        address: Some(value.into()),
    }
}
fn question(company: i64, contact: Contact, channel: Channel) -> Question {
    Question {
        company_id: company,
        contact,
        channel,
        purpose: "marketing".into(),
    }
}
/// Whether a row lock of `mode` on company `id` is granted within a second
/// on a connection of its own, then gives it back.
async fn lockable(client: &mut Client, id: i64, mode: &str) -> Result<bool> {
    let tx = client.transaction().await?;
    tx.batch_execute("SET LOCAL lock_timeout='1s'").await?;
    let taken = tx
        .query_opt(
            &format!("SELECT id FROM companies WHERE tenant_id=1 AND id=$1 {mode}"),
            &[&id],
        )
        .await;
    tx.rollback().await?;
    match taken {
        Ok(_) => Ok(true),
        Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE) => {
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

struct Proof<'a> {
    consent: &'a Consent,
    boundary: &'a MutationBoundary,
    scope: &'a Scope,
}
impl Proof<'_> {
    async fn decide(
        &self,
        client: &mut Client,
        who: Contact,
        channel: Channel,
        granted: bool,
    ) -> Result<()> {
        let mut tx = self
            .boundary
            .begin(client, self.scope.mutation_context("walkthrough.consent"))
            .await?;
        let input = DecisionInput {
            company_id: 1,
            contact: who,
            purpose: "marketing".into(),
            channel,
            granted,
            source: Source::Portal,
            captured_on: Capture::Web,
            evidence: None,
        };
        self.consent
            .record(&mut tx, self.scope, &input)
            .await
            .map_err(|e| format!("record: {e:?}"))?;
        tx.commit().await?;
        Ok(())
    }
    async fn check(&self, client: &mut Client, q: &Question) -> Result<Answer> {
        let mut tx = self
            .boundary
            .begin(client, self.scope.mutation_context("walkthrough.consent"))
            .await?;
        let answer = self
            .consent
            .check(&mut tx, self.scope, q, "proof-1", "walkthrough/campaigns")
            .await
            .map_err(|e| format!("check: {e:?}"))?;
        tx.commit().await?;
        Ok(answer)
    }
}

async fn log_rows(client: &Client) -> Result<i64> {
    Ok(client
        .query_one("SELECT count(*) FROM core_consent_log", &[])
        .await?
        .get(0))
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
    // Consent writes need a desk in their company (ADR 19).
    db.get().await?.batch_execute("INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,4,2) ON CONFLICT DO NOTHING; \
        SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35574")?;
    app.services
        .get::<Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin = app.security.guard(&login.token).await?;
    let consent = app.services.get::<Consent>(CONSENT)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let proof = Proof {
        consent: &consent,
        boundary: &boundary,
        scope: &admin,
    };
    let mut one = db.get().await?;
    let mut two = db.get().await?;
    let mut three = db.get().await?;

    println!("== #587: a held check does not hold the company exclusively");
    let mut held = boundary
        .begin(&mut one, admin.mutation_context("walkthrough.consent"))
        .await?;
    consent
        .check(
            &mut held,
            &admin,
            &question(1, person(1), Channel::Email),
            "held-1",
            "walkthrough/campaigns",
        )
        .await
        .map_err(|e| format!("held check: {e:?}"))?;
    ensure(
        lockable(&mut two, 1, "FOR NO KEY UPDATE").await?,
        "another write into company 1 (FOR NO KEY UPDATE) is not kept waiting by the held check",
    )?;
    let second = {
        let mut tx = boundary
            .begin(&mut two, admin.mutation_context("walkthrough.consent"))
            .await?;
        tx.sql()
            .batch_execute("SET LOCAL lock_timeout='1s'")
            .await?;
        let answer = consent
            .check(
                &mut tx,
                &admin,
                &question(1, person(2), Channel::Email),
                "held-2",
                "walkthrough/campaigns",
            )
            .await;
        if answer.is_ok() {
            tx.commit().await?;
        }
        answer
    };
    ensure(
        second.is_ok(),
        &format!(
            "a second sender's check in company 1 is answered while the first is held ({second:?})"
        ),
    )?;
    ensure(
        !lockable(&mut three, 1, "FOR UPDATE").await?,
        "archiving company 1 (FOR UPDATE) still waits for the held check",
    )?;
    held.commit().await?;
    ensure(
        lockable(&mut three, 1, "FOR UPDATE").await?,
        "after the sender commits, archiving takes the company",
    )?;
    three
        .execute(
            "UPDATE companies SET status='archived' WHERE tenant_id=1 AND id=2",
            &[],
        )
        .await?;
    let mut tx = boundary
        .begin(&mut one, admin.mutation_context("walkthrough.consent"))
        .await?;
    let archived = consent
        .check(
            &mut tx,
            &admin,
            &question(2, person(1), Channel::Email),
            "archived-1",
            "walkthrough/campaigns",
        )
        .await;
    drop(tx);
    ensure(
        matches!(archived, Err(WriteError::CompanyArchived)),
        &format!("a check in an archived company is refused as archived ({archived:?})"),
    )?;

    println!("== #604: a later any-channel withdrawal ends an earlier channel grant");
    proof
        .decide(&mut one, person(3), Channel::Email, true)
        .await?;
    let before = proof
        .check(&mut one, &question(1, person(3), Channel::Email))
        .await?;
    ensure(before == Answer::Allowed, "the email grant allows email")?;
    proof
        .decide(&mut one, person(3), Channel::Any, false)
        .await?;
    let after = proof
        .check(&mut one, &question(1, person(3), Channel::Email))
        .await?;
    ensure(
        after == Answer::Refused(Refusal::Withdrawn),
        &format!("after \"stop everything\" on any channel, email is withdrawn ({after:?})"),
    )?;
    proof
        .decide(&mut one, person(3), Channel::Email, true)
        .await?;
    let regranted = proof
        .check(&mut one, &question(1, person(3), Channel::Email))
        .await?;
    ensure(
        regranted == Answer::Allowed,
        "a newer email grant after the blanket withdrawal allows email again",
    )?;
    proof
        .decide(&mut one, person(3), Channel::Sms, false)
        .await?;
    proof
        .decide(&mut one, person(3), Channel::Any, true)
        .await?;
    let sms = proof
        .check(&mut one, &question(1, person(3), Channel::Sms))
        .await?;
    ensure(
        sms == Answer::Refused(Refusal::Withdrawn),
        "a later any-channel grant does not undo an earlier sms withdrawal",
    )?;

    println!("== #587: check_many answers as check does, in one pass");
    proof
        .decide(&mut one, person(1), Channel::Email, true)
        .await?;
    {
        let mut tx = boundary
            .begin(&mut one, admin.mutation_context("walkthrough.consent"))
            .await?;
        consent
            .suppress(
                &mut tx,
                &admin,
                &SuppressionInput {
                    company_id: 1,
                    contact: address("bounced@example.net"),
                    channel: Channel::Email,
                    reason: Reason::HardBounce,
                    evidence: None,
                },
            )
            .await
            .map_err(|e| format!("suppress: {e:?}"))?;
        tx.commit().await?;
    }
    let contacts = [
        person(1),
        person(3),
        address("bounced@example.net"),
        address(" New.Reader@Example.NET "),
        Contact {
            person: Some(PersonId::new(2)?),
            address: Some("bounced@example.net".into()),
        },
        person(1),
    ];
    let expected = [
        Answer::Allowed,
        Answer::Allowed,
        Answer::Refused(Refusal::Suppressed),
        Answer::Refused(Refusal::NoConsent),
        Answer::Refused(Refusal::Suppressed),
        Answer::Allowed,
    ];
    let mut singles = Vec::new();
    for contact in &contacts {
        singles.push(
            consent
                .preview(&admin, &question(1, contact.clone(), Channel::Email))
                .await
                .map_err(|e| format!("preview: {e:?}"))?,
        );
    }
    let batch = Batch {
        company_id: 1,
        channel: Channel::Email,
        purpose: "marketing".into(),
        recipients: contacts
            .iter()
            .enumerate()
            .map(|(n, contact)| Recipient {
                contact: contact.clone(),
                key: format!("batch-{n}"),
            })
            .collect(),
    };
    let rows = log_rows(&one).await?;
    let mut tx = boundary
        .begin(&mut one, admin.mutation_context("walkthrough.consent"))
        .await?;
    let answers = consent
        .check_many(&mut tx, &admin, &batch, "walkthrough/campaigns")
        .await
        .map_err(|e| format!("check_many: {e:?}"))?;
    tx.commit().await?;
    ensure(
        answers == expected && answers == singles,
        &format!("each recipient gets the answer check gives it, in order ({answers:?})"),
    )?;
    let logged: Vec<(String, String, Option<String>)> = one
        .query(
            "SELECT key,outcome,address FROM core_consent_log WHERE key LIKE 'batch-%' ORDER BY id",
            &[],
        )
        .await?
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    ensure(
        log_rows(&one).await? == rows + 6
            && logged.len() == 6
            && logged
                .iter()
                .zip(&answers)
                .enumerate()
                .all(|(n, ((key, outcome, _), answer))| {
                    *key == format!("batch-{n}") && outcome == answer.code()
                })
            && logged[3].2.as_deref() == Some("new.reader@example.net"),
        "one log row per recipient under its own key and outcome, the address normalised",
    )?;
    let rows = log_rows(&one).await?;
    let mut bad = batch.clone();
    bad.recipients[1].key = " ".into();
    let mut tx = boundary
        .begin(&mut one, admin.mutation_context("walkthrough.consent"))
        .await?;
    let refused = consent
        .check_many(&mut tx, &admin, &bad, "walkthrough/campaigns")
        .await;
    drop(tx);
    ensure(
        matches!(&refused, Err(WriteError::Invalid(errors)) if errors.iter().any(|e| e.field == "key" && e.message.starts_with("recipient 2:")))
            && log_rows(&one).await? == rows,
        &format!(
            "a batch with an invalid recipient is refused whole, naming recipient 2 ({refused:?})"
        ),
    )?;
    let mut over = batch.clone();
    over.recipients = (0..=BATCH_MAX)
        .map(|n| Recipient {
            contact: person(1),
            key: format!("over-{n}"),
        })
        .collect();
    let mut tx = boundary
        .begin(&mut one, admin.mutation_context("walkthrough.consent"))
        .await?;
    let over = consent
        .check_many(&mut tx, &admin, &over, "walkthrough/campaigns")
        .await;
    drop(tx);
    ensure(
        matches!(&over, Err(WriteError::Invalid(errors)) if errors.iter().any(|e| e.field == "recipients")),
        "a batch over BATCH_MAX recipients is refused",
    )?;

    println!("== cost of {BATCH_MAX} recipients");
    let recipients: Vec<Recipient> = (0..BATCH_MAX)
        .map(|n| Recipient {
            contact: address(&format!("reader{n}@example.net")),
            key: format!("cost-{n}"),
        })
        .collect();
    let started = Instant::now();
    let mut tx = boundary
        .begin(&mut one, admin.mutation_context("walkthrough.consent"))
        .await?;
    for recipient in &recipients {
        consent
            .check(
                &mut tx,
                &admin,
                &question(1, recipient.contact.clone(), Channel::Email),
                &recipient.key,
                "walkthrough/campaigns",
            )
            .await
            .map_err(|e| format!("check: {e:?}"))?;
    }
    tx.commit().await?;
    let singly = started.elapsed();
    let started = Instant::now();
    let mut tx = boundary
        .begin(&mut one, admin.mutation_context("walkthrough.consent"))
        .await?;
    let answers = consent
        .check_many(
            &mut tx,
            &admin,
            &Batch {
                recipients,
                ..batch.clone()
            },
            "walkthrough/campaigns",
        )
        .await
        .map_err(|e| format!("check_many: {e:?}"))?;
    tx.commit().await?;
    let together = started.elapsed();
    println!("  {BATCH_MAX} checks one by one: {singly:?}; one check_many: {together:?}");
    ensure(
        answers.len() == BATCH_MAX
            && answers
                .iter()
                .all(|a| *a == Answer::Refused(Refusal::NoConsent)),
        "check_many answered every recipient",
    )?;
    println!("dispatch-check proof: every check held");
    Ok(())
}
