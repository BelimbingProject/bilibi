//! Running proof of tenant rules (`base/rules`, ADR 28, #682) on a fresh
//! disposable database, migrated and seeded first (`bilibi-web migrate`,
//! then `BILIBI_ENV=development bilibi-web seed-production`, which stores the
//! Dev instance role so grant changes ask for no second factor nobody
//! enrolled). Two compositions:
//!
//! ```text
//! cargo run -p bilibi-web --example rules-walkthrough           # 1. nothing mounted
//! cp -r fixtures/containers/rule_cases apps/domains/ && python3 scripts/discover-modules.py
//! cargo run -p bilibi-web --example rules-walkthrough           # 2. the reference owner mounted
//! rm -r apps/domains/rule_cases && python3 scripts/discover-modules.py && git checkout -- Cargo.lock
//! ```
//!
//! Unmounted, the engine boots with no trigger or action, and its contracts
//! answer an empty vocabulary and refuse a draft on an unknown trigger. With
//! the reference owner (`fixtures/containers/rule_cases`, a case with a
//! priority, an amount, a margin and a queue) mounted, two tenants get the
//! same rule over the same facts with different destinations, and each
//! affects only its own case. Then: a restart between the commit and the
//! rule's job gives one effect, and so does a retry after the effect
//! committed; an agent drafts a second rule that a person approves; a denied
//! capability, a restricted field, an archived company, a suspended tenant,
//! a stale version and a switched-off rule each stop the effect; a rule that
//! would start itself is refused at approval, and a two-rule loop is refused and a noncyclic chain stops at
//! the chain budget; a threshold change is a new approved version, no
//! release; and a burst in one tenant waits its turn while the other tenant's
//! rule runs at once. Every check prints `ok` or `FAIL`; one failure ends the
//! run with an error.
use bilibi_agent_api::plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans};
use bilibi_authz::grants::Grants;
use bilibi_base::events::{Events, DELIVER, PRINCIPAL as DELIVERY_PRINCIPAL};
use bilibi_base::queue::{Config, Executor};
use bilibi_base::rules::{Rules, SERVICE};
use bilibi_database::{pool, Db, Error, MutationBoundary, Result};
use bilibi_module_registry::{verify_composition, Services};
use bilibi_tenancy::{Authentication, ProofFuture, Scope};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{sync::Arc, time::Duration};
use tokio_postgres::Transaction;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const OPENED: &str = "rule_cases/cases.opened";
const ROUTED: &str = "rule_cases/cases.routed";
const FLAGGED: &str = "rule_cases/cases.flagged";
const RULE_CAPABILITIES: [&str; 4] = [
    "rules.run.execute",
    "rule-cases.case.view",
    "rule-cases.case.route",
    "rule-cases.case.flag",
];

static FAILURES: AtomicUsize = AtomicUsize::new(0);
fn check(ok: bool, what: &str) {
    if !ok {
        FAILURES.fetch_add(1, Ordering::SeqCst);
    }
    println!("{} {what}", if ok { "ok  " } else { "FAIL" });
}

/// The plan door's credential check, as a browser session or agent
/// connection proves it; the scope was authenticated above.
struct Held<'a>(&'a Scope);
impl PlanAuthority for Held<'_> {
    fn verify<'a>(&'a self, _tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            Ok(PlanIdentity {
                scope: self.0.clone(),
                credential_binding: "walkthrough:rules".into(),
            })
        })
    }
}

struct Proof {
    db: Db,
    services: Services,
    plans: Arc<Plans>,
    rules: Arc<Rules>,
    one: Scope,
    two: Scope,
    agent: Scope,
    executor: Option<(
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    )>,
}
impl Proof {
    fn start(&mut self) -> Result<()> {
        let executor = Executor::from_services(
            &self.services,
            Config {
                concurrency: 4,
                poll_interval: Duration::from_millis(100),
                lease_seconds: 30,
                max_runtime: Duration::from_secs(60),
            },
        )?;
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let _ = executor.run(shutdown).await;
        });
        self.executor = Some((stop, handle));
        Ok(())
    }
    /// Stops the queue, as a server stopping between a commit and the job.
    async fn stop(&mut self) {
        if let Some((stop, handle)) = self.executor.take() {
            let _ = stop.send(true);
            let _ = handle.await;
        }
    }
    /// A reviewed write through the agent door's plans: schema, capability,
    /// the owner's preview, then its commit.
    async fn write(
        &self,
        scope: &Scope,
        key: &str,
        input: Value,
    ) -> std::result::Result<Value, String> {
        let op = PlannedOperation {
            key: key.into(),
            input,
        };
        self.plans
            .commit(scope, &Held(scope), std::slice::from_ref(&op))
            .await
            .map(|result| result["results"][0]["result"]["result"].clone())
            .map_err(|error| match error {
                bilibi_agent_api::Error::PlanRejected(results) => results.to_string(),
                bilibi_agent_api::Error::InvalidInput(fields) => format!("{fields:?}"),
                other => other.to_string(),
            })
    }
    async fn review(&self, scope: &Scope, key: &str, input: Value) -> Value {
        let op = PlannedOperation {
            key: key.into(),
            input,
        };
        self.plans
            .preview(&Held(scope), std::slice::from_ref(&op))
            .await
            .map(|review| review["results"][0].clone())
            .unwrap_or_else(|error| json!({"error": error.to_string()}))
    }
    async fn draft(&self, scope: &Scope, rule: Value) -> Result<(i64, i64)> {
        match self.write(scope, "base.rules.draft", rule).await {
            Ok(result) => Ok((
                result["rule_id"].as_i64().unwrap_or_default(),
                result["version"].as_i64().unwrap_or_default(),
            )),
            Err(why) => {
                println!("     draft refused: {why}");
                Err(Error::Invalid)
            }
        }
    }
    async fn approve(
        &self,
        scope: &Scope,
        rule: i64,
        version: i64,
    ) -> std::result::Result<Value, String> {
        self.write(
            scope,
            "base.rules.approve",
            json!({"rule_id": rule, "version": version}),
        )
        .await
    }
    async fn switch(
        &self,
        scope: &Scope,
        rule: i64,
        on: bool,
    ) -> std::result::Result<Value, String> {
        self.write(
            scope,
            "base.rules.switch",
            json!({"rule_id": rule, "enabled": on}),
        )
        .await
    }
    async fn open(
        &self,
        scope: &Scope,
        company: i64,
        title: &str,
        priority: &str,
        amount: &str,
    ) -> Result<i64> {
        let result = self
            .write(scope, "rule_cases.cases.open", json!({"company_id": company, "title": title, "priority": priority, "amount": amount, "margin": "250.00"}))
            .await
            .map_err(|why| {
                println!("     open refused: {why}");
                Error::Invalid
            })?;
        result["case_id"].as_i64().ok_or(Error::Invalid)
    }
    async fn case(&self, id: i64) -> Result<(Option<String>, i32, i32)> {
        let row = self
            .db
            .get()
            .await?
            .query_one(
                "SELECT queue,routed,flags FROM rule_cases_cases WHERE id=$1",
                &[&id],
            )
            .await?;
        Ok((row.get(0), row.get(1), row.get(2)))
    }
    /// Waits until no delivery or rule job is left to run (scheduled ones,
    /// deferred to a later minute, are not waited for).
    async fn settled(&self) -> Result<()> {
        for _ in 0..600 {
            let open: i64 = self.db.get().await?.query_one(
                "SELECT count(*) FROM base_queue_jobs WHERE worker_id IN ('base/events-delivery','base/rules-run') AND state IN ('available','executing','retryable') AND available_at <= now() + interval '2 seconds'",
                &[],
            ).await?.get(0);
            if open == 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!("timed out waiting for the queue");
        Err(Error::Internal)
    }
    /// The receipts of runs on one case: (rule, state, reason, effects).
    async fn receipts(
        &self,
        case: i64,
    ) -> Result<Vec<(i64, String, Option<String>, Vec<(String, Option<String>)>)>> {
        let client = self.db.get().await?;
        let rows = client
            .query(
                "SELECT id,rule_id,state,reason FROM base_rule_runs WHERE subject=$1 ORDER BY id",
                &[&case.to_string()],
            )
            .await?;
        let mut receipts = Vec::new();
        for row in rows {
            let run: i64 = row.get(0);
            let effects = client
                .query(
                    "SELECT state,reason FROM base_rule_effects WHERE run_id=$1 ORDER BY position",
                    &[&run],
                )
                .await?
                .iter()
                .map(|e| (e.get(0), e.get(1)))
                .collect();
            receipts.push((row.get(1), row.get(2), row.get(3), effects));
        }
        Ok(receipts)
    }
    async fn show(&self, case: i64) -> Result<()> {
        for (rule, state, reason, effects) in self.receipts(case).await? {
            println!(
                "     case {case}: rule {rule} {state}{} effects {effects:?}",
                reason.map(|r| format!(" ({r})")).unwrap_or_default()
            );
        }
        Ok(())
    }
    async fn grant(
        &self,
        scope: &Scope,
        principal: &str,
        key: &str,
        allowed: Option<bool>,
    ) -> Result<()> {
        self.services
            .get::<Grants>("base/authz.grants")?
            .set_system(scope, principal, key, allowed)
            .await
    }
    async fn setting(&self, scope: &Scope, key: &str, value: Value) -> Result<()> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .services
            .get::<MutationBoundary>("base/database.mutations")?
            .begin(
                &mut client,
                scope.mutation_context("rules.walkthrough.setting"),
            )
            .await?;
        self.services
            .get::<bilibi_base::settings::Settings>("base/settings.store")?
            .put(
                &mut tx,
                scope,
                bilibi_base::settings::Target::Tenant,
                key,
                value,
            )
            .await?;
        tx.commit().await
    }
}

fn urgent_rule(name: &str, queue: &str, threshold: &str) -> Value {
    json!({
        "name": name, "trigger": OPENED, "match": "all",
        "conditions": [
            {"field": "priority", "op": "eq", "value": "urgent"},
            {"field": "amount", "op": "gt", "value": threshold}
        ],
        "actions": [{"operation": "rule_cases.cases.route", "input": json!({"queue": queue}).to_string()}],
        "examples": [
            {"facts": json!({"priority": "urgent", "amount": "15000"}).to_string(), "expect": "match"},
            {"facts": json!({"priority": "urgent", "amount": "1.00"}).to_string(), "expect": "no_match"},
            {"facts": json!({"priority": "normal", "amount": "50000"}).to_string(), "expect": "no_match"}
        ],
        "explanation": format!("Send new urgent cases over {threshold} to {queue}.")
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    bilibi_base::link();
    bilibi_core::link();
    bilibi_domains::link();
    bilibi_extensions::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_module_registry::discover()?;
    verify_composition(
        &modules,
        bilibi_domains::MODULES,
        bilibi_extensions::MODULES,
    )?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    services
        .get::<Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    // Tenant 2's manager becomes its administrator (a grant-all role), as a
    // tenant's owner would be.
    let client = db.get().await?;
    client.batch_execute(
        "INSERT INTO roles(tenant_id,name,grant_all) SELECT 2,'Walkthrough administrator',true WHERE NOT EXISTS (SELECT 1 FROM roles WHERE tenant_id=2 AND name='Walkthrough administrator');
         INSERT INTO principal_roles(tenant_id,user_id,role_id) SELECT 2,5,id FROM roles WHERE tenant_id=2 AND name='Walkthrough administrator' ON CONFLICT DO NOTHING;",
    ).await?;
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    let one = authentication
        .authenticate(&**client, 1, 4, "rules".into())
        .await?;
    let two = authentication
        .authenticate(&**client, 2, 5, "rules".into())
        .await?;
    // Tenant 1's manager may draft rules (not approve them), and has an AI
    // agent: a live connection the person granted, as Core Agent stores it
    // once the person approves it. The agent works at the manager's desk.
    let grants = services.get::<Grants>("base/authz.grants")?;
    // The reference owner's read key exists only while it is mounted.
    let reference = bilibi_authz::vocabulary()?
        .capabilities
        .contains_key("rule-cases.case.view");
    for key in [
        "admin.rules.view",
        "admin.rules.manage",
        "rule-cases.case.view",
    ]
    .into_iter()
    .filter(|key| reference || !key.starts_with("rule-cases."))
    {
        grants
            .set_direct(
                &one,
                1,
                key,
                Some(bilibi_authz::grants::Direct::Allow(
                    bilibi_authz::Reach::All,
                )),
            )
            .await?;
    }
    let manager = authentication
        .authenticate(&**client, 1, 1, "rules".into())
        .await?;
    let connection: i64 = client.query_one(
        "INSERT INTO core_agent_connections(tenant_id,user_id,person_id,agent_id,expires_at) VALUES(1,1,1,'rules-walkthrough-agent',now()+interval '1 hour') RETURNING id",
        &[],
    ).await?.get(0);
    drop(client);
    let agent = manager.for_agent(connection, None, "rules-walkthrough-agent".into(), None);
    let mut proof = Proof {
        db: db.clone(),
        plans: services.get("base/agent_api.plans")?,
        rules: services.get(SERVICE)?,
        services,
        one,
        two,
        agent,
        executor: None,
    };
    let events = proof.services.get::<Events>(bilibi_base::events::SERVICE)?;
    let mounted = events.event_types().any(|name| name == OPENED);
    println!(
        "mounted subscribers: {:?}",
        events.subscribers().collect::<Vec<_>>()
    );
    let vocabulary = {
        let client = db.get().await?;
        proof.rules.vocabulary(&**client, &proof.one).await?
    };
    println!(
        "vocabulary: triggers {} actions {}",
        vocabulary["triggers"],
        vocabulary["actions"].as_array().map_or(0, Vec::len)
    );
    if !mounted {
        check(
            vocabulary["triggers"].as_array().is_some_and(Vec::is_empty),
            "with no owner mounted the engine offers no trigger",
        );
        let refused = proof
            .write(
                &proof.one,
                "base.rules.draft",
                urgent_rule("Urgent", "priority", "1000"),
            )
            .await;
        check(
            refused
                .as_ref()
                .is_err_and(|why| why.contains("is not an installed trigger")),
            "a draft on a trigger nobody mounts is refused",
        );
        return finish();
    }
    proof.start()?;
    let result = mounted_proof(&mut proof).await;
    proof.stop().await;
    result?;
    finish()
}

fn finish() -> Result<()> {
    let failures = FAILURES.load(Ordering::SeqCst);
    if failures > 0 {
        println!("{failures} check(s) failed");
        return Err(Error::Internal);
    }
    println!("every check passed");
    Ok(())
}

async fn mounted_proof(proof: &mut Proof) -> Result<()> {
    let (one, two) = (proof.one.clone(), proof.two.clone());
    // Consent: a tenant grants event delivery and the rules principal its
    // declared bounds by name. Declaring them granted nothing.
    for scope in [&one, &two] {
        proof
            .grant(scope, DELIVERY_PRINCIPAL, DELIVER, Some(true))
            .await?;
        for key in RULE_CAPABILITIES {
            proof.grant(scope, "rules", key, Some(true)).await?;
        }
    }

    println!("\n== Same facts, two tenants, two destinations");
    let (r1, v1) = proof
        .draft(
            &one,
            urgent_rule("Urgent cases to the priority desk", "priority-desk", "1000"),
        )
        .await?;
    let (r2, v2) = proof
        .draft(
            &two,
            urgent_rule("Urgent cases to the VIP line", "vip-line", "1000"),
        )
        .await?;
    let review = proof
        .review(
            &one,
            "base.rules.approve",
            json!({"rule_id": r1, "version": v1}),
        )
        .await;
    println!("     approval review: {review}");
    check(
        proof.approve(&one, r1, v1).await.is_ok(),
        "tenant 1 approves its version 1",
    );
    check(
        proof.approve(&two, r2, v2).await.is_ok(),
        "tenant 2 approves its version 1",
    );
    check(
        proof.approve(&one, r2, v2).await.is_err(),
        "tenant 1 cannot approve tenant 2's rule",
    );
    let a = proof
        .open(&one, 1, "Server down", "urgent", "1500.00")
        .await?;
    let b = proof
        .open(&two, 100, "Server down", "urgent", "1500.00")
        .await?;
    let quiet = proof
        .open(&one, 1, "Printer jam", "normal", "1500.00")
        .await?;
    proof.settled().await?;
    for case in [a, b, quiet] {
        proof.show(case).await?;
    }
    check(
        proof.case(a).await?.0.as_deref() == Some("priority-desk"),
        "tenant 1's urgent case went to its priority desk",
    );
    check(
        proof.case(b).await?.0.as_deref() == Some("vip-line"),
        "tenant 2's urgent case went to its VIP line",
    );
    let cross: i64 = proof.db.get().await?.query_one(
        "SELECT count(*) FROM base_rule_runs u JOIN rule_cases_cases c ON c.id::text=u.subject WHERE u.tenant_id<>c.tenant_id", &[],
    ).await?.get(0);
    check(cross == 0, "no run touched a case of another tenant");
    check(
        proof
            .receipts(quiet)
            .await?
            .first()
            .is_some_and(|r| r.1 == "no_match"),
        "a normal case is a receipt with no match, and stays where it was",
    );

    println!("\n== A restart between the commit and the rule's job");
    proof.stop().await;
    let c = proof
        .open(&one, 1, "Database down", "urgent", "2500.00")
        .await?;
    check(
        proof.case(c).await?.0.is_none(),
        "committed with the queue stopped: nothing has acted yet",
    );
    proof.start()?;
    proof.settled().await?;
    check(
        proof.case(c).await?.1 == 1,
        "after the restart the case was routed exactly once",
    );
    // The state a crash leaves after the effect committed and before the
    // job's completion was written: the run still matched, the job alive.
    let client = proof.db.get().await?;
    client.execute("UPDATE base_rule_runs SET state='matched',finished_at=NULL WHERE subject=$1 AND rule_id=$2", &[&c.to_string(), &r1]).await?;
    client.execute(
        "UPDATE base_queue_jobs SET state='available',available_at=now(),attempt=1 WHERE worker_id='base/rules-run' AND args->>'event_id'=(SELECT event_id::text FROM base_rule_runs WHERE subject=$1 AND rule_id=$2)",
        &[&c.to_string(), &r1],
    ).await?;
    drop(client);
    proof.settled().await?;
    proof.show(c).await?;
    check(
        proof.case(c).await?.1 == 1,
        "a retry after the effect committed finds its receipt: still routed once",
    );

    println!("\n== An agent drafts a second rule; a person approves it");
    let flag = json!({
        "name": "Flag big cases", "trigger": OPENED, "match": "all",
        "conditions": [{"field": "amount", "op": "ge", "value": "10000"}],
        "actions": [{"operation": "rule_cases.cases.flag", "input": json!({"note": "big"}).to_string()}],
        "examples": [
            {"facts": json!({"amount": "10000.00"}).to_string(), "expect": "match"},
            {"facts": json!({"amount": "9999.99"}).to_string(), "expect": "no_match"}
        ],
        "explanation": "Flag every case of ten thousand or more."
    });
    let review = proof
        .review(&proof.agent.clone(), "base.rules.draft", flag.clone())
        .await;
    println!("     the agent's review card: {review}");
    check(
        review["preview"]["result"]["examples"]
            .as_array()
            .is_some_and(|e| e.iter().all(|x| x["passes"] == json!(true))),
        "the review shows each example passing",
    );
    let agent = proof.agent.clone();
    let (r3, v3) = proof.draft(&agent, flag).await?;
    let via: String = proof
        .db
        .get()
        .await?
        .query_one(
            "SELECT drafted_via FROM base_rule_versions WHERE rule_id=$1 AND number=$2",
            &[&r3, &(v3 as i32)],
        )
        .await?
        .get(0);
    check(
        via == "agent",
        "the version records that an agent drafted it",
    );
    let wrong = json!({"name": "Wrong example", "trigger": OPENED, "conditions": [{"field": "amount", "op": "ge", "value": "10000"}],
        "actions": [{"operation": "rule_cases.cases.flag", "input": "{}"}],
        "examples": [{"facts": json!({"amount": "5"}).to_string(), "expect": "match"}]});
    check(
        proof
            .write(&agent, "base.rules.draft", wrong)
            .await
            .is_err_and(|w| w.contains("expects a match")),
        "a draft whose example does not come out as stated is refused",
    );
    check(
        proof.approve(&agent, r3, v3).await.is_err(),
        "the manager's agent may draft but not approve",
    );
    check(
        proof.approve(&one, r3, v3).await.is_ok(),
        "the administrator approves the agent's draft",
    );
    let big = proof
        .open(&one, 1, "Outage at the plant", "urgent", "25000.00")
        .await?;
    proof.settled().await?;
    proof.show(big).await?;
    let (queue, _, flags) = proof.case(big).await?;
    check(
        queue.as_deref() == Some("priority-desk") && flags == 1,
        "both approved rules acted, with no AI call",
    );

    let imprecise = json!({"name": "Unrepresentable decimal", "trigger": OPENED,
        "conditions": [{"field": "amount", "op": "eq", "value": "1.00000000000000000000000000001"}],
        "actions": [{"operation": "rule_cases.cases.flag", "input": "{}"}],
        "examples": [{"facts": json!({"amount": "1.00"}).to_string(), "expect": "match"}]});
    check(
        proof
            .write(&one, "base.rules.draft", imprecise)
            .await
            .is_err_and(|why| why.contains("representable exactly")),
        "an unrepresentable decimal is refused rather than rounded to one",
    );
    let exact = json!({"name": "Exact decimal boundary", "trigger": OPENED,
        "conditions": [{"field": "amount", "op": "eq", "value": "1.0000000000000000000000000001"}],
        "actions": [{"operation": "rule_cases.cases.flag", "input": "{}"}],
        "examples": [{"facts": json!({"amount": "1.00"}).to_string(), "expect": "no_match"},
            {"facts": json!({"amount": "1.0000000000000000000000000001"}).to_string(), "expect": "match"}]});
    let (exact_rule, exact_version) = proof.draft(&one, exact).await?;
    check(
        proof.approve(&one, exact_rule, exact_version).await.is_ok(),
        "an exactly representable decimal retains its last digit",
    );
    let one_unit = proof.open(&one, 1, "Exact amount", "low", "1.00").await?;
    proof.settled().await?;
    check(
        proof.case(one_unit).await?.2 == 0,
        "one does not equal one plus 10 to the power minus 28",
    );
    proof
        .switch(&one, exact_rule, false)
        .await
        .map_err(|_| Error::Internal)?;

    println!("\n== A denied capability stops the effect");
    proof
        .grant(&one, "rules", "rule-cases.case.route", Some(false))
        .await?;
    let denied = proof
        .open(&one, 1, "Network down", "urgent", "1500.00")
        .await?;
    proof.settled().await?;
    proof.show(denied).await?;
    check(
        proof.case(denied).await?.0.is_none(),
        "with route denied to the rules principal the case was not routed",
    );
    check(
        proof
            .receipts(denied)
            .await?
            .iter()
            .any(|r| r.3.iter().any(|e| e.1.as_deref() == Some("forbidden"))),
        "the receipt says the action was forbidden",
    );
    proof
        .grant(&one, "rules", "rule-cases.case.route", Some(true))
        .await?;

    println!("\n== A restricted field is never read");
    let margin = json!({"name": "Thin margins", "trigger": OPENED, "conditions": [{"field": "margin", "op": "lt", "value": "500"}],
        "actions": [{"operation": "rule_cases.cases.flag", "input": "{}"}],
        "examples": [{"facts": json!({"margin": "100"}).to_string(), "expect": "match"}]});
    let (r4, v4) = proof.draft(&one, margin.clone()).await?;
    check(
        proof.approve(&one, r4, v4).await.is_ok(),
        "a rule on the margin is approved while nobody restricts it",
    );
    let historical_margin = proof
        .open(&one, 1, "Previously readable margin", "low", "800.00")
        .await?;
    proof.settled().await?;
    check(
        proof.case(historical_margin).await?.2 == 1,
        "the margin rule records a successful match before restrictions change",
    );
    let restrictions = proof
        .services
        .get::<bilibi_authz::restrictions::FieldRestrictions>("base/authz.field_restrictions")?;
    restrictions.put(&one, "rule_cases", "margin", &[2]).await?;
    let thin = proof
        .open(&one, 1, "Low-margin deal", "low", "800.00")
        .await?;
    proof.settled().await?;
    proof.show(thin).await?;
    check(
        proof
            .receipts(thin)
            .await?
            .iter()
            .any(|r| r.0 == r4 && r.1 == "refused" && r.2.as_deref() == Some("restricted_field")),
        "once the margin is restricted, the rule is refused before it reads it",
    );
    check(proof.case(thin).await?.2 == 0, "and nothing was flagged");
    let (r5, v5) = proof.draft(&one, json!({"name": "Thin margins again", "trigger": OPENED, "conditions": [{"field": "margin", "op": "lt", "value": "500"}],
        "actions": [{"operation": "rule_cases.cases.flag", "input": "{}"}],
        "examples": [{"facts": json!({"margin": "100"}).to_string(), "expect": "match"}]})).await.unwrap_or((0, 0));
    let refused = proof.approve(&one, r5, v5).await;
    check(
        refused
            .as_ref()
            .is_err_and(|w| w.contains("hidden from automation"))
            || r5 == 0,
        "a new rule on the restricted margin cannot be approved",
    );
    restrictions.remove(&one, "rule_cases", "margin").await?;
    proof
        .switch(&one, r4, false)
        .await
        .map_err(|_| Error::Internal)?;

    println!("\n== Cycles are refused at approval; a chain stops at the depth budget");
    let selfish = json!({"name": "Reroute forever", "trigger": ROUTED, "conditions": [],
        "actions": [{"operation": "rule_cases.cases.route", "input": json!({"queue": "again"}).to_string()}],
        "examples": [{"facts": "{}", "expect": "match"}]});
    let (rs, vs) = proof.draft(&one, selfish).await?;
    check(
        proof
            .approve(&one, rs, vs)
            .await
            .is_err_and(|w| w.contains("would start itself")),
        "a rule whose action records its own trigger is refused at approval",
    );
    let (ra, va) = proof.draft(&one, json!({"name": "Flag routed cases", "trigger": ROUTED, "conditions": [],
        "actions": [{"operation": "rule_cases.cases.flag", "input": "{}"}], "examples": [{"facts": "{}", "expect": "match"}]})).await?;
    let (rb, vb) = proof.draft(&one, json!({"name": "Route flagged cases", "trigger": FLAGGED, "conditions": [],
        "actions": [{"operation": "rule_cases.cases.route", "input": json!({"queue": "loop"}).to_string()}], "examples": [{"facts": "{}", "expect": "match"}]})).await?;
    check(
        proof.approve(&one, ra, va).await.is_ok(),
        "\"flag routed cases\" is approved",
    );
    check(
        proof
            .approve(&one, rb, vb)
            .await
            .is_err_and(|why| why.contains("cycle")),
        "the second rule of the routed-to-flagged-to-routed cycle is refused",
    );
    proof.setting(&one, "rules.chain_depth", json!(1)).await?;
    let looped = proof
        .open(&one, 1, "Loop starter", "urgent", "1500.00")
        .await?;
    proof.settled().await?;
    proof.show(looped).await?;
    let depth: i32 = proof
        .db
        .get()
        .await?
        .query_one(
            "SELECT max(depth) FROM base_rule_runs WHERE subject=$1",
            &[&looped.to_string()],
        )
        .await?
        .get(0);
    let budget = proof
        .receipts(looped)
        .await?
        .iter()
        .any(|r| r.2.as_deref() == Some("chain_budget"));
    let (_, routed, flags) = proof.case(looped).await?;
    println!("     the loop ran {routed} route(s) and {flags} flag(s), deepest run {depth}");
    check(
        budget && depth == 2 && routed == 1 && flags == 0,
        "the noncyclic opened-to-route-to-flag chain stopped with a visible chain_budget refusal",
    );
    proof
        .switch(&one, ra, false)
        .await
        .map_err(|_| Error::Internal)?;
    proof.setting(&one, "rules.chain_depth", json!(3)).await?;

    println!("\n== An archived company stops the effect");
    proof.stop().await;
    let archived = proof
        .open(&one, 5, "Archived customer's case", "urgent", "1500.00")
        .await?;
    proof
        .services
        .get::<bilibi_company::CompanyService>(bilibi_company::SERVICE)?
        .archive_company(&one, 5, Some("Rules walkthrough"))
        .await
        .map_err(|_| Error::Internal)?;
    proof.start()?;
    proof.settled().await?;
    proof.show(archived).await?;
    check(
        proof.case(archived).await?.0.is_none(),
        "the case of a company archived before the rule acted was not routed",
    );
    check(
        proof
            .receipts(archived)
            .await?
            .iter()
            .any(|r| r.3.iter().any(|e| e.1.as_deref() == Some("refused"))),
        "the receipt keeps the owner's refusal",
    );

    println!("\n== A suspended tenant stops the effect");
    proof.stop().await;
    let suspended = proof
        .open(&two, 100, "Suspended tenant's case", "urgent", "1500.00")
        .await?;
    let operator = {
        let client = proof.db.get().await?;
        proof
            .services
            .get::<Authentication>("base/tenancy.authentication")?
            .authenticate(&**client, 1, 6, "rules".into())
            .await?
    };
    let tenants = proof
        .services
        .get::<bilibi_tenancy::Tenants>("base/tenancy.tenants")?;
    let boundary = proof
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    for change in [bilibi_tenancy::Move::Suspend] {
        let mut client = proof.db.get().await?;
        let mut tx = boundary
            .begin(
                &mut client,
                operator.mutation_context("rules.walkthrough.tenant"),
            )
            .await?;
        let moved = tenants
            .move_in(&mut tx, &operator, 2, change, "Rules walkthrough")
            .await?;
        check(moved.is_ok(), "the operator suspends tenant 2");
        tx.commit().await?;
    }
    proof.start()?;
    proof.settled().await?;
    check(
        proof.case(suspended).await?.0.is_none()
            && proof
                .receipts(suspended)
                .await?
                .iter()
                .all(|r| r.1 != "done"),
        "nothing acted for the suspended tenant",
    );
    {
        let mut client = proof.db.get().await?;
        let mut tx = boundary
            .begin(
                &mut client,
                operator.mutation_context("rules.walkthrough.tenant"),
            )
            .await?;
        let _ = tenants
            .move_in(
                &mut tx,
                &operator,
                2,
                bilibi_tenancy::Move::Reactivate,
                "Rules walkthrough",
            )
            .await?;
        tx.commit().await?;
    }

    println!("\n== A stale version and a switched-off rule, and a threshold change");
    // Hold tenant 1's next runs back (its work bound), so they wait while
    // the rule changes under them.
    proof
        .setting(&one, "rules.runs_per_minute", json!(1))
        .await?;
    let waiting = proof
        .open(&one, 1, "Held back", "urgent", "30000.00")
        .await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let queued = proof.receipts(waiting).await?;
    check(
        queued.len() == 2 && queued.iter().all(|r| r.1 == "queued"),
        "over its work bound the tenant's runs wait for a later minute",
    );
    let (r1b, v1b) = proof.draft(&one, json!({"rule_id": r1, "name": "Urgent cases to the priority desk",
        "trigger": OPENED, "conditions": [{"field": "priority", "op": "eq", "value": "urgent"}, {"field": "amount", "op": "gt", "value": "5000"}],
        "actions": [{"operation": "rule_cases.cases.route", "input": json!({"queue": "priority-desk"}).to_string()}],
        "examples": [{"facts": json!({"priority": "urgent", "amount": "6000"}).to_string(), "expect": "match"}, {"facts": json!({"priority": "urgent", "amount": "3000"}).to_string(), "expect": "no_match"}],
        "explanation": "Raise the threshold to 5000."})).await?;
    check(
        r1b == r1 && v1b == 2,
        "the threshold change is the rule's version 2, data only",
    );
    check(
        proof.approve(&one, r1, v1b).await.is_ok(),
        "version 2 is approved, with no release",
    );
    check(
        proof.switch(&one, r3, false).await.is_ok(),
        "\"flag big cases\" is switched off",
    );
    proof
        .setting(&one, "rules.runs_per_minute", json!(60))
        .await?;
    println!("     waiting for the held runs' next minute");
    for _ in 0..90 {
        if proof
            .receipts(waiting)
            .await?
            .iter()
            .all(|r| r.1 != "queued")
        {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    proof.show(waiting).await?;
    let held = proof.receipts(waiting).await?;
    check(
        held.iter()
            .any(|r| r.0 == r1 && r.2.as_deref() == Some("stale_version")),
        "the run pinned to version 1 is refused as stale",
    );
    check(
        held.iter()
            .any(|r| r.0 == r3 && r.2.as_deref() == Some("rule_off")),
        "the switched-off rule's run is refused",
    );
    check(
        proof.case(waiting).await? == (None, 0, 0),
        "and the held case was neither routed nor flagged",
    );
    let mid = proof
        .open(&one, 1, "Mid-size outage", "urgent", "3000.00")
        .await?;
    let large = proof
        .open(&one, 1, "Large outage", "urgent", "6000.00")
        .await?;
    proof.settled().await?;
    check(
        proof.case(mid).await?.0.is_none()
            && proof.case(large).await?.0.as_deref() == Some("priority-desk"),
        "version 2 runs: 3000 stays, 6000 goes to the priority desk",
    );

    println!("\n== A burst in one tenant waits; the other tenant is not held up");
    proof
        .setting(&one, "rules.runs_per_minute", json!(3))
        .await?;
    let mut burst = Vec::new();
    for n in 0..12 {
        burst.push(
            proof
                .open(&one, 1, &format!("Burst {n}"), "urgent", "9000.00")
                .await?,
        );
    }
    let other = proof
        .open(&two, 100, "Meanwhile in tenant 2", "urgent", "1500.00")
        .await?;
    proof.settled().await?;
    let client = proof.db.get().await?;
    let subjects: Vec<String> = burst.iter().map(i64::to_string).collect();
    let started: i64 = client.query_one("SELECT count(*) FROM base_rule_runs WHERE tenant_id=1 AND started_at IS NOT NULL AND subject=ANY($1)", &[&subjects]).await?.get(0);
    let waiting: i64 = client.query_one("SELECT count(*) FROM base_rule_runs WHERE tenant_id=1 AND state='queued' AND subject=ANY($1)", &[&subjects]).await?.get(0);
    drop(client);
    println!("     of the burst's 12 runs {started} started this minute and {waiting} wait for a later one");
    check(
        started <= 3 && waiting >= 9,
        "tenant 1's burst is held to its bound of 3 runs a minute",
    );
    check(
        proof.case(other).await?.0.as_deref() == Some("vip-line"),
        "tenant 2's case was routed at once",
    );
    println!("\n== A receipt shows a record's values only to who may read the record");
    proof
        .services
        .get::<Grants>("base/authz.grants")?
        .set_direct(
            &one,
            2,
            "admin.rules.view",
            Some(bilibi_authz::grants::Direct::Allow(
                bilibi_authz::Reach::All,
            )),
        )
        .await?;
    let client = proof.db.get().await?;
    let viewer = proof
        .services
        .get::<Authentication>("base/tenancy.authentication")?
        .authenticate(&**client, 1, 2, "rules".into())
        .await?;
    let theirs = proof.rules.runs(&**client, &viewer, Some(r1), 50).await?;
    let mine = proof.rules.runs(&**client, &one, Some(r1), 50).await?;
    drop(client);
    check(
        !theirs.is_empty()
            && theirs.iter().all(|r| {
                r.facts == json!({"restricted": true}) && r.outcomes == json!({"restricted": true})
            }),
        "a reader of rules who may not read cases sees each receipt's facts as restricted",
    );
    check(
        mine.iter()
            .any(|r| r.facts.is_object() && r.facts.get("restricted").is_none()),
        "the administrator, who may read cases, sees the facts",
    );
    proof
        .services
        .get::<Grants>("base/authz.grants")?
        .set_direct(
            &one,
            2,
            "rule-cases.case.view",
            Some(bilibi_authz::grants::Direct::Allow(
                bilibi_authz::Reach::All,
            )),
        )
        .await?;
    let client = proof.db.get().await?;
    let visible = proof.rules.runs(&**client, &viewer, Some(r4), 50).await?;
    check(
        visible.iter().any(|run| {
            run.subject == historical_margin.to_string() && run.facts.get("margin").is_some()
        }),
        "the receipt is visible while its field is readable",
    );
    drop(client);
    restrictions.put(&one, "rule_cases", "margin", &[2]).await?;
    let client = proof.db.get().await?;
    let hidden = proof.rules.runs(&**client, &viewer, Some(r4), 50).await?;
    check(hidden.iter().any(|run| run.state == "done" && run.facts == json!({"restricted": true}) && run.outcomes == json!({"restricted": true})), "a later field restriction hides historical facts and outcomes despite the record read grant");
    drop(client);
    restrictions.remove(&one, "rule_cases", "margin").await?;
    let before = proof.case(historical_margin).await?;
    for (operation, input) in [
        (
            "rule_cases.cases.route",
            json!({"case_id": historical_margin, "expected_version": 1, "queue": "stale destination"}),
        ),
        (
            "rule_cases.cases.flag",
            json!({"case_id": historical_margin, "expected_version": 1}),
        ),
    ] {
        check(
            proof.write(&one, operation, input).await.is_err(),
            "the owner refuses an outdated record version",
        );
    }
    check(
        proof.case(historical_margin).await? == before,
        "stale route and flag calls preserve the newer record",
    );
    for _ in 0..21 {
        let mut draft = urgent_rule("Pending threshold review", "priority-desk", "1000");
        draft["rule_id"] = json!(r1);
        proof.draft(&one, draft).await?;
    }
    let client = proof.db.get().await?;
    let view = proof.rules.get(&**client, &one, r1).await?;
    check(
        view.versions
            .iter()
            .any(|version| Some(version.number) == view.summary.running),
        "the active definition survives more than twenty newer drafts",
    );
    check(
        view.runs.iter().all(|run| {
            view.versions
                .iter()
                .any(|version| version.number == run.version)
        }),
        "every returned receipt has its version definition",
    );
    Ok(())
}
