//! The live operation registry and MCP dispatcher on a fresh fixture database.
use bilibi_agent::{AgentAccess, AgentClaims, Decision, OAuthError, CLIENT_ID};
use bilibi_agent_api::{
    mcp::{self, Outcome, Session},
    page::PageContracts,
    plans::{PlanAuthority, PlanIdentity, Plans},
    Registry, Reviews,
};
use bilibi_authz::grants::Grants;
use bilibi_database::{pool, Db, Error};
use bilibi_step_up::StepUp;
use bilibi_tenancy::{ProofFuture, Scope};
use bilibi_ui::page::{PageRequest, PageService};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio_postgres::Transaction;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Authority<'a> {
    access: &'a AgentAccess,
    claims: AgentClaims,
}
impl PlanAuthority for Authority<'_> {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            let scope = self
                .access
                .prove(tx, &self.claims, true, "workspace-walkthrough", None)
                .await
                .map_err(|error| match error {
                    OAuthError::Other(error) => error,
                    _ => Error::Unauthorized,
                })?;
            Ok(PlanIdentity {
                scope,
                credential_binding: format!("agent-connection:{}", self.claims.connection),
            })
        })
    }
}

struct Connector<'a> {
    db: &'a Db,
    scope: &'a Scope,
    authority: &'a Authority<'a>,
    registry: &'a Registry,
    plans: &'a Plans,
    step_up: &'a StepUp,
    reviews: &'a Reviews,
}
impl Connector<'_> {
    async fn call(&self, name: &str, arguments: Value) -> Result<Value> {
        let message = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}});
        let result = mcp::dispatch(
            self.registry,
            self.plans,
            Some(Session {
                db: self.db,
                scope: self.scope,
                authority: self.authority,
                step_up: self.step_up,
                reviews: self.reviews,
            }),
            &message,
        )
        .await;
        let Outcome::Reply(reply) = result else {
            return Err(format!("{name} did not return an MCP reply").into());
        };
        if reply["result"]["isError"] != false {
            return Err(format!("{name} refused: {}", reply["result"]["structuredContent"]).into());
        }
        Ok(reply["result"]["structuredContent"].clone())
    }
    async fn read(&self, key: &str) -> Result<Value> {
        let answer = self
            .call("read", json!({"key":key,"input":{}}))
            .await
            .map_err(|error| format!("{key}: {error}"))?;
        Ok(answer["result"].clone())
    }
    async fn plan(&self, operations: &Value) -> Result<Value> {
        self.call("plan", json!({"operations":operations})).await
    }
    async fn commit(&self, operations: &Value) -> Result<()> {
        self.call("commit", json!({"operations":operations}))
            .await?;
        Ok(())
    }
    async fn write(&self, operations: &Value) -> Result<()> {
        let preview = self.plan(operations).await?;
        if preview["valid"] != true {
            return Err(format!("workspace plan was refused: {}", preview["results"]).into());
        }
        self.commit(operations).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_WORKSPACE_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_WORKSPACE_DEMO=disposable on a disposable database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_web::modules()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    {
        // Add the two desks this specimen needs. The common fixture gives the
        // manager a desk but leaves the viewer unlinked and the AI account without one.
        let client = db.get().await?;
        let person: i64 = client.query_one(
            "INSERT INTO persons(tenant_id,code,name,kind) VALUES(1,'workspace_viewer','Workspace viewer','human') RETURNING id", &[],
        ).await?.get(0);
        client
            .execute(
                "UPDATE users SET person_id=$1 WHERE tenant_id=1 AND id=2",
                &[&person],
            )
            .await?;
        client
            .execute(
                "INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,3,1),(1,$1,1)",
                &[&person],
            )
            .await?;
    }
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35572")?;
    app.services
        .get::<Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let login = app
        .user
        .login("ai@agent.my", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let browser = app.security.guard(&login.token).await?;
    let viewer = app
        .user
        .login("viewer@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let viewer = app.security.guard(&viewer.token).await?;
    let manager = app
        .user
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let manager = app.security.guard(&manager.token).await?;
    let access = app.services.get::<AgentAccess>("core/agent.grants")?;
    let device = access
        .start_device(CLIENT_ID, "Workspace walkthrough")
        .await?;
    access
        .decide(&browser, &device.user_code, Decision::Approve)
        .await?;
    {
        // Poll immediately in this disposable database without sleeping.
        let client = db.get().await?;
        client
            .execute(
                "UPDATE core_agent_devices SET next_poll_at=now() WHERE status='approved'",
                &[],
            )
            .await?;
    }
    let tokens = access.exchange(CLIENT_ID, &device.device_code).await?;
    let claims = access.authenticate(&tokens.access_token, None).await?;
    let scope = access.seal(&claims, "workspace-walkthrough", None).await?;
    let authority = Authority {
        access: &access,
        claims,
    };
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let step_up = app.services.get::<StepUp>(bilibi_step_up::SERVICE)?;
    let reviews = app.services.get::<Reviews>("base/agent_api.reviews")?;
    let pages = app
        .services
        .get::<PageContracts>(bilibi_agent_api::page::SERVICE)?;
    let connector = Connector {
        db: &db,
        scope: &scope,
        authority: &authority,
        registry: &registry,
        plans: &plans,
        step_up: &step_up,
        reviews: &reviews,
    };

    let managed = connector.read("base.tiling.manage").await?;
    let company = &managed["company_id"];
    assert_eq!(managed["layouts"], json!([]));
    let tree = "h.5(/companies,/system/sessions)";
    let publish = json!([{"key":"base.tiling.publish","input":{
        "company_id":company,"label":"Service desk","slug":"service-desk","tree":tree,"roles":["viewer"],"version":""
    }}]);
    assert_eq!(connector.plan(&publish).await?["valid"], true);
    let mut wrong_slug = publish.clone();
    wrong_slug[0]["input"]["slug"] = json!("service-desk-2");
    assert_eq!(connector.plan(&wrong_slug).await?["valid"], false);
    let mut unserved_tile = publish.clone();
    unserved_tile[0]["input"]["tree"] = json!("h.5(/companies,/workspace-route-typo)");
    assert_eq!(connector.plan(&unserved_tile).await?["valid"], false);

    assert_eq!(
        connector.read("base.tiling.manage").await?["layouts"],
        json!([])
    );
    connector.commit(&publish).await?;
    assert_eq!(connector.plan(&publish).await?["valid"], false);
    let visible = pages.read(&viewer, "base.tiling.read", &json!({})).await?;
    assert_eq!(visible["result"]["layouts"][0]["tree"], tree);
    assert_eq!(
        visible["result"]["layouts"][0]["url"],
        "/workspace/shared/service-desk"
    );
    let hidden = pages.read(&manager, "base.tiling.read", &json!({})).await?;
    assert_eq!(hidden["result"]["layouts"], json!([]));
    assert!(pages
        .read(&viewer, "base.tiling.manage", &json!({}))
        .await
        .is_err());
    let workspace = app
        .services
        .get::<PageService<Scope, Error>>("base/tiling.pages")?;
    let path = HashMap::from([("shared".into(), "service-desk".into())]);
    let query = HashMap::new();
    let page = workspace
        .0
        .render(PageRequest {
            operation: "index",
            scope: Some(&viewer),
            path: &path,
            query: &query,
            body: &[],
            framed: false,
        })
        .await?;
    assert!(page.html.contains("src=\"/companies\""));
    assert!(page.html.contains("open /system/sessions"));
    assert!(!page.html.contains("src=\"/system/sessions\""));
    let unknown = json!([{"key":"base.tiling.publish","input":{
        "company_id":company,"label":"Unknown audience","slug":"unknown-audience","tree":tree,"roles":["not_a_role"],"version":""
    }}]);
    assert_eq!(connector.plan(&unknown).await?["valid"], false);

    let pins = connector.read("core.user.pins.read").await?;
    let landing = connector.read("core.user.landing.read").await?;
    let dashboard = connector.read("base.dashboard_agent.read").await?;
    let menu = landing["pages"]
        .as_array()
        .ok_or("no landing choices")?
        .iter()
        .find(|page| page["url"] == "/companies")
        .ok_or("companies not offered for landing")?;
    assert!(!dashboard["grid"]["default"]
        .as_array()
        .ok_or("no grid choices")?
        .is_empty());
    let proposed = json!([
        {"key":"core.user.pins.set","input":{"version":pins["version"],"pins":[
            {"url":"/workspace/shared/service-desk","label":"Service desk"},
            {"url":"/companies","label":"Companies","icon":"building-office"}
        ]}},
        {"key":"core.user.landing.set","input":{"version":landing["version"],"menu_id":menu["id"]}},
        {"key":"base.dashboard_agent.set","input":{"version":dashboard["grid"]["version"],"region":"grid","entry_ids":[]}},
        {"key":"base.dashboard_agent.set","input":{"version":dashboard["sections"]["version"],"region":"sections","entry_ids":[]}}
    ]);
    connector.write(&proposed).await?;
    let pins = connector.read("core.user.pins.read").await?;
    let landing = connector.read("core.user.landing.read").await?;
    let dashboard = connector.read("base.dashboard_agent.read").await?;
    assert_eq!(pins["pins"][0]["label"], "Service desk");
    assert_eq!(landing["menu_id"], menu["id"]);
    assert_eq!(
        app.user.landing_route(&login.token).await?,
        Some("/companies".into())
    );
    assert_eq!(dashboard["grid"]["placed"], json!([]));
    assert_eq!(dashboard["grid"]["customised"], true);
    assert_eq!(dashboard["sections"]["customised"], true);
    assert_eq!(
        pages
            .read(&viewer, "core.user.pins.read", &json!({}))
            .await?["result"]["pins"],
        json!([])
    );
    assert_eq!(
        pages
            .read(&viewer, "core.user.landing.read", &json!({}))
            .await?["result"]["menu_id"],
        ""
    );
    assert_eq!(
        pages
            .read(&viewer, "base.dashboard_agent.read", &json!({}))
            .await?["result"]["grid"]["customised"],
        false
    );

    let managed = connector.read("base.tiling.manage").await?;
    let saved = &managed["layouts"][0];
    let restore = json!([
        {"key":"base.tiling.withdraw","input":{"company_id":company,"slug":saved["slug"],"version":saved["version"]}},
        {"key":"core.user.pins.restore","input":{"version":pins["version"]}},
        {"key":"core.user.landing.restore","input":{"version":landing["version"]}},
        {"key":"base.dashboard_agent.restore","input":{"version":dashboard["grid"]["version"],"region":"grid"}},
        {"key":"base.dashboard_agent.restore","input":{"version":dashboard["sections"]["version"],"region":"sections"}}
    ]);
    connector.write(&restore).await?;
    assert_eq!(
        connector.read("base.tiling.manage").await?["layouts"],
        json!([])
    );
    assert_eq!(
        connector.read("core.user.pins.read").await?["pins"],
        json!([])
    );
    assert_eq!(
        connector.read("core.user.landing.read").await?["menu_id"],
        ""
    );
    assert_eq!(
        connector.read("base.dashboard_agent.read").await?["grid"]["placed"],
        dashboard["grid"]["default"]
    );
    let client = db.get().await?;
    let audit = client.query_one(
        "SELECT count(*) FROM base_audit_mutations m WHERE m.tenant_id=$1 AND m.auditable_type IN ('base/settings','user_pin') AND EXISTS (SELECT 1 FROM base_audit_actions a WHERE a.tenant_id=m.tenant_id AND a.trace_id=m.trace_id AND a.event='base.agent_api.write' AND a.payload->>'agent_connection'=$2)",
        &[&scope.tenant(), &authority.claims.connection.to_string()],
    ).await?;
    assert!(audit.get::<_, i64>(0) >= 12, "missing agent audit evidence");
    println!("PASS workspace publishing, role visibility, forbidden tile refusal and stale review refusal; account pins, landing and dashboard set/restore; isolated reads and audited connector writes.");
    Ok(())
}
