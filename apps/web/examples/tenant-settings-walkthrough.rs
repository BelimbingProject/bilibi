//! Real tenant-setting operation walkthrough on a disposable migrated database.
//! It proves explicit delegation, source reporting, reviewed set/restore,
//! secret redaction, mutation audit and isolation from a second tenant.
use bilibi_agent_api::{
    mcp::{self, Outcome, Session},
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
    Registry, Reviews,
};
use bilibi_authz::{grants::Grants, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_base::settings::{
    Definition, Editor, ScopeKind, Setting, TenantAdmin, ValueType, DEFINITIONS, TENANT_ADMIN,
};
use bilibi_base::settings_editor::{TENANT_MANAGE, TENANT_VIEW};
use bilibi_database::{pool, Error};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance};
use bilibi_tenancy::ProofFuture;
use serde_json::{json, Value};
use tokio_postgres::Transaction;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const PLAIN: &str = "walkthrough.tenant_settings.plain";
const SECRET: &str = "walkthrough.tenant_settings.secret";
const SECRET_TEXT: &str = "walkthrough-secret-must-not-appear";
const SUBMITTED_SECRET: &str = "submitted-secret-must-not-appear";

macro_rules! provenance {
    () => {
        Provenance {
            module: "extensions/tenant_settings_reference",
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! {
    Module { id: "extensions/tenant_settings_reference", layer: Layer::Extensions,
        dependencies: &["base/authz", "base/settings", "base/settings_editor"], migrations: &[], services: &[] }
}
inventory::submit! { Contribution::new(VOCABULARY, "tenant_settings_reference.roles", provenance!(), &VocabularyContribution {
    domains: &[], verbs: &[], capabilities: &[],
    roles: &[RoleDefinition { code: "manager", name: "manager", description: "Extension of the fixture manager role", grant_all: false, capabilities: &[TENANT_VIEW, TENANT_MANAGE] }],
    system_principals: &[],
}) }
inventory::submit! { Contribution::new(DEFINITIONS, PLAIN, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: &[ScopeKind::Global, ScopeKind::Tenant], default: || json!("inherited"),
    nullable: false, encrypted: false, minimum: None, maximum: None, validator: None,
    editor: Some(Editor { group: "Walkthrough", label: "Plain tenant setting", help: "A walkthrough value delegated to tenant administrators.", capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(TENANT_ADMIN, PLAIN, provenance!(), &TenantAdmin) }
inventory::submit! { Contribution::new(DEFINITIONS, SECRET, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: &[ScopeKind::Tenant], default: || json!(SECRET_TEXT),
    nullable: false, encrypted: true, minimum: None, maximum: None, validator: None,
    editor: Some(Editor { group: "Walkthrough", label: "Secret tenant setting", help: "A walkthrough secret that must remain masked.", capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(TENANT_ADMIN, SECRET, provenance!(), &TenantAdmin) }

struct Browser<'a> {
    app: &'a bilibi_web::App,
    token: &'a str,
}
impl PlanAuthority for Browser<'_> {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            let scope = self.app.security.sessions.hold(tx, self.token).await?;
            Ok(PlanIdentity {
                scope,
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}

fn setting(key: &str, value: &str) -> PlannedOperation {
    PlannedOperation {
        key: "base.settings_editor.tenant.set".into(),
        input: json!({"key":key,"value":value}),
    }
}

fn restore(key: &str) -> PlannedOperation {
    PlannedOperation {
        key: "base.settings_editor.tenant.restore".into(),
        input: json!({"key":key}),
    }
}

async fn list(
    registry: &Registry,
    client: &mut tokio_postgres::Client,
    scope: &bilibi_tenancy::Scope,
) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(registry
        .call(
            client,
            scope,
            "base.settings_editor.tenant.list",
            &json!({}),
        )
        .await?)
}

fn prove_masked(value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let text = value.to_string();
    if text.contains(SECRET_TEXT) {
        return Err("tenant setting list revealed a secret".into());
    }
    let secret = value["result"]["settings"]
        .as_array()
        .and_then(|settings| settings.iter().find(|setting| setting["key"] == SECRET))
        .ok_or("delegated secret missing from list")?;
    if secret.get("value") != Some(&Value::Null)
        || secret["secret"] != true
        || secret["secret_is_set"] != true
    {
        return Err("delegated secret was not masked".into());
    }
    Ok(())
}

fn prove_secret_refusal(
    response: &Value,
    fields: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let text = response.to_string();
    if text.contains(SUBMITTED_SECRET)
        || text.contains(SECRET_TEXT)
        || !fields["key"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message
                    .as_str()
                    .is_some_and(|message| message.contains("/system/settings"))
            })
        })
    {
        return Err(
            "encrypted Set must refuse the value and direct the person to System Settings".into(),
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_TENANT_SETTINGS_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_TENANT_SETTINGS_DEMO=disposable and DATABASE_URL to a disposable migrated database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35579")?;
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let admin_login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let other_login = app
        .user
        .login(
            "manager@other.example.test",
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await?;
    let admin = app.security.guard(&admin_login.token).await?;
    let other = app.security.guard(&other_login.token).await?;
    let admin_browser = Browser {
        app: &app,
        token: &admin_login.token,
    };
    let before = {
        let mut client = db.get().await?;
        list(&registry, &mut client, &admin).await?
    };
    prove_masked(&before)?;
    println!("tenant 1 before: {before}");

    let secret_operations = [setting(SECRET, SUBMITTED_SECRET)];
    let secret_preview = plans.preview(&admin_browser, &secret_operations).await?;
    if secret_preview["valid"] != false {
        return Err("dry run accepted an encrypted Set".into());
    }
    prove_secret_refusal(
        &secret_preview,
        &secret_preview["results"][0]["error"]["fields"],
    )?;
    match plans
        .commit(&admin, &admin_browser, &secret_operations)
        .await
    {
        Err(bilibi_agent_api::Error::PlanRejected(results)) => {
            prove_secret_refusal(&results, &results[0]["error"]["fields"])?;
        }
        _ => return Err("commit did not refuse the encrypted Set during validation".into()),
    }
    let reviews = app.services.get::<Reviews>("base/agent_api.reviews")?;
    let step_up = app
        .services
        .get::<bilibi_step_up::StepUp>(bilibi_step_up::SERVICE)?;
    for arguments in [
        json!({"key":secret_operations[0].key,"entries":[secret_operations[0].input]}),
        json!({"operations":secret_operations}),
    ] {
        let message = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"review_entries","arguments":arguments}});
        let Outcome::Reply(reply) = mcp::dispatch(
            &registry,
            &plans,
            Some(Session {
                db: &db,
                scope: &admin,
                authority: &admin_browser,
                step_up: &step_up,
                reviews: &reviews,
            }),
            &message,
        )
        .await
        else {
            return Err("review opener returned no tool reply".into());
        };
        if reply["result"]["isError"] != true {
            return Err("review opener accepted an encrypted Set".into());
        }
        prove_secret_refusal(&reply, &reply["result"]["structuredContent"]["fields"])?;
    }
    println!("encrypted Set is refused without reflecting its value");

    let matching_default = [setting(PLAIN, "inherited")];
    let preview = plans.preview(&admin_browser, &matching_default).await?;
    let change = &preview["results"][0]["preview"]["result"];
    if preview["valid"] != true
        || change["source_before"] != "default"
        || change["source_after"] != "tenant"
        || change["will_change"] != true
    {
        return Err("setting the inherited value must preview a new override".into());
    }
    plans
        .commit(&admin, &admin_browser, &matching_default)
        .await?;
    {
        let mut client = db.get().await?;
        let listed = list(&registry, &mut client, &admin).await?;
        let plain = listed["result"]["settings"]
            .as_array()
            .and_then(|settings| settings.iter().find(|setting| setting["key"] == PLAIN))
            .ok_or("delegated plain setting missing from list")?;
        if plain["value"] != "inherited"
            || plain["source"] != "tenant"
            || plain["set_for_tenant"] != true
        {
            return Err("setting the inherited value must persist a tenant override".into());
        }
    }

    let operations = [setting(PLAIN, "tenant-one")];
    let preview = plans.preview(&admin_browser, &operations).await?;
    println!("set review: {preview}");
    let set = plans.commit(&admin, &admin_browser, &operations).await?;
    println!("set result: {set}");

    let tenant_one = {
        let mut client = db.get().await?;
        list(&registry, &mut client, &admin).await?
    };
    let tenant_two = {
        let mut client = db.get().await?;
        list(&registry, &mut client, &other).await?
    };
    prove_masked(&tenant_one)?;
    prove_masked(&tenant_two)?;
    if tenant_one["result"]["settings"] == tenant_two["result"]["settings"] {
        return Err("tenant setting override was not isolated".into());
    }
    println!("tenant 1 after set: {tenant_one}");
    println!("tenant 2 remains inherited: {tenant_two}");

    let operations = [restore(PLAIN)];
    let preview = plans.preview(&admin_browser, &operations).await?;
    println!("restore review: {preview}");
    let restored = plans.commit(&admin, &admin_browser, &operations).await?;
    println!("restore result: {restored}");

    let client = db.get().await?;
    let row = client
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type='base/settings'",
            &[&admin.tenant()],
        )
        .await?;
    let audit_count: i64 = row.get(0);
    if audit_count < 2 {
        return Err(Error::Internal.into());
    }
    println!("settings mutation audit rows for tenant 1: {audit_count}");
    println!("walkthrough complete; no secret value was printed");
    Ok(())
}
