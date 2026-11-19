//! Real operation contracts against a disposable two-tenant fixture.
use bilibi_agent_api::{
    page::PageContracts,
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
};
use bilibi_base::menu_order::{MenuOrder, MANAGE, SERVICE, VIEW};
use bilibi_database::pool;
use bilibi_tenancy::ProofFuture;
use serde_json::{json, Value};
use tokio_postgres::Transaction;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

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
fn op(key: &str, input: Value) -> PlannedOperation {
    PlannedOperation {
        key: key.into(),
        input,
    }
}
fn group<'a>(list: &'a Value, parent: &str) -> &'a Value {
    list["result"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|v| v["parent"] == parent)
        .expect("parent")
}
fn children<'a>(tree: &'a Value, parent: &str) -> &'a Vec<Value> {
    tree.as_array()
        .expect("tree")
        .iter()
        .find(|node| node["id"] == parent)
        .expect("visible parent")["children"]
        .as_array()
        .expect("children")
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_MENU_ORDER_DEMO").as_deref() != Ok("disposable") {
        return Err("use a disposable database and set BILIBI_MENU_ORDER_DEMO=disposable".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_web::modules()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35567")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    // Fixture-only grants let the two limited readers use the preview contract.
    db.get().await?.execute("INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed,reach) VALUES(1,2,$1,true,'all'),(2,5,$1,true,'all')", &[&VIEW]).await?;
    let admin_login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let reader_login = app
        .user
        .login("viewer@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let other_login = app
        .user
        .login(
            "manager@other.example.test",
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await?;
    let admin = app.security.guard(&admin_login.token).await?;
    let reader = app.security.guard(&reader_login.token).await?;
    let other = app.security.guard(&other_login.token).await?;
    let browser = Browser {
        app: &app,
        token: &admin_login.token,
    };
    let reader_browser = Browser {
        app: &app,
        token: &reader_login.token,
    };
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let contracts = app
        .services
        .get::<PageContracts>(bilibi_agent_api::page::SERVICE)?;
    let service = app.services.get::<MenuOrder>(SERVICE)?;
    let list = contracts
        .read(&admin, "base.menu_order.list", &json!({}))
        .await?;
    let before = contracts
        .read(&admin, "base.menu_order.preview", &json!({}))
        .await?;
    let product = children(&before["result"], "admin");
    assert!(product.len() >= 2);
    let first = product[0]["id"].as_str().ok_or("first")?.to_owned();
    let last = product.last().ok_or("last")?["id"]
        .as_str()
        .ok_or("last id")?
        .to_owned();
    assert_eq!(group(&list, "admin")["version"], "0:0");
    let proposed = contracts
        .read(
            &admin,
            "base.menu_order.preview",
            &json!({"parent":"admin","item_ids":[last]}),
        )
        .await?;
    assert_eq!(children(&proposed["result"], "admin")[0]["id"], last);
    assert_eq!(
        contracts
            .read(&admin, "base.menu_order.preview", &json!({}))
            .await?["result"],
        before["result"]
    );
    let second_before = contracts
        .read(&other, "base.menu_order.preview", &json!({}))
        .await?;
    let set = [op(
        "base.menu_order.set",
        json!({"parent":"admin","item_ids":[last],"version":"0:0"}),
    )];
    let duplicate = [op(
        "base.menu_order.set",
        json!({"parent":"admin","item_ids":[last,last],"version":"0:0"}),
    )];
    assert_eq!(plans.preview(&browser, &duplicate).await?["valid"], false);
    let wrong_parent = [op(
        "base.menu_order.set",
        json!({"parent":"core","item_ids":[last],"version":"0:0"}),
    )];
    assert_eq!(
        plans.preview(&browser, &wrong_parent).await?["valid"],
        false
    );
    assert_eq!(plans.preview(&browser, &set).await?["valid"], true);
    plans.commit(&admin, &browser, &set).await?;
    assert!(
        plans.commit(&admin, &browser, &set).await.is_err(),
        "stale token refused"
    );
    let preview = contracts
        .read(&admin, "base.menu_order.preview", &json!({}))
        .await?;
    let visible = children(&preview["result"], "admin");
    assert_eq!(visible[0]["id"], last);
    let remaining: Vec<_> = product
        .iter()
        .filter(|node| node["id"] != last)
        .map(|node| node["id"].clone())
        .collect();
    let actual: Vec<_> = visible
        .iter()
        .skip(1)
        .map(|node| node["id"].clone())
        .collect();
    assert_eq!(actual, remaining, "unlisted siblings stay in product order");
    assert_eq!(
        contracts
            .read(&other, "base.menu_order.preview", &json!({}))
            .await?["result"],
        second_before["result"]
    );
    let reader_preview = contracts
        .read(&reader, "base.menu_order.preview", &json!({}))
        .await?;
    let reader_ids = reader_preview.to_string();
    assert!(
        !reader_ids.contains(&last),
        "ordering did not grant a restricted item"
    );
    assert!(
        plans.commit(&reader, &reader_browser, &set).await.is_err(),
        "reader cannot write"
    );
    let audit: i64 = db.get().await?.query_one("SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type='menu_order'", &[]).await?.get(0);
    assert_eq!(audit, 1);
    // Simulate a product release removing one previously saved item.
    db.get().await?.execute("UPDATE base_menu_order SET item_ids=array_prepend('removed.item',item_ids),version=version+1 WHERE tenant_id=1 AND parent='admin'", &[]).await?;
    let client = db.get().await?;
    let mut tree = app.menu.visible_tree(&|_| true);
    service.orders(&**client, &admin).await?.apply(&mut tree);
    let admin_node = tree
        .iter()
        .find(|n| n.item.item.id == "admin")
        .ok_or("admin node")?;
    assert_eq!(admin_node.children[0].item.item.id, last);
    assert!(admin_node.children.iter().any(|n| n.item.item.id == first));
    drop(client);
    let list = contracts
        .read(&admin, "base.menu_order.list", &json!({}))
        .await?;
    let version = group(&list, "admin")["version"].as_str().ok_or("version")?;
    let restore = [op(
        "base.menu_order.restore",
        json!({"parent":"admin","version":version}),
    )];
    assert_eq!(plans.preview(&browser, &restore).await?["valid"], true);
    plans.commit(&admin, &browser, &restore).await?;
    let list = contracts
        .read(&admin, "base.menu_order.list", &json!({}))
        .await?;
    assert!(group(&list, "admin")["item_ids"].is_null());
    let after = contracts
        .read(&admin, "base.menu_order.preview", &json!({}))
        .await?;
    assert_eq!(children(&after["result"], "admin")[0]["id"], first);

    let parent = "admin.system";
    let hidden = "admin.system.installation";
    let visible = "admin.system.info";
    let replacement = "admin.system.localization";
    let sql = db.get().await?;
    sql.execute(
        "INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed,reach)
         SELECT 1,2,capability,true,'all' FROM unnest($1::text[]) AS capability",
        &[&&[
            MANAGE,
            "admin.system.info.view",
            "admin.system.localization.manage",
        ][..]],
    )
    .await?;
    sql.execute(
        "INSERT INTO base_menu_order(tenant_id,parent,item_ids) VALUES(1,$1,ARRAY[$2,$3])",
        &[&parent, &hidden, &visible],
    )
    .await?;
    drop(sql);
    let list = contracts
        .read(&reader, "base.menu_order.list", &json!({}))
        .await?;
    assert_eq!(group(&list, parent)["item_ids"], json!([visible]));
    assert!(!list.to_string().contains(hidden));
    assert!(!list.to_string().contains("admin.system.database-table"));
    let version = group(&list, parent)["version"].as_str().ok_or("version")?;
    assert!(contracts
        .read(
            &reader,
            "base.menu_order.preview",
            &json!({"parent":parent,"item_ids":[hidden]}),
        )
        .await
        .is_err());
    let invalid_set = [op(
        "base.menu_order.set",
        json!({"parent":parent,"item_ids":[hidden],"version":version}),
    )];
    assert_eq!(
        plans.preview(&reader_browser, &invalid_set).await?["valid"],
        false
    );
    let set = [op(
        "base.menu_order.set",
        json!({"parent":parent,"item_ids":[replacement],"version":version}),
    )];
    let review = plans.preview(&reader_browser, &set).await?;
    assert_eq!(review["valid"], true);
    assert!(!review.to_string().contains(hidden));
    let result = plans.commit(&reader, &reader_browser, &set).await?;
    assert!(!result.to_string().contains(hidden));
    let sql = db.get().await?;
    let saved: Vec<String> = sql
        .query_one(
            "SELECT item_ids FROM base_menu_order WHERE tenant_id=1 AND parent=$1",
            &[&parent],
        )
        .await?
        .get(0);
    assert_eq!(saved, [hidden, replacement]);
    drop(sql);
    let list = contracts
        .read(&reader, "base.menu_order.list", &json!({}))
        .await?;
    let version = group(&list, parent)["version"].as_str().ok_or("version")?;
    let restore = [op(
        "base.menu_order.restore",
        json!({"parent":parent,"version":version}),
    )];
    let review = plans.preview(&reader_browser, &restore).await?;
    assert_eq!(review["valid"], true);
    assert!(!review.to_string().contains(hidden));
    let result = plans.commit(&reader, &reader_browser, &restore).await?;
    assert!(!result.to_string().contains(hidden));
    let saved: Vec<String> = db
        .get()
        .await?
        .query_one(
            "SELECT item_ids FROM base_menu_order WHERE tenant_id=1 AND parent=$1",
            &[&parent],
        )
        .await?
        .get(0);
    assert_eq!(saved, [hidden]);
    for unsupported in ["portal", visible, hidden] {
        let restore = [op(
            "base.menu_order.restore",
            json!({"parent":unsupported,"version":"0:0"}),
        )];
        assert_eq!(
            plans.preview(&reader_browser, &restore).await?["valid"],
            false
        );
    }
    println!("PASS: visible sibling validation, hidden override preservation and unsupported restore parents");
    println!("PASS: reviewed set and restore, product fallback, removed ID, stale write, audit, reader permissions and second-tenant isolation");
    Ok(())
}
