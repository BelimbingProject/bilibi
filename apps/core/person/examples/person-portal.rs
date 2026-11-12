//! The customer's own contact details on the portal (#474) through the real
//! doors on a disposable, migrated database: a customer account signs in as
//! a Person, and `PersonService::portal_read` and `portal_update_in` show and
//! change that Person only, with the portal capability, the tenant's
//! editable list, Core's validation, the version check and one audited
//! change by the customer. Each line is `ok` or `FAIL`; the run fails if any
//! check does.
//!
//! ```text
//! BILIBI_PERSON_DEMO=disposable DATABASE_URL=… APP_ORIGIN=… \
//!   cargo run -p bilibi-person --example person-portal
//! ```
use bilibi_custom_fields::{CustomFields, NewField};
use bilibi_database::{pool, Error, MutationBoundary};
use bilibi_person::{
    Custom, PersonInput, PersonKind, PersonService, PortalUpdate, Update, WriteError, PORTAL,
    PORTAL_EDITABLE_KEY, SERVICE,
};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{Authentication, PortalScope};
use serde_json::json;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Failure = Box<dyn std::error::Error + Send + Sync>;

#[derive(Default)]
struct Proof {
    failed: Vec<String>,
}
impl Proof {
    fn check(&mut self, label: &str, ok: bool) {
        println!("{} {label}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            self.failed.push(label.into());
        }
    }
}

fn human(name: &str, email: &str) -> PersonInput {
    PersonInput {
        name: name.into(),
        kind: PersonKind::Human,
        email: Some(email.into()),
        phone: Some("+60 12 345 6789".into()),
    }
}

/// How many audited changes a person has.
async fn changes(client: &tokio_postgres::Client, id: i64) -> Result<i64, Failure> {
    Ok(client
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE auditable_type='person' AND auditable_id=$1",
            &[&id.to_string()],
        )
        .await?
        .get(0))
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_PERSON_DEMO").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_PERSON_DEMO=disposable and use a disposable migrated DATABASE_URL".into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let origin = std::env::var("APP_ORIGIN")?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    let grants = app
        .services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let people = app.services.get::<PersonService>(SERVICE)?;
    let fields = app
        .services
        .get::<CustomFields>(bilibi_custom_fields::SERVICE)?;
    let lists = app
        .services
        .get::<bilibi_picklists::PickLists>(bilibi_picklists::SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let password = bilibi_walkthrough_fixture::PASSWORD;
    let manager = app
        .security
        .guard(
            &app.user
                .login("manager@example.test", password)
                .await?
                .token,
        )
        .await?;
    let admin = app
        .security
        .guard(&app.user.login("admin@example.test", password).await?.token)
        .await?;
    let mut proof = Proof::default();
    let mut client = db.get().await?;

    println!("-- setup");
    // Fixture setup: the fixture tenant gets its pick-lists as a new tenant
    // would; then two contact fields, a text and a choice.
    bilibi_picklists::provision_deployment(&**client, manager.tenant()).await?;
    let (list, _) = *fields
        .choice_lists()
        .first()
        .ok_or("no standalone pick-list to choose from")?;
    let values = lists.values(&**client, &admin, list, false).await?;
    let (first, second) = match values.as_slice() {
        [first, second, ..] => (first.code.clone(), second.code.clone()),
        _ => return Err(format!("pick-list {list} needs two values").into()),
    };
    for (key, label, kind, picklist) in [
        ("segment", "Segment", "text", None),
        ("tier", "Tier", "choice", Some(list)),
    ] {
        let mut tx = boundary
            .begin(&mut client, admin.mutation_context("person.custom.define"))
            .await?;
        let defined = fields
            .define(
                &mut tx,
                &admin,
                &NewField {
                    record_type: "person".into(),
                    key: key.into(),
                    label: label.into(),
                    kind: kind.into(),
                    picklist: picklist.map(str::to_owned),
                    ..NewField::default()
                },
            )
            .await?;
        if let Err(errors) = &defined {
            return Err(format!("{key}: {errors:?}").into());
        }
        tx.commit().await?;
    }
    let mine = people
        .create(&manager, &human("Siti Customer", "siti@example.test"))
        .await?;
    let mut tier = Custom::new();
    tier.insert("tier".into(), json!(first));
    let mine = people
        .update(
            &manager,
            mine.id,
            &Update {
                version: mine.version,
                identity: human("Siti Customer", "siti@example.test"),
                custom: tier,
            },
        )
        .await?;
    let other = people
        .create(&manager, &human("Other Customer", "other@example.test"))
        .await?;
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("walkthrough.portal.customer"),
        )
        .await?;
    let customer = app
        .user
        .create_customer_in(
            &mut tx,
            &bilibi_user::NewCustomer {
                name: "Siti Customer",
                email: "siti-portal@example.test",
                password,
                person: mine.id,
                affiliation: None,
            },
        )
        .await?;
    tx.commit().await?;
    let portal = PortalScope::seal(
        authentication
            .authenticate(&**client, manager.tenant(), customer, String::new())
            .await?,
    )?;

    println!("-- capability");
    proof.check(
        "a customer without the portal role cannot read its own contact details",
        matches!(people.portal_read(&portal).await, Err(Error::Forbidden)),
    );
    // Core Portal gives this role when an invitation is redeemed.
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("walkthrough.portal.role"),
        )
        .await?;
    grants
        .assign_portal_role_in(&mut tx, &portal, "portal_customer")
        .await?;
    tx.commit().await?;
    let staff_denied = bilibi_authz::authorize_key(&**client, &manager, PORTAL)
        .await
        .is_err()
        && bilibi_authz::authorize_key(&**client, &admin, PORTAL)
            .await
            .is_err();
    proof.check(
        "no staff role holds core.person.portal.manage, not even grant-all",
        staff_denied,
    );
    let staff_facade = people.read(portal.scope(), mine.id).await.is_err()
        && people.read(portal.scope(), other.id).await.is_err()
        && bilibi_authz::authorize_key(&**client, portal.scope(), bilibi_person::VIEW)
            .await
            .is_err();
    proof.check(
        "the customer holds no staff capability: the staff read refuses its own and another person",
        staff_facade,
    );

    println!("-- read");
    let read = people.portal_read(&portal).await?;
    let tier_field = read.custom.iter().find(|field| field.key == "tier");
    proof.check(
        "the portal read is the proof's Person: name, email, phone and the active custom fields",
        read.id == mine.id
            && read.name == "Siti Customer"
            && read.email.as_deref() == Some("siti@example.test")
            && read.phone.as_deref() == Some("+60 12 345 6789")
            && read.version == mine.version
            && read.custom.len() == 2
            && tier_field.is_some_and(|field| {
                field.kind == "choice"
                    && field.value.as_deref() == Some(first.as_str())
                    && field.text.is_some()
            }),
    );
    proof.check(
        "by default the tenant lets a customer change nothing",
        read.editable.is_empty() && read.custom.iter().all(|field| !field.editable),
    );
    let json = serde_json::to_value(&read)?;
    proof.check(
        "custom fields read as {key, label, type, value}",
        json["custom"][0]["type"] == "text" && json["custom"][0]["label"] == "Segment",
    );

    println!("-- nothing editable");
    let before = changes(&client, mine.id.get()).await?;
    let mut tx = boundary
        .begin(
            &mut client,
            portal.mutation_context("walkthrough.portal.profile"),
        )
        .await?;
    let refused = people
        .portal_update_in(
            &mut tx,
            &portal,
            &PortalUpdate {
                version: read.version,
                name: Some("Siti C".into()),
                ..PortalUpdate::default()
            },
        )
        .await;
    tx.rollback().await?;
    proof.check(
        "a name change the tenant did not allow is refused on name",
        matches!(&refused, Err(WriteError::Validation(errors)) if errors.iter().any(|e| e.field == "name")),
    );
    let mut tx = boundary
        .begin(
            &mut client,
            portal.mutation_context("walkthrough.portal.profile"),
        )
        .await?;
    let unchanged = people
        .portal_update_in(
            &mut tx,
            &portal,
            &PortalUpdate {
                version: read.version,
                name: Some(" Siti Customer ".into()),
                phone: read.phone.clone(),
                ..PortalUpdate::default()
            },
        )
        .await?;
    tx.commit().await?;
    proof.check(
        "posting the facts as they stand is accepted and writes nothing",
        unchanged.version == read.version && changes(&client, mine.id.get()).await? == before,
    );
    proof.check(
        "an email key is refused: email is the sign-in address and never editable",
        serde_json::from_value::<PortalUpdate>(
            json!({"version": read.version, "email": "new@example.test"}),
        )
        .is_err(),
    );
    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("walkthrough.portal.setting"),
        )
        .await?;
    let email_setting = settings
        .put(
            &mut tx,
            &admin,
            Target::Tenant,
            PORTAL_EDITABLE_KEY,
            json!(["name", "email"]),
        )
        .await;
    tx.rollback().await?;
    proof.check("the editable list refuses email", email_setting.is_err());

    println!("-- tenant allows name, phone and segment");
    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("walkthrough.portal.setting"),
        )
        .await?;
    settings
        .put(
            &mut tx,
            &admin,
            Target::Tenant,
            PORTAL_EDITABLE_KEY,
            json!(["name", "phone", "cf_segment"]),
        )
        .await?;
    tx.commit().await?;
    let read = people.portal_read(&portal).await?;
    proof.check(
        "the read names what may change: name, phone and segment, not tier",
        read.can_edit("name")
            && read.can_edit("phone")
            && read.can_edit("cf_segment")
            && !read.can_edit("cf_tier")
            && read
                .custom
                .iter()
                .all(|field| field.editable == (field.key == "segment")),
    );
    let mut segment = Custom::new();
    segment.insert("segment".into(), json!("x".repeat(1_001)));
    let mut tx = boundary
        .begin(
            &mut client,
            portal.mutation_context("walkthrough.portal.profile"),
        )
        .await?;
    let too_long = people
        .portal_update_in(
            &mut tx,
            &portal,
            &PortalUpdate {
                version: read.version,
                custom: segment,
                ..PortalUpdate::default()
            },
        )
        .await;
    let blank = people
        .portal_update_in(
            &mut tx,
            &portal,
            &PortalUpdate {
                version: read.version,
                name: Some("  ".into()),
                ..PortalUpdate::default()
            },
        )
        .await;
    tx.rollback().await?;
    proof.check(
        "Core's own checks apply: a too-long custom value and a blank name are refused",
        matches!(&too_long, Err(WriteError::CustomFields(errors)) if errors.iter().any(|e| e.field == "cf_segment"))
            && matches!(&blank, Err(WriteError::Validation(errors)) if errors.iter().any(|e| e.field == "name")),
    );
    let mut custom = Custom::new();
    custom.insert("segment".into(), json!("Retail"));
    custom.insert("tier".into(), json!(first));
    let mut tx = boundary
        .begin(
            &mut client,
            portal.mutation_context("walkthrough.portal.profile"),
        )
        .await?;
    let saved = people
        .portal_update_in(
            &mut tx,
            &portal,
            &PortalUpdate {
                version: read.version,
                name: Some("Siti binti Customer".into()),
                phone: Some("+60 19 876 5432".into()),
                custom,
            },
        )
        .await?;
    tx.commit().await?;
    let audit = client
        .query_one(
            "SELECT count(*), max(actor_id), max(actor_role) FROM base_audit_mutations WHERE auditable_type='person' AND auditable_id=$1 AND actor_id=$2",
            &[&mine.id.get().to_string(), &customer],
        )
        .await?;
    proof.check(
        "the customer changes its name, phone and segment, with the unchanged tier riding along",
        saved.name == "Siti binti Customer"
            && saved.phone.as_deref() == Some("+60 19 876 5432")
            && saved.email.as_deref() == Some("siti@example.test")
            && saved.version == read.version + 1
            && saved
                .custom
                .iter()
                .any(|field| field.key == "segment" && field.value.as_deref() == Some("Retail")),
    );
    proof.check(
        "one audited person change, attributed to the customer account as customer",
        audit.get::<_, i64>(0) == 1
            && audit.get::<_, Option<i64>>(1) == Some(customer)
            && audit.get::<_, Option<String>>(2).as_deref() == Some("customer"),
    );
    let mut tier = Custom::new();
    tier.insert("tier".into(), json!(second));
    let mut tx = boundary
        .begin(
            &mut client,
            portal.mutation_context("walkthrough.portal.profile"),
        )
        .await?;
    let tier_change = people
        .portal_update_in(
            &mut tx,
            &portal,
            &PortalUpdate {
                version: saved.version,
                custom: tier,
                ..PortalUpdate::default()
            },
        )
        .await;
    let stale = people
        .portal_update_in(
            &mut tx,
            &portal,
            &PortalUpdate {
                version: read.version,
                name: Some("Siti Stale".into()),
                ..PortalUpdate::default()
            },
        )
        .await;
    tx.rollback().await?;
    proof.check(
        "a custom field the tenant did not list is refused on cf_tier",
        matches!(&tier_change, Err(WriteError::CustomFields(errors)) if errors.iter().any(|e| e.field == "cf_tier")),
    );
    proof.check(
        "a stale version is a conflict",
        matches!(stale, Err(WriteError::Backend(Error::Conflict))),
    );

    println!("-- another person");
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("walkthrough.portal.borrowed"),
        )
        .await?;
    let borrowed = people
        .portal_update_in(
            &mut tx,
            &portal,
            &PortalUpdate {
                version: saved.version,
                name: Some("Not the customer".into()),
                ..PortalUpdate::default()
            },
        )
        .await;
    tx.rollback().await?;
    proof.check(
        "a transaction attributed to anyone but the customer is refused",
        matches!(borrowed, Err(WriteError::Backend(Error::Forbidden))),
    );
    let other_now = people.read(&manager, other.id).await?;
    proof.check(
        "the other person is untouched; no portal method takes a Person ID",
        other_now.name == "Other Customer"
            && other_now.version == other.version
            && changes(&client, other.id.get()).await? == 1,
    );

    println!("-- standing");
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("walkthrough.portal.revoke"),
        )
        .await?;
    app.user
        .set_customer_active_in(&mut tx, &manager, customer, false)
        .await?;
    tx.commit().await?;
    proof.check(
        "a revoked customer reads nothing: the portal scope is re-proved",
        people.portal_read(&portal).await.is_err(),
    );
    drop(client);

    println!("-- {} checks failed", proof.failed.len());
    if !proof.failed.is_empty() {
        return Err(format!("failed: {:?}", proof.failed).into());
    }
    Ok(())
}
