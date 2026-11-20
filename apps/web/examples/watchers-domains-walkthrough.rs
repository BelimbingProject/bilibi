//! Mounted proof of official Domain watchers on fresh disposable databases.
//! Run the companion scripts and source setup documented in
//! `docs/watchers-logical-event-handoff.md`; Supply Chain also requires its source hook.
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
                credential_binding: "watchers-domains-proof".into(),
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
async fn wait_for_runs(
    db: &Db,
    events: &[i64],
    watchers: &[i64],
    refusal: Option<&str>,
) -> Result<()> {
    for _ in 0..300 {
        let client = db.get().await?;
        let row = client.query_one(
            "SELECT count(*), count(*) FILTER (WHERE d.delivered_at IS NOT NULL AND
                CASE WHEN $3::text IS NULL THEN r.state='done' AND EXISTS(
                    SELECT 1 FROM base_queue_jobs j WHERE j.tenant_id=1 AND j.worker_id='base/watchers-run' AND j.args->>'event_id'=e.id::text AND j.args->>'run_id'=r.id::text AND j.state='completed'
                ) ELSE r.state='refused' AND r.reason=$3 AND r.finished_at IS NOT NULL AND
                    CASE WHEN $3='stale_activation' THEN EXISTS(
                        SELECT 1 FROM base_queue_jobs j WHERE j.tenant_id=1 AND j.worker_id='base/watchers-run' AND j.args->>'event_id'=e.id::text AND j.args->>'run_id'=r.id::text AND j.state='completed'
                    ) ELSE NOT EXISTS(
                        SELECT 1 FROM base_queue_jobs j WHERE j.tenant_id=1 AND j.worker_id='base/watchers-run' AND j.args->>'event_id'=e.id::text AND j.args->>'run_id'=r.id::text
                    ) END END),
                count(*) FILTER (WHERE r.state NOT IN ('queued','matched') AND
                    CASE WHEN $3 IS NULL THEN r.state<>'done' ELSE r.state<>'refused' OR r.reason IS DISTINCT FROM $3 END)
             FROM unnest($1::bigint[]) e(id) CROSS JOIN unnest($2::bigint[]) watcher(id)
             LEFT JOIN base_events_deliveries d ON d.event_id=e.id AND d.subscriber='base/watchers.triggers'
             LEFT JOIN base_watcher_runs r ON r.tenant_id=1 AND r.event_id=e.id AND r.watcher_id=watcher.id",
            &[&events, &watchers, &refusal],
        ).await?;
        if row.get::<_, i64>(2) != 0 {
            return check(false, "a source part had an unexpected watcher outcome");
        }
        if row.get::<_, i64>(0) == row.get::<_, i64>(1) {
            return Ok(());
        }
        drop(client);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    check(
        false,
        "timed out waiting for every source delivery and expected watcher outcome",
    )
}
async fn source_events(db: &Db, event: &str, subject: &str, source: i64) -> Result<Vec<i64>> {
    Ok(db
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
            let id = payload.pointer(subject)?;
            (id.as_i64() == Some(source) || id.as_str() == Some(source.to_string().as_str()))
                .then(|| row.get(0))
        })
        .collect())
}
async fn commerce_order(plans: &Plans, admin: &Scope) -> Result<i64> {
    let line = json!({"description":"Watchers proof","quantity":"1","unit":"hour","unit_price":"10.00","tax_code":"ZERO","tax_rate":"0"});
    let order = write(plans, admin, "commerce.sales.order.create", json!({"company_id":1,"customer_kind":"company","customer_code":"company_002","currency_code":"MYR","lines":[line.clone(),line]})).await?;
    order["id"].as_i64().ok_or(Error::Invalid)
}
async fn commerce_event(
    db: &Db,
    event: &str,
    company: i64,
    order: i64,
    status: &str,
    revision: i64,
) -> Result<i64> {
    let events: Vec<i64> = db.get().await?.query(
        "SELECT id FROM base_events WHERE tenant_id=1 AND publisher='commerce/sales' AND name=$1 AND subject=$2 ORDER BY id",
        &[&event, &format!("order:{order}")],
    ).await?.iter().map(|row| row.get(0)).collect();
    check(
        events.len() == 1,
        "the two-line order recorded one compact occurrence",
    )?;
    let id = events[0];
    let payload: Value = db
        .get()
        .await?
        .query_one(
            "SELECT payload FROM base_events WHERE tenant_id=1 AND id=$1",
            &[&id],
        )
        .await?
        .get(0);
    check(
        payload
            == json!({"company_id": company, "order_id": order, "status": status, "revision": revision}),
        "the compact occurrence names the correct order, company, status and revision",
    )?;
    Ok(id)
}
async fn run_until(
    services: &Services,
    ready: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    let executor = Executor::from_services(
        services,
        Config {
            concurrency: 2,
            poll_interval: Duration::from_millis(100),
            lease_seconds: 30,
            max_runtime: Duration::from_secs(60),
        },
    )?;
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(async move { executor.run(shutdown).await });
    let result = ready.await;
    let _ = stop.send(true);
    let stopped = worker.await.map_err(|_| Error::Internal)?;
    result?;
    stopped
}
async fn source_change(mode: &str, source: i64) -> Result<()> {
    let helper = std::env::var_os("BILIBI_WATCHERS_SOURCE_HELPER").ok_or_else(|| {
        eprintln!(
            "Supply Chain requires BILIBI_WATCHERS_SOURCE_HELPER from its watchers-followup.sh"
        );
        Error::Invalid
    })?;
    let status = tokio::process::Command::new(helper)
        .arg(mode)
        .arg(source.to_string())
        .status()
        .await
        .map_err(|error| {
            eprintln!("source helper {mode}: {error}");
            Error::Invalid
        })?;
    check(
        status.success(),
        "the source helper completed its supported write",
    )
}
async fn snapshot_tasks(
    db: &Db,
    events: &[i64],
    watchers: &[i64],
    tasks: i64,
    revisions: i64,
) -> Result<()> {
    let client = db.get().await?;
    for watcher in watchers {
        let row = client.query_one(
            "SELECT count(*),count(DISTINCT f.result->>'activity_id'),count(DISTINCT r.logical_event),count(DISTINCT r.facts->>'revision') FROM base_watcher_runs r JOIN base_watcher_effects f ON f.tenant_id=r.tenant_id AND f.run_id=r.id WHERE r.tenant_id=1 AND r.watcher_id=$1 AND r.event_id=ANY($2) AND r.state='done' AND f.state='done'",
            &[watcher, &events],
        ).await?;
        check(
            row.get::<_, i64>(0) == events.len() as i64
                && row.get::<_, i64>(1) == tasks
                && row.get::<_, i64>(2) == tasks
                && row.get::<_, i64>(3) == revisions,
            "each watcher has the expected task, immutable snapshot and evaluated revision counts",
        )?;
        let snapshots = client.query(
            "SELECT count(*),count(DISTINCT f.result->>'activity_id') FROM base_watcher_runs r JOIN base_watcher_effects f ON f.tenant_id=r.tenant_id AND f.run_id=r.id WHERE r.tenant_id=1 AND r.watcher_id=$1 AND r.event_id=ANY($2) AND r.state='done' AND f.state='done' GROUP BY r.logical_event",
            &[watcher, &events],
        ).await?;
        check(
            snapshots.len() as i64 == tasks
                && snapshots
                    .iter()
                    .all(|row| row.get::<_, i64>(0) >= 2 && row.get::<_, i64>(1) == 1),
            "all parts of each approval snapshot return exactly one task",
        )?;
    }
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    bilibi_base::link();
    bilibi_core::link();
    bilibi_domains::link();
    bilibi_extensions::link();
    let domain = std::env::args().nth(1).unwrap_or_else(|| "commerce".into());
    let (event, action, read, manage, kind, source_query) = match domain.as_str() {
        "commerce" => ("commerce/sales.order_transitioned", "commerce.sales.order.follow_up", "commerce.sales.view", "commerce.sales.followup.manage", "commerce.sales.order", "SELECT id,revision,company_id FROM commerce_sales_documents WHERE tenant_id=1 AND kind='order' AND status='confirmed' ORDER BY id DESC LIMIT 1"),
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
        .authenticate(&**client, 1, 4, "watchers-domains".into())
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
            json!({"name":"Watchers proof administrator","kind":"human"}),
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
        .authenticate(&**client, 1, 4, "watchers-domains".into())
        .await?;
    drop(client);
    // The proof uses real permission grants, event consent and the watchers principal.
    grants
        .set_system(
            &admin,
            "domain-events",
            "events.delivery.deliver",
            Some(true),
        )
        .await?;
    for key in [
        "watchers.run.execute",
        read,
        manage,
        "core.activity.system.create",
    ] {
        grants
            .set_system(&admin, "watchers", key, Some(true))
            .await?;
    }
    let (source, company): (i64, i64) = match domain.as_str() {
        "commerce" => (commerce_order(&plans, &admin).await?, 1),
        "people" => {
            let client = db.get().await?;
            let row = client.query_one(
                "SELECT company_id,claim_type_id,(incurred_on+1)::text,currency,submitted_by_actor_id FROM people_claim_submissions WHERE tenant_id=1 AND status='approved' ORDER BY id DESC LIMIT 1",
                &[],
            ).await?;
            let company: i64 = row.get(0);
            let staff = auth
                .authenticate(&**client, 1, row.get(4), "watchers-domains".into())
                .await?;
            drop(client);
            let claim = write(&plans, &staff, "people.claims.request.submit", json!({
                "company_id":company,"claim_type_id":row.get::<_,i64>(1),"incurred_on":row.get::<_,String>(2),
                "currency":row.get::<_,String>(3),"amount":"10.00","description":"Watchers proof"
            })).await?;
            (claim["id"].as_i64().ok_or(Error::Invalid)?, company)
        }
        _ => {
            let row = db
                .get()
                .await?
                .query_opt(source_query, &[])
                .await?
                .ok_or_else(|| {
                    eprintln!(
                        "Run the {domain} source walkthrough first; no approved source remains."
                    );
                    Error::Invalid
                })?;
            (row.get(0), row.get(2))
        }
    };
    let recipient = 4_i64;
    let draft = json!({
        "name":format!("{domain} follow-up proof"),"watch":event,"if": {"match": "all", "conditions":[{"field":"id","op":"eq","value":source.to_string()},{"field":"company_id","op":"eq","value":company.to_string()}]},
        "do":[{"operation":action,"input":json!({"assignee_user_id":recipient,"title":"Review the approved record"})}],
        "examples":[{"facts":json!({"id":source,"company_id":company}).to_string(),"expect":"match"},{"facts":json!({"id":source+1,"company_id":company}).to_string(),"expect":"no_match"}],
        "explanation":"Create one private follow-up after the owner event, without changing its decision or payment facts."
    });
    let mut watcher_ids = Vec::new();
    for name in ["First follow-up watcher", "Independent follow-up watcher"] {
        let mut draft = draft.clone();
        draft["name"] = json!(name);
        let watcher = write(&plans, &admin, "base.watchers.draft", draft).await?;
        if watcher_ids.is_empty() {
            let client = db.get().await?;
            check(
                bilibi_authz::decide(&**client, &admin, bilibi_activity::MANAGE)
                    .await?
                    .allowed,
                "the normal approver may manage activities",
            )?;
            drop(client);
            grants
                .set_direct(
                    &admin,
                    admin.user(),
                    bilibi_activity::MANAGE,
                    Some(Direct::Deny),
                )
                .await?;
            let client = db.get().await?;
            for key in [read, manage, "admin.watchers.approve"] {
                check(
                    bilibi_authz::decide(&**client, &admin, key).await?.allowed,
                    "the restricted approver retains Domain and Watchers permissions",
                )?;
            }
            check(
                !bilibi_authz::decide(&**client, &admin, bilibi_activity::MANAGE)
                    .await?
                    .allowed,
                "the direct deny removes human activity management",
            )?;
            drop(client);
            check(
                write(
                    &plans,
                    &admin,
                    "base.watchers.approve",
                    json!({"watcher_id":watcher["watcher_id"],"version":watcher["version"]}),
                )
                .await
                .is_err(),
                "approval refuses a human denied the nested activity permission",
            )?;
            let inactive: bool = db
                .get()
                .await?
                .query_one(
                    "SELECT active_version_id IS NULL FROM base_watchers WHERE tenant_id=1 AND id=$1",
                    &[&watcher["watcher_id"].as_i64().ok_or(Error::Invalid)?],
                )
                .await?
                .get(0);
            check(inactive, "refused approval leaves the draft inactive")?;
            grants
                .set_direct(&admin, admin.user(), bilibi_activity::MANAGE, None)
                .await?;
        }

        write(
            &plans,
            &admin,
            "base.watchers.approve",
            json!({"watcher_id":watcher["watcher_id"],"version":watcher["version"]}),
        )
        .await?;
        if watcher_ids.is_empty() {
            write(
                &plans,
                &admin,
                "base.watchers.switch",
                json!({"watcher_id":watcher["watcher_id"],"enabled":false}),
            )
            .await?;
            grants
                .set_direct(
                    &admin,
                    admin.user(),
                    bilibi_activity::MANAGE,
                    Some(Direct::Deny),
                )
                .await?;
            check(
                write(
                    &plans,
                    &admin,
                    "base.watchers.switch",
                    json!({"watcher_id":watcher["watcher_id"],"enabled":true}),
                )
                .await
                .is_err(),
                "re-enabling refuses a human denied the nested activity permission",
            )?;
            let disabled: bool = db
                .get()
                .await?
                .query_one(
                    "SELECT disabled_at IS NOT NULL FROM base_watchers WHERE tenant_id=1 AND id=$1",
                    &[&watcher["watcher_id"].as_i64().ok_or(Error::Invalid)?],
                )
                .await?
                .get(0);
            check(disabled, "refused re-enabling leaves the watcher off")?;
            grants
                .set_direct(&admin, admin.user(), bilibi_activity::MANAGE, None)
                .await?;
            write(
                &plans,
                &admin,
                "base.watchers.switch",
                json!({"watcher_id":watcher["watcher_id"],"enabled":true}),
            )
            .await?;
            let enabled: bool = db.get().await?.query_one(
                "SELECT active_version_id IS NOT NULL AND disabled_at IS NULL FROM base_watchers WHERE tenant_id=1 AND id=$1",
                &[&watcher["watcher_id"].as_i64().ok_or(Error::Invalid)?],
            ).await?.get(0);
            check(
                enabled,
                "the normally authorized human approves and re-enables the watcher",
            )?;
        }
        watcher_ids.push(watcher["watcher_id"].as_i64().ok_or(Error::Invalid)?);
    }
    let trigger = services
        .catalog()?
        .entries::<bilibi_base::watchers::Trigger>(bilibi_base::watchers::TRIGGERS)?
        .iter()
        .find(|entry| entry.id == event)
        .ok_or(Error::Invalid)?
        .get::<bilibi_base::watchers::Trigger>()?;
    let expected_events = match domain.as_str() {
        "commerce" => {
            write(
                &plans,
                &admin,
                "commerce.sales.order.transition",
                json!({"company_id":company,"id":source,"revision":1,"status":"confirmed"}),
            )
            .await?;
            vec![commerce_event(&db, event, company, source, "confirmed", 2).await?]
        }
        "people" => {
            let client = db.get().await?;
            let row = client.query_one(
                "SELECT decided_by_actor_id FROM people_claim_submissions WHERE tenant_id=1 AND company_id=$1 AND status='approved' ORDER BY id DESC LIMIT 1",
                &[&company],
            ).await?;
            let manager = auth
                .authenticate(&**client, 1, row.get(0), "watchers-domains".into())
                .await?;
            drop(client);
            write(
                &plans,
                &manager,
                "people.claims.request.approve",
                json!({"company_id":company,"request_id":source,"approved_amount":"10.00","decision_reason":"Watchers proof"}),
            )
            .await?;
            source_events(&db, event, trigger.subject, source).await?
        }
        _ => {
            let previous = source_events(&db, event, trigger.subject, source).await?;
            source_change("reapprove", source).await?;
            source_events(&db, event, trigger.subject, source)
                .await?
                .into_iter()
                .filter(|id| !previous.contains(id))
                .collect()
        }
    };
    check(
        !expected_events.is_empty(),
        "the source recorded its expected events",
    )?;
    if domain == "supply_chain" {
        check(
            expected_events.len() == 4,
            "reapproval recorded both original and both replacement line parts",
        )?;
    }
    db.get()
        .await?
        .batch_execute(&format!(
            "CREATE SEQUENCE watchers_followup_retry_probe;
         CREATE FUNCTION watchers_followup_retry_probe() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NEW.tenant_id=1 AND NEW.watcher_id={} AND NEW.event_id={} AND NEW.state='done' THEN
             IF nextval('watchers_followup_retry_probe')=1 THEN
               RAISE EXCEPTION 'walkthrough failure after effect commit';
             END IF;
           END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER watchers_followup_retry_probe BEFORE UPDATE ON base_watcher_runs
         FOR EACH ROW EXECUTE FUNCTION watchers_followup_retry_probe();",
            watcher_ids[0], expected_events[0],
        ))
        .await?;
    let result: Result<()> = async {
        if domain == "supply_chain" {
            let mut client = db.get().await?;
            let held = client.transaction().await?;
            held.query(
                "SELECT id FROM base_queue_jobs WHERE tenant_id=1 AND worker_id='base/events-delivery' AND args->>'event_id'<>$1 FOR UPDATE",
                &[&expected_events[0].to_string()],
            ).await?;
            run_until(&services, wait_for_runs(&db, &expected_events[..1], &watcher_ids, None)).await?;
            let second = held.query_one(
                "SELECT d.delivered_at IS NULL, NOT EXISTS(SELECT 1 FROM base_watcher_runs r WHERE r.tenant_id=1 AND r.event_id=d.event_id) FROM base_events_deliveries d WHERE d.event_id=$1 AND d.subscriber='base/watchers.triggers'",
                &[&expected_events[1]],
            ).await?;
            check(second.get(0) && second.get(1), "the second source part has neither delivered nor evaluated")?;
            check(task_count(&db, kind, source).await? == 2, "part one created exactly one task per independent watcher")?;
            source_change("amend-header", source).await?;
            held.rollback().await?;
            drop(client);
            run_until(&services, wait_for_runs(&db, &expected_events, &watcher_ids, None)).await?;
            snapshot_tasks(&db, &expected_events, &watcher_ids, 1, 2).await?;
            check(task_count(&db, kind, source).await? == 2, "a header amendment between parts leaves exactly one task per watcher")?;
        } else {
            run_until(&services, wait_for_runs(&db, &expected_events, &watcher_ids, None)).await?;
        }
        let retry = db.get().await?.query_one(
            "SELECT EXISTS(SELECT 1 FROM base_queue_jobs WHERE tenant_id=1 AND worker_id='base/watchers-run' AND args->>'event_id'=$1 AND state='completed' AND attempt>=2), (SELECT last_value FROM watchers_followup_retry_probe)",
            &[&expected_events[0].to_string()],
        ).await?;
        check(retry.get::<_,bool>(0) && retry.get::<_,i64>(1)>=2, "the worker retried after the committed effect and completed successfully")?;
        check(task_count(&db, kind, source).await? == 2, "each independent watcher created one task across all source parts")?;
        let client = db.get().await?;
        let rows = client.query("SELECT shared,created_by,assignee_user_id,id FROM core_activities WHERE tenant_id=1 AND related_kind=$1 AND related_id=$2 ORDER BY id", &[&kind,&source.to_string()]).await?;
        check(rows.iter().all(|row| !row.get::<_,bool>(0) && row.get::<_,i64>(1)==recipient && row.get::<_,i64>(2)==recipient), "both tasks are private and addressed to the configured recipient")?;
        let task: i64 = rows[0].get(3);
        for watcher in &watcher_ids {
            let row = client.query_one("SELECT count(DISTINCT f.result->>'activity_id'),count(*) FROM base_watcher_effects f JOIN base_watcher_runs r ON r.tenant_id=f.tenant_id AND r.id=f.run_id WHERE f.tenant_id=1 AND r.watcher_id=$1 AND r.event_id=ANY($2) AND f.state='done'", &[watcher, &expected_events]).await?;
            check(row.get::<_,i64>(0)==1 && row.get::<_,i64>(1)==expected_events.len() as i64, "every part of one watcher returned the same task")?;
        }
        drop(client);
        let mut watcher_tasks = 2;
        if domain == "supply_chain" {
            source_change("reapprove", source).await?;
            source_change("reapprove", source).await?;
            let last_expected = *expected_events.last().ok_or(Error::Invalid)?;
            let approvals: Vec<i64> = source_events(&db, event, trigger.subject, source).await?
                .into_iter().filter(|&id| id > last_expected).collect();
            let client = db.get().await?;
            let snapshots = client.query(
                "SELECT count(*),count(DISTINCT payload->>'part_index'),min((payload->>'part_index')::bigint),max((payload->>'part_index')::bigint),min((payload->>'part_count')::bigint),max((payload->>'part_count')::bigint) FROM base_events WHERE tenant_id=1 AND id=ANY($1) GROUP BY payload->>'snapshot_key'",
                &[&approvals],
            ).await?;
            check(snapshots.len()==2 && snapshots.iter().all(|row| {
                let count: i64 = row.get(0);
                count>=2 && row.get::<_,i64>(1)==count && row.get::<_,i64>(2)==1 && row.get::<_,i64>(3)==count && row.get::<_,i64>(4)==count && row.get::<_,i64>(5)==count
            }), "two real reapprovals recorded every declared snapshot part, including retained line generations")?;
            let undelivered: i64 = client.query_one(
                "SELECT count(*) FROM base_events_deliveries d JOIN base_events e ON e.id=d.event_id WHERE e.tenant_id=1 AND e.id=ANY($1) AND d.subscriber='base/watchers.triggers' AND d.delivered_at IS NULL",
                &[&approvals],
            ).await?.get(0);
            let evaluated: i64 = client.query_one("SELECT count(*) FROM base_watcher_runs WHERE tenant_id=1 AND event_id=ANY($1)", &[&approvals]).await?.get(0);
            check(undelivered==approvals.len() as i64 && evaluated==0, "both approval snapshots wait until after the latest approval")?;
            drop(client);
            run_until(&services, wait_for_runs(&db, &approvals, &watcher_ids, None)).await?;
            snapshot_tasks(&db, &approvals, &watcher_ids, 2, 1).await?;
            watcher_tasks += 4;
            check(task_count(&db, kind, source).await? == watcher_tasks, "two delayed approval snapshots create two new tasks per watcher at one live revision")?;
            // The companion helper may relink its fixture administrator to a
            // new Person. Resume human operations with the current actor proof.
            let client = db.get().await?;
            admin = auth
                .authenticate(&**client, 1, 4, "watchers-domains".into())
                .await?;
        }
        let replay = write(&plans, &admin, "base.events.replay", json!({"subscriber":"base/watchers.triggers","from_event":expected_events[0],"limit":1000})).await?;
        check(replay["created"] == 0 && replay["pending"] == 0, "replay of delivered parts is a no-op, not duplicate execution")?;
        check(task_count(&db, kind, source).await? == watcher_tasks, "no-op replay leaves task counts unchanged")?;
        let row = db.get().await?.query_one(source_query, &[]).await?;
        check(row.get::<_,i64>(0)==source, "manual checks still target the same live source")?;
        let revision: i64 = row.get(1);
        let valid = json!({"record_id":source,"expected_revision":revision,"assignee_user_id":recipient,"title":"Review the approved record"});
        write(&plans, &admin, action, valid.clone()).await?;
        check(task_count(&db, kind, source).await? == watcher_tasks + 1, "a manual request is independent of both watchers")?;
        write(&plans, &admin, action, valid.clone()).await?;
        check(task_count(&db, kind, source).await? == watcher_tasks + 2, "identical manual requests create independent tasks")?;
        let mut stale = valid.clone(); stale["expected_revision"] = json!(revision+1);
        check(write(&plans,&admin,action,stale).await.is_err(), "a stale source revision is refused")?;
        let mut foreign = valid.clone(); foreign["assignee_user_id"] = json!(5);
        check(write(&plans,&admin,action,foreign).await.is_err(), "another tenant's recipient is refused")?;
        let client = db.get().await?;
        let viewer = auth.authenticate(&**client,1,2,"watchers-domains".into()).await?;
        drop(client);
        grants.set_direct(&admin, 2, "core.activity.view", Some(Direct::Allow(bilibi_authz::Reach::All))).await?;
        let activities = services.get::<bilibi_activity::Activities>(bilibi_activity::SERVICE)?;
        check(activities.get(&viewer, task).await.is_err(), "an uninvolved viewer cannot read the private follow-up")?;
        check(task_count(&db, kind, source).await? == watcher_tasks + 2, "refused writes left no extra task")?;
        let version = activities.get(&admin, task).await.map_err(|_| Error::Invalid)?.version;
        write(&plans, &admin, "core.activity.delete", json!({"id":task,"version":version})).await?;
        write(&plans, &admin, action, valid).await?;
        check(task_count(&db, kind, source).await? == watcher_tasks + 3, "deleting a watcher task does not suppress a later request")?;
        Ok(())
    }.await;
    db.get().await?.batch_execute(
        "DROP TRIGGER watchers_followup_retry_probe ON base_watcher_runs; DROP FUNCTION watchers_followup_retry_probe(); DROP SEQUENCE watchers_followup_retry_probe;",
    ).await?;
    result?;
    if domain == "commerce" {
        grants
            .set_system(&admin, "watchers", "commerce.sales.manage", Some(true))
            .await?;
        grants
            .set_system(&admin, "watchers", "commerce.sales.confirm", Some(true))
            .await?;
        let created = "commerce/sales.order_created";
        let backlog = commerce_order(&plans, &admin).await?;
        let backlog_event = commerce_event(&db, created, 1, backlog, "draft", 1).await?;
        write(
            &plans,
            &admin,
            "commerce.sales.order.transition",
            json!({"company_id":1,"id":backlog,"revision":1,"status":"confirmed"}),
        )
        .await?;
        commerce_event(&db, event, 1, backlog, "confirmed", 2).await?;
        let definition = json!({"name":"Cancel newly created orders", "watch":created, "do":[{"operation":"commerce.sales.order.transition","input":{"status":"cancelled","reason":"Proof cancellation"}}], "examples":[], "explanation":"Cancel a new order in the disposable walkthrough."});
        let watcher = write(&plans, &admin, "base.watchers.draft", definition).await?;
        write(
            &plans,
            &admin,
            "base.watchers.approve",
            json!({"watcher_id":watcher["watcher_id"],"version":watcher["version"]}),
        )
        .await?;
        let id = commerce_order(&plans, &admin).await?;
        let creation_event = commerce_event(&db, created, 1, id, "draft", 1).await?;
        let watcher_id = watcher["watcher_id"].as_i64().ok_or(Error::Invalid)?;
        run_until(&services, async {
            wait_for_runs(
                &db,
                &[backlog_event],
                &[watcher_id],
                Some("event_before_approval"),
            )
            .await?;
            wait_for_runs(&db, &[creation_event], &[watcher_id], None).await
        })
        .await?;
        let untouched: bool = db.get().await?.query_one(
            "SELECT status='confirmed' AND revision=2 FROM commerce_sales_documents WHERE tenant_id=1 AND id=$1",
            &[&backlog],
        ).await?.get(0);
        check(
            untouched,
            "the backlogged creation left the previously confirmed order unchanged",
        )?;
        let row = db
            .get()
            .await?
            .query_one(
                "SELECT status,revision FROM commerce_sales_documents WHERE tenant_id=1 AND id=$1",
                &[&id],
            )
            .await?;
        check(
            row.get::<_, String>(0) == "cancelled" && row.get::<_, i64>(1) == 2,
            "the creation watcher cancelled the order once",
        )?;
        commerce_event(&db, event, 1, id, "cancelled", 2).await?;
        write(
            &plans,
            &admin,
            "base.watchers.switch",
            json!({"watcher_id":watcher_id,"enabled":false}),
        )
        .await?;
        let while_off = commerce_order(&plans, &admin).await?;
        let off_event = commerce_event(&db, created, 1, while_off, "draft", 1).await?;
        run_until(
            &services,
            wait_for_runs(&db, &[off_event], &[watcher_id], Some("watcher_off")),
        )
        .await?;
        let delayed = commerce_order(&plans, &admin).await?;
        let delayed_event = commerce_event(&db, created, 1, delayed, "draft", 1).await?;
        write(
            &plans,
            &admin,
            "base.watchers.switch",
            json!({"watcher_id":watcher_id,"enabled":true}),
        )
        .await?;
        run_until(
            &services,
            wait_for_runs(
                &db,
                &[delayed_event],
                &[watcher_id],
                Some("event_before_enabled"),
            ),
        )
        .await?;
        // Hold only the Watchers job, so Events can enqueue a real run before
        // the off/on cycle. The disposable queue is released immediately after.
        db.get().await?.batch_execute("CREATE FUNCTION watcher_activation_hold() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.worker_id='base/watchers-run' THEN NEW.available_at=clock_timestamp()+interval '1 hour'; NEW.state='scheduled'; END IF; RETURN NEW; END $$; CREATE TRIGGER watcher_activation_hold BEFORE INSERT ON base_queue_jobs FOR EACH ROW EXECUTE FUNCTION watcher_activation_hold();").await?;
        let queued = commerce_order(&plans, &admin).await?;
        let queued_event = commerce_event(&db, created, 1, queued, "draft", 1).await?;
        run_until(&services, async {
            for _ in 0..300 {
                let exists: bool = db.get().await?.query_one("SELECT EXISTS(SELECT 1 FROM base_watcher_runs WHERE tenant_id=1 AND watcher_id=$1 AND event_id=$2 AND state='queued')", &[&watcher_id,&queued_event]).await?.get(0);
                if exists { return Ok(()); }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            check(false, "the event queued its run before switching off")
        }).await?;
        write(
            &plans,
            &admin,
            "base.watchers.switch",
            json!({"watcher_id":watcher_id,"enabled":false}),
        )
        .await?;
        write(
            &plans,
            &admin,
            "base.watchers.switch",
            json!({"watcher_id":watcher_id,"enabled":true}),
        )
        .await?;
        let client = db.get().await?;
        client.batch_execute("DROP TRIGGER watcher_activation_hold ON base_queue_jobs; DROP FUNCTION watcher_activation_hold();").await?;
        client.execute("UPDATE base_queue_jobs SET state='available',available_at=clock_timestamp() WHERE tenant_id=1 AND worker_id='base/watchers-run' AND args->>'event_id'=$1", &[&queued_event.to_string()]).await?;
        drop(client);
        run_until(
            &services,
            wait_for_runs(
                &db,
                &[queued_event],
                &[watcher_id],
                Some("stale_activation"),
            ),
        )
        .await?;
        let after_on = commerce_order(&plans, &admin).await?;
        let after_event = commerce_event(&db, created, 1, after_on, "draft", 1).await?;
        run_until(
            &services,
            wait_for_runs(&db, &[after_event], &[watcher_id], None),
        )
        .await?;
        let client = db.get().await?;
        let row = client.query_one("SELECT count(*) FILTER (WHERE id=ANY($1) AND status='draft'), count(*) FILTER (WHERE id=$2 AND status='cancelled') FROM commerce_sales_documents WHERE tenant_id=1", &[&vec![while_off,delayed,queued],&after_on]).await?;
        check(
            row.get::<_, i64>(0) == 3 && row.get::<_, i64>(1) == 1,
            "off-period events are skipped and a new event acts after switching on",
        )?;
    }
    println!("every {domain} watcher follow-up check passed");
    Ok(())
}
