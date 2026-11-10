//! Person custom fields (#424) through the real doors on a disposable,
//! migrated database: an administrator defines text, number, date and
//! pick-list fields for contacts (`base/custom_fields`), and Core Person keeps
//! the values on the person, checked in its own writes. Each line is `ok` or
//! `FAIL`; the run fails if any check does. The list part searches, filters
//! and sorts by custom values through Core Person's search and
//! `core.person.list`. With `serve` (or on a database it
//! already prepared) it only serves the app for the browser.
//!
//! ```text
//! BILIBI_PERSON_DEMO=disposable DATABASE_URL=… APP_ORIGIN=… BIND_ADDR=… \
//!   cargo run -p bilibi-person --example person-custom-fields
//! ```
//!
//! Fixture setup with no page of its own is plain SQL and says so: the
//! fixture tenant's pick-lists, provisioned as a new tenant's are.
use bilibi_agent_api::{
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
    Registry,
};
use bilibi_custom_fields::{CustomFields, NewField};
use bilibi_database::{pool, Error, MutationBoundary};
use bilibi_person::{CustomFilter, PersonService, Search, Update, SERVICE};
use bilibi_tenancy::{ProofFuture, Scope};
use serde_json::{json, Value};
use tokio_postgres::Transaction;
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

/// A plan's browser session, as the walkthrough fixture signs it in.
struct Browser<'a> {
    app: &'a bilibi_web::App,
    token: &'a str,
}
impl PlanAuthority for Browser<'_> {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            tx.query_opt(
                "SELECT id FROM sessions WHERE id=$1 AND expires_at>clock_timestamp() FOR UPDATE",
                &[&self.token],
            )
            .await?
            .ok_or(Error::Unauthorized)?;
            Ok(PlanIdentity {
                scope: self.app.security.sessions.check(tx, self.token).await?,
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}

/// The field errors a plan refused, by field.
fn refused(plan: &Value) -> Value {
    plan["results"][0]["error"]["fields"].clone()
}
fn custom_of<'a>(record: &'a Value, key: &str) -> Option<&'a Value> {
    record["custom"]
        .as_array()?
        .iter()
        .find(|item| item["key"] == key)
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
    let bind = std::env::var("BIND_ADDR")?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let prepared: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM base_custom_fields WHERE record_type='person'",
            &[],
        )
        .await?
        .get(0);
    if std::env::args().nth(1).as_deref() == Some("serve") || prepared > 0 {
        return serve(app, &bind, &origin).await;
    }
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
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let password = bilibi_walkthrough_fixture::PASSWORD;
    let mut logins = Vec::new();
    for email in [
        "manager@example.test",
        "viewer@example.test",
        "admin@example.test",
        "manager@other.example.test",
    ] {
        logins.push(app.user.login(email, password).await?);
    }
    let scopes: Vec<Scope> = {
        let mut scopes = Vec::new();
        for login in &logins {
            scopes.push(app.security.guard(&login.token).await?);
        }
        scopes
    };
    let (manager, admin, other) = (&scopes[0], &scopes[2], &scopes[3]);
    let browser = Browser {
        app: &app,
        token: &logins[0].token,
    };
    let viewer_browser = Browser {
        app: &app,
        token: &logins[1].token,
    };
    let mut proof = Proof::default();
    let mut client = db.get().await?;

    println!("-- definitions");
    // Fixture setup: the fixture tenant gets its pick-lists as a new tenant
    // would.
    bilibi_picklists::provision_deployment(&**client, 1).await?;
    let (list, _) = *fields
        .choice_lists()
        .first()
        .ok_or("no standalone pick-list to choose from")?;
    let choices = lists.values(&**client, admin, list, false).await?;
    let (first, second) = match choices.as_slice() {
        [first, second, ..] => (first.clone(), second.clone()),
        _ => return Err(format!("pick-list {list} needs two values").into()),
    };
    for (key, label, kind, picklist) in [
        ("nickname", "Nickname", "text", None),
        ("credit_limit", "Credit limit", "number", None),
        ("anniversary", "Anniversary", "date", None),
        ("tier", "Tier", "choice", Some(list)),
    ] {
        let mut tx = boundary
            .begin(&mut client, admin.mutation_context("person.custom.define"))
            .await?;
        let defined = fields
            .define(
                &mut tx,
                admin,
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
            println!("     {key}: {errors:?}");
        }
        tx.commit().await?;
    }
    let active = fields.active(&**client, manager, "person").await?;
    proof.check(
        "an administrator defines four person fields; contacts are a custom-field record type",
        active.len() == 4 && fields.record_types().any(|t| t.id == "person"),
    );

    println!("-- create");
    let create = |custom: Value| {
        [PlannedOperation {
            key: "core.person.create".into(),
            input: json!({"name": "Nadia Rahman", "kind": "human", "custom": custom}),
        }]
    };
    let bad = plans
        .preview(
            &browser,
            &create(json!([
                {"key": "shoe_size", "value": "9"},
                {"key": "credit_limit", "value": "lots"},
                {"key": "anniversary", "value": "2026-13-40"},
                {"key": "tier", "value": "no-such-code"},
            ])),
        )
        .await?;
    let errors = refused(&bad);
    proof.check(
        "an unknown field, a bad number, a bad date and an unknown choice are each refused on cf_<key>",
        bad["valid"] == false
            && ["cf_shoe_size", "cf_credit_limit", "cf_anniversary", "cf_tier"]
                .iter()
                .all(|field| errors.get(field).is_some()),
    );
    if bad["valid"] != false {
        println!("     {bad}");
    }
    let good = create(json!([
        {"key": "nickname", "value": "Nad"},
        {"key": "credit_limit", "value": "15000.50"},
        {"key": "anniversary", "value": "2024-02-29"},
        {"key": "tier", "value": first.code},
    ]));
    let preview = plans.preview(&browser, &good).await?;
    let committed = plans.commit(manager, &browser, &good).await?;
    let nadia = committed["results"][0]["result"]["result"].clone();
    let nadia_id = nadia["id"].as_i64().ok_or("no person id")?;
    proof.check(
        "a contact is created with all four values, as previewed, and reads them back with labels and types",
        preview["valid"] == true
            && preview["results"][0]["preview"]["result"]["identity"]["custom"]["nickname"] == "Nad"
            && custom_of(&nadia, "nickname").is_some_and(|f| f["value"] == "Nad" && f["label"] == "Nickname" && f["type"] == "text")
            && custom_of(&nadia, "credit_limit").is_some_and(|f| f["value"] == "15000.50")
            && custom_of(&nadia, "anniversary").is_some_and(|f| f["value"] == "2024-02-29" && f["type"] == "date")
            && custom_of(&nadia, "tier").is_some_and(|f| f["value"] == first.code.as_str()),
    );
    if nadia_id == 0 || custom_of(&nadia, "tier").is_none() {
        println!("     {committed}");
    }
    let audited: Option<Value> = client
        .query_opt(
            "SELECT new_values FROM base_audit_mutations WHERE auditable_type='person' AND auditable_id=$1 ORDER BY id DESC LIMIT 1",
            &[&nadia_id.to_string()],
        )
        .await?
        .map(|row| row.get(0));
    proof.check(
        "the new values are in the person's audit row",
        audited.is_some_and(|v| v["custom"]["credit_limit"] == "15000.50"),
    );

    println!("-- update");
    let person_id = bilibi_person::PersonId::new(nadia_id)?;
    let current = people.read(manager, person_id).await?;
    let identity = bilibi_person::PersonInput {
        name: current.name.clone(),
        kind: current.kind,
        email: None,
        phone: None,
    };
    let patch = |pairs: &[(&str, Value)]| {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect::<serde_json::Map<_, _>>()
    };
    let updated = people
        .update(
            manager,
            person_id,
            &Update {
                version: current.version,
                identity: identity.clone(),
                custom: patch(&[("nickname", json!("Nads")), ("credit_limit", Value::Null)]),
            },
        )
        .await?;
    proof.check(
        "an update sets one value and clears another; the ones it does not name are kept",
        updated.custom.get("nickname") == Some(&json!("Nads"))
            && !updated.custom.contains_key("credit_limit")
            && updated.custom.get("anniversary") == Some(&json!("2024-02-29"))
            && updated.custom.get("tier") == Some(&json!(first.code)),
    );
    let stale = people
        .update(
            manager,
            person_id,
            &Update {
                version: current.version,
                identity: identity.clone(),
                custom: patch(&[("nickname", json!("Stale"))]),
            },
        )
        .await;
    proof.check(
        "a stale version is refused",
        matches!(
            stale,
            Err(bilibi_person::WriteError::Backend(Error::Conflict))
        ),
    );
    let viewer_plan = plans
        .preview(
            &viewer_browser,
            &[PlannedOperation {
                key: "core.person.update".into(),
                input: json!({"person_id": nadia_id, "version": updated.version, "identity": {"name": "Nadia Rahman", "kind": "human"}, "custom": [{"key": "nickname", "value": "Hacked"}]}),
            }],
        )
        .await?;
    let foreign = registry
        .call(
            &**client,
            other,
            "core.person.get",
            &json!({"person_id": nadia_id}),
        )
        .await;
    proof.check(
        "the viewer may not change a value, and another tenant cannot read the person",
        viewer_plan["valid"] != true && foreign.is_err(),
    );

    println!("-- retired choices and archived fields");
    // The held choice is retired: the person keeps it, a new one cannot take it.
    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("person.custom.retire-choice"),
        )
        .await?;
    let retired = lists
        .retire(&mut tx, admin, list, &first.code, first.version)
        .await?;
    tx.commit().await?;
    let kept = people
        .update(
            manager,
            person_id,
            &Update {
                version: updated.version,
                identity: identity.clone(),
                custom: patch(&[("nickname", json!("Nadia"))]),
            },
        )
        .await;
    let fresh = plans
        .preview(
            &browser,
            &[PlannedOperation {
                key: "core.person.create".into(),
                input: json!({"name": "Omar Tan", "kind": "human", "custom": [{"key": "tier", "value": first.code}]}),
            }],
        )
        .await?;
    proof.check(
        "a retired choice stays on the contact who holds it and cannot be chosen anew",
        retired.is_ok()
            && kept
                .as_ref()
                .is_ok_and(|p| p.custom.get("tier") == Some(&json!(first.code)))
            && fresh["valid"] == false
            && refused(&fresh).get("cf_tier").is_some(),
    );
    let after_retire = kept.map_err(|e| format!("{e:?}"))?;
    let nickname = active
        .iter()
        .find(|d| d.key == "nickname")
        .ok_or("nickname")?;
    let mut tx = boundary
        .begin(&mut client, admin.mutation_context("person.custom.archive"))
        .await?;
    fields
        .archive(&mut tx, admin, "person", &nickname.key, nickname.version)
        .await?
        .map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    let read = registry
        .call(
            &**client,
            manager,
            "core.person.get",
            &json!({"person_id": nadia_id}),
        )
        .await?;
    let read = serde_json::to_value(&read)?;
    let stored: Value = client
        .query_one(
            "SELECT custom_fields FROM persons WHERE id=$1",
            &[&nadia_id],
        )
        .await?
        .get(0);
    let archived_write = people
        .update(
            manager,
            person_id,
            &Update {
                version: after_retire.version,
                identity: identity.clone(),
                custom: patch(&[("nickname", json!("Again"))]),
            },
        )
        .await;
    proof.check(
        "an archived field leaves reads and writes, and its value stays on the person",
        !read.to_string().contains("\"nickname\"")
            && stored["nickname"] == "Nadia"
            && matches!(archived_write, Err(bilibi_person::WriteError::CustomFields(ref e)) if e.iter().any(|f| f.field == "cf_nickname")),
    );
    if read.to_string().contains("\"nickname\"") {
        println!("     {read}");
    }

    println!("-- list: filters and sorts");
    // A text field for the list, and three more contacts with values.
    let mut tx = boundary
        .begin(&mut client, admin.mutation_context("person.custom.define"))
        .await?;
    fields
        .define(
            &mut tx,
            admin,
            &NewField {
                record_type: "person".into(),
                key: "region".into(),
                label: "Region".into(),
                kind: "text".into(),
                ..NewField::default()
            },
        )
        .await?
        .map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    for (name, custom) in [
        (
            "Lim Ah Kow",
            json!([{"key": "credit_limit", "value": "500"}, {"key": "anniversary", "value": "2023-01-15"}, {"key": "tier", "value": second.code}, {"key": "region", "value": "Klang Valley"}]),
        ),
        (
            "Siti Aminah",
            json!([{"key": "credit_limit", "value": "2500.75"}, {"key": "anniversary", "value": "2025-06-01"}, {"key": "tier", "value": second.code}]),
        ),
        (
            "Raj Kumar",
            json!([{"key": "credit_limit", "value": "10000"}, {"key": "region", "value": "Penang"}]),
        ),
    ] {
        let ops = [PlannedOperation {
            key: "core.person.create".into(),
            input: json!({"name": name, "kind": "human", "custom": custom}),
        }];
        let made = plans.commit(manager, &browser, &ops).await?;
        if made["results"][0]["result"]["result"]["id"].is_null() {
            println!("     {name}: {made}");
        }
    }
    let find = |custom: Vec<(&str, &str, &str)>, sort: &str, descending: bool| Search {
        custom: custom
            .into_iter()
            .map(|(key, op, value)| CustomFilter {
                key: key.into(),
                op: op.into(),
                value: value.into(),
            })
            .collect(),
        sort: sort.into(),
        descending,
        page_size: 50,
        ..Search::default()
    };
    let names = |page: &bilibi_person::Page| -> Vec<String> {
        page.entries.iter().map(|p| p.name.clone()).collect()
    };
    let at_least = people
        .search(
            manager,
            &find(vec![("credit_limit", "gte", "1000")], "name", false),
        )
        .await?;
    let at_most = people
        .search(
            manager,
            &find(vec![("credit_limit", "lte", "1000")], "name", false),
        )
        .await?;
    proof.check(
        "a number filter compares as a number, and a contact without a value matches neither bound",
        names(&at_least) == ["Raj Kumar", "Siti Aminah"] && names(&at_most) == ["Lim Ah Kow"],
    );
    let dated = people
        .search(
            manager,
            &find(
                vec![
                    ("anniversary", "gte", "2024-01-01"),
                    ("anniversary", "lte", "2025-12-31"),
                ],
                "name",
                false,
            ),
        )
        .await?;
    let tiered = people
        .search(
            manager,
            &find(vec![("tier", "eq", &second.code)], "name", false),
        )
        .await?;
    let region = people
        .search(
            manager,
            &find(vec![("region", "contains", "KLANG")], "name", false),
        )
        .await?;
    proof.check(
        "a date range, a choice and a text contains (any case) each find their contacts",
        names(&dated) == ["Nadia Rahman", "Siti Aminah"]
            && names(&tiered) == ["Lim Ah Kow", "Siti Aminah"]
            && names(&region) == ["Lim Ah Kow"],
    );
    let ascending = people
        .search(manager, &find(vec![], "cf_credit_limit", false))
        .await?;
    let descending = people
        .search(manager, &find(vec![], "cf_credit_limit", true))
        .await?;
    let ordered = |page: &bilibi_person::Page| -> Vec<String> {
        names(page)
            .into_iter()
            .filter(|n| {
                ["Lim Ah Kow", "Siti Aminah", "Raj Kumar", "Nadia Rahman"].contains(&n.as_str())
            })
            .collect()
    };
    proof.check(
        "sorting by a number field is numeric both ways, with contacts lacking a value last",
        ordered(&ascending) == ["Lim Ah Kow", "Siti Aminah", "Raj Kumar", "Nadia Rahman"]
            && ordered(&descending) == ["Raj Kumar", "Siti Aminah", "Lim Ah Kow", "Nadia Rahman"],
    );
    if ordered(&ascending) != ["Lim Ah Kow", "Siti Aminah", "Raj Kumar", "Nadia Rahman"] {
        println!("     {:?} / {:?}", names(&ascending), names(&descending));
    }
    let mut refusals = Vec::new();
    for (label, query) in [
        (
            "unknown field",
            find(vec![("shoe_size", "eq", "9")], "name", false),
        ),
        (
            "archived field",
            find(vec![("nickname", "contains", "Na")], "name", false),
        ),
        (
            "wrong operator",
            find(vec![("credit_limit", "contains", "5")], "name", false),
        ),
        (
            "bad number",
            find(vec![("credit_limit", "gte", "lots")], "name", false),
        ),
        (
            "bad date",
            find(vec![("anniversary", "gte", "2026-13-01")], "name", false),
        ),
        (
            "quoted key",
            find(vec![("x') OR 1=1 --", "eq", "1")], "name", false),
        ),
        ("unknown sort", find(vec![], "cf_shoe_size", false)),
        ("archived sort", find(vec![], "cf_nickname", false)),
    ] {
        if !matches!(people.search(manager, &query).await, Err(Error::Invalid)) {
            refusals.push(label);
        }
    }
    proof.check(
        "an unknown or archived field, a wrong operator, a bad number or date, a quoted key and an unknown sort are refused",
        refusals.is_empty(),
    );
    if !refusals.is_empty() {
        println!("     not refused: {refusals:?}");
    }
    let listed = registry
        .call(
            &**client,
            manager,
            "core.person.list",
            &json!({"custom": [{"key": "tier", "op": "eq", "value": second.code}], "sort": "cf_credit_limit", "descending": true}),
        )
        .await?;
    let listed = serde_json::to_value(&listed)?;
    let listed_names: Vec<&str> = listed["result"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r["name"].as_str()).collect())
        .unwrap_or_default();
    proof.check(
        "core.person.list takes the same conditions and custom sort",
        listed_names == ["Siti Aminah", "Lim Ah Kow"],
    );
    if listed_names != ["Siti Aminah", "Lim Ah Kow"] {
        println!("     {listed}");
    }
    let started = std::time::Instant::now();
    let searches: Vec<_> = (0..40)
        .map(|_| {
            let (people, manager, query) = (
                people.clone(),
                manager.clone(),
                find(
                    vec![("credit_limit", "gte", "100"), ("tier", "eq", &second.code)],
                    "cf_anniversary",
                    true,
                ),
            );
            tokio::spawn(async move {
                tokio::time::timeout(std::time::Duration::from_secs(30), async move {
                    people.search(&manager, &query).await
                })
                .await
            })
        })
        .collect();
    let mut searched = 0;
    for task in searches {
        if matches!(task.await?, Ok(Ok(ref page)) if page.total == 2) {
            searched += 1;
        }
    }
    proof.check(
        "40 concurrent filtered, custom-sorted searches all answer",
        searched == 40,
    );
    println!("     40 searches in {} ms", started.elapsed().as_millis());

    println!("-- concurrency");
    // Each plan has its own session: plans sharing one wait on its row and
    // are refused as retryable, which is not what this check is about.
    let mut tokens = Vec::with_capacity(40);
    for _ in 0..40 {
        tokens.push(
            app.user
                .login("manager@example.test", password)
                .await?
                .token,
        );
    }
    let started = std::time::Instant::now();
    let input = json!({"person_id": nadia_id, "version": after_retire.version, "identity": {"name": "Nadia Rahman", "kind": "human"}, "custom": [{"key": "tier", "value": second.code}]});
    let tasks: Vec<_> = tokens
        .iter()
        .map(|token| {
            let (plans, app, token, input) =
                (plans.clone(), app.clone(), token.clone(), input.clone());
            tokio::spawn(async move {
                let browser = Browser {
                    app: &app,
                    token: &token,
                };
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    plans.preview(
                        &browser,
                        &[PlannedOperation {
                            key: "core.person.update".into(),
                            input,
                        }],
                    ),
                )
                .await
            })
        })
        .collect();
    let mut answered = 0;
    let mut first_refusal = None;
    for task in tasks {
        match task.await? {
            Ok(Ok(plan)) if plan["valid"] == true => answered += 1,
            other => first_refusal = first_refusal.or(Some(format!("{other:?}"))),
        }
    }
    if let Some(refusal) = first_refusal {
        println!("     {refusal}");
    }
    proof.check(
        "40 concurrent update previews with custom values all answer",
        answered == 40,
    );
    println!("     40 previews in {} ms", started.elapsed().as_millis());
    drop(client);

    println!("-- {} checks failed", proof.failed.len());
    if !proof.failed.is_empty() {
        return Err(format!("failed: {:?}", proof.failed).into());
    }
    println!(
        "Sign in at {origin} as manager@example.test with the fixture password. Nadia Rahman: {origin}/contacts/{nadia_id}  Contacts: {origin}/contacts  Custom fields: {origin}/admin/custom-fields (admin@example.test)"
    );
    serve(app, &bind, &origin).await
}

async fn serve(app: bilibi_web::App, bind: &str, origin: &str) -> Result<(), Failure> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("Serving {origin}");
    axum::serve(listener, bilibi_web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
