//! Structured custom fields (#685) through the real doors on a disposable,
//! migrated database: several choices from a governed pick-list and a record
//! field pointing at a company, kept in the contact's own JSON column, then
//! read through reports (Grid's scoped catalog) and global search. Each line
//! is `ok` or `FAIL`; the run fails if any check does.
//!
//! ```text
//! BILIBI_CUSTOM_FIELDS_DEMO=disposable DATABASE_URL=… APP_ORIGIN=… \
//!   cargo run -p bilibi-custom-fields --example structured-fields-walkthrough
//! ```
//!
//! Fixture setup with no page of its own is plain SQL and says so: the
//! fixture tenant's pick-lists, provisioned as a new tenant's are, and a
//! direct deny that keeps one account from seeing companies.
use bilibi_agent_api::{
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
    Registry,
};
use bilibi_database::{pool, Db};
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
            Ok(PlanIdentity {
                scope: self.app.security.sessions.hold(tx, self.token).await?,
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}

/// One signed-in person, calling operations as the pages and agents do.
struct Person<'a> {
    app: &'a bilibi_web::App,
    db: &'a Db,
    token: String,
    scope: Scope,
}
impl Person<'_> {
    /// A reviewed write: the preview, then (when it is valid) the commit.
    async fn plan(&self, key: &str, input: Value) -> Result<Value, Failure> {
        let plans = self.app.services.get::<Plans>("base/agent_api.plans")?;
        let browser = Browser {
            app: self.app,
            token: &self.token,
        };
        let ops = [PlannedOperation {
            key: key.into(),
            input,
        }];
        let preview = plans.preview(&browser, &ops).await?;
        if preview["valid"] != true {
            return Ok(preview);
        }
        Ok(plans.commit(&self.scope, &browser, &ops).await?)
    }
    async fn read(&self, key: &str, input: Value) -> Result<Value, Failure> {
        let registry = self
            .app
            .services
            .get::<Registry>("base/agent_api.registry")?;
        let client = self.db.get().await?;
        Ok(registry.call(&**client, &self.scope, key, &input).await?["result"].clone())
    }
    /// The ids of the contacts a global search finds.
    async fn found(&self, text: &str) -> Result<Vec<String>, Failure> {
        let answer = self.read("base.search.records", json!({"q": text})).await?;
        Ok(answer["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|hit| hit["kind"] == "persons")
            .filter_map(|hit| hit["id"].as_str().map(str::to_owned))
            .collect())
    }
}

fn result(plan: &Value) -> &Value {
    &plan["results"][0]["result"]["result"]
}
fn refusals(plan: &Value) -> &Value {
    &plan["results"][0]["error"]["fields"]
}
fn custom<'a>(record: &'a Value, key: &str) -> &'a Value {
    record["custom"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["key"] == key))
        .map_or(&Value::Null, |item| &item["value"])
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_CUSTOM_FIELDS_DEMO").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_CUSTOM_FIELDS_DEMO=disposable and use a disposable migrated DATABASE_URL"
                .into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let origin = std::env::var("APP_ORIGIN")?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    {
        let client = db.get().await?;
        // Fixture setup: the tenant's pick-lists, as a new tenant gets them;
        // reports, records and search for the fixture accounts, which an
        // administrator grants on the Roles page; and a direct deny that keeps
        // Sam Staff (user 3) from companies.
        bilibi_picklists::provision_deployment(&**client, 1).await?;
        client
            .batch_execute(
                "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES
                 (1,1,'reports.view',true),(1,1,'reports.author',true),(1,1,'base.records.view',true),(1,1,'base.search.records.list',true),
                 (1,3,'reports.view',true),(1,3,'base.records.view',true),(1,3,'base.search.records.list',true),
                 (2,5,'reports.view',true),(2,5,'base.search.records.list',true),
                 (1,3,'admin.company.view',false),(1,3,'admin.company.list',false) ON CONFLICT DO NOTHING",
            )
            .await?;
    }
    let mut people = Vec::new();
    for email in [
        "admin@example.test",
        "manager@example.test",
        "staff@example.test",
        "manager@other.example.test",
    ] {
        let login = app
            .user
            .login(email, bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        let scope = app.security.guard(&login.token).await?;
        people.push(Person {
            app: &app,
            db: &db,
            token: login.token,
            scope,
        });
    }
    let (admin, manager, staff, other) = (&people[0], &people[1], &people[2], &people[3]);
    let mut proof = Proof::default();

    println!("-- definitions");
    let kinds = admin
        .read("base.custom_fields.record_types", json!({}))
        .await?;
    let list = kinds["pick_lists"][0]["key"]
        .as_str()
        .ok_or("no standalone pick-list")?
        .to_owned();
    proof.check(
        "record types offer companies as a record field's target",
        kinds["record_kinds"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind["key"] == "company")),
    );
    let codes: Vec<String> = {
        let client = db.get().await?;
        client
            .query(
                "SELECT code FROM base_picklist_values WHERE tenant_id=1 AND list=$1 AND retired_on IS NULL ORDER BY position LIMIT 2",
                &[&list],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    let [first, second] = codes.as_slice() else {
        return Err(format!("pick-list {list} needs two active values").into());
    };
    for (key, label, kind, extra) in [
        ("memo", "Memo", "text", json!({})),
        ("old_note", "Old note", "text", json!({})),
        ("tags", "Tags", "choices", json!({"picklist": list})),
        (
            "partner",
            "Partner",
            "reference",
            json!({"target": "company"}),
        ),
    ] {
        let mut input = json!({"record_type": "person", "key": key, "label": label, "kind": kind});
        input
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let defined = admin.plan("base.custom_fields.define", input).await?;
        if result(&defined)["key"] != key {
            println!("     {key}: {defined}");
        }
    }
    let wrong = admin
        .plan(
            "base.custom_fields.define",
            json!({"record_type": "person", "key": "bad_target", "label": "Bad", "kind": "reference", "target": "no_such_kind"}),
        )
        .await?;
    proof.check(
        "a record field must point at a mounted kind of record",
        refusals(&wrong)["target"].is_array(),
    );

    println!("-- values");
    let created = manager
        .plan(
            "core.person.create",
            json!({"name": "Zara Quill", "kind": "human", "custom": [
                {"key": "memo", "value": "Velvetine orchard"},
                {"key": "old_note", "value": "Quokkaberry"},
                {"key": "tags", "values": [first, second, first]},
                {"key": "partner", "value": "01"},
            ]}),
        )
        .await?;
    let zara = result(&created).clone();
    let zara_id = zara["id"]
        .as_i64()
        .ok_or_else(|| format!("no contact: {created}"))?;
    proof.check(
        "several choices are stored as one array of codes, each once, in the order chosen",
        custom(&zara, "tags") == &json!([first, second]),
    );
    proof.check(
        "a record field stores the record's id in its one spelling",
        custom(&zara, "partner") == "1",
    );
    let stored: Value = db
        .get()
        .await?
        .query_one(
            "SELECT custom_fields FROM persons WHERE tenant_id=1 AND id=$1",
            &[&zara_id],
        )
        .await?
        .get(0);
    proof.check(
        "the values live in the contact's own JSON column",
        stored["tags"] == json!([first, second]) && stored["partner"] == "1",
    );
    for (what, pair, key) in [
        (
            "an unknown code",
            json!({"key": "tags", "values": [first, "no-such-code"]}),
            "cf_tags",
        ),
        (
            "a single text value for several choices",
            json!({"key": "tags", "value": first}),
            "cf_tags",
        ),
        (
            "several values for a text field",
            json!({"key": "memo", "values": ["a", "b"]}),
            "cf_memo",
        ),
        (
            "a forged record id",
            json!({"key": "partner", "value": "999999"}),
            "cf_partner",
        ),
    ] {
        let refused = manager
            .plan(
                "core.person.create",
                json!({"name": "Refused", "kind": "human", "custom": [pair]}),
            )
            .await?;
        proof.check(
            &format!("{what} is refused on {key}"),
            refused["valid"] == false && refusals(&refused)[key].is_array(),
        );
    }

    println!("-- the record field follows the target's access");
    let hidden = staff
        .plan(
            "core.person.create",
            json!({"name": "Hidden Partner", "kind": "human", "custom": [{"key": "partner", "value": "1"}]}),
        )
        .await?;
    proof.check(
        "a person who may not see companies cannot choose one",
        hidden["valid"] == false && refusals(&hidden)["cf_partner"].is_array(),
    );
    let read = staff
        .read("core.person.get", json!({"id": zara_id}))
        .await?;
    let version = read["version"].clone();
    let edited = staff
        .plan(
            "core.person.update",
            json!({"id": zara_id, "version": version, "name": "Zara Quill", "kind": "human", "custom": [{"key": "memo", "value": "Velvetine orchard, east gate"}]}),
        )
        .await?;
    proof.check(
        "they may still change another field of a contact that holds it",
        edited["valid"] == true
            && custom(result(&edited), "partner") == "1"
            && custom(result(&edited), "memo") == "Velvetine orchard, east gate",
    );
    let names = staff
        .read(
            "base.records.resolve",
            json!({"references": [{"kind": "company", "id": "1"}]}),
        )
        .await?;
    proof.check(
        "and the held company stays unnamed for them",
        names
            .as_array()
            .and_then(|items| items.first())
            .is_some_and(|item| item["label"].is_null()),
    );

    println!("-- reports");
    let fields = manager
        .read("base.reports.fields", json!({"record_type": "persons"}))
        .await?;
    let specs = |fields: &Value| -> Vec<String> {
        fields["columns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|column| column["spec"].as_str().map(str::to_owned))
            .collect()
    };
    let offered = specs(&fields);
    proof.check(
        "a report on contacts offers each active custom field as a column",
        ["cf_memo", "cf_tags", "cf_partner", "cf_old_note"]
            .iter()
            .all(|spec| offered.iter().any(|s| s == spec)),
    );
    let staff_offered = specs(
        &staff
            .read("base.reports.fields", json!({"record_type": "persons"}))
            .await?,
    );
    proof.check(
        "a record field is offered only to readers who may read its kind",
        staff_offered.iter().any(|s| s == "cf_memo")
            && !staff_offered.iter().any(|s| s == "cf_partner"),
    );
    let other_offered = specs(
        &other
            .read("base.reports.fields", json!({"record_type": "persons"}))
            .await?,
    );
    proof.check(
        "another tenant's catalog has none of these fields",
        !other_offered.iter().any(|s| s.starts_with("cf_")),
    );
    let report = manager
        .plan(
            "base.reports.create",
            json!({"name": "Velvetine contacts", "record_type": "persons", "kind": "rows", "definition": {
                "columns": ["name", "cf_memo", "cf_tags", "cf_partner"],
                "filters": [{"column": "cf_memo", "test": "contains", "value": "velvetine"}],
                "sort": [{"column": "cf_memo"}],
            }}),
        )
        .await?;
    let report_id = result(&report)["id"]
        .as_i64()
        .ok_or_else(|| format!("no report: {report}"))?;
    let run = manager
        .read("base.reports.run", json!({"id": report_id}))
        .await?;
    let rows = run["report"]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let company: String = db
        .get()
        .await?
        .query_one("SELECT name FROM companies WHERE tenant_id=1 AND id=1", &[])
        .await?
        .get(0);
    let cells = rows.first().map(|row| row[1].clone()).unwrap_or_default();
    let cell = |index: usize| {
        let value = &cells[index];
        value
            .get("value")
            .or(Some(value))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    proof.check(
        "the report filters, sorts and shows custom values: text, the codes chosen and the partner's name",
        rows.len() == 1
            && cell(1).contains("Velvetine")
            && cell(2) == format!("{first}, {second}")
            && cell(3) == company,
    );
    if rows.len() != 1 {
        println!("     {run}");
    }
    let staff_run = staff
        .read("base.reports.run", json!({"id": report_id}))
        .await;
    proof.check(
        "the report is the author's: another reader cannot run it unshared",
        staff_run.is_err(),
    );
    let grouped = manager
        .plan(
            "base.reports.create",
            json!({"name": "Contacts by tags", "record_type": "persons", "kind": "summary", "definition": {
                "groupings": [{"column": "cf_tags"}],
                "measures": [{"column": "id", "agg": "count"}],
            }}),
        )
        .await?;
    let grouped_id = result(&grouped)["id"].as_i64().ok_or("no summary report")?;
    let summary = manager
        .read("base.reports.run", json!({"id": grouped_id}))
        .await?;
    proof.check(
        "a summary groups by a custom field",
        summary["report"]["lines"]
            .as_array()
            .is_some_and(|lines| !lines.is_empty()),
    );

    println!("-- global search");
    proof.check(
        "a contact is found by a word of a custom text field",
        manager
            .found("velvetine")
            .await?
            .contains(&zara_id.to_string()),
    );
    proof.check(
        "another tenant does not find it",
        other.found("velvetine").await?.is_empty(),
    );
    proof.check(
        "a pick-list code is not searchable text",
        !manager.found(first).await?.contains(&zara_id.to_string()),
    );
    proof.check(
        "an active field's text is found before it is archived",
        manager
            .found("quokkaberry")
            .await?
            .contains(&zara_id.to_string()),
    );
    let definition = admin
        .read("base.custom_fields.list", json!({"record_type": "person"}))
        .await?;
    let old_version = definition
        .as_array()
        .and_then(|items| items.iter().find(|item| item["key"] == "old_note"))
        .map(|item| item["version"].clone())
        .ok_or("no old_note")?;
    admin
        .plan(
            "base.custom_fields.archive",
            json!({"record_type": "person", "key": "old_note", "version": old_version}),
        )
        .await?;
    proof.check(
        "an archived field's text no longer finds the record",
        !manager
            .found("quokkaberry")
            .await?
            .contains(&zara_id.to_string()),
    );
    let after = specs(
        &manager
            .read("base.reports.fields", json!({"record_type": "persons"}))
            .await?,
    );
    proof.check(
        "an archived field leaves the report columns",
        !after.iter().any(|s| s == "cf_old_note") && after.iter().any(|s| s == "cf_memo"),
    );

    if proof.failed.is_empty() {
        println!("Structured custom fields walkthrough passed");
        Ok(())
    } else {
        Err(format!("{} checks failed: {:?}", proof.failed.len(), proof.failed).into())
    }
}
