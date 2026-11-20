//! Two-tenant customisations inventory and owner-operation walkthrough.
use bilibi_agent_api::page::{Call, PageContracts};
use bilibi_authz::grants::Grants;
use bilibi_base::session::session_id;
use bilibi_database::pool;
use bilibi_tenancy::{Authentication, Scope};
use bilibi_ui::page::{LiveInput, LiveRequest, PageRequest, PageService};
use serde_json::{json, Value};
use std::collections::HashMap;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

fn request<'a>(
    scope: &'a Scope,
    credential: &'a str,
    input: LiveInput<'a>,
) -> LiveRequest<'a, Scope> {
    LiveRequest {
        scope,
        credential,
        input,
    }
}

fn row_for<'a>(html: &'a str, label: &str) -> &'a str {
    html.split("</tr>")
        .filter_map(|part| part.rsplit_once("<tr"))
        .map(|(_, row)| row)
        .find(|row| row.contains(label))
        .expect("rendered inventory row")
}

fn confirmation_event(html: &str, event: &str) -> Value {
    let button = html
        .split("<button")
        .find(|button| button.starts_with(" id=\"customisations-confirm-confirm\""))
        .and_then(|button| button.split_once('>').map(|(attrs, _)| attrs))
        .expect("rendered confirm button");
    assert!(button.contains(&format!("data-event=\"{event}\"")));
    let key = button
        .split_once("data-value-key=\"")
        .expect("versioned confirmation key")
        .1
        .split('"')
        .next()
        .expect("confirmation key");
    json!({"type":event, "key":key})
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_CUSTOMISATIONS_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_CUSTOMISATIONS_DEMO=disposable on a disposable database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_web::modules()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35567")?;
    app.services
        .get::<Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let auth = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let contracts = app
        .services
        .get::<PageContracts>(bilibi_agent_api::page::SERVICE)?;
    let pages = app
        .services
        .get::<PageService<Scope, bilibi_database::Error>>("base/labels.customisations")?;
    let token1 = uuid::Uuid::new_v4().simple().to_string();
    let token2 = uuid::Uuid::new_v4().simple().to_string();
    let (one, two) = {
        let client = db.get().await?;
        client.execute("INSERT INTO principal_roles(tenant_id,user_id,role_id) SELECT 2,5,id FROM roles WHERE tenant_id=2 AND code='tenant_owner' ON CONFLICT DO NOTHING", &[]).await?;
        for (tenant, user, token) in [(1_i64, 4_i64, &token1), (2, 5, &token2)] {
            client.execute("INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at) VALUES($1,$2,$3,$4,now()+interval '1 hour')", &[&session_id(token), &tenant, &user, &uuid::Uuid::new_v4().simple().to_string()]).await?;
        }
        (
            auth.authenticate(&**client, 1, 4, String::new()).await?,
            auth.authenticate(&**client, 2, 5, String::new()).await?,
        )
    };
    {
        let client = db.get().await?;
        let grants = bilibi_authz::effective_capabilities(&**client, &two).await?;
        assert!(grants.contains("base.labels.customisations.view"));
        assert!(grants.contains("base.custom-fields.manage"));
        assert!(!grants.contains("admin.audit.log.list"));
    }
    // The setup guide must reflect the live registry and configured plan limit.
    {
        let client = db.get().await?;
        let registry = app
            .services
            .get::<bilibi_agent_api::Registry>("base/agent_api.registry")?;
        let guide = registry
            .guide(&**client, &one, "base.labels.customise")
            .await?;
        let body = guide["body"].as_str().expect("guide body");
        let described = registry
            .describe(&**client, &one, "base.custom_fields.define")
            .await?;
        let settings = app
            .services
            .get::<bilibi_base::settings::Settings>("base/settings.store")?;
        let limit = settings
            .get_global(&**client, "agent_api.plan_max_operations")
            .await?;
        assert!(body.contains(&described["input"].to_string()));
        assert!(body.contains(&format!("one atomic plan: {limit}")));
        assert!(!body.contains("{{contract:") && !body.contains("{{plan_limit}}"));
        assert!(body.contains("uncertain") && body.contains("watchers"));
    }
    let label_call = |scope, credential, contract| Call {
        scope,
        credential,
        service: "base/labels.pages",
        event: "save_field",
        contract,
    };
    for (scope, credential, label) in [
        (&one, token1.as_str(), "Tenant one word"),
        (&two, token2.as_str(), "Tenant two word"),
    ] {
        contracts
            .submit(
                &label_call(scope, credential, "base.labels.set"),
                json!({"kind":"record", "subject":"companies", "label":label, "version":"0:0"}),
            )
            .await?;
    }
    contracts.submit(
        &label_call(&one, &token1, "base.labels.set"),
        json!({"kind":"menu", "subject":"admin.labels", "label":"Second override", "version":"0:0"}),
    ).await?;
    contracts.submit(&Call { scope: &one, credential: &token1, service: "base/custom_fields.pages", event: "create", contract: "base.custom_fields.define" }, json!({"record_type":"company", "key":"walkthrough_field", "label":"Tenant one field", "kind":"text"})).await?;
    let path = HashMap::new();
    let query = HashMap::new();
    let show = |scope| PageRequest {
        operation: "index",
        scope: Some(scope),
        path: &path,
        query: &query,
        body: &[],
        framed: false,
    };
    let a = pages.0.render(show(&one)).await?.html;
    let b = pages.0.render(show(&two)).await?.html;
    assert!(
        a.contains("Tenant one word")
            && a.contains("Tenant one field")
            && !a.contains("Tenant two word")
    );
    let attributed = row_for(&a, "Tenant one word");
    assert!(attributed.contains("Alex Admin") && attributed.contains("<time"));
    {
        db.get().await?.execute("INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES(1,4,'admin.audit.log.list',false)", &[]).await?;
        let denied = pages.0.render(show(&one)).await?.html;
        let restricted = row_for(&denied, "Tenant one word");
        assert!(
            restricted.contains("Restricted")
                && !restricted.contains("Alex Admin")
                && restricted.contains("<time")
        );
        db.get().await?.execute("DELETE FROM principal_capabilities WHERE tenant_id=1 AND user_id=4 AND capability='admin.audit.log.list'", &[]).await?;
    }
    assert!(b.contains("<time") && b.contains("Restricted"));
    assert!(
        b.contains("Tenant two word")
            && !b.contains("Tenant one word")
            && !b.contains("Tenant one field")
    );
    {
        db.get().await?.execute("INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES(1,4,'base.custom-fields.view',false)", &[]).await?;
        let restricted = pages.0.render(show(&one)).await?.html;
        assert!(restricted.contains("Tenant one word"));
        assert!(restricted.contains("You do not have access to these sections"));
        assert!(
            restricted.contains("Archived custom fields")
                && !restricted.contains("Tenant one field")
        );
        db.get().await?.execute("DELETE FROM principal_capabilities WHERE tenant_id=1 AND user_id=4 AND capability='base.custom-fields.view'", &[]).await?;
    }
    let mut live = pages
        .0
        .mount(show(&one))
        .await?
        .ok_or("missing live page")?;
    live.update(request(&one, &token1, LiveInput::Refresh))
        .await?;
    live.update(request(
        &one,
        &token1,
        LiveInput::Event(&json!({"type":"grid","op":"remove","spec":"actor"})),
    ))
    .await?;
    live.update(request(
        &one,
        &token1,
        LiveInput::Event(&json!({"type":"grid","op":"suggest","add":"Changed by"})),
    ))
    .await?;
    let refreshed = live
        .update(request(&one, &token1, LiveInput::Refresh))
        .await?
        .html;
    assert!(refreshed.contains("customisations-table-suggest-actor"));
    let labels = contracts.read(&one, "base.labels.list", &json!({})).await?;
    let label_rows = labels["result"].as_array().ok_or("missing labels")?;
    let mut shown = Vec::new();
    for (subject, word) in [
        ("companies", "Tenant one word"),
        ("admin.labels", "Second override"),
    ] {
        let label = label_rows
            .iter()
            .find(|row| row["subject"] == subject)
            .ok_or("missing label")?;
        let token = label["version"].as_str().ok_or("missing label version")?;
        let (id, revision) = token.split_once(':').ok_or("invalid label version")?;
        let open = json!({"type":"request_undo","key":format!("label:{id}@{revision}")});
        let dialog = live
            .update(request(&one, &token1, LiveInput::Event(&open)))
            .await?
            .html;
        assert!(dialog.contains(&format!("→ {word}?")));
        shown.push(confirmation_event(&dialog, "customisation.labels"));
    }
    let stale = live
        .update(request(&one, &token1, LiveInput::Event(&shown[0])))
        .await?
        .html;
    assert!(stale.contains("Tenant one word") && stale.contains("Second override"));
    let cleared = live
        .update(request(&one, &token1, LiveInput::Event(&shown[1])))
        .await?
        .html;
    assert!(!cleared.contains("Second override") && cleared.contains("Tenant one word"));
    let fields = contracts
        .read(&one, "base.custom_fields.list", &json!({}))
        .await?;
    let field = fields["result"]
        .as_array()
        .ok_or("missing fields")?
        .iter()
        .find(|row| row["key"] == "walkthrough_field")
        .ok_or("missing field")?;
    let archive = json!({"type":"request_undo", "key":format!("custom_field:{}@{}", field["id"], field["version"])});
    let confirmation = live
        .update(request(&one, &token1, LiveInput::Event(&archive)))
        .await?
        .html;
    assert!(confirmation.contains("Approved rules that use this field may stop running"));
    let commit = confirmation_event(&confirmation, "customisation.fields");
    contracts.submit(
        &Call { scope: &one, credential: &token1, service: "base/custom_fields.pages", event: "save_field", contract: "base.custom_fields.update" },
        json!({"record_type":"company","key":"walkthrough_field","label":"Renamed field","version":field["version"]}),
    ).await?;
    let rejected = live
        .update(request(&one, &token1, LiveInput::Event(&commit)))
        .await?
        .html;
    assert!(rejected.contains("the field changed; reload before saving"));
    assert!(!rejected.contains("Archived custom fields") && rejected.contains("Renamed field"));
    let fields = contracts
        .read(&one, "base.custom_fields.list", &json!({}))
        .await?;
    let field = fields["result"]
        .as_array()
        .ok_or("missing fields")?
        .iter()
        .find(|row| row["key"] == "walkthrough_field")
        .ok_or("missing field")?;
    let archive = json!({"type":"request_undo","key":format!("custom_field:{}@{}", field["id"], field["version"])});
    let confirmation = live
        .update(request(&one, &token1, LiveInput::Event(&archive)))
        .await?
        .html;
    let commit = confirmation_event(&confirmation, "customisation.fields");
    let after = live
        .update(request(&one, &token1, LiveInput::Event(&commit)))
        .await?
        .html;
    assert!(after.contains("Archived custom fields") && after.contains("Renamed field"));
    let other = pages.0.render(show(&two)).await?.html;
    assert!(other.contains("Tenant two word"));
    {
        let client = db.get().await?;
        client.execute("INSERT INTO base_settings(key, tenant_id, scope_type, scope_id, value, is_encrypted) VALUES('base.custom_fields.max_fields',1,'tenant',1,'1',false)", &[]).await?;
    }
    contracts.submit(&Call { scope: &one, credential: &token1, service: "base/custom_fields.pages", event: "create", contract: "base.custom_fields.define" }, json!({"record_type":"company", "key":"active_field", "label":"Active field", "kind":"text"})).await?;
    let fields = contracts
        .read(
            &one,
            "base.custom_fields.list",
            &json!({"include_archived":true}),
        )
        .await?;
    let archived = fields["result"]
        .as_array()
        .ok_or("missing fields")?
        .iter()
        .find(|row| row["key"] == "walkthrough_field")
        .ok_or("missing archived field")?;
    let restore = json!({"type":"request_undo", "key":format!("custom_field:{}@{}", archived["id"], archived["version"])});
    live.update(request(&one, &token1, LiveInput::Refresh))
        .await?;
    let confirmation = live
        .update(request(&one, &token1, LiveInput::Event(&restore)))
        .await?
        .html;
    assert!(confirmation.contains("Rules already marked for review still need reapproval"));
    let refused = live
        .update(request(
            &one,
            &token1,
            LiveInput::Event(&confirmation_event(
                &confirmation,
                "customisation.fields_restore",
            )),
        ))
        .await?
        .html;
    assert!(
        refused.contains("archive one first") && !refused.contains("customisations-confirm-title")
    );
    println!(
        "PASS: tenant isolation, owner undo operations, section denials, picker refresh, revision-bound confirmations, actor disclosure and owner refusals"
    );
    Ok(())
}
