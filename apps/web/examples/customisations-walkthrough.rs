//! Two-tenant customisations inventory and owner-operation walkthrough.
use bilibi_agent_api::page::{Call, PageContracts};
use bilibi_authz::grants::Grants;
use bilibi_base::session::session_id;
use bilibi_database::pool;
use bilibi_tenancy::{Authentication, Scope};
use bilibi_ui::page::{LiveInput, LiveRequest, PageRequest, PageService};
use serde_json::json;
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
    assert!(a.contains("<time") && !a.contains("Unavailable"));
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
    let label = labels["result"]
        .as_array()
        .ok_or("missing labels")?
        .iter()
        .find(|row| row["subject"] == "companies" && row["kind"] == "record")
        .ok_or("missing company label")?;
    contracts
        .submit(
            &Call {
                scope: &one,
                credential: &token1,
                service: "base/labels.customisations",
                event: "customisation.labels",
                contract: "base.labels.clear",
            },
            json!({"kind":"record", "subject":"companies", "version":label["version"]}),
        )
        .await?;
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
    let after = live
        .update(request(
            &one,
            &token1,
            LiveInput::Event(&json!({"type":"customisation.fields"})),
        ))
        .await?
        .html;
    assert!(
        !after.contains("Tenant one word")
            && after.contains("Archived custom fields")
            && after.contains("Tenant one field")
    );
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
            LiveInput::Event(&json!({"type":"customisation.fields_restore"})),
        ))
        .await?
        .html;
    assert!(
        refused.contains("archive one first") && !refused.contains("customisations-confirm-title")
    );
    println!(
        "PASS: tenant isolation, owner undo operations, section denials, picker refresh, rule consequences and owner refusals"
    );
    Ok(())
}
