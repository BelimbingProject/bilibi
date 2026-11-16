//! Mounted proof of official Domain rules. On a fresh disposable database:
//! `cargo run -p bilibi-web --example rules-domains-walkthrough -- commerce`.
//! For supply_chain or people, first run that Domain's disposable walkthrough,
//! leaving an approved purchase order or claim; then pass its Domain id here.
//! The real owner event is delivered by Events and Queue, never manufactured.
use bilibi_agent_api::plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans};
use bilibi_authz::grants::{Direct, Grants};
use bilibi_base::queue::{Config, Executor};
use bilibi_database::{pool, Db, Error, Result};
use bilibi_module_registry::Services;
use bilibi_tenancy::{Authentication, ProofFuture, Scope};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio_postgres::Transaction;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

struct Held<'a>(&'a Scope);
impl PlanAuthority for Held<'_> {
    fn verify<'a>(&'a self, _: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            Ok(PlanIdentity {
                scope: self.0.clone(),
                credential_binding: "rules-domains-proof".into(),
            })
        })
    }
}
async fn write(plans: &Plans, scope: &Scope, key: &str, input: Value) -> Result<Value> {
    plans
        .commit(
            scope,
            &Held(scope),
            &[PlannedOperation {
                key: key.into(),
                input,
            }],
        )
        .await
        .map(|result| result["results"][0]["result"]["result"].clone())
        .map_err(|error| {
            eprintln!("{key}: {error:?}");
            Error::Invalid
        })
}
fn check(ok: bool, message: &str) -> Result<()> {
    if !ok {
        eprintln!("FAIL {message}");
        return Err(Error::Invalid);
    }
    println!("ok {message}");
    Ok(())
}
async fn task_count(db: &Db, kind: &str, source: i64) -> Result<i64> {
    Ok(db.get().await?.query_one("SELECT count(*) FROM core_activities WHERE tenant_id=1 AND related_kind=$1 AND related_id=$2", &[&kind, &source.to_string()]).await?.get(0))
}
async fn wait_for_runs(db: &Db, events: &[i64], rules: &[i64]) -> Result<()> {
    for _ in 0..300 {
        let client = db.get().await?;
        let row = client.query_one(
            "SELECT count(*), count(*) FILTER (WHERE d.delivered_at IS NOT NULL AND r.state='done'), count(*) FILTER (WHERE r.state NOT IN ('queued','matched','done')) FROM unnest($1::bigint[]) e(id) CROSS JOIN unnest($2::bigint[]) rule(id) LEFT JOIN base_events_deliveries d ON d.event_id=e.id AND d.subscriber='base/rules.triggers' LEFT JOIN base_rule_runs r ON r.tenant_id=1 AND r.event_id=e.id AND r.rule_id=rule.id",
            &[&events, &rules],
        ).await?;
        if row.get::<_, i64>(2) != 0 {
            return check(false, "every expected source part must finish successfully");
        }
        if row.get::<_, i64>(0) == row.get::<_, i64>(1) {
            return Ok(());
        }
        drop(client);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    check(
        false,
        "timed out waiting for every source delivery and successful rule run",
    )
}
#[tokio::main]
async fn main() -> Result<()> {
    bilibi_base::link();
    bilibi_core::link();
    bilibi_domains::link();
    bilibi_extensions::link();
    let domain = std::env::args().nth(1).unwrap_or_else(|| "commerce".into());
    let (event, action, read, manage, kind, source_query) = match domain.as_str() {
        "commerce" => ("commerce/sales.order_confirmed", "commerce.sales.order.follow_up", "commerce.sales.view", "commerce.sales.followup.manage", "commerce.sales.order", "SELECT id,revision,company_id FROM commerce_sales_documents WHERE tenant_id=1 AND kind='order' AND status='confirmed' ORDER BY id DESC LIMIT 1"),
        "supply_chain" => ("supply_chain/purchasing.order_approved", "supply_chain.purchasing.order.follow_up", "supply-chain.purchasing.view", "supply-chain.purchasing.followup.manage", "supply_chain.purchasing.order", "SELECT id,rule_revision,company_id FROM supply_chain_purchasing_orders WHERE tenant_id=1 AND status='approved' ORDER BY id DESC LIMIT 1"),
        "people" => ("people/claims.request_approved", "people.claims.request.follow_up", "people.claims.followup.view", "people.claims.followup.manage", "people.claims.request", "SELECT id,rule_revision,company_id FROM people_claim_submissions WHERE tenant_id=1 AND status='approved' ORDER BY id DESC LIMIT 1"),
        _ => return Err(Error::Invalid),
    };
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    if domain == "commerce" {
        bilibi_walkthrough_fixture::install(&db).await?;
    }
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    bilibi_database::seeds::run(&db, &modules, services.catalog()?, &services).await?;
    let grants = services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let auth = services.get::<Authentication>("base/tenancy.authentication")?;
    let client = db.get().await?;
    let mut admin = auth
        .authenticate(&**client, 1, 4, "rules-domains".into())
        .await?;
    drop(client);
    let plans = services.get::<Plans>("base/agent_api.plans")?;
    // The fixture administrator starts without a Person or desk. Give this
    // real account a workplace through the owners, rather than bypass desk checks.
    let users = services.get::<bilibi_user::UserService>("core/user.accounts")?;
    let person = if let Some(person) = admin.actor().person() {
        person
    } else {
        let created = write(
            &plans,
            &admin,
            "core.person.create",
            json!({"name":"Rules proof administrator","kind":"human"}),
        )
        .await?;
        let person = bilibi_person::PersonId::new(created["id"].as_i64().ok_or(Error::Invalid)?)
            .map_err(|_| Error::Invalid)?;
        users
            .link_person(&admin, 4, person)
            .await
            .map_err(|_| Error::Invalid)?;
        person
    };
    if admin.current_company().is_none() {
        users.set_desk(&admin, 1, person.get(), true).await?;
    }
    let client = db.get().await?;
    admin = auth
        .authenticate(&**client, 1, 4, "rules-domains".into())
        .await?;
    drop(client);
    // The proof uses real permission grants, event consent and the rules principal.
    grants
        .set_system(
            &admin,
            "domain-events",
            "events.delivery.deliver",
            Some(true),
        )
        .await?;
    for key in [
        "rules.run.execute",
        read,
        manage,
        "core.activity.system.create",
    ] {
        grants.set_system(&admin, "rules", key, Some(true)).await?;
    }
    let draft_order = if domain == "commerce" {
        let line = json!({"description":"Follow-up proof","quantity":"1","unit":"hour","unit_price":"10.00","tax_code":"ZERO","tax_rate":"0"});
        let result = write(&plans, &admin, "commerce.sales.order.create", json!({"company_id":1,"customer_kind":"company","customer_code":"company_002","currency_code":"MYR","lines":[line.clone(),line]})).await?;
        Some(result["id"].as_i64().ok_or(Error::Invalid)?)
    } else {
        None
    };
    let (source, revision, company): (i64, i64, i64) = if let Some(id) = draft_order {
        (id, 2, 1)
    } else {
        let row = db
            .get()
            .await?
            .query_opt(source_query, &[])
            .await?
            .ok_or_else(|| {
                eprintln!("Run the {domain} source walkthrough first; no approved source remains.");
                Error::Invalid
            })?;
        (row.get(0), row.get(1), row.get(2))
    };
    let recipient = 4_i64;
    let draft = json!({
        "name":format!("{domain} follow-up proof"),"trigger":event,"match":"all",
        "conditions":[{"field":"id","op":"eq","value":source.to_string()},{"field":"company_id","op":"eq","value":company.to_string()}],
        "actions":[{"operation":action,"input":json!({"assignee_user_id":recipient,"title":"Review the approved record"}).to_string()}],
        "examples":[{"facts":json!({"id":source,"company_id":company}).to_string(),"expect":"match"},{"facts":json!({"id":source+1,"company_id":company}).to_string(),"expect":"no_match"}],
        "explanation":"Create one private follow-up after the owner event, without changing its decision or payment facts."
    });
    let mut rule_ids = Vec::new();
    for name in ["First follow-up rule", "Independent follow-up rule"] {
        let mut draft = draft.clone();
        draft["name"] = json!(name);
        let rule = write(&plans, &admin, "base.rules.draft", draft).await?;
        write(
            &plans,
            &admin,
            "base.rules.approve",
            json!({"rule_id":rule["rule_id"],"version":rule["version"]}),
        )
        .await?;
        rule_ids.push(rule["rule_id"].as_i64().ok_or(Error::Invalid)?);
    }
    if let Some(id) = draft_order {
        write(
            &plans,
            &admin,
            "commerce.sales.order.transition",
            json!({"company_id":company,"id":id,"revision":1,"status":"confirmed"}),
        )
        .await?;
    }
    let trigger = services
        .catalog()?
        .entries::<bilibi_base::rules::Trigger>(bilibi_base::rules::TRIGGERS)?
        .iter()
        .find(|entry| entry.id == event)
        .ok_or(Error::Invalid)?
        .get::<bilibi_base::rules::Trigger>()?;
    let expected_events: Vec<i64> = db
        .get()
        .await?
        .query(
            "SELECT id,payload FROM base_events WHERE tenant_id=1 AND name=$1 ORDER BY id",
            &[&event],
        )
        .await?
        .into_iter()
        .filter_map(|row| {
            let payload: Value = row.get(1);
            let id = payload.pointer(trigger.subject)?;
            (id.as_i64() == Some(source) || id.as_str() == Some(source.to_string().as_str()))
                .then(|| row.get(0))
        })
        .collect();
    check(
        !expected_events.is_empty(),
        "the source recorded its expected events",
    )?;
    if domain == "commerce" {
        check(
            expected_events.len() == 2,
            "the two-line order recorded both source parts",
        )?;
    }
    let executor = Executor::from_services(
        &services,
        Config {
            concurrency: 2,
            poll_interval: Duration::from_millis(100),
            lease_seconds: 30,
            max_runtime: Duration::from_secs(60),
        },
    )?;
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(async move { executor.run(shutdown).await });
    let result: Result<()> = async {
        wait_for_runs(&db, &expected_events, &rule_ids).await?;
        check(task_count(&db, kind, source).await? == 2, "each independent rule created one task across all source parts")?;
        let client = db.get().await?;
        let rows = client.query("SELECT shared,created_by,assignee_user_id,id FROM core_activities WHERE tenant_id=1 AND related_kind=$1 AND related_id=$2 ORDER BY id", &[&kind,&source.to_string()]).await?;
        check(rows.iter().all(|row| !row.get::<_,bool>(0) && row.get::<_,i64>(1)==recipient && row.get::<_,i64>(2)==recipient), "both tasks are private and addressed to the configured recipient")?;
        let task: i64 = rows[0].get(3);
        for rule in &rule_ids {
            let row = client.query_one("SELECT count(DISTINCT f.result->>'activity_id'),count(*) FROM base_rule_effects f JOIN base_rule_runs r ON r.tenant_id=f.tenant_id AND r.id=f.run_id WHERE f.tenant_id=1 AND r.rule_id=$1 AND r.event_id=ANY($2) AND f.state='done'", &[rule, &expected_events]).await?;
            check(row.get::<_,i64>(0)==1 && row.get::<_,i64>(1)==expected_events.len() as i64, "every part of one rule returned the same task")?;
        }
        drop(client);
        write(&plans, &admin, "base.events.replay", json!({"subscriber":"base/rules.triggers","from_event":expected_events[0],"limit":1000})).await?;
        wait_for_runs(&db, &expected_events, &rule_ids).await?;
        check(task_count(&db, kind, source).await? == 2, "replaying delivered source parts creates no duplicate tasks")?;
        let valid = json!({"record_id":source,"expected_revision":revision,"assignee_user_id":recipient,"title":"Review the approved record"});
        write(&plans, &admin, action, valid.clone()).await?;
        check(task_count(&db, kind, source).await? == 3, "a manual request is independent of both rules")?;
        write(&plans, &admin, action, valid.clone()).await?;
        check(task_count(&db, kind, source).await? == 4, "identical manual requests create independent tasks")?;
        let mut stale = valid.clone(); stale["expected_revision"] = json!(revision+1);
        check(write(&plans,&admin,action,stale).await.is_err(), "a stale source revision is refused")?;
        let mut foreign = valid.clone(); foreign["assignee_user_id"] = json!(5);
        check(write(&plans,&admin,action,foreign).await.is_err(), "another tenant's recipient is refused")?;
        let client = db.get().await?;
        let viewer = auth.authenticate(&**client,1,2,"rules-domains".into()).await?;
        drop(client);
        grants.set_direct(&admin, 2, "core.activity.view", Some(Direct::Allow(bilibi_authz::Reach::All))).await?;
        let activities = services.get::<bilibi_activity::Activities>(bilibi_activity::SERVICE)?;
        check(activities.get(&viewer, task).await.is_err(), "an uninvolved viewer cannot read the private follow-up")?;
        check(task_count(&db, kind, source).await? == 4, "refused writes left no extra task")?;
        let version = activities.get(&admin, task).await.map_err(|_| Error::Invalid)?.version;
        write(&plans, &admin, "core.activity.delete", json!({"id":task,"version":version})).await?;
        write(&plans, &admin, action, valid).await?;
        check(task_count(&db, kind, source).await? == 5, "deleting a rule task does not suppress a later request")?;
        Ok(())
    }.await;
    let _ = stop.send(true);
    let _ = worker.await;
    result?;
    println!("every {domain} rule follow-up check passed");
    Ok(())
}
