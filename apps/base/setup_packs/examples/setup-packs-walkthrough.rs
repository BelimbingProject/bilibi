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
        check(
            "The operation plan passes its dry run",
            preview["valid"] == true,
        )?;
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
        "Tenant A exports the definition and override in version 1",
        pack == json!({
            "version":1,
            "labels":[{"kind":"menu","subject":LABEL,"label":"Vocabulary"}],
            "custom_fields":[field]
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
            == json!({"labels_added":1,"custom_fields_added":1})
            && counts(target.db, target.scope.tenant()).await? == (1, 1, 1, 1)
            && counts(source.db, source.scope.tenant()).await? == (1, 1, 1, 1),
    )?;
    let repeat = target.read(PREVIEW, &request).await?;
    check(
        "A repeat import shows an empty difference",
        repeat
            == json!({
                "ready":true,"additions":{"labels":[],"custom_fields":[]},
                "expected_additions":{"labels":[],"custom_fields":[]},
                "collisions":[],"missing":[],"invalid":[]
            }),
    )?;
    let outcome = target.write(&[write]).await?;
    check(
        "Reapplying the original reviewed request writes no rows or audit changes",
        outcome["results"][0]["result"]["result"]
            == json!({"labels_added":0,"custom_fields_added":0})
            && counts(target.db, target.scope.tenant()).await? == (1, 1, 1, 1),
    )?;
    Ok(request)
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
             'base.labels.manage','base.custom-fields.view','base.custom-fields.manage'
         ]) capability ON CONFLICT DO NOTHING",
        )
        .await?;
    let source = Person::login(&app, &db, "ai@agent.my").await?;
    let target = Person::login(&app, &db, "manager@other.example.test").await?;
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
                &json!({"pack":{"version":1,"labels":[],"custom_fields":[]}}),
            )
            .await?["ready"]
            == true
            && staff.read(PREVIEW, &request).await.is_err(),
    )?;
    refusals(&target, request.clone()).await?;
    stale_review(&target, &request).await
}
