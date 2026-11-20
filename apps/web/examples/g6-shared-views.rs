use bilibi_agent_api::{
    page::{Call, PageContracts},
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
};
use bilibi_authz::grants::Grants;
use bilibi_base::session::session_id;
use bilibi_database::pool;
use bilibi_grid::{PageColumns, PageViews, View, ViewCaller, PAGE_VIEWS, SHARE_VIEWS};
use bilibi_tenancy::{Authentication, ProofFuture, Scope};
use bilibi_ui::{BuiltinColumn, ListSpec, ListState};
use serde_json::json;
use std::collections::HashMap;
use tokio_postgres::Transaction;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

struct HeldAgent<'a>(&'a Scope);

impl PlanAuthority for HeldAgent<'_> {
    fn verify<'a>(&'a self, _tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            Ok(PlanIdentity {
                scope: self.0.clone(),
                credential_binding: "walkthrough:grid-views".into(),
            })
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_GRID_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_GRID_DEMO=disposable on a disposable migrated database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    assert_eq!(db.status().max_size, 1, "run with DATABASE_POOL_SIZE=1");
    bilibi_database::migrate(&db, &bilibi_web::modules()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35567")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let auth = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let views = app.services.get::<PageViews>(PAGE_VIEWS)?;
    let contracts = app
        .services
        .get::<PageContracts>(bilibi_agent_api::page::SERVICE)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let second_token = uuid::Uuid::new_v4().simple().to_string();
    let reader_token = uuid::Uuid::new_v4().simple().to_string();
    let role;
    let admin;
    let second_admin;
    let reader;
    {
        let client = db.get().await?;
        admin = auth.authenticate(&**client, 1, 4, String::new()).await?;
        second_admin = auth.authenticate(&**client, 1, 6, String::new()).await?;
        reader = auth.authenticate(&**client, 1, 1, String::new()).await?;
        client.execute(
            "INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at) VALUES($1,1,4,$2,now()+interval '1 hour')",
            &[&session_id(&token), &uuid::Uuid::new_v4().simple().to_string()],
        ).await?;
        client.execute(
            "INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at) VALUES($1,1,6,$2,now()+interval '1 hour')",
            &[&session_id(&second_token), &uuid::Uuid::new_v4().simple().to_string()],
        ).await?;
        client.execute(
            "INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at) VALUES($1,1,1,$2,now()+interval '1 hour')",
            &[&session_id(&reader_token), &uuid::Uuid::new_v4().simple().to_string()],
        ).await?;
        client.execute(
            "INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,1,2),(1,2,1) ON CONFLICT DO NOTHING",
            &[],
        ).await?;
        role = client.query_one(
            "INSERT INTO roles(tenant_id,company_id,name,code) VALUES(1,1,'Shared view readers','grid_shared_view_readers') RETURNING id",
            &[],
        ).await?.get::<_, i64>(0);
        client.execute(
            "INSERT INTO principal_roles(tenant_id,user_id,role_id,company_id) VALUES(1,1,$1,1),(1,7,$1,NULL)",
            &[&role],
        ).await?;
    }
    let call = |contract| Call {
        scope: &admin,
        credential: &token,
        service: "core/company.pages",
        event: "grid",
        contract,
    };
    contracts
        .submit(
            &call("base.grid.view.save"),
            json!({
                "page":"reference_items", "view":View::new("reference_items").to_json().to_string(),
                "params":"{}", "name":"Company Monday", "audience":{"roles":[role]}
            }),
        )
        .await?;
    // Agent discovery and review use the same page contracts and reader scope.
    let input = json!({"page":"companies", "view":json!({"columns":["name,revenue"],"lenses":{},"zoom":18,"since":null}).to_string(), "params":json!({"status":"active"}).to_string(), "name":"Reviewed companies", "audience":{"roles":[role]}});
    let review = contracts
        .preview(&call("base.grid.view.save"), input.clone())
        .await?;
    assert_eq!(review["valid"], true, "{review}");
    let preview = &review["results"][0]["preview"]["result"]["audience_preview"];
    assert_eq!(
        preview["audience"]["roles"][0]["name"],
        "Shared view readers"
    );
    let columns: Vec<_> = preview["visible_to_current_reader"]
        .as_array()
        .ok_or("missing column preview")?
        .iter()
        .map(|column| &column["spec"])
        .collect();
    assert_eq!(columns, [&json!("name"), &json!("revenue")]);
    let saved = contracts
        .submit(&call("base.grid.view.save"), input)
        .await?;
    let company_id = saved["result"]["id"].as_i64().ok_or("missing saved id")?;
    assert_eq!(saved["result"]["view"]["name"], "Reviewed companies");
    assert_eq!(
        saved["result"]["view"]["view"]["columns"],
        json!(["name", "revenue"])
    );
    assert!(saved["result"]["view"].get("query").is_none());
    let list = contracts
        .read(&admin, "base.grid.view.list", &json!({"page":"companies"}))
        .await?;
    assert!(list["result"]["views"]
        .as_array()
        .ok_or("missing view list")?
        .iter()
        .any(|v| v["id"] == company_id));
    let saved = contracts
        .read(
            &admin,
            "base.grid.view.get",
            &json!({"page":"companies","id":company_id}),
        )
        .await?;
    assert_eq!(saved["result"]["params"]["status"], "active");
    assert_eq!(
        saved["result"]["view"]["columns"],
        json!(["name", "revenue"])
    );
    assert!(saved["result"].get("query").is_none());
    let shared = views.shared(&admin, "companies").await?;
    let link = shared
        .iter()
        .find(|v| v.id == company_id)
        .ok_or("missing Companies link")?;
    let query = link.query();
    let params: HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(params["cols"], "name,revenue");
    let users = contracts
        .submit(
            &call("base.grid.view.save"),
            json!({"page":"users", "view":"{}", "params":json!({"search":"staff@example.test"}).to_string(), "name":"Employee lookup", "audience":{"everyone":true}}),
        )
        .await?;
    let users_id = users["result"]["id"]
        .as_i64()
        .ok_or("missing Users view ID")?;
    db.get().await?.execute(
        "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed,reach) VALUES(1,1,'admin.user.list',false,'all')",
        &[],
    ).await?;
    for (operation, input) in [
        ("base.grid.view.list", json!({"page":"users"})),
        ("base.grid.view.get", json!({"page":"users","id":users_id})),
    ] {
        assert!(
            matches!(
                contracts.read(&reader, operation, &input).await,
                Err(bilibi_agent_api::Error::Backend(
                    bilibi_database::Error::Forbidden
                ))
            ),
            "a staff reader without the Users page grant must be refused by {operation}"
        );
    }
    assert_eq!(
        users["result"]["view"]["params"]["search"],
        "staff@example.test"
    );
    db.get().await?.execute(
        "DELETE FROM principal_capabilities WHERE tenant_id=1 AND user_id=1 AND capability='admin.user.list' AND NOT is_allowed",
        &[],
    ).await?;
    let users_view = contracts
        .read(
            &reader,
            "base.grid.view.get",
            &json!({"page":"users","id":users_id}),
        )
        .await?;
    assert_eq!(
        users_view["result"]["params"]["search"],
        "staff@example.test"
    );
    let fields = contracts
        .read(
            &admin,
            "base.grid.view.fields",
            &json!({"page":"companies"}),
        )
        .await?;
    assert!(fields["result"]["filters"]
        .as_array()
        .ok_or("missing filters")?
        .iter()
        .any(|f| f["key"] == "status"));
    assert_eq!(fields["result"]["search_key"], "search");
    for removed in ["sorts", "sort_key", "direction_key"] {
        assert!(fields["result"].get(removed).is_none());
    }
    let members = contracts
        .read(
            &admin,
            "base.grid.view.fields",
            &json!({"page":"team-members"}),
        )
        .await?;
    assert_eq!(members["result"]["filters"], json!([]));
    assert!(members["result"].get("search_key").is_none());

    let exchange_fields = contracts
        .read(
            &admin,
            "base.grid.view.fields",
            &json!({"page":"integration-exchanges","search":"outcome"}),
        )
        .await?;
    assert_eq!(
        exchange_fields["result"]["columns"],
        json!([{"spec":"outcome","label":"Outcome","type":"page"}])
    );
    let exchange = contracts
        .preview(
            &Call { service: "base/integration.pages", ..call("base.grid.view.save") },
            json!({"page":"integration-exchanges","view":json!({"columns":["outcome"]}).to_string(),"params":"{}","name":"By outcome","audience":{"everyone":true}}),
        )
        .await?;
    assert_eq!(exchange["valid"], true, "{exchange}");
    let exchange_preview = &exchange["results"][0]["preview"]["result"]["audience_preview"];
    assert_eq!(
        exchange_preview["visible_to_current_reader"],
        json!([{"spec":"outcome","label":"Outcome"}])
    );

    assert!(admin.current_company().is_none());
    assert!(matches!(
        contracts
            .read(
                &admin,
                "base.grid.view.fields",
                &json!({"page":"product-prices"})
            )
            .await,
        Err(bilibi_agent_api::Error::Backend(
            bilibi_database::Error::Forbidden
        ))
    ));
    let prices = json!({"page":"product-prices","view":json!({"columns":["amount"]}).to_string(),"params":"{}","name":"Price amount","audience":{"everyone":true}});
    let price_review = contracts
        .preview(
            &Call {
                service: "core/product.pages",
                ..call("base.grid.view.save")
            },
            prices.clone(),
        )
        .await?;
    assert_eq!(price_review["valid"], false);
    assert_eq!(
        price_review["results"][0]["error"]["code"],
        "missing_capability"
    );
    db.get().await?.execute(
        "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed,reach) VALUES(1,1,'products.view',true,'all')",
        &[],
    ).await?;
    assert!(reader.current_company().is_some());
    let price_fields = contracts
        .read(
            &reader,
            "base.grid.view.fields",
            &json!({"page":"product-prices"}),
        )
        .await?;
    assert_eq!(price_fields["result"]["filters"], json!([]));
    assert!(price_fields["result"].get("search_key").is_none());
    assert!(price_fields["result"]["columns"]
        .as_array()
        .ok_or("missing prices columns")?
        .iter()
        .any(|column| column["spec"] == "amount"));
    db.get().await?.execute("INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed,reach) VALUES(1,1,'admin.company.list',true,'all') ON CONFLICT DO NOTHING", &[]).await?;
    db.get().await?.execute("INSERT INTO field_restrictions(tenant_id,role_id,table_id,field_id) VALUES(1,$1,'companies','revenue') ON CONFLICT DO NOTHING", &[&role]).await?;
    let restricted = contracts
        .read(
            &reader,
            "base.grid.view.fields",
            &json!({"page":"companies", "search":"revenue"}),
        )
        .await?;
    assert!(!restricted["result"]["columns"]
        .as_array()
        .ok_or("missing restricted columns")?
        .iter()
        .any(|c| c["spec"] == "revenue"));
    let reader_view = contracts
        .read(
            &reader,
            "base.grid.view.get",
            &json!({"page":"companies", "id":company_id}),
        )
        .await?;
    assert_eq!(reader_view["result"]["name"], "Reviewed companies");
    let hidden_audience = json!({"name":"People with a role it is shared with"});
    assert_eq!(reader_view["result"]["audience"], hidden_audience);
    let reader_list = contracts
        .read(&reader, "base.grid.view.list", &json!({"page":"companies"}))
        .await?;
    assert_eq!(
        reader_list["result"]["views"][0]["audience"],
        hidden_audience
    );
    let reader_discovery = contracts
        .read(
            &reader,
            "base.labels.customisations.discover",
            &json!({"on_page":"/companies","purpose":"shared list views"}),
        )
        .await?;
    let choices = reader_discovery["result"]["choices"]
        .as_array()
        .ok_or("missing Companies view choices")?;
    for subject in ["companies".to_owned(), format!("companies:{company_id}")] {
        let choice = choices
            .iter()
            .find(|choice| choice["subject"] == subject)
            .ok_or("missing shared view choice")?;
        assert_eq!(choice["available_writes"], json!([]));
    }
    for (scope, count) in [(&admin, 1), (&reader, 0)] {
        let found = contracts
            .read(
                scope,
                "base.labels.customisations.discover",
                &json!({"on_page":"/companies","purpose":"delete view"}),
            )
            .await?;
        assert_eq!(found["meta"]["page"]["total"], count);
        if count == 1 {
            assert_eq!(found["result"]["choices"][0]["input"]["id"], company_id);
            assert_eq!(
                found["result"]["choices"][0]["available_writes"],
                json!(["base.grid.view.delete"])
            );
        }
    }
    let connection = {
        let client = db.get().await?;
        client.execute(
            "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed,reach) VALUES(1,1,$1,true,'all')",
            &[&SHARE_VIEWS],
        ).await?;
        client.query_one(
            "INSERT INTO core_agent_connections(tenant_id,user_id,person_id,agent_id,expires_at) VALUES(1,1,1,'grid-views-walkthrough-agent',now()+interval '1 hour') RETURNING id",
            &[],
        ).await?.get::<_, i64>(0)
    };
    let agent = reader
        .clone()
        .for_agent(connection, None, "grid-views-walkthrough".into(), None);
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let save = |input| {
        [PlannedOperation {
            key: "base.grid.view.save".into(),
            input,
        }]
    };
    let price_save = save(prices);
    let price_authority = HeldAgent(&agent);
    let valid = plans.preview(&price_authority, &price_save).await?;
    assert_eq!(valid["valid"], true, "{valid}");
    let price_preview = &valid["results"][0]["preview"]["result"]["audience_preview"];
    assert_eq!(
        price_preview["visible_to_current_reader"],
        json!([{"spec":"amount","label":"Amount per base unit"}])
    );
    plans.commit(&agent, &price_authority, &price_save).await?;
    let authorized = contracts
        .read(&reader, "base.grid.view.pages", &json!({}))
        .await?;
    let price_page = authorized["result"]["pages"]
        .as_array()
        .and_then(|pages| pages.iter().find(|page| page["page"] == "product-prices"))
        .ok_or("missing product prices page for reader with a desk")?;
    assert_eq!(price_page["saved_views"][0]["name"], "Price amount");
    let denied = contracts
        .read(&admin, "base.grid.view.pages", &json!({}))
        .await?;
    assert!(denied["result"]["pages"]
        .as_array()
        .ok_or("missing pages")?
        .iter()
        .all(|page| page["page"] != "product-prices"));
    let denied = contracts
        .read(
            &admin,
            "base.labels.customisations.discover",
            &json!({"record_kind":"product-prices"}),
        )
        .await?;
    assert_eq!(denied["result"]["choices"], json!([]));
    let typo = save(
        json!({"page":"contacts","view":"{}","params":"{}","name":"Mistyped page","audience":{"everyone":true}}),
    );
    for scope in [agent.clone(), agent.over_mcp()] {
        let authority = HeldAgent(&scope);
        let review = plans.preview(&authority, &typo).await?;
        assert_eq!(review["valid"], false);
        assert_eq!(
            review["results"][0]["error"]["fields"]["page"],
            json!(["choose a registered list page"])
        );
        assert!(matches!(
            plans.commit(&scope, &authority, &typo).await,
            Err(bilibi_agent_api::Error::PlanRejected(_))
        ));
    }
    assert!(views.shared(&reader, "contacts").await?.is_empty());
    db.get()
        .await?
        .execute(
            "DELETE FROM principal_capabilities WHERE tenant_id=1 AND user_id=1 AND capability=$1",
            &[&SHARE_VIEWS],
        )
        .await?;
    let other = {
        let client = db.get().await?;
        auth.authenticate(&**client, 2, 5, String::new()).await?
    };
    assert!(contracts
        .read(
            &other,
            "base.grid.view.get",
            &json!({"page":"companies", "id":company_id})
        )
        .await
        .is_err());
    db.get().await?.execute("DELETE FROM field_restrictions WHERE tenant_id=1 AND role_id=$1 AND table_id='companies' AND field_id='revenue'", &[&role]).await?;
    contracts
        .submit(&call("base.grid.view.delete"), json!({"id":company_id}))
        .await?;
    println!("PASS: scoped view discovery, named audience review, saved readback, restricted reader and second tenant.");
    let owned = views.shared(&admin, "reference_items").await?;
    assert_eq!(owned.len(), 1);
    assert!(owned[0].owned);
    let id = owned[0].id;
    for (user, company, visible) in [(1, 1, true), (1, 2, false), (7, 1, true), (7, 2, true)] {
        let scope = {
            let client = db.get().await?;
            auth.authenticate_session(&**client, 1, user, None, Some(company), String::new())
                .await?
        };
        assert_eq!(
            scope.current_company().map(|company| company.get()),
            Some(company)
        );
        let found = views.shared(&scope, "reference_items").await?;
        assert_eq!(
            found.iter().any(|view| view.id == id),
            visible,
            "user {user}, company {company}"
        );
    }
    let other_tenant = {
        let client = db.get().await?;
        auth.authenticate(&**client, 2, 5, String::new()).await?
    };
    assert!(views
        .shared(&other_tenant, "reference_items")
        .await?
        .is_empty());
    let denied = contracts
        .submit(
            &Call {
                scope: &reader,
                credential: &reader_token,
                service: "core/company.pages",
                event: "grid",
                contract: "base.grid.view.delete",
            },
            json!({"id":id}),
        )
        .await;
    assert!(matches!(
        denied,
        Err(bilibi_agent_api::Error::Backend(
            bilibi_database::Error::Forbidden
        ))
    ));
    println!("PASS: a non-owner without sharing permission cannot delete a view.");
    db.get().await?.execute(
        "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed,reach) VALUES(1,4,$1,false,'all')",
        &[&SHARE_VIEWS],
    ).await?;
    {
        let client = db.get().await?;
        assert!(!bilibi_authz::effective_capabilities(&**client, &admin)
            .await?
            .contains(SHARE_VIEWS));
        let registry = app
            .services
            .get::<bilibi_agent_api::Registry>("base/agent_api.registry")?;
        let description = registry
            .describe(&**client, &admin, "base.grid.view.delete")
            .await?;
        assert_eq!(
            description["capability"],
            json!(bilibi_agent_api::SIGNED_IN)
        );
        let operations = registry
            .search(
                &**client,
                &admin,
                "base.grid.view.delete",
                bilibi_agent_api::SearchOptions {
                    limit: 50,
                    ..Default::default()
                },
            )
            .await?;
        assert!(operations
            .iter()
            .any(|operation| operation["key"] == "base.grid.view.delete"
                && operation["available"] == true));
    }
    assert!(views
        .shared(&admin, "reference_items")
        .await?
        .iter()
        .any(|view| view.id == id && view.owned));
    contracts
        .submit(&call("base.grid.view.delete"), json!({"id":id}))
        .await?;
    assert!(views.shared(&admin, "reference_items").await?.is_empty());
    println!("PASS: an owner without sharing permission can discover and delete their view.");
    db.get()
        .await?
        .execute(
            "DELETE FROM principal_capabilities WHERE tenant_id=1 AND user_id=4 AND capability=$1",
            &[&SHARE_VIEWS],
        )
        .await?;
    let mut columns = PageColumns::mount(
        views.clone(),
        "reference_items",
        vec![BuiltinColumn::new("name", "Name")],
    )?;
    let mut list = ListState::parse(&HashMap::new(), ListSpec::default().shared()?)?;
    columns.from_params(&admin, &HashMap::new()).await?;
    list.apply_filters(&HashMap::from([("search".into(), "latest filter".into())]));
    columns
        .handle(
            &admin,
            &HashMap::from([
                ("op".into(), "save_view".into()),
                ("name".into(), "Queued filter".into()),
                ("audience".into(), "everyone".into()),
                ("query".into(), r#"{"search":"stale filter"}"#.into()),
            ]),
            ViewCaller {
                contracts: &contracts,
                credential: &token,
                service: "core/company.pages",
                event: "grid",
                query: list.to_params(),
            },
        )
        .await?;
    let saved = views.shared(&admin, "reference_items").await?;
    let queued = saved
        .iter()
        .find(|view| view.name == "Queued filter")
        .ok_or("missing queued view")?;
    assert_eq!(
        queued.params.get("search").map(String::as_str),
        Some("latest filter")
    );
    contracts
        .submit(
            &Call {
                scope: &second_admin,
                credential: &second_token,
                service: "core/company.pages",
                event: "grid",
                contract: "base.grid.view.delete",
            },
            json!({"id":queued.id}),
        )
        .await?;
    assert!(views.shared(&admin, "reference_items").await?.is_empty());
    println!("PASS: owners without sharing permission can discover and delete their views; other owners need sharing permission; queued saves use the latest server filters.");
    let label_call = |contract| Call {
        scope: &admin,
        credential: &token,
        service: "base/labels.pages",
        event: "save_field",
        contract,
    };
    contracts.submit(&label_call("base.labels.set"), json!({
        "kind":"record", "subject":"reference_items", "label":"Monday items", "version":"0:0"
    })).await?;
    let listed = contracts
        .read(&admin, "base.labels.list", &json!({"kind":"record"}))
        .await?;
    let label = listed["result"]
        .as_array()
        .ok_or("missing label list")?
        .iter()
        .find(|label| label["subject"] == "reference_items")
        .ok_or("missing record label")?;
    assert_eq!(label["label"], json!("Monday items"));
    let version = label["version"]
        .as_str()
        .ok_or("missing label version")?
        .to_owned();
    contracts
        .submit(
            &label_call("base.labels.clear"),
            json!({
                "kind":"record", "subject":"reference_items", "version":version
            }),
        )
        .await?;
    let cleared = contracts
        .read(&admin, "base.labels.list", &json!({"kind":"record"}))
        .await?;
    let label = cleared["result"]
        .as_array()
        .ok_or("missing cleared label list")?
        .iter()
        .find(|label| label["subject"] == "reference_items")
        .ok_or("missing cleared record label")?;
    assert!(label["label"].is_null());
    assert_eq!(label["version"], json!("0:0"));
    // Field labels (#686): a rename shows in the catalog's columns (list heads,
    // filters, pickers, exports), by the stable table.field ID, and clearing restores.
    let catalog_label =
        |catalog: &bilibi_grid::Catalog| -> Result<String, Box<dyn std::error::Error>> {
            let root = catalog.table("reference_items").ok_or("missing table")?;
            Ok(catalog.resolve(root, "quantity")?.label)
        };
    {
        let client = db.get().await?;
        let before = bilibi_grid::Catalog::for_scope(&**client, &admin).await?;
        assert_eq!(catalog_label(&before)?, "Quantity");
    }
    contracts.submit(&label_call("base.labels.set"), json!({
        "kind":"field", "subject":"reference_items.quantity", "label":"Stock on hand", "version":"0:0"
    })).await?;
    {
        let client = db.get().await?;
        let renamed = bilibi_grid::Catalog::for_scope(&**client, &admin).await?;
        assert_eq!(catalog_label(&renamed)?, "Stock on hand");
        let other = bilibi_grid::Catalog::for_scope(&**client, &reader).await?;
        assert_eq!(
            catalog_label(&other)?,
            "Stock on hand",
            "every reader of the tenant sees the rename"
        );
        let words = bilibi_grid::RecordWords::load(&**client, &admin, "reference_items").await?;
        assert_eq!(
            words.get(&bilibi_ui::RecordField::new(
                "quantity",
                "Quantity",
                bilibi_ui::RecordFieldType::Integer,
            )),
            "Stock on hand"
        );
        assert_eq!(
            words.get(&bilibi_ui::RecordField::new(
                "unknown_field",
                "Owner word",
                bilibi_ui::RecordFieldType::String,
            )),
            "Owner word"
        );
    }
    let refused = contracts
        .submit(&label_call("base.labels.set"), json!({
            "kind":"field", "subject":"reference_items.group_id", "label":"Hidden", "version":"0:0"
        }))
        .await;
    assert!(
        refused.is_err(),
        "a hidden or unknown field cannot be renamed"
    );
    let listed = contracts
        .read(
            &admin,
            "base.labels.list",
            &json!({"kind":"field","search":"stock on hand"}),
        )
        .await?;
    let field = listed["result"][0].clone();
    assert_eq!(field["subject"], json!("reference_items.quantity"));
    assert_eq!(field["default"], json!("Quantity"));
    contracts
        .submit(
            &label_call("base.labels.clear"),
            json!({"kind":"field", "subject":"reference_items.quantity", "version":field["version"]}),
        )
        .await?;
    {
        let client = db.get().await?;
        let restored = bilibi_grid::Catalog::for_scope(&**client, &admin).await?;
        assert_eq!(catalog_label(&restored)?, "Quantity");
    }
    for (subject, owner_word, tenant_word) in [
        ("companies.tax_id", "Tax ID", "Tax reference"),
        ("persons.email", "Personal email", "Mailbox"),
    ] {
        contracts
            .submit(
                &label_call("base.labels.set"),
                json!({
                    "kind":"field", "subject":subject, "label":tenant_word, "version":"0:0"
                }),
            )
            .await?;
        let (heading, form, filters) = bilibi_web::render_fields(&app, Some(&admin), async {
            let columns = bilibi_ui::ListColumns::mount(
                "renderer-proof",
                vec![BuiltinColumn::new("display-slot", owner_word).grid_field(subject)],
            )
            .map_err(|_| bilibi_database::Error::Internal)?;
            let table = bilibi_ui::FlexTable::new("renderer-list", columns.flex_columns(), vec![]);
            let field = bilibi_ui::Field::in_form("create", "input_key")
                .grid_field(subject)
                .label(owner_word);
            let form = bilibi_ui::Form {
                id: "renderer-create",
                body: bilibi_ui::Html::render(&field)
                    .map_err(|_| bilibi_database::Error::Internal)?,
                ..bilibi_ui::Form::default()
            };
            // A set filter: its toolbar label and its narrow-screen chip.
            let filters = bilibi_ui::FilterToolbar::new(
                "renderer-filters",
                vec![bilibi_ui::ToolbarControl::select(
                    "renderer-filter",
                    "filter_key",
                    owner_word,
                    "set",
                    vec![
                        bilibi_ui::Choice::new("All", ""),
                        bilibi_ui::Choice::new("Set", "set"),
                    ],
                )
                .grid_field(subject)],
            );
            Ok((
                bilibi_ui::Html::render(&table)
                    .map_err(|_| bilibi_database::Error::Internal)?
                    .to_string(),
                bilibi_ui::Html::render(&form)
                    .map_err(|_| bilibi_database::Error::Internal)?
                    .to_string(),
                bilibi_ui::Html::render(&filters)
                    .map_err(|_| bilibi_database::Error::Internal)?
                    .to_string(),
            ))
        })
        .await?;
        assert!(
            filters.contains(&format!(">{tenant_word}<"))
                && filters.contains(&format!("{tenant_word}: Set"))
                && !filters.contains(owner_word),
            "the shared filter label and its chip use the saved tenant label for {subject}"
        );
        assert!(
            heading.contains(tenant_word),
            "the shared list heading uses the saved tenant label for {subject}"
        );
        assert!(
            form.contains(tenant_word),
            "the shared create-form label uses the saved tenant label for {subject}"
        );
        let labels = contracts
            .read(&admin, "base.labels.list", &json!({"kind":"field"}))
            .await?;
        let version = &labels["result"]
            .as_array()
            .ok_or("missing labels")?
            .iter()
            .find(|label| label["subject"] == subject)
            .ok_or("missing saved label")?["version"];
        contracts
            .submit(
                &label_call("base.labels.clear"),
                json!({
                    "kind":"field", "subject":subject, "version":version,
                }),
            )
            .await?;
        let restored = bilibi_web::render_fields(&app, Some(&admin), async {
            Ok(bilibi_ui::ListColumns::mount(
                "renderer-proof",
                vec![BuiltinColumn::new("display-slot", owner_word).grid_field(subject)],
            )
            .map_err(|_| bilibi_database::Error::Internal)?
            .flex_columns()[0]
                .label
                .clone())
        })
        .await?;
        assert_eq!(
            restored, owner_word,
            "clearing restores the owner word on the next render"
        );
    }
    println!("PASS: a field label set by table.field shows in Grid columns for every reader of the tenant and in shared filter labels and chips, a hidden field is refused, and clearing restores the module's word.");
    println!("PASS: company-scoped roles receive views only in their company; tenant-wide roles receive them everywhere; owners retain visibility; cross-tenant readers cannot see them.");
    println!(
        "PASS: shared-view save/delete and label set/list/clear finish with a one-connection pool."
    );
    db.resize(2);
    let save = |name: String, audience| {
        json!({
            "page":"companies", "view":View::new("companies").to_json().to_string(),
            "params":"{}", "name":name, "audience":audience
        })
    };
    for index in 0..99 {
        contracts
            .submit(
                &call("base.grid.view.save"),
                save(format!("Limit {index}"), json!({"roles":[role]})),
            )
            .await?;
    }
    let before = contracts
        .read(
            &second_admin,
            "base.labels.customisations.discover",
            &json!({"on_page":"/companies","purpose":"save view"}),
        )
        .await?;
    assert_eq!(before["result"]["choices"][0]["value"], json!([]));
    assert_eq!(
        before["result"]["choices"][0]["available_writes"],
        json!(["base.grid.view.save"])
    );

    let first = tokio::spawn({
        let contracts = contracts.clone();
        let scope = admin.clone();
        let token = token.clone();
        let input = save("Concurrent first".into(), json!({"everyone":true}));
        async move {
            contracts
                .submit(
                    &Call {
                        scope: &scope,
                        credential: &token,
                        service: "core/company.pages",
                        event: "grid",
                        contract: "base.grid.view.save",
                    },
                    input,
                )
                .await
        }
    });
    let second = tokio::spawn({
        let contracts = contracts.clone();
        let scope = second_admin.clone();
        let token = second_token.clone();
        let input = save("Concurrent second".into(), json!({"everyone":true}));
        async move {
            contracts
                .submit(
                    &Call {
                        scope: &scope,
                        credential: &token,
                        service: "core/company.pages",
                        event: "grid",
                        contract: "base.grid.view.save",
                    },
                    input,
                )
                .await
        }
    });
    let (first, second) = tokio::join!(first, second);
    let (first, second) = (first?, second?);
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let refused = first
        .err()
        .or_else(|| second.err())
        .ok_or("expected one refused save")?;
    assert!(matches!(
        refused,
        bilibi_agent_api::Error::Backend(bilibi_database::Error::Refused(_))
    ));
    assert_eq!(views.shared(&admin, "companies").await?.len(), 100);
    let total: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM base_grid_shared_views WHERE tenant_id=1 AND page='companies'",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(total, 100);
    for (scope, visible) in [(&admin, 100), (&second_admin, 1)] {
        let after = contracts
            .read(
                scope,
                "base.labels.customisations.discover",
                &json!({"on_page":"/companies","purpose":"save view"}),
            )
            .await?;
        let choice = &after["result"]["choices"][0];
        assert_eq!(choice["subject"], "companies");
        assert_eq!(choice["value"].as_array().map(Vec::len), Some(visible));
        assert_eq!(choice["available_writes"], json!([]));
    }
    println!("PASS: concurrent saves at 99 views commit exactly one additional view and refuse the other.");
    Ok(())
}
