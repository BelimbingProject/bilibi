//! Native reference walkthrough: actual owner writes and caller-client reads.
//! An empty disposable DB is required. No unit-test runner or lead workflow.
use bilibi_affiliation::{AffiliationInput, AffiliationService, CalendarDate, EffectiveDates};
use bilibi_authz::{
    grants::Grants,
    restrictions::{FieldRestrictions, Mode},
};
use bilibi_database::{pool, Error, MutationBoundary};
use bilibi_person::{ContactAddress, PersonInput, PersonKind, PersonService};
use bilibi_tenancy::{Authentication, Scope};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn personal() -> PersonInput {
    PersonInput {
        name: "Contact Lookup Reference".into(),
        kind: PersonKind::Human,
        email: Some("Personal.Lookup@Example.TEST".into()),
        phone: Some("+60 (12)-345.6790".into()),
    }
}
fn email(value: &str) -> ContactAddress {
    ContactAddress::Email(value.into())
}
fn phone(value: &str) -> ContactAddress {
    ContactAddress::Phone(value.into())
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_CONTACT_MATCHING").as_deref() != Ok("disposable") {
        return Err("set BILIBI_CONTACT_MATCHING=disposable for an empty disposable DB".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_module_registry::discover()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    if std::env::args().nth(1).as_deref() == Some("seed") {
        return Ok(bilibi_web::seed::production(&db).await?);
    }
    let origin = std::env::var("APP_ORIGIN")?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    app.services
        .get::<bilibi_base::step_up::StepUp>(bilibi_base::step_up::SERVICE)?
        .set_enforced(false);
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let auth = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let people = app.services.get::<PersonService>(bilibi_person::SERVICE)?;
    let work = app
        .services
        .get::<AffiliationService>(bilibi_affiliation::SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let fields = app
        .services
        .get::<FieldRestrictions>("base/authz.field_restrictions")?;
    let mut connection = db.get().await?;
    let manager = auth
        .authenticate(&**connection, 1, 1, String::new())
        .await?;
    let admin = auth
        .authenticate(&**connection, 1, 4, String::new())
        .await?;
    let other = auth
        .authenticate(&**connection, 2, 5, String::new())
        .await?;
    let viewer = auth
        .authenticate(&**connection, 1, 2, String::new())
        .await?;
    let person = people.create(&manager, &personal()).await?;
    let today = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date();
    let date = |day: jiff::civil::Date| -> CalendarDate { day.to_string().try_into().unwrap() };
    let input = AffiliationInput {
        person_id: person.id,
        organisation_id: bilibi_affiliation::OrganisationId::new(1)?,
        title: Some("Reference position".into()),
        work_email: Some("Work.Lookup@Example.TEST".into()),
        work_phone: Some("00 60 (12)-345.6791".into()),
        dates: EffectiveDates::new(Some(date(today)), Some(date(today)))?,
    };
    let affiliation = work.create(&manager, &input).await?;
    let sql: &tokio_postgres::Client = &connection;
    assert_eq!(
        people.read(&manager, person.id).await?.phone.as_deref(),
        Some("+60 (12)-345.6790")
    );
    assert_eq!(
        people.read(&manager, person.id).await?.email.as_deref(),
        Some("Personal.Lookup@Example.TEST")
    );
    assert_eq!(
        work.read(&manager, affiliation.id)
            .await?
            .affiliation
            .work_phone
            .as_deref(),
        Some("00 60 (12)-345.6791")
    );
    assert_eq!(
        work.read(&manager, affiliation.id)
            .await?
            .affiliation
            .work_email
            .as_deref(),
        Some("Work.Lookup@Example.TEST")
    );
    for spelling in [
        "+60123456790",
        "0060123456790",
        "+60 (12)-345.6790",
        "00 60 (12)-345.6790",
    ] {
        let found = people
            .match_contact_in(sql, &manager, &phone(spelling), 100)
            .await?;
        assert_eq!(found.entries, [person.id]);
        assert!(found.complete());
    }
    for spelling in ["+60123456791", "0060123456791", "+60 (12)-345.6791"] {
        let found = work
            .match_contact_in(sql, &manager, &phone(spelling), 100)
            .await?;
        assert!(found.complete());
        assert_eq!(found.entries.len(), 1);
        assert_eq!(found.entries[0].person, person.id);
        assert_eq!(found.entries[0].affiliation, affiliation.id);
    }
    let found = people
        .match_contact_in(sql, &manager, &email(" personal.lookup@example.test "), 1)
        .await?;
    assert_eq!(found.entries, [person.id]);
    assert!(found.complete());
    let found = work
        .match_contact_in(sql, &manager, &email(" WORK.LOOKUP@EXAMPLE.TEST "), 1)
        .await?;
    assert_eq!(found.entries[0].person, person.id);
    assert!(found.complete());
    for criterion in [phone("+60129999999"), email("nonmatch@example.test")] {
        let personal = people
            .match_contact_in(sql, &manager, &criterion, 100)
            .await?;
        let current = work
            .match_contact_in(sql, &manager, &criterion, 100)
            .await?;
        assert!(personal.entries.is_empty() && personal.complete());
        assert!(current.entries.is_empty() && current.complete());
    }
    for criterion in [
        phone("+60123456790"),
        phone("+60123456791"),
        email("personal.lookup@example.test"),
        email("work.lookup@example.test"),
    ] {
        assert!(people
            .match_contact_in(sql, &other, &criterion, 100)
            .await?
            .entries
            .is_empty());
        assert!(work
            .match_contact_in(sql, &other, &criterion, 100)
            .await?
            .entries
            .is_empty());
    }
    println!("PASS preserved originals, exact personal/work IDs, complete negatives and tenant separation");

    // The supplied Scope is deliberately reused after permission changes.
    for capability in [
        bilibi_person::LIST,
        bilibi_person::VIEW,
        "admin.affiliation.list",
        "admin.affiliation.view",
    ] {
        let visible = work
            .match_contact_in(sql, &viewer, &email("work.lookup@example.test"), 100)
            .await?;
        assert!(visible.complete());
        assert_eq!(visible.entries[0].affiliation, affiliation.id);
        let visible = people
            .match_contact_in(sql, &viewer, &email("personal.lookup@example.test"), 100)
            .await?;
        assert!(visible.complete());
        assert_eq!(visible.entries, [person.id]);
        grants
            .set_direct(&admin, viewer.user(), capability, Some(false))
            .await?;
        for criterion in [
            email("work.lookup@example.test"),
            email("nonmatch@example.test"),
        ] {
            assert!(matches!(
                work.match_contact_in(sql, &viewer, &criterion, 100).await,
                Err(Error::Forbidden)
            ));
        }
        if capability.starts_with("admin.person.") {
            assert!(matches!(
                people
                    .match_contact_in(sql, &viewer, &email("nonmatch@example.test"), 100)
                    .await,
                Err(Error::Forbidden)
            ));
        } else {
            let personal = people
                .match_contact_in(sql, &viewer, &email("personal.lookup@example.test"), 100)
                .await?;
            assert_eq!(personal.entries, [person.id]);
            assert!(personal.complete());
        }
        grants
            .set_direct(&admin, viewer.user(), capability, None)
            .await?;
    }
    println!("PASS live source/list/detail denials with independent personal access");
    // Both restriction modes refuse both present and absent criteria.
    let viewer_role: i64 = sql
        .query_one(
            "SELECT id FROM roles WHERE tenant_id=$1 AND code='viewer'",
            &[&viewer.tenant()],
        )
        .await?
        .get(0);
    for mode in [Mode::Withhold, Mode::Mask] {
        for (table, field, criterion) in [
            ("persons", "email", email("personal.lookup@example.test")),
            ("persons", "phone", phone("+60123456790")),
            (
                "person_affiliations",
                "work_email",
                email("work.lookup@example.test"),
            ),
            ("person_affiliations", "work_phone", phone("+60123456791")),
        ] {
            if table == "persons" {
                let visible = people
                    .match_contact_in(sql, &viewer, &criterion, 100)
                    .await?;
                assert_eq!(visible.entries, [person.id]);
                assert!(visible.complete());
            } else {
                let visible = work.match_contact_in(sql, &viewer, &criterion, 100).await?;
                assert_eq!(visible.entries[0].affiliation, affiliation.id);
                assert!(visible.complete());
            }
            fields
                .put_mode(&admin, table, field, &[viewer_role], mode)
                .await?;
            if table == "persons" {
                assert!(matches!(
                    people.match_contact_in(sql, &viewer, &criterion, 100).await,
                    Err(Error::Forbidden)
                ));
                let absent = if field == "email" {
                    email("nonmatch@example.test")
                } else {
                    phone("+60129999999")
                };
                assert!(matches!(
                    people.match_contact_in(sql, &viewer, &absent, 100).await,
                    Err(Error::Forbidden)
                ));
            } else {
                assert!(matches!(
                    work.match_contact_in(sql, &viewer, &criterion, 100).await,
                    Err(Error::Forbidden)
                ));
                let absent = if field == "work_email" {
                    email("nonmatch@example.test")
                } else {
                    phone("+60129999999")
                };
                assert!(matches!(
                    work.match_contact_in(sql, &viewer, &absent, 100).await,
                    Err(Error::Forbidden)
                ));
            }
            fields.remove(&admin, table, field).await?;
            if table == "persons" {
                let visible = people
                    .match_contact_in(sql, &viewer, &criterion, 100)
                    .await?;
                assert_eq!(visible.entries, [person.id]);
                assert!(visible.complete());
            } else {
                let visible = work.match_contact_in(sql, &viewer, &criterion, 100).await?;
                assert_eq!(visible.entries[0].affiliation, affiliation.id);
                assert!(visible.complete());
            }
        }
    }
    println!("PASS live source/list/detail denial and hidden/masked refusal for present and absent criteria");

    let mut person_changes = people.subscribe();
    let mut work_changes = work.subscribe();
    let mut tx = boundary
        .begin(
            &mut connection,
            manager.mutation_context("contact.lookup.reference"),
        )
        .await?;
    let mut pending = personal();
    pending.email = Some("pending@example.test".into());
    pending.phone = Some("+60 12-345 6792".into());
    let pending_person = people.create_in(&mut tx, &manager, &pending).await?;
    let mut pending_work = input.clone();
    pending_work.person_id = pending_person.id;
    pending_work.work_email = Some("pending-work@example.test".into());
    pending_work.work_phone = Some("+60 12-345 6793".into());
    let pending_affiliation = work.create_in(&mut tx, &manager, &pending_work).await?;
    for criterion in [email("pending@example.test"), phone("+60123456792")] {
        let found = people
            .match_contact_in(tx.sql(), &manager, &criterion, 100)
            .await?;
        assert_eq!(found.entries, [pending_person.id]);
        assert!(found.complete());
    }
    for criterion in [email("pending-work@example.test"), phone("+60123456793")] {
        let found = work
            .match_contact_in(tx.sql(), &manager, &criterion, 100)
            .await?;
        assert_eq!(found.entries[0].affiliation, pending_affiliation.id);
        assert!(found.complete());
        assert!(work
            .match_contact_in(tx.sql(), &other, &criterion, 100)
            .await?
            .entries
            .is_empty());
    }
    tx.rollback().await?;
    assert!(person_changes.try_recv().is_err() && work_changes.try_recv().is_err());
    assert!(matches!(
        people.read(&manager, pending_person.id).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        work.read(&manager, pending_affiliation.id).await,
        Err(Error::NotFound)
    ));
    assert!(people
        .match_contact_in(&**connection, &manager, &email("pending@example.test"), 100)
        .await?
        .entries
        .is_empty());
    assert!(work
        .match_contact_in(&**connection, &manager, &phone("+60123456793"), 100)
        .await?
        .entries
        .is_empty());
    println!("PASS same-client uncommitted owner writes, tenant separation, rollback and no notifications");

    // More than the former per-person search page; lookup filters before its
    // result bound. Different positions legitimately share a work contact.
    let mut tx = boundary
        .begin(
            &mut connection,
            manager.mutation_context("contact.lookup.overflow"),
        )
        .await?;
    for _ in 0..101 {
        work.create_in(&mut tx, &manager, &input).await?;
    }
    let mut last = input.clone();
    last.work_email = Some("last-work.lookup@example.test".into());
    let last = work.create_in(&mut tx, &manager, &last).await?;
    let found = work
        .match_contact_in(
            tx.sql(),
            &manager,
            &email("last-work.lookup@example.test"),
            1,
        )
        .await?;
    assert!(found.complete());
    assert_eq!(found.entries.len(), 1);
    assert_eq!(found.entries[0].affiliation, last.id);
    let found = work
        .match_contact_in(tx.sql(), &manager, &email("nonmatch@example.test"), 1)
        .await?;
    assert!(found.entries.is_empty() && found.complete());
    let found = work
        .match_contact_in(tx.sql(), &manager, &email("work.lookup@example.test"), 100)
        .await?;
    assert_eq!(found.entries.len(), 100);
    assert!(found.overflow && !found.complete());
    let mut another = personal();
    another.name = "Second exact holder".into();
    people.create_in(&mut tx, &manager, &another).await?;
    let found = people
        .match_contact_in(tx.sql(), &manager, &phone("+60123456790"), 1)
        .await?;
    assert!(found.entries.len() == 1 && found.overflow && !found.complete());
    let mut national = personal();
    national.phone = Some("012-345 6790".into());
    people.create_in(&mut tx, &manager, &national).await?;
    let mut national_work = input.clone();
    national_work.work_phone = Some("012-345 6791".into());
    work.create_in(&mut tx, &manager, &national_work).await?;
    let found = people
        .match_contact_in(tx.sql(), &manager, &phone("+60129999999"), 100)
        .await?;
    assert!(found.entries.is_empty() && found.uncomparable && !found.complete());
    let found = work
        .match_contact_in(tx.sql(), &manager, &phone("+60129999999"), 100)
        .await?;
    assert!(found.entries.is_empty() && found.uncomparable && !found.complete());
    for bare in [
        "0123456790",
        "60 12 345 6790",
        "(212) 555-1234",
        "2125551234",
    ] {
        assert!(matches!(
            people
                .match_contact_in(tx.sql(), &manager, &phone(bare), 100)
                .await,
            Err(Error::Invalid)
        ));
        assert!(matches!(
            work.match_contact_in(tx.sql(), &manager, &phone(bare), 100)
                .await,
            Err(Error::Invalid)
        ));
    }
    tx.rollback().await?;
    let mut tx = boundary
        .begin(
            &mut connection,
            manager.mutation_context("contact.lookup.no_trunk_zero"),
        )
        .await?;
    let mut bare = personal();
    bare.phone = Some("(212) 555-1234".into());
    let bare_person = people.create_in(&mut tx, &manager, &bare).await?;
    let mut bare_work = input.clone();
    bare_work.work_phone = Some("(212) 555-1234".into());
    let bare_work = work.create_in(&mut tx, &manager, &bare_work).await?;
    assert_eq!(bare_person.phone.as_deref(), Some("(212) 555-1234"));
    assert_eq!(
        bare_work.affiliation.work_phone.as_deref(),
        Some("(212) 555-1234")
    );
    for (criterion, country) in [("+12125551234", "US"), ("+2125551234", "Morocco")] {
        let personal = people
            .match_contact_in(tx.sql(), &manager, &phone(criterion), 100)
            .await?;
        assert!(
            personal.entries.is_empty() && personal.uncomparable && !personal.complete(),
            "{country}"
        );
        let current = work
            .match_contact_in(tx.sql(), &manager, &phone(criterion), 100)
            .await?;
        assert!(
            current.entries.is_empty() && current.uncomparable && !current.complete(),
            "{country}"
        );
    }
    tx.rollback().await?;
    println!("PASS bounded overflow, incompleteness for bare stored phones, bare-input refusal");

    for dates in [
        EffectiveDates::new(Some(date(today.tomorrow()?)), None)?,
        EffectiveDates::new(None, Some(date(today.yesterday()?)))?,
    ] {
        let mut dated = input.clone();
        dated.dates = dates;
        dated.work_email = Some("outside-date@example.test".into());
        dated.work_phone = Some("0123".into());
        work.create(&manager, &dated).await?;
    }
    let found = work
        .match_contact_in(
            &**connection,
            &manager,
            &email("outside-date@example.test"),
            100,
        )
        .await?;
    assert!(found.entries.is_empty() && found.complete());
    assert!(work
        .match_contact_in(&**connection, &manager, &phone("+60129999999"), 100)
        .await?
        .complete());
    let unlinked = work
        .unlink(&manager, affiliation.id, affiliation.version)
        .await?;
    assert!(unlinked.unlinked_at.is_some());
    let found = work
        .match_contact_in(&**connection, &manager, &phone("+60123456791"), 100)
        .await?;
    assert!(found.entries.is_empty() && found.complete());
    // Retained retired contacts still match; a lookup issues no live proof.
    let retained = work.create(&manager, &input).await?;
    people.retire(&manager, person.id, person.version).await?;
    assert_eq!(
        people
            .match_contact_in(&**connection, &manager, &phone("+60123456790"), 100)
            .await?
            .entries,
        [person.id]
    );
    assert_eq!(
        work.match_contact_in(&**connection, &manager, &phone("+60123456791"), 100)
            .await?
            .entries[0]
            .affiliation,
        retained.id
    );
    println!("PASS inclusive today, future/ended/unlinked exclusion, retired historical matches");

    erase_reference(&app, &mut connection, &manager, &admin).await?;

    // A held Scope cannot retain authority after its actor is withdrawn.
    let login = app
        .user
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let stale: Scope = app.security.guard(&login.token).await?;
    let visible = people
        .match_contact_in(
            &**connection,
            &stale,
            &email("personal.lookup@example.test"),
            100,
        )
        .await?;
    assert_eq!(visible.entries, [person.id]);
    assert!(visible.complete());
    let visible = work
        .match_contact_in(
            &**connection,
            &stale,
            &email("work.lookup@example.test"),
            100,
        )
        .await?;
    assert_eq!(visible.entries[0].affiliation, retained.id);
    assert!(visible.complete());
    let actor = stale.actor().person().ok_or("Manager has no Person")?;
    let actor_record = people.read(&admin, actor).await?;
    people.retire(&admin, actor, actor_record.version).await?;
    assert!(matches!(
        people
            .match_contact_in(&**connection, &stale, &email("nonmatch@example.test"), 100)
            .await,
        Err(Error::Unauthorized)
    ));
    assert!(matches!(
        work.match_contact_in(&**connection, &stale, &email("nonmatch@example.test"), 100)
            .await,
        Err(Error::Unauthorized)
    ));
    println!("PASS supplied-client Scope reproof refuses a withdrawn actor");
    Ok(())
}

/// Use the native approved erasure flow and actual queue/store handlers. No
/// fabricated grant or direct contact clearing stands in for owner behavior.
async fn erase_reference(
    app: &bilibi_web::App,
    client: &mut tokio_postgres::Client,
    receiver: &Scope,
    admin: &Scope,
) -> Result<()> {
    use bilibi_base::{personal_data::Identifier, privacy::*, workflow::*};
    let people = app.services.get::<PersonService>(bilibi_person::SERVICE)?;
    let work = app
        .services
        .get::<AffiliationService>(bilibi_affiliation::SERVICE)?;
    let privacy = app.services.get::<Privacy>(bilibi_base::privacy::SERVICE)?;
    let engine = app.services.get::<Engine>(bilibi_base::workflow::SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let login = app
        .user
        .login("ai@agent.my", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let initial = app.security.guard(&login.token).await?;
    let company = bilibi_affiliation::OrganisationId::new(1)?;
    let approver_person = initial.actor().person().ok_or("Approver has no Person")?;
    app.user
        .set_desk(receiver, company.get(), approver_person.get(), true)
        .await?;
    let approver = app.security.guard(&login.token).await?;
    assert_eq!(approver.current_company(), Some(company));
    let enrollment = app
        .user
        .begin_two_factor(&approver, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|_| "Approver authenticator enrollment refused")?;
    app.user
        .confirm_two_factor(&approver, &authenticator_code(&enrollment.key, -1))
        .await
        .map_err(|_| "Approver authenticator confirmation refused")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    for capability in [REQUEST_RECEIVE, REQUEST_VIEW] {
        grants
            .set_direct(admin, receiver.user(), capability, Some(true))
            .await?;
    }
    let mut identity = personal();
    identity.email = Some("erase.lookup@example.test".into());
    identity.phone = Some("+60 12-345 6794".into());
    let person = people.create(receiver, &identity).await?;
    work.create(
        receiver,
        &AffiliationInput {
            person_id: person.id,
            organisation_id: company,
            title: None,
            work_email: Some("erase-work.lookup@example.test".into()),
            work_phone: Some("+60 12-345 6795".into()),
            dates: EffectiveDates::new(None, None)?,
        },
    )
    .await?;
    let mut tx = boundary
        .begin(
            client,
            approver.mutation_context("contact.lookup.erasure.rules"),
        )
        .await?;
    engine
        .publish_approval_policy_in(
            &mut tx,
            &approver,
            ApprovalPolicyChange {
                case: ERASURE_CASE,
                company: company.get(),
                expected: None,
                policy: erasure_rules(),
                reason: "Disposable contact lookup reference",
            },
        )
        .await?;
    tx.commit().await?;
    let received = privacy
        .receive(
            client,
            receiver,
            NewRequest {
                kind: RequestKind::Erasure,
                person: person.id,
                company,
                identifiers: vec![Identifier::Email("erase.lookup@example.test".into())],
            },
        )
        .await?;
    let verified = privacy
        .verify(
            client,
            receiver,
            received.id,
            received.version,
            "Reference identity checked",
            &[],
        )
        .await?;
    let sent = privacy
        .submit_erasure(client, receiver, verified.id, verified.version)
        .await?;
    let round = sent.approval_request.ok_or("No erasure approval round")?;
    let detail = engine.approval_detail(&approver, round).await?;
    let stage = &detail.request.stages[detail.request.stage_index];
    let mut action = ActionRequest::new(
        ERASURE_DECIDE,
        format!("contact-lookup:{round}"),
        detail.subject_version,
    );
    action.payload = serde_json::json!({
        "request": round, "version": detail.request.version, "stage": stage.key,
        "seat": stage.seats[0].key, "routing_version": detail.request.routing_version,
        "approve": true, "reason": "Reference identity and scope checked", "input": {},
    });
    engine
        .execute_action(
            &approver
                .clone()
                .presenting_step_up(&authenticator_code(&enrollment.key, 0)),
            bilibi_base::privacy::SUBJECT,
            sent.id,
            action,
        )
        .await?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config {
            concurrency: 1,
            ..Default::default()
        },
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));
    let result = async {
        let mut complete = false;
        for _ in 0..120 {
            let request = privacy.request(client, receiver, sent.id).await?;
            if request.status == "completed" {
                complete = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        if !complete {
            let pending = privacy.request(client, receiver, sent.id).await?;
            return Err(format!("Approved erasure did not complete within 60 seconds (status={}, finish_error={:?})", pending.status, pending.finish_error).into());
        }
        assert!(people.read(receiver, person.id).await?.erased_at.is_some());
        for criterion in [email("erase.lookup@example.test"), phone("+60123456794")] {
            let found = people
                .match_contact_in(client, receiver, &criterion, 100)
                .await?;
            assert!(found.entries.is_empty() && found.complete());
        }
        for criterion in [
            email("erase-work.lookup@example.test"),
            phone("+60123456795"),
        ] {
            let found = work
                .match_contact_in(client, receiver, &criterion, 100)
                .await?;
            assert!(found.entries.is_empty() && found.complete());
        }
        println!("PASS approved real-owner erasure removes personal/work matches and derived keys");
        Ok(())
    }
    .await;
    let _ = stop.send(true);
    workers.await??;
    result
}

/// RFC 6238 authenticator for the disposable account's newly enrolled key,
/// as in the native privacy walkthrough. Neither key nor codes are printed.
fn authenticator_code(key: &str, offset: i64) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut secret = Vec::new();
    let (mut buffer, mut bits) = (0u64, 0u32);
    for ch in key.chars().filter(char::is_ascii_alphanumeric) {
        let value = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .find(ch.to_ascii_uppercase())
            .expect("base32 enrollment") as u64;
        buffer = buffer << 5 | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            secret.push((buffer >> bits) as u8);
        }
    }
    let step = (jiff::Timestamp::now().as_second() / 30 + offset) as u64;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).expect("authenticator key");
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
