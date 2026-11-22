//! Exports from one fixture tenant and previews and applies in the other
//! through the same Registry and Plans that dispatch agent operations.
use bilibi_agent_api::{
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
    Error, Registry,
};
use bilibi_database::{pool, Db};
use bilibi_tenancy::{ProofFuture, Scope};
use serde_json::{json, Value};
use tokio_postgres::Transaction;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Failure = Box<dyn std::error::Error + Send + Sync>;
const EXPORT: &str = "base.setup_packs.export";
const PREVIEW: &str = "base.setup_packs.preview";
const APPLY: &str = "base.setup_packs.apply";
const SET: &str = "base.labels.set";
const DEFINE: &str = "base.custom_fields.define";
const LABEL: &str = "admin.labels";
const OTHER_LABEL: &str = "admin.custom_fields";

struct Person<'a> {
    app: &'a bilibi_web::App,
    db: &'a Db,
    token: String,
    scope: Scope,
}

impl PlanAuthority for Person<'_> {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            Ok(PlanIdentity {
                scope: self.app.security.sessions.hold(tx, &self.token).await?,
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}

impl<'a> Person<'a> {
    async fn login(app: &'a bilibi_web::App, db: &'a Db, email: &str) -> Result<Self, Failure> {
        let login = app
            .user
            .login(email, bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        let scope = app.security.guard(&login.token).await?;
        Ok(Self {
            app,
            db,
            token: login.token,
            scope,
        })
    }

    fn plans(&self) -> Result<std::sync::Arc<Plans>, Failure> {
        Ok(self.app.services.get("base/agent_api.plans")?)
    }

    async fn read(&self, key: &str, input: &Value) -> Result<Value, Failure> {
        let registry = self
            .app
            .services
            .get::<Registry>("base/agent_api.registry")?;
        Ok(registry.read(self.db, &self.scope, key, input).await?["result"].clone())
    }

    async fn write(&self, operations: &[PlannedOperation]) -> Result<Value, Failure> {
        let plans = self.plans()?;
        let preview = plans.preview(self, operations).await?;
        if preview["valid"] != true {
            return Err(format!(
                "The operation plan refused the dry run: {}",
                preview["results"]
            )
            .into());
        }
        Ok(plans.commit(&self.scope, self, operations).await?)
    }
}

fn op(key: &str, input: Value) -> PlannedOperation {
    PlannedOperation {
        key: key.into(),
        input,
    }
}

fn set_label(subject: &str, label: &str) -> PlannedOperation {
    op(
        SET,
        json!({"kind":"menu", "subject":subject, "label":label, "version":"0:0"}),
    )
}

fn apply(request: &Value, preview: &Value) -> PlannedOperation {
    let mut input = request.clone();
    input["expected_additions"] = preview["expected_additions"].clone();
    op(APPLY, input)
}

fn check(label: &str, pass: bool) -> Result<(), Failure> {
    if !pass {
        return Err(label.to_owned().into());
    }
    println!("ok  {label}");
    Ok(())
}

async fn counts(db: &Db, tenant: i64) -> Result<(i64, i64, i64, i64), Failure> {
    let client = db.get().await?;
    let row = client.query_one(
        "SELECT (SELECT count(*) FROM base_labels WHERE tenant_id=$1),
                (SELECT count(*) FROM base_custom_fields WHERE tenant_id=$1),
                (SELECT count(*) FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type='label'),
                (SELECT count(*) FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type='custom_field')",
        &[&tenant],
    ).await?;
    Ok((row.get(0), row.get(1), row.get(2), row.get(3)))
}

async fn round_trip(source: &Person<'_>, target: &Person<'_>) -> Result<Value, Failure> {
    let field = json!({
        "record_type":"company", "key":"contract_stage", "label":"Contract stage",
        "help":"The stage of the contract", "kind":"choice",
        "picklist":"core.document.status", "watcher_condition":true
    });
    source
        .write(&[set_label(LABEL, "Vocabulary"), op(DEFINE, field.clone())])
        .await?;
    let selection = json!({
        "labels":[{"kind":"menu","subject":LABEL}],
        "custom_fields":[{"record_type":"company","key":"contract_stage"}]
    });
    let pack = source.read(EXPORT, &selection).await?;
    check(
        "Tenant A exports the definition and override in version 3",
        pack == json!({
            "version":3,
            "labels":[{"kind":"menu","subject":LABEL,"label":"Vocabulary"}],
            "custom_fields":[field], "views":[], "watchers":[]
        }),
    )?;
    check(
        "Tenant B cannot export definitions and overrides held only by tenant A",
        target.read(EXPORT, &selection).await.is_err(),
    )?;

    let request = json!({
        "pack":pack, "picklist_map":[{"from":"core.document.status","to":"core.document.category"}]
    });
    let difference = target.read(PREVIEW, &request).await?;
    check(
        "Tenant B previews both additions and maps the destination pick-list",
        difference["ready"] == true
            && difference["additions"]["labels"][0]["label"] == "Vocabulary"
            && difference["additions"]["custom_fields"][0]["picklist"] == "core.document.category"
            && counts(target.db, target.scope.tenant()).await? == (0, 0, 0, 0),
    )?;

    let write = apply(&request, &difference);
    let outcome = target.write(std::slice::from_ref(&write)).await?;
    check(
        "Tenant B applies both owner changes with one audit row each",
        outcome["results"][0]["result"]["result"]
            == json!({"labels_added":1,"custom_fields_added":1,"views_added":0,"watchers_added":0})
            && counts(target.db, target.scope.tenant()).await? == (1, 1, 1, 1)
            && counts(source.db, source.scope.tenant()).await? == (1, 1, 1, 1),
    )?;
    let repeat = target.read(PREVIEW, &request).await?;
    check(
        "A repeat import shows an empty difference",
        repeat
            == json!({
                "ready":true,"additions":{"labels":[],"custom_fields":[],"views":[],"watchers":[]},
                "expected_additions":{"labels":[],"custom_fields":[],"views":[],"watchers":[]},
                "collisions":[],"missing":[],"invalid":[]
            }),
    )?;
    let outcome = target.write(&[write]).await?;
    check(
        "Reapplying the original reviewed request writes no rows or audit changes",
        outcome["results"][0]["result"]["result"]
            == json!({"labels_added":0,"custom_fields_added":0,"views_added":0,"watchers_added":0})
            && counts(target.db, target.scope.tenant()).await? == (1, 1, 1, 1),
    )?;
    Ok(request)
}

async fn shared_views(source: &Person<'_>, target: &Person<'_>) -> Result<(), Failure> {
    let client = source.db.get().await?;
    let source_role: i64 = client
        .query_one(
            "INSERT INTO roles(tenant_id,name) VALUES($1,'Setup pack reviewers') RETURNING id",
            &[&source.scope.tenant()],
        )
        .await?
        .get(0);
    let target_role: i64 = client
        .query_one(
            "INSERT INTO roles(tenant_id,name) VALUES($1,'Local reviewers') RETURNING id",
            &[&target.scope.tenant()],
        )
        .await?
        .get(0);
    drop(client);
    let outcome = source
        .write(&[op(
            "base.grid.view.save",
            json!({
                "page":"companies","name":"Regional companies",
                "view":json!({"columns":["name"],"lenses":{},"zoom":18,"since":null}).to_string(),
                "params":json!({"status":"active"}).to_string(),"audience":{"roles":[source_role]}
            }),
        )])
        .await?;
    let id = outcome["results"][0]["result"]["result"]["id"].clone();
    let pack = source
        .read(
            EXPORT,
            &json!({"labels":[],"custom_fields":[],"views":[{"page":"companies","id":id}]}),
        )
        .await?;
    check(
        "Export carries the view and portable audience key",
        pack["views"][0]["roles"] == json!(["name:Setup pack reviewers"]),
    )?;
    let mut request = json!({"pack":pack});
    let missing = target.read(PREVIEW, &request).await?;
    check(
        "A custom role needs an explicit mapping",
        missing["ready"] == false
            && missing["missing"]
                .as_array()
                .is_some_and(|issues| issues.len() == 1),
    )?;
    request["role_map"] = json!([{"from":"name:Setup pack reviewers","to":target_role}]);
    let ready = target.read(PREVIEW, &request).await?;
    check(
        "The review names the mapped audience",
        ready["ready"] == true
            && ready["expected_additions"]["views"][0]["role_ids"] == json!([target_role]),
    )?;

    let mut unavailable = request.clone();
    unavailable["pack"]["watchers"] = json!([{
        "name":"Unavailable watcher","watch":"unmounted.opened",
        "definition":{"do":[{"operation":"unmounted.route","input":{}}]}
    }]);
    let missing = target.read(PREVIEW, &unavailable).await?;
    check(
        "Missing watcher modules block the otherwise valid view",
        missing["ready"] == false
            && missing["missing"]
                .as_array()
                .is_some_and(|issues| issues.len() == 2)
            && target
                .plans()?
                .preview(target, &[apply(&unavailable, &missing)])
                .await?["valid"]
                == false,
    )?;
    let result = target.write(&[apply(&request, &ready)]).await?;
    let views = target
        .read("base.grid.view.list", &json!({"page":"companies"}))
        .await?;
    check(
        "The view uses the destination role and keeps its filter",
        result["results"][0]["result"]["result"]["views_added"] == 1
            && views["views"].as_array().is_some_and(|views| {
                views.iter().any(|view| {
                    view["name"] == "Regional companies"
                        && view["params"]["status"] == "active"
                        && view["audience"]["roles"] == json!([target_role])
                })
            }),
    )?;
    let repeat = target.read(PREVIEW, &request).await?;
    check(
        "A repeat role-mapped view import is empty",
        repeat["ready"] == true && repeat["additions"]["views"] == json!([]),
    )?;
    request["pack"]["views"][0]["params"]["status"] = json!("suspended");
    let collision = target.read(PREVIEW, &request).await?;
    check(
        "A same-name view with a different filter collides",
        collision["ready"] == false
            && collision["collisions"]
                .as_array()
                .is_some_and(|issues| issues.len() == 1),
    )
}

const OPENED: &str = "watcher_cases/cases.opened";

async fn watcher_draft(source: &Person<'_>, target: &Person<'_>) -> Result<(), Failure> {
    let field = json!({"record_type":"watcher_case","key":"pack_threshold","label":"Pack threshold","kind":"number","watcher_condition":true});
    source.write(&[op(DEFINE, field)]).await?;
    let draft = source
        .write(&[op(
            "base.watchers.draft",
            json!({
                "name":"Route high thresholds","watch":OPENED,
                "if":{"conditions":[{"field":"cf_pack_threshold","op":"gt","value":"100"},{"field":"title","op":"eq","value":"Urgent"}]},
                "do":[{"operation":"watcher_cases.cases.route","input":{"queue":"review"}}],
                "examples":[{"facts":"{\"cf_pack_threshold\":\"150\",\"title\":\"Urgent\"}","expect":"match"}],
                "explanation":"Source-only request"
            }),
        )])
        .await?;
    let draft_id = &draft["results"][0]["result"]["result"]["watcher_id"];
    let pack = source.read(EXPORT, &json!({
        "labels":[],"custom_fields":[{"record_type":"watcher_case","key":"pack_threshold"}],"watchers":[draft_id]
    })).await?;
    check(
        "Export contains the authored configuration without source evidence",
        pack["watchers"][0]["watch"] == OPENED
            && pack["watchers"][0].as_object().is_some_and(|draft| {
                draft.len() == 4
                    && draft.contains_key("definition")
                    && !draft.contains_key("examples")
            }),
    )?;
    let condition_path = "/if/conditions/1/value";
    let action_path = "/do/0/input/queue";
    let flags = &pack["watchers"][0]["warnings"];
    let has_path = |flags: &Value, path: &str| {
        flags.as_array().is_some_and(|flags| {
            flags
                .iter()
                .any(|flag| flag.as_str().is_some_and(|reason| reason.contains(path)))
        })
    };
    check(
        "Export flags the free-text condition and action parameter",
        has_path(flags, condition_path) && has_path(flags, action_path),
    )?;
    let without_examples = json!({"pack":pack});
    let incomplete = target.read(PREVIEW, &without_examples).await?;
    check(
        "New filtered drafts need destination examples",
        incomplete["ready"] == false,
    )?;
    let mut request = without_examples.clone();
    request["watcher_examples"] = json!({"Route high thresholds":[
        {"facts":{"cf_pack_threshold":"200","title":"Urgent"},"expect":"match"}
    ]});
    let preview = target.read(PREVIEW, &request).await?;
    check(
        "Preview resolves the watcher condition against a field in the same pack",
        preview["ready"] == true
            && preview["additions"]["watchers"]
                .as_array()
                .is_some_and(|entries| entries.len() == 1),
    )?;
    let mut edited = request.clone();
    edited["pack"]["watchers"][0]["warnings"] = json!([]);
    let edited_preview = target.read(PREVIEW, &edited).await?;
    let reasons = |preview: &Value| {
        Value::Array(
            preview["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|issue| issue["reason"].clone())
                .collect(),
        )
    };
    let warnings = reasons(&edited_preview);
    check(
        "Preview flags both literals even when source flags are removed",
        has_path(&warnings, condition_path)
            && has_path(&warnings, action_path)
            && edited_preview["additions"]["watchers"][0]
                .get("examples")
                .is_none(),
    )?;
    let outcome = target.write(&[apply(&request, &preview)]).await?;
    let watchers = target
        .read(
            "base.watchers.list",
            &json!({"search":"Route high thresholds"}),
        )
        .await?;
    let imported = &watchers["watchers"][0];
    check(
        "Import creates the field and an unapproved watcher with no running version",
        outcome["results"][0]["result"]["result"]["watchers_added"] == 1
            && imported["awaiting_approval"] == true
            && imported["running_version"].is_null(),
    )?;
    let repeat = target.read(PREVIEW, &without_examples).await?;
    check(
        "A repeat draft import is empty",
        repeat["ready"] == true && repeat["additions"]["watchers"] == json!([]),
    )?;
    let mut distinct = request.clone();
    distinct["pack"]["watchers"][0]["name"] = json!("Route High Thresholds");
    distinct["watcher_examples"]["Route High Thresholds"] =
        distinct["watcher_examples"]["Route high thresholds"].clone();
    distinct["watcher_examples"]
        .as_object_mut()
        .ok_or("missing examples")?
        .remove("Route high thresholds");
    let different_case = target.read(PREVIEW, &distinct).await?;
    check(
        "A differently capitalized watcher is a distinct addition",
        different_case["ready"] == true
            && different_case["additions"]["watchers"][0]["name"] == "Route High Thresholds",
    )?;
    let receipts = target.read("base.labels.receipts", &json!({})).await?;
    let receipt = receipts["receipts"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["changes"].as_array().into_iter().flatten())
        .find(|change| change["kind"] == "base/watchers.watcher")
        .ok_or("missing imported watcher receipt")?;
    check(
        "The import receipt offers discard and shows the rule without example facts",
        receipt["undo"]["operation"] == "base.watchers.discard_draft"
            && receipt["after"]["rule"]["conditions"][1]["field"] == "title"
            && receipt["effective"]["rule"] == receipt["after"]["rule"]
            && receipt["after"]["rule"]["example outcomes"] == json!(["match"])
            && receipt["after"]["rule"].get("examples").is_none(),
    )?;
    let mut collision = edited;
    collision["pack"]["watchers"][0]["definition"]["do"][0]["input"]["queue"] = json!("another");
    let preview = target.read(PREVIEW, &collision).await?;
    check(
        "A collision still warns about both incoming free-text literals",
        preview["ready"] == false
            && preview["collisions"]
                .as_array()
                .is_some_and(|issues| issues.len() == 1)
            && has_path(&reasons(&preview), condition_path)
            && has_path(&reasons(&preview), action_path),
    )?;
    collision["pack"]["watchers"][0]["watch"] = json!("unmounted.opened");
    let missing = target.read(PREVIEW, &collision).await?;
    check(
        "A missing trigger still warns about both incoming free-text literals",
        missing["ready"] == false
            && missing["missing"]
                .as_array()
                .is_some_and(|issues| issues.len() == 1)
            && has_path(&reasons(&missing), condition_path)
            && has_path(&reasons(&missing), action_path),
    )?;
    let discard = op(
        "base.watchers.discard_draft",
        json!({"watcher_id":imported["watcher_id"]}),
    );
    let set_read = |allowed| async move {
        let sql = target.db.get().await?;
        sql.execute(
            "UPDATE principal_capabilities SET is_allowed=$3 WHERE tenant_id=$1 AND user_id=$2 AND capability='admin.watchers.view'",
            &[&target.scope.tenant(), &target.scope.user(), &allowed],
        ).await?;
        Ok::<_, Failure>(())
    };
    set_read(false).await?;
    let refused = target
        .plans()?
        .preview(target, std::slice::from_ref(&discard))
        .await?;
    check(
        "A watcher manager denied read access cannot preview discard",
        refused["valid"] == false,
    )?;
    set_read(true).await?;
    check(
        "An initial imported draft can be discarded through its reviewed operation",
        target.write(&[discard]).await?["results"][0]["result"]["result"]["discarded"] == true,
    )
}

async fn refusals(target: &Person<'_>, mut request: Value) -> Result<(), Failure> {
    request["pack"]["labels"][0]["label"] = json!("A different label");
    request["pack"]["custom_fields"][0]["key"] = json!("new_stage");
    let collision = target.read(PREVIEW, &request).await?;
    check(
        "A collision is reported alongside an otherwise valid addition",
        collision["ready"] == false
            && collision["collisions"][0]["entry"] == format!("menu label {LABEL}")
            && collision["additions"]["custom_fields"][0]["key"] == "new_stage",
    )?;
    let plans = target.plans()?;
    let operations = [apply(&request, &collision)];
    check(
        "A collision refuses the entire pack at dry run and commit",
        plans.preview(target, &operations).await?["valid"] == false
            && matches!(
                plans.commit(&target.scope, target, &operations).await,
                Err(Error::PlanRejected(_))
            )
            && counts(target.db, target.scope.tenant()).await? == (1, 1, 1, 1),
    )?;

    request["pack"]["labels"][0] =
        json!({"kind":"menu","subject":OTHER_LABEL,"label":"Extra facts"});
    let mut unavailable = request.clone();
    unavailable["pack"]["custom_fields"][0]["record_type"] = json!("unmounted");
    unavailable["pack"]["custom_fields"] = json!([
        unavailable["pack"]["custom_fields"][0],
        {
            "record_type":"unmounted", "key":"related_item", "label":"Related item",
            "kind":"reference", "target":"unmounted_item", "watcher_condition":false
        }
    ]);
    unavailable["picklist_map"][0]["to"] = json!("unmounted.status");
    let missing = target.read(PREVIEW, &unavailable).await?;
    check(
        "The preview reports all missing dependencies for both fields",
        missing["ready"] == false
            && missing["missing"]
                == json!([
                    {"entry":"custom field unmounted.new_stage",
                     "reason":"Record type “unmounted” is unavailable."},
                    {"entry":"custom field unmounted.new_stage",
                     "reason":"Pick-list “unmounted.status” is unavailable. Choose a destination pick-list mapping."},
                    {"entry":"custom field unmounted.related_item",
                     "reason":"Record type “unmounted” is unavailable."},
                    {"entry":"custom field unmounted.related_item",
                     "reason":"Reference kind “unmounted_item” is unavailable."}
                ])
            && missing["additions"]["custom_fields"] == json!([]),
    )?;

    let ready = target.read(PREVIEW, &request).await?;
    let late_collision = [
        apply(&request, &ready),
        set_label(OTHER_LABEL, "A competing label"),
    ];
    check(
        "A later owner conflict rolls back both pack writes and their audit rows",
        plans.preview(target, &late_collision).await?["valid"] == true
            && matches!(
                plans.commit(&target.scope, target, &late_collision).await,
                Err(Error::Backend(bilibi_database::Error::Conflict))
            )
            && counts(target.db, target.scope.tenant()).await? == (1, 1, 1, 1),
    )?;
    Ok(())
}

async fn stale_review(target: &Person<'_>, request: &Value) -> Result<(), Failure> {
    let before = target.read(PREVIEW, request).await?;
    let reviewed = [apply(request, &before)];
    let labels = target
        .read("base.labels.list", &json!({"kind":"menu", "search":LABEL}))
        .await?;
    let version = labels
        .as_array()
        .and_then(|rows| rows.iter().find(|label| label["subject"] == LABEL))
        .map(|label| &label["version"])
        .ok_or("missing label version")?;
    target
        .write(&[op(
            "base.labels.clear",
            json!({"kind":"menu", "subject":LABEL, "version":version}),
        )])
        .await?;
    check(
        "An identical entry removed after review needs a fresh preview before apply",
        matches!(
            target
                .plans()?
                .commit(&target.scope, target, &reviewed)
                .await,
            Err(Error::PlanRejected(_))
        ) && counts(target.db, target.scope.tenant()).await? == (0, 1, 2, 1),
    )
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_SETUP_PACKS_DEMO").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_SETUP_PACKS_DEMO=disposable against a disposable migrated database".into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &std::env::var("APP_ORIGIN")?)?;
    // Fixture grants only. Both actors sign in through the ordinary boundary.
    db.get()
        .await?
        .batch_execute(
            "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed)
         SELECT 2,5,capability,true FROM unnest(ARRAY[
             'base.setup-packs.view','base.setup-packs.manage','base.labels.view',
             'base.labels.manage','base.labels.customisations.view','base.custom-fields.view','base.custom-fields.manage',
             'grid.views.share','admin.watchers.view','admin.watchers.manage','watcher-cases.case.view'
         ]) capability ON CONFLICT DO NOTHING",
        )
        .await?;
    let source = Person::login(&app, &db, "ai@agent.my").await?;
    let target = Person::login(&app, &db, "manager@other.example.test").await?;
    shared_views(&source, &target).await?;
    let vocabulary = source.read("base.watchers.vocabulary", &json!({})).await?;
    if vocabulary["triggers"]
        .as_array()
        .is_some_and(|triggers| triggers.iter().any(|trigger| trigger["trigger"] == OPENED))
    {
        return watcher_draft(&source, &target).await;
    }
    let request = round_trip(&source, &target).await?;
    let staff = Person::login(&app, &db, "staff@example.test").await?;
    check(
        "A caller without pack view capability is refused",
        staff.read(PREVIEW, &request).await.is_err(),
    )?;
    // Simulate a role with the pack capability but neither owner's grant.
    db.get()
        .await?
        .execute(
            "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed)
         VALUES (1,3,'base.setup-packs.view',true) ON CONFLICT DO NOTHING",
            &[],
        )
        .await?;
    check(
        "Pack view capability alone cannot read the owners' destination state",
        staff
            .read(
                PREVIEW,
                &json!({"pack":{"version":3,"labels":[],"custom_fields":[],"views":[],"watchers":[]}}),
            )
            .await?["ready"]
            == true
            && staff.read(PREVIEW, &request).await.is_err(),
    )?;
    refusals(&target, request.clone()).await?;
    stale_review(&target, &request).await
}
