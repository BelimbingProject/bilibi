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

/// "What can I change here?" through the read contract, for both tenants.
async fn discovery(
    db: &bilibi_database::Db,
    contracts: &PageContracts,
    one: &Scope,
    two: &Scope,
    credential: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    const KEY: &str = "base.labels.customisations.discover";
    let section_ids = |answer: &Value| -> Vec<String> {
        answer["result"]["sections"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|section| section["section"].as_str().map(str::to_owned))
            .collect()
    };
    let choice = |answer: &Value, subject: &str| -> Option<Value> {
        answer["result"]["choices"]
            .as_array()?
            .iter()
            .find(|choice| choice["subject"] == subject)
            .cloned()
    };

    // Every section answers, on one bounded page, with choices nobody changed.
    let all = contracts.read(one, KEY, &json!({})).await?;
    let sections = section_ids(&all);
    for id in [
        "base/labels.customisations",
        "base/custom_fields.customisations",
        "base/grid.customisations",
        "base/menu_order.customisations",
    ] {
        assert!(sections.iter().any(|section| section == id), "{id}");
    }
    let page = &all["meta"]["page"];
    assert_eq!(page["page_size"], 25);
    assert_eq!(all["result"]["choices"].as_array().map(Vec::len), Some(25));
    assert!(page["total"].as_u64().is_some_and(|total| total > 25));
    let last = contracts
        .read(one, KEY, &json!({"page": 1000000, "page_size": 100}))
        .await?;
    assert_eq!(last["result"]["choices"], json!([]));
    assert!(contracts
        .read(one, KEY, &json!({"page_size": 101}))
        .await
        .is_err());

    // By record kind: the label override, the custom field and the list's views.
    let company = contracts
        .read(
            one,
            KEY,
            &json!({"record_kind": "company", "page_size": 100}),
        )
        .await?;
    let word = choice(&company, "record:companies").ok_or("missing record label")?;
    assert_eq!(word["value"], "Tenant one word");
    assert_eq!(word["default"], "Companies");
    assert_eq!(word["source"], "tenant");
    assert_eq!(word["customised"], true);
    assert_eq!(word["target"], "tenant");
    let field = choice(&company, "company.walkthrough_field").ok_or("missing field")?;
    assert_eq!(field["value"]["label"], "Tenant one field");
    assert_eq!(field["value"]["kind"], "reference");
    assert_eq!(field["value"]["target"], "company");
    assert_eq!(field["value"]["position"], 10);
    assert_eq!(field["source"], "tenant");
    let add = choice(&company, "company").ok_or("missing record type")?;
    assert_eq!(add["value"], json!(["walkthrough_field"]));
    assert_eq!(add["limits"]["max_fields"], 50);
    assert_eq!(add["limits"]["active_fields"], 1);
    let views = choice(&company, "companies").ok_or("missing list views")?;
    assert_eq!(views["section"], "base/grid.customisations");
    assert_eq!(views["customised"], false);
    assert_eq!(views["value"], json!([]));
    assert!(choice(&company, "admin.labels").is_none());
    let labels = company["result"]["sections"]
        .as_array()
        .and_then(|sections| {
            sections
                .iter()
                .find(|section| section["section"] == "base/labels.customisations")
        })
        .ok_or("missing labels section")?;
    assert_eq!(labels["operations"]["change"][0]["key"], "base.labels.set");
    assert_eq!(labels["operations"]["change"][0]["available"], true);
    assert_eq!(labels["operations"]["restore"]["key"], "base.labels.clear");
    assert_eq!(labels["operations"]["verify"]["key"], "base.labels.list");
    assert!(labels["limits"]
        .as_str()
        .is_some_and(|limits| limits.contains("60")));

    let company_page = contracts
        .read(one, KEY, &json!({"on_page": "/companies"}))
        .await?;
    for subject in [
        "record:companies",
        "field:companies.name",
        "company",
        "company.walkthrough_field",
        "companies",
    ] {
        let found = choice(&company_page, subject)
            .unwrap_or_else(|| panic!("{subject} missing on /companies"));
        assert_eq!(found["on_page"], "/companies");
    }

    for (page, subjects) in [
        (
            "/documents",
            &[
                "record:documents",
                "field:documents.name",
                "field:document_revisions.label",
            ][..],
        ),
        (
            "/activities",
            &["record:activities", "field:activities.subject"][..],
        ),
        ("/geonames/countries", &["field:countries.country"][..]),
        ("/work-calendars", &["field:work_calendars.timezone"][..]),
        (
            "/companies/legal-entity-types",
            &["field:legal_entity_types.name"][..],
        ),
    ] {
        let listed = contracts
            .read(one, KEY, &json!({"on_page":page,"page_size":100}))
            .await?;
        for subject in subjects {
            assert!(
                choice(&listed, subject).is_some(),
                "{subject} missing on {page}"
            );
        }
        assert!(!section_ids(&listed)
            .iter()
            .any(|section| section == "base/grid.customisations"));
    }
    let columns = contracts
        .read(
            one,
            KEY,
            &json!({"on_page":"/companies","purpose":"rename a column"}),
        )
        .await?;
    assert!(choice(&columns, "field:companies.name").is_some());
    assert!(choice(&columns, "record:companies").is_none());
    assert!(columns["result"]["choices"]
        .as_array()
        .ok_or("missing column choices")?
        .iter()
        .all(|choice| !choice["subject"]
            .as_str()
            .unwrap_or_default()
            .starts_with("menu:")));
    let add_fields = contracts
        .read(
            one,
            KEY,
            &json!({"on_page":"/companies","purpose":"add field"}),
        )
        .await?;
    assert_eq!(add_fields["meta"]["page"]["total"], 1);
    assert_eq!(add_fields["result"]["choices"][0]["subject"], "company");

    for filters in [
        json!({"on_page":"/admin/labels", "record_kind":"company"}),
        json!({"record_kind":"Company"}),
        json!({"on_page":"?page=2"}),
        json!({"on_page":"/admin/labels/?page=2"}),
        json!({"on_page":"/admin/labels/"}),
        json!({"on_page":"/admin/labels#top"}),
        json!({"on_page":""}),
    ] {
        assert_eq!(
            contracts.read(one, KEY, &filters).await?["result"]["choices"],
            json!([])
        );
    }
    // By page, and by purpose.
    let here = contracts
        .read(one, KEY, &json!({"on_page": "/admin/labels"}))
        .await?;
    assert_eq!(here["meta"]["page"]["total"], 1);
    assert_eq!(here["result"]["choices"][0]["value"], "Second override");
    assert_eq!(here["result"]["choices"][0]["on_page"], "/admin/labels");
    let override_choice = &here["result"]["choices"][0];
    let token = override_choice["version"]
        .as_str()
        .ok_or("missing label version")?;
    assert_ne!(token, "0:0");
    assert_eq!(override_choice["input"]["version"], token);
    let clear = Call {
        scope: one,
        credential,
        service: "base/labels.customisations",
        event: "customisation.labels",
        contract: "base.labels.clear",
    };
    let input = override_choice["input"].clone();
    assert_eq!(
        contracts.preview(&clear, input.clone()).await?["valid"],
        true
    );
    contracts.commit(&clear, input).await?;
    let restored = contracts
        .read(one, KEY, &json!({"on_page": "/admin/labels"}))
        .await?;
    let default = &restored["result"]["choices"][0];
    assert_eq!(default["value"], "Labels");
    assert_eq!(default["source"], "default");
    assert_eq!(default["version"], "0:0");
    let mut input = default["input"].clone();
    input["label"] = json!("Second override");
    contracts
        .submit(
            &Call {
                scope: one,
                credential,
                service: "base/labels.pages",
                event: "save_field",
                contract: "base.labels.set",
            },
            input,
        )
        .await?;
    let order = contracts
        .read(one, KEY, &json!({"purpose": "Sidebar navigation"}))
        .await?;
    assert_eq!(section_ids(&order), ["base/menu_order.customisations"]);
    let preview = &order["result"]["sections"][0]["operations"]["preview"];
    assert_eq!(preview["key"], "base.menu_order.preview");

    let theirs = contracts
        .read(two, KEY, &json!({"on_page": "/companies"}))
        .await?;
    assert_eq!(
        choice(&theirs, "record:companies").ok_or("missing tenant two label")?["value"],
        "Tenant two word"
    );
    assert!(choice(&theirs, "company.walkthrough_field").is_none());
    db.get().await?.execute("INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES(1,4,'base.custom-fields.view',false)", &[]).await?;
    let denied = contracts
        .read(one, KEY, &json!({"on_page": "/companies"}))
        .await?;
    assert!(choice(&denied, "company.walkthrough_field").is_none());
    assert!(choice(&denied, "record:companies").is_some());
    assert!(!section_ids(&denied)
        .iter()
        .any(|section| section == "base/custom_fields.customisations"));
    let logged: i64 = db.get().await?.query_one("SELECT count(*) FROM authz_decision_log WHERE tenant_id=1 AND user_id=4 AND capability='base.custom-fields.view' AND NOT allowed", &[]).await?.get(0);
    assert_eq!(logged, 0);
    db.get().await?.execute("DELETE FROM principal_capabilities WHERE tenant_id=1 AND user_id=4 AND capability='base.custom-fields.view'", &[]).await?;
    Ok(())
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
    contracts.submit(
        &label_call(&one, &token1, "base.labels.set"),
        json!({"kind":"menu", "subject":"admin.labels", "label":"Second override", "version":"0:0"}),
    ).await?;
    contracts.submit(&Call { scope: &one, credential: &token1, service: "base/custom_fields.pages", event: "create", contract: "base.custom_fields.define" }, json!({"record_type":"company", "key":"walkthrough_field", "label":"Tenant one field", "kind":"reference", "target":"company"})).await?;
    discovery(&db, &contracts, &one, &two, &token1).await?;
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
    assert!(confirmation.contains("Watchers already marked for review still need reapproval"));
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
