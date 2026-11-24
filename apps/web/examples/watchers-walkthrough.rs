//! Running proof of tenant watchers (`base/watchers`, ADR 28, #682) on a fresh
//! disposable database, migrated and seeded first (`bilibi-web migrate`,
//! then `BILIBI_ENV=development bilibi-web seed-production`, which stores the
//! Dev instance role so grant changes ask for no second factor nobody
//! enrolled). Two compositions:
//!
//! ```text
//! cargo run -p bilibi-web --example watchers-walkthrough           # 1. nothing mounted
//! cp -r fixtures/containers/watcher_cases apps/domains/ && python3 scripts/discover-modules.py
//! cargo run -p bilibi-web --example watchers-walkthrough           # 2. the reference owner mounted
//! rm -r apps/domains/watcher_cases && python3 scripts/discover-modules.py && git checkout -- Cargo.lock
//! ```
//!
//! Unmounted, the engine boots with no trigger or action, and its contracts
//! answer an empty vocabulary and refuse a draft on an unknown trigger. With
//! the reference owner (`fixtures/containers/watcher_cases`, a case with a
//! priority, an amount, a margin and a queue) mounted, two tenants get the
//! same watcher over the same facts with different destinations, and each
//! affects only its own case. Then: a restart between the commit and the
//! watcher's job gives one effect, and so does a retry after the effect
//! committed; an agent drafts a second watcher that a person approves; a denied
//! capability, a restricted field, an archived company, a suspended tenant,
//! a stale version and a switched-off watcher each stop the effect; a tenant
//! custom field is a condition only once the tenant offers it to watchers, routes
//! its case, and refuses the watcher when restricted or archived; a linked
//! company's offered custom field (`company.cf_segment`) is a condition once
//! automation may read companies, in a trial and in a run; a watcher that
//! would start itself is refused at approval, and a two-watcher loop is refused and a noncyclic chain stops at
//! the chain budget; a threshold change is a new approved version, no
//! release; and watchers share a tenant bucket, wait in order for a refill,
//! and leave another tenant free to run. A person drafts and previews the input the Watchers page
//! builds (an `in` list, then the watcher's next version) with the same contract. Every check prints `ok` or `FAIL`; one failure ends the
//! run with an error.
use bilibi_agent_api::plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans};
use bilibi_authz::grants::Grants;
use bilibi_base::events::{Events, DELIVER, PRINCIPAL as DELIVERY_PRINCIPAL};
use bilibi_base::queue::{Config, Executor};
use bilibi_base::system::{InstallationMode, Instances};
use bilibi_base::watchers::{Watchers, SERVICE};
use bilibi_database::{pool, Db, Error, MutationBoundary, Result};
use bilibi_module_registry::{Composition, Container, ExtensionCode, Layer, PinState, Services};
use bilibi_tenancy::{Authentication, ProofFuture, Scope};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{sync::Arc, time::Duration};
use tokio_postgres::Transaction;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const OPENED: &str = "watcher_cases/cases.opened";
const ROUTED: &str = "watcher_cases/cases.routed";
const FLAGGED: &str = "watcher_cases/cases.flagged";
const WATCHER_CAPABILITIES: [&str; 4] = [
    "watchers.run.execute",
    "watcher-cases.case.view",
    "watcher-cases.case.route",
    "watcher-cases.case.flag",
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
                credential_binding: "walkthrough:watchers".into(),
            })
        })
    }
}

struct Proof {
    db: Db,
    services: Services,
    plans: Arc<Plans>,
    watchers: Arc<Watchers>,
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
    async fn draft(&self, scope: &Scope, watcher: Value) -> Result<(i64, i64)> {
        match self.write(scope, "base.watchers.draft", watcher).await {
            Ok(result) => Ok((
                result["watcher_id"].as_i64().unwrap_or_default(),
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
        watcher: i64,
        version: i64,
    ) -> std::result::Result<Value, String> {
        self.write(
            scope,
            "base.watchers.approve",
            json!({"watcher_id": watcher, "version": version}),
        )
        .await
    }
    async fn restore(
        &self,
        scope: &Scope,
        watcher: i64,
        version: i64,
    ) -> std::result::Result<Value, String> {
        self.write(
            scope,
            "base.watchers.restore",
            json!({"watcher_id": watcher, "version": version}),
        )
        .await
    }
    async fn switch(
        &self,
        scope: &Scope,
        watcher: i64,
        on: bool,
    ) -> std::result::Result<Value, String> {
        self.write(
            scope,
            "base.watchers.switch",
            json!({"watcher_id": watcher, "enabled": on}),
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
        self.open_with(scope, company, title, priority, amount, json!([]))
            .await
    }
    /// [`open`](Self::open) with tenant custom field values.
    async fn open_with(
        &self,
        scope: &Scope,
        company: i64,
        title: &str,
        priority: &str,
        amount: &str,
        custom: Value,
    ) -> Result<i64> {
        let result = self
            .write(scope, "watcher_cases.cases.open", json!({"company_id": company, "title": title, "priority": priority, "amount": amount, "margin": "250.00", "custom": custom}))
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
                "SELECT queue,routed,flags FROM watcher_cases_cases WHERE id=$1",
                &[&id],
            )
            .await?;
        Ok((row.get(0), row.get(1), row.get(2)))
    }
    /// Waits until no delivery or watcher job is ready to run.
    async fn settled(&self) -> Result<()> {
        for _ in 0..600 {
            let open: i64 = self
                .db
                .get()
                .await?
                .query_one(
                    "SELECT count(*) FROM base_queue_jobs q
                 WHERE q.worker_id IN ('base/events-delivery','base/watchers-admit','base/watchers-run')
                     AND q.state IN ('available','executing','retryable','scheduled')
                     AND q.available_at <= now() + interval '2 seconds'
                     AND NOT (q.fifo_key IS NOT NULL AND EXISTS (
                         SELECT 1 FROM base_queue_jobs head
                         WHERE head.tenant_id=q.tenant_id AND head.worker_id=q.worker_id
                             AND head.fifo_key=q.fifo_key AND head.id<q.id AND head.state IN ('retryable','scheduled')
                             AND head.available_at > now() + interval '2 seconds'
                     ))",
                    &[],
                )
                .await?
                .get(0);
            if open == 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!("timed out waiting for the queue");
        Err(Error::Internal)
    }
    async fn hold_matching_case(&mut self, scope: &Scope, title: &str) -> Result<i64> {
        self.stop().await;
        self.db
            .get()
            .await?
            .execute(
                "INSERT INTO base_watcher_buckets (tenant_id,tokens,refilled_at)
             VALUES ($1,0,clock_timestamp() + interval '1 hour')
             ON CONFLICT (tenant_id) DO UPDATE SET tokens=0,refilled_at=EXCLUDED.refilled_at",
                &[&scope.tenant()],
            )
            .await?;
        let case = self.open(scope, 1, title, "urgent", "12000.00").await?;
        self.start()?;
        self.settled().await?;
        self.stop().await;
        let receipts = self.receipts(case).await?;
        check(
            receipts.iter().any(|r| r.1 == "queued")
                && receipts.iter().all(|r| {
                    (r.1 == "queued" || (r.1 == "refused" && r.2.as_deref() == Some("watcher_off")))
                        && r.3.is_empty()
                })
                && self.case(case).await? == (None, 0, 0),
            "the matching case has pinned runs and no effects before resuming the job",
        );
        Ok(case)
    }

    async fn resume_watcher_jobs(&self, scope: &Scope, spent_attempts: i32) -> Result<u64> {
        let client = self.db.get().await?;
        client.execute(
            "UPDATE base_watcher_buckets SET refilled_at=clock_timestamp() - interval '1 minute'
             WHERE tenant_id=$1",
            &[&scope.tenant()],
        ).await?;
        Ok(client.execute(
            "UPDATE base_queue_jobs SET available_at=clock_timestamp(),attempt=$2
             WHERE tenant_id=$1 AND worker_id='base/watchers-admit' AND state='scheduled' AND attempt=0",
            &[&scope.tenant(), &spent_attempts],
        ).await?)
    }

    /// The receipts of runs on one case: (watcher, state, reason, effects).
    async fn receipts(
        &self,
        case: i64,
    ) -> Result<Vec<(i64, String, Option<String>, Vec<(String, Option<String>)>)>> {
        let client = self.db.get().await?;
        let rows = client
            .query(
                "SELECT id,watcher_id,state,reason FROM base_watcher_runs WHERE subject=$1 ORDER BY id",
                &[&case.to_string()],
            )
            .await?;
        let mut receipts = Vec::new();
        for row in rows {
            let run: i64 = row.get(0);
            let effects = client
                .query(
                    "SELECT state,reason FROM base_watcher_effects WHERE run_id=$1 ORDER BY position",
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
        for (watcher, state, reason, effects) in self.receipts(case).await? {
            println!(
                "     case {case}: watcher {watcher} {state}{} effects {effects:?}",
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
                scope.mutation_context("watchers.walkthrough.setting"),
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

fn urgent_watcher(name: &str, queue: &str, threshold: &str) -> Value {
    json!({
        "name": name, "watch": OPENED, "if": {"match": "all", "conditions": [
            {"field": "priority", "op": "eq", "value": "urgent"},
            {"field": "amount", "op": "gt", "value": threshold}
        ]},
        "do": [{"operation": "watcher_cases.cases.route", "input": json!({"queue": queue})}],
        "examples": [
            {"facts": json!({"priority": "urgent", "amount": "15000"}).to_string(), "expect": "match"},
            {"facts": json!({"priority": "urgent", "amount": "1.00"}).to_string(), "expect": "no_match"},
            {"facts": json!({"priority": "normal", "amount": "50000"}).to_string(), "expect": "no_match"}
        ],
        "explanation": format!("Send new urgent cases over {threshold} to {queue}.")
    })
}

/// #730: a tenant custom field is a condition only once the tenant offers it
/// to watchers; restricting or archiving it refuses the watcher visibly, never as
/// an empty value.
async fn custom_field_proof(
    proof: &Proof,
    restrictions: &bilibi_authz::restrictions::FieldRestrictions,
) -> Result<()> {
    let one = proof.one.clone();
    println!("\n== A tenant custom field as a condition");
    let version = || async {
        let row = proof.db.get().await?.query_one(
            "SELECT version FROM base_custom_fields WHERE tenant_id=$1 AND record_type='watcher_case' AND key='segment'",
            &[&one.tenant()],
        ).await?;
        Ok::<i64, Error>(row.get(0))
    };
    let defined = proof
        .write(&one, "base.custom_fields.define", json!({"record_type": "watcher_case", "key": "segment", "label": "Segment", "kind": "text"}))
        .await;
    check(
        defined.is_ok(),
        "an administrator adds a Segment text field to cases",
    );
    let watcher = json!({"name": "Enterprise cases to key accounts", "watch": OPENED, "if": {"match": "all", "conditions": [{"field": "cf_segment", "op": "eq", "value": "Enterprise"}]},
    "do": [{"operation": "watcher_cases.cases.route", "input": json!({"queue": "key-accounts"})}],
    "examples": [
        {"facts": json!({"cf_segment": "Enterprise"}).to_string(), "expect": "match"},
        {"facts": json!({"cf_segment": "SMB"}).to_string(), "expect": "no_match"}
    ]});
    let fields = |vocabulary: &Value| -> Vec<(String, String)> {
        vocabulary["triggers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["trigger"] == OPENED)
            .flat_map(|t| t["fields"].as_array().cloned().unwrap_or_default())
            .map(|f| {
                (
                    f["field"].as_str().unwrap_or_default().to_owned(),
                    f["label"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    };
    let client = proof.db.get().await?;
    let offered = fields(&proof.watchers.vocabulary(&**client, &one).await?);
    drop(client);
    check(
        !offered.iter().any(|(field, _)| field == "cf_segment"),
        "a custom field not offered to watchers is not in the watchers vocabulary",
    );
    let refused = proof
        .write(&one, "base.watchers.draft", watcher.clone())
        .await;
    println!("     draft on an unoffered field: {refused:?}");
    check(
        refused
            .as_ref()
            .is_err_and(|why| why.contains("not offered to watchers")),
        "a draft on a custom field the tenant has not offered is refused with the reason",
    );
    let offer = proof
        .write(&one, "base.custom_fields.update", json!({"record_type": "watcher_case", "key": "segment", "label": "Segment", "watcher_condition": true, "version": version().await?}))
        .await;
    check(
        offer.is_ok(),
        "the administrator offers Segment to watcher conditions",
    );
    let refused_kind = proof
        .write(&one, "base.custom_fields.define", json!({"record_type": "watcher_case", "key": "notes", "label": "Notes", "kind": "long_text", "watcher_condition": true}))
        .await;
    check(
        refused_kind.is_err_and(|why| why.contains("watcher condition")),
        "a long text field cannot be offered to watcher conditions",
    );
    let client = proof.db.get().await?;
    let offered = fields(&proof.watchers.vocabulary(&**client, &one).await?);
    drop(client);
    check(
        offered
            .iter()
            .any(|(field, label)| field == "cf_segment" && label == "Segment"),
        "the offered field is in the vocabulary under its tenant label",
    );
    let (watcher_id, number) = proof.draft(&one, watcher.clone()).await?;
    let approved = proof.approve(&one, watcher_id, number).await;
    println!("     approval: {approved:?}");
    check(
        approved.is_ok(),
        "a watcher on the offered custom field is approved",
    );
    let enterprise = proof
        .open_with(
            &one,
            1,
            "Enterprise renewal",
            "low",
            "800.00",
            json!([{"key": "segment", "value": "Enterprise"}]),
        )
        .await?;
    let smb = proof
        .open_with(
            &one,
            1,
            "Small shop question",
            "low",
            "800.00",
            json!([{"key": "segment", "value": "SMB"}]),
        )
        .await?;
    proof.settled().await?;
    proof.show(enterprise).await?;
    check(
        proof.case(enterprise).await?.0.as_deref() == Some("key-accounts"),
        "an Enterprise case is routed to the key-account queue",
    );
    check(
        proof.case(smb).await?.0.is_none()
            && proof
                .receipts(smb)
                .await?
                .iter()
                .any(|r| r.0 == watcher_id && r.1 == "no_match"),
        "an SMB case does not match, and its receipt says so",
    );
    let client = proof.db.get().await?;
    let facts: Option<Value> = client
        .query_one(
            "SELECT facts FROM base_watcher_runs WHERE watcher_id=$1 AND subject=$2",
            &[&watcher_id, &enterprise.to_string()],
        )
        .await?
        .get(0);
    let role: i64 = client.query_one(
        "SELECT id FROM roles WHERE tenant_id=$1 AND id NOT IN (SELECT role_id FROM principal_roles WHERE tenant_id=$1 AND user_id=$2) ORDER BY id LIMIT 1",
        &[&one.tenant(), &one.user()],
    ).await?.get(0);
    drop(client);
    check(
        facts
            .as_ref()
            .is_some_and(|facts| facts["cf_segment"] == "Enterprise"),
        "the receipt keeps the custom field's value as evidence",
    );

    println!("\n== A restricted custom field is never read");
    let restricted = restrictions
        .put(&one, "watcher_cases", "cf_segment", &[role])
        .await;
    check(
        restricted.is_ok(),
        "an operator restricts the custom field like a declared one",
    );
    let withheld = proof
        .open_with(
            &one,
            1,
            "Enterprise while restricted",
            "low",
            "800.00",
            json!([{"key": "segment", "value": "Enterprise"}]),
        )
        .await?;
    proof.settled().await?;
    proof.show(withheld).await?;
    check(
        proof.receipts(withheld).await?.iter().any(|r| {
            r.0 == watcher_id && r.1 == "refused" && r.2.as_deref() == Some("restricted_field")
        }),
        "once it is restricted, the watcher is refused before it reads it",
    );
    check(
        proof.case(withheld).await?.0.is_none(),
        "and the case stays where it was",
    );
    let (again, again_number) = proof
        .draft(&one, json!({"name": "Enterprise again", "watch": OPENED, "if": watcher["if"], "do": watcher["do"], "examples": watcher["examples"]}))
        .await?;
    let refused = proof.approve(&one, again, again_number).await;
    println!("     approval while restricted: {refused:?}");
    check(
        refused.is_err_and(|why| why.contains("hidden from automation")),
        "a new watcher on the restricted custom field cannot be approved",
    );
    restrictions
        .remove(&one, "watcher_cases", "cf_segment")
        .await?;

    println!("\n== An archived custom field refuses its watcher");
    let archived = proof
        .write(
            &one,
            "base.custom_fields.archive",
            json!({"record_type": "watcher_case", "key": "segment", "version": version().await?}),
        )
        .await;
    check(archived.is_ok(), "the administrator archives Segment");
    let after = proof
        .open(&one, 1, "Enterprise after archiving", "low", "800.00")
        .await?;
    proof.settled().await?;
    proof.show(after).await?;
    check(
        proof.receipts(after).await?.iter().any(|r| {
            r.0 == watcher_id && r.1 == "refused" && r.2.as_deref() == Some("catalog_changed")
        }),
        "the watcher is refused as changed, not read as an empty value",
    );
    let client = proof.db.get().await?;
    let needs_review: bool = client
        .query_one(
            "SELECT needs_review FROM base_watchers WHERE id=$1",
            &[&watcher_id],
        )
        .await?
        .get(0);
    drop(client);
    check(needs_review, "and the watcher is marked Needs review");
    let refused = proof.write(&one, "base.watchers.draft", json!({"watcher_id": watcher_id, "name": "Enterprise cases to key accounts", "watch": OPENED, "if": watcher["if"], "do": watcher["do"], "examples": watcher["examples"]})).await;
    check(
        refused.is_err_and(|why| why.contains("cf_segment is not a field this trigger offers")),
        "a next version on the archived field is refused at draft",
    );
    proof
        .switch(&one, watcher_id, false)
        .await
        .map_err(|_| Error::Internal)?;
    Ok(())
}

/// #862: a condition may read an offered custom field of a record the
/// trigger's record links to (`company.cf_segment`), as the `watchers`
/// principal with reach to that record, at evaluation.
async fn linked_custom_field_proof(proof: &Proof) -> Result<()> {
    let one = proof.one.clone();
    println!("\n== A linked company's custom field as a condition");
    let version = || async {
        let row = proof.db.get().await?.query_one(
            "SELECT version FROM base_custom_fields WHERE tenant_id=$1 AND record_type='company' AND key='segment'",
            &[&one.tenant()],
        ).await?;
        Ok::<i64, Error>(row.get(0))
    };
    let segment = |company: i64, value: &'static str| {
        let one = one.clone();
        async move {
            let version: i64 = proof
                .db
                .get()
                .await?
                .query_one(
                    "SELECT version FROM companies WHERE tenant_id=$1 AND id=$2",
                    &[&one.tenant(), &company],
                )
                .await?
                .get(0);
            proof
                .write(&one, "core.company.update", json!({"company_id": company, "version": version, "fields": {"custom": [{"key": "segment", "value": value}]}}))
                .await
                .map_err(|why| {
                    println!("     company update refused: {why}");
                    Error::Invalid
                })
        }
    };
    let defined = proof
        .write(
            &one,
            "base.custom_fields.define",
            json!({"record_type": "company", "key": "segment", "label": "Segment", "kind": "text"}),
        )
        .await;
    check(
        defined.is_ok(),
        "an administrator adds a Segment text field to companies",
    );
    segment(1, "Gold").await?;
    segment(5, "Silver").await?;
    let watcher = json!({"name": "Gold companies' cases to the gold desk", "watch": OPENED, "if": {"match": "all", "conditions": [{"field": "company.cf_segment", "op": "eq", "value": "Gold"}]},
    "do": [{"operation": "watcher_cases.cases.route", "input": json!({"queue": "gold-desk"})}],
    "examples": [
        {"facts": json!({"company.cf_segment": "Gold"}).to_string(), "expect": "match"},
        {"facts": json!({"company.cf_segment": "Silver"}).to_string(), "expect": "no_match"}
    ]});
    let linked = |vocabulary: &Value| -> Option<String> {
        vocabulary["triggers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["trigger"] == OPENED)
            .flat_map(|t| t["fields"].as_array().cloned().unwrap_or_default())
            .find(|f| f["field"] == "company.cf_segment")
            .map(|f| f["label"].as_str().unwrap_or_default().to_owned())
    };
    let client = proof.db.get().await?;
    let offered = linked(&proof.watchers.vocabulary(&**client, &one).await?);
    drop(client);
    check(
        offered.is_none(),
        "a linked record's custom field not offered to watchers is not in the vocabulary",
    );
    let refused = proof
        .write(&one, "base.watchers.draft", watcher.clone())
        .await;
    println!("     draft on an unoffered linked field: {refused:?}");
    check(
        refused.is_err_and(|why| why.contains("not offered to watchers")),
        "a draft on a linked record's unoffered custom field is refused with the reason",
    );
    let offer = proof
        .write(&one, "base.custom_fields.update", json!({"record_type": "company", "key": "segment", "label": "Segment", "watcher_condition": true, "version": version().await?}))
        .await;
    check(
        offer.is_ok(),
        "the administrator offers the company's Segment to watcher conditions",
    );
    let client = proof.db.get().await?;
    let offered = linked(&proof.watchers.vocabulary(&**client, &one).await?);
    drop(client);
    println!("     vocabulary: company.cf_segment as {offered:?}");
    check(
        offered.as_deref() == Some("Company \u{203a} Segment"),
        "the offered field is in the vocabulary by its path, under the link and the tenant label",
    );
    let (watcher_id, number) = proof.draft(&one, watcher.clone()).await?;
    let client = proof.db.get().await?;
    let review = proof
        .watchers
        .review_approval(&**client, &one, watcher_id, number as i32)
        .await?;
    drop(client);
    println!("     approval review: {:?}", review.problems);
    check(
        review.problems.iter().any(|(path, reason)| {
            path == "if.conditions.0.field" && reason.contains("hidden from automation")
        }),
        "the approval review refuses the linked field while automation cannot read companies",
    );
    proof
        .grant(&one, "watchers", "admin.company.list", Some(true))
        .await?;

    proof
        .grant(&one, "watchers", "watcher-cases.case.view", None)
        .await?;
    check(
        proof.approve(&one, watcher_id, number).await.is_err(),
        "approval is refused while automation cannot read the trigger table",
    );
    proof
        .grant(&one, "watchers", "watcher-cases.case.view", Some(true))
        .await?;

    println!("\n== A trial reads the linked field the same way");
    let silver_subject = proof
        .open(
            &one,
            5,
            "Silver company's earlier question",
            "low",
            "800.00",
        )
        .await?;
    proof.settled().await?;
    let registry = proof
        .services
        .get::<bilibi_agent_api::Registry>("base/agent_api.registry")?;
    let trial = registry
        .read(
            &proof.db,
            &one,
            "base.watchers.trial",
            &json!({"draft": watcher.clone(), "days": 30, "limit": 100}),
        )
        .await
        .map_err(|error| {
            println!("     trial: {error:?}");
            Error::Invalid
        })?;
    let rows = trial["result"]["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let silver_subject = silver_subject.to_string();
    check(
        rows.iter().any(|r| {
            r["state"] == "match" && r["current_record"]["condition_outcomes"] == json!([true])
        }),
        "a trial on past events matches a Gold company's case",
    );
    check(
        rows.iter().any(|r| {
            r["subject"] == silver_subject
                && r["state"] == "near_miss"
                && r["current_record"]["condition_outcomes"] == json!([false])
        }),
        "the Silver company's case is a near miss with a false condition outcome",
    );
    check(
        rows.iter()
            .filter(|r| matches!(r["state"].as_str(), Some("match" | "near_miss")))
            .all(|r| {
                r["current_record"]["facts"]["company.cf_segment"]
                    == json!({"omitted": "linked custom field"})
            }),
        "trial evidence records the linked path without its value",
    );

    println!("\n== The approved watcher routes by the company's value");
    let approved = proof.approve(&one, watcher_id, number).await;
    println!("     approval: {approved:?}");
    check(
        approved.is_ok(),
        "once automation may read companies, the watcher is approved",
    );
    let gold = proof
        .open(&one, 1, "Gold company's question", "low", "800.00")
        .await?;
    let silver = proof
        .open(&one, 5, "Silver company's question", "low", "800.00")
        .await?;
    proof.settled().await?;
    proof.show(gold).await?;
    check(
        proof.case(gold).await?.0.as_deref() == Some("gold-desk"),
        "a Gold company's case is routed to the gold desk",
    );
    check(
        proof.case(silver).await?.0.is_none()
            && proof
                .receipts(silver)
                .await?
                .iter()
                .any(|r| r.0 == watcher_id && r.1 == "no_match"),
        "a Silver company's case does not match, and its receipt says so",
    );
    let client = proof.db.get().await?;
    for (subject, held) in [(gold, true), (silver, false)] {
        let row = client
            .query_one(
                "SELECT facts,outcomes FROM base_watcher_runs WHERE watcher_id=$1 AND subject=$2",
                &[&watcher_id, &subject.to_string()],
            )
            .await?;
        let facts: Option<Value> = row.get(0);
        let outcomes: Option<Value> = row.get(1);
        check(
            facts.is_some_and(|facts| {
                facts["company.cf_segment"] == json!({"omitted": "linked custom field"})
            }) && outcomes == Some(json!([held])),
            "the receipt keeps the linked path and condition outcome without its value",
        );
    }
    drop(client);
    proof.setting(&one, "watchers.facts_read", json!(1)).await?;
    let over_budget = proof
        .open(&one, 1, "Gold over the fact budget", "low", "800.00")
        .await?;
    proof.settled().await?;
    check(
        proof.receipts(over_budget).await?.iter().any(|r| {
            r.0 == watcher_id && r.1 == "refused" && r.2.as_deref() == Some("watcher_limit")
        }) && proof.case(over_budget).await?.0.is_none(),
        "the linked condition and trigger version exceed a one-fact budget, so no action runs",
    );
    proof
        .setting(&one, "watchers.facts_read", json!(21))
        .await?;
    segment(5, "Gold").await?;
    let promoted = proof
        .open(&one, 5, "Promoted company's question", "low", "800.00")
        .await?;
    proof.settled().await?;
    check(
        proof.case(promoted).await?.0.as_deref() == Some("gold-desk"),
        "the value is read at evaluation: a company made Gold routes its next case",
    );

    println!("\n== Without reach to the linked record the watcher is refused");
    proof
        .grant(&one, "watchers", "admin.company.list", None)
        .await?;
    let unreachable = proof
        .open(&one, 1, "Gold while companies are closed", "low", "800.00")
        .await?;
    proof.settled().await?;
    proof.show(unreachable).await?;
    check(
        proof.receipts(unreachable).await?.iter().any(|r| {
            r.0 == watcher_id && r.1 == "refused" && r.2.as_deref() == Some("read_refused")
        }) && proof.case(unreachable).await?.0.is_none(),
        "when automation may no longer read companies, the run is refused and the case stays",
    );
    proof
        .grant(&one, "watchers", "admin.company.list", Some(true))
        .await?;

    println!("\n== Withdrawing the linked field refuses its watcher");
    let withdrawn = proof
        .write(&one, "base.custom_fields.update", json!({"record_type": "company", "key": "segment", "label": "Segment", "watcher_condition": false, "version": version().await?}))
        .await;
    check(
        withdrawn.is_ok(),
        "the administrator withdraws the company's Segment from watchers",
    );
    let after = proof
        .open(&one, 1, "Gold after withdrawing", "low", "800.00")
        .await?;
    proof.settled().await?;
    proof.show(after).await?;
    check(
        proof.receipts(after).await?.iter().any(|r| {
            r.0 == watcher_id && r.1 == "refused" && r.2.as_deref() == Some("catalog_changed")
        }) && proof.case(after).await?.0.is_none(),
        "the watcher is refused as changed, not read as an empty value",
    );
    let needs_review: bool = proof
        .db
        .get()
        .await?
        .query_one(
            "SELECT needs_review FROM base_watchers WHERE id=$1",
            &[&watcher_id],
        )
        .await?
        .get(0);
    check(needs_review, "and the watcher is marked Needs review");
    proof
        .switch(&one, watcher_id, false)
        .await
        .map_err(|_| Error::Internal)?;
    Ok(())
}

fn main() -> Result<()> {
    bilibi_base::watchers::enter_worker();
    walkthrough()
}

#[tokio::main]
async fn walkthrough() -> Result<()> {
    bilibi_base::link();
    bilibi_core::link();
    bilibi_domains::link();
    bilibi_extensions::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_web::verify_composition(&db).await?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let customer_release = Composition {
        version: "walkthrough",
        commit: None,
        tag: None,
        pin: PinState::Unpinned,
        containers: vec![Container {
            layer: Layer::Extensions,
            id: "customer_example",
            modules: vec!["customer_example/change"],
            commit: None,
            version: None,
            extension_code: Some(ExtensionCode::Customer),
        }],
    };
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(customer_release))?;
    services.install(&modules)?;
    services
        .get::<Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    // Tenant 2's manager becomes its administrator (a grant-all role), as a
    // tenant's owner would be.
    let client = db
        .get_detached("the walkthrough's own checking connection")
        .await?;
    client.batch_execute(
        "INSERT INTO roles(tenant_id,name,grant_all) SELECT 2,'Walkthrough administrator',true WHERE NOT EXISTS (SELECT 1 FROM roles WHERE tenant_id=2 AND name='Walkthrough administrator');
         INSERT INTO principal_roles(tenant_id,user_id,role_id) SELECT 2,5,id FROM roles WHERE tenant_id=2 AND name='Walkthrough administrator' ON CONFLICT DO NOTHING;",
    ).await?;
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    let one = authentication
        .authenticate(&**client, 1, 4, "watchers".into())
        .await?;
    let two = authentication
        .authenticate(&**client, 2, 5, "watchers".into())
        .await?;
    // Tenant 1's manager may draft watchers (not approve them), and has an AI
    // agent: a live connection the person granted, as Core Agent stores it
    // once the person approves it. The agent works at the manager's desk.
    let grants = services.get::<Grants>("base/authz.grants")?;
    // The reference owner's read key exists only while it is mounted.
    let reference = bilibi_authz::vocabulary()?
        .capabilities
        .contains_key("watcher-cases.case.view");
    for key in [
        "admin.watchers.view",
        "admin.watchers.manage",
        "watcher-cases.case.view",
    ]
    .into_iter()
    .filter(|key| reference || !key.starts_with("watcher-cases."))
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
        .authenticate(&**client, 1, 1, "watchers".into())
        .await?;
    let connection: i64 = client.query_one(
        "INSERT INTO core_agent_connections(tenant_id,user_id,person_id,agent_id,expires_at) VALUES(1,1,1,'watchers-walkthrough-agent',now()+interval '1 hour') RETURNING id",
        &[],
    ).await?.get(0);
    drop(client);
    let agent = manager.for_agent(connection, None, "watchers-walkthrough-agent".into(), None);
    let mut proof = Proof {
        db: db.clone(),
        plans: services.get("base/agent_api.plans")?,
        watchers: services.get(SERVICE)?,
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
    let customer_instances = proof
        .services
        .get::<Instances>(bilibi_base::system::SERVICE)?;
    let client = db.get().await?;
    client.execute(
        "INSERT INTO user_bootstrap(id,tenant_name,company_name,company_code,admin_email,tenant_id,user_id,company_id,affiliation_settled_at) VALUES(1,'Walkthrough','Company 001','C001','admin@example.test',1,4,1,now()) ON CONFLICT DO NOTHING",
        &[],
    ).await?;
    bilibi_tenancy::prove_owner(&**client, &proof.one).await?;
    drop(client);
    installation_mode(&db, InstallationMode::Shared).await?;
    check(
        customer_instances
            .validate_composition()
            .await
            .is_err_and(|error| {
                matches!(&error, Error::Registry(_))
                    && error.to_string().contains("customer_example")
            }),
        "startup validation names and refuses the customer Extension in Shared service",
    );
    installation_mode(&db, InstallationMode::Isolated).await?;
    check(
        customer_instances.validate_composition().await.is_ok(),
        "startup validation admits the same customer Extension in Isolated installation",
    );
    check(
        matches!(
            customer_instances
                .change_installation_mode(
                    &proof.one,
                    InstallationMode::Isolated,
                    InstallationMode::Shared,
                )
                .await,
            Err(Error::Invalid)
        ),
        "the protected owner mutation refuses Shared service while customer code is composed",
    );
    check(
        customer_instances.installation_mode().await? == InstallationMode::Isolated,
        "the refused owner mutation preserves Isolated installation",
    );
    check(
        matches!(
            customer_instances
                .change_installation_mode(
                    &proof.two,
                    InstallationMode::Isolated,
                    InstallationMode::Shared,
                )
                .await,
            Err(Error::Forbidden)
        ),
        "a tenant administrator cannot change installation policy",
    );
    installation_mode(&db, InstallationMode::Shared).await?;
    let vocabulary = {
        let client = db.get().await?;
        proof.watchers.vocabulary(&**client, &proof.one).await?
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
                "base.watchers.draft",
                urgent_watcher("Urgent", "priority", "1000"),
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
    let result = if std::env::args().any(|arg| arg == "--scripts") {
        scripts::walkthrough(&proof).await
    } else if std::env::args().any(|arg| arg == "--observations") {
        observations::walkthrough(&mut proof).await
    } else {
        mounted_proof(&mut proof).await
    };
    proof.stop().await;
    result?;
    finish()
}

async fn installation_mode(db: &Db, mode: InstallationMode) -> Result<()> {
    db.get().await?.execute(
        "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES('installation.mode',0,'global',0,to_jsonb($1::text),false) ON CONFLICT(key,tenant_id,scope_type,scope_id) DO UPDATE SET value=excluded.value",
        &[&mode.as_str()],
    ).await?;
    Ok(())
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
    // Consent: a tenant grants event delivery and the watchers principal its
    // declared bounds by name. Declaring them granted nothing.
    for scope in [&one, &two] {
        proof
            .grant(scope, DELIVERY_PRINCIPAL, DELIVER, Some(true))
            .await?;
        for key in WATCHER_CAPABILITIES {
            proof.grant(scope, "watchers", key, Some(true)).await?;
        }
    }

    println!("\n== Same facts, two tenants, two destinations");
    let (r1, v1) = proof
        .draft(
            &one,
            urgent_watcher("Urgent cases to the priority desk", "priority-desk", "1000"),
        )
        .await?;
    let (r2, v2) = proof
        .draft(
            &two,
            urgent_watcher("Urgent cases to the VIP line", "vip-line", "1000"),
        )
        .await?;
    let review = proof
        .review(
            &one,
            "base.watchers.approve",
            json!({"watcher_id": r1, "version": v1}),
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
        "tenant 1 cannot approve tenant 2's watcher",
    );
    for mode in InstallationMode::ALL {
        installation_mode(&proof.db, mode).await?;
        let urgent = proof
            .open(&one, 1, "Mode parity urgent", "urgent", "1500.00")
            .await?;
        let normal = proof
            .open(&one, 1, "Mode parity normal", "normal", "1500.00")
            .await?;
        proof.settled().await?;
        check(
            proof.case(urgent).await? == (Some("priority-desk".into()), 1, 0),
            &format!(
                "{}: the same approved watcher routes an urgent 1500.00 case exactly once",
                mode.label()
            ),
        );
        check(
            proof.case(normal).await? == (None, 0, 0),
            &format!(
                "{}: the same approved watcher leaves a normal 1500.00 case unchanged",
                mode.label()
            ),
        );
    }
    installation_mode(&proof.db, InstallationMode::Shared).await?;
    let limited_draft = urgent_watcher("Condition budget proof", "priority-desk", "1000");
    let (limited_watcher, limited_version) = proof.draft(&one, limited_draft.clone()).await?;
    proof.setting(&one, "watchers.conditions", json!(1)).await?;
    let client = proof.db.get().await?;
    let limits = proof.watchers.vocabulary(&**client, &one).await?;
    check(
        limits["limits"]["conditions"] == json!(1),
        "the vocabulary reports the lowered condition budget",
    );
    drop(client);
    check(
        proof
            .write(&one, "base.watchers.draft", limited_draft)
            .await
            .is_err_and(|why| why.contains("watchers.conditions")),
        "a lowered condition budget refuses a larger draft",
    );
    check(
        proof
            .approve(&one, limited_watcher, limited_version)
            .await
            .is_err_and(|why| why.contains("watchers.conditions")),
        "approval enforces a condition budget lowered after drafting",
    );
    proof
        .setting(&one, "watchers.conditions", json!(20))
        .await?;
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
        "SELECT count(*) FROM base_watcher_runs u JOIN watcher_cases_cases c ON c.id::text=u.subject WHERE u.tenant_id<>c.tenant_id", &[],
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

    println!("\n== A restart between the commit and the watcher's job");
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
    proof.stop().await;
    let client = proof.db.get().await?;
    client.execute("UPDATE base_watcher_runs SET state='matched',finished_at=NULL WHERE subject=$1 AND watcher_id=$2", &[&c.to_string(), &r1]).await?;
    let retry: i64 = client.query_one(
        "UPDATE base_queue_jobs SET state='retryable',available_at=now() + interval '1 hour',
            attempt=1,completed_at=NULL WHERE worker_id='base/watchers-run'
            AND args->>'run_id'=(SELECT id::text FROM base_watcher_runs WHERE subject=$1 AND watcher_id=$2)
            RETURNING id",
        &[&c.to_string(), &r1],
    ).await?.get(0);
    drop(client);
    let later = proof
        .open(
            &one,
            1,
            "Later event during action backoff",
            "urgent",
            "2500.00",
        )
        .await?;
    proof.start()?;
    proof.settled().await?;
    let retry_waits: bool = proof
        .db
        .get()
        .await?
        .query_one(
            "SELECT state='retryable' FROM base_queue_jobs WHERE tenant_id=$1 AND id=$2",
            &[&one.tenant(), &retry],
        )
        .await?
        .get(0);
    check(
        retry_waits && proof.case(later).await?.1 == 1,
        "an admitted run's retry backoff leaves later events in the tenant free to act",
    );
    proof
        .db
        .get()
        .await?
        .execute(
            "UPDATE base_queue_jobs SET available_at=now() WHERE tenant_id=$1 AND id=$2",
            &[&one.tenant(), &retry],
        )
        .await?;
    proof.settled().await?;
    proof.show(c).await?;
    check(
        proof.case(c).await?.1 == 1,
        "a retry after the effect committed finds its receipt: still routed once",
    );

    println!("\n== An agent drafts a second watcher; a person approves it");
    let flag = json!({
        "name": "Flag big cases", "watch": OPENED, "if": {"match": "all", "conditions": [{"field": "amount", "op": "ge", "value": "10000"}]},
        "do": [{"operation": "watcher_cases.cases.flag", "input": json!({"note": "big"})}],
        "examples": [
            {"facts": json!({"amount": "10000.00"}).to_string(), "expect": "match"},
            {"facts": json!({"amount": "9999.99"}).to_string(), "expect": "no_match"}
        ],
        "explanation": "Flag every case of ten thousand or more."
    });
    let review = proof
        .review(&proof.agent.clone(), "base.watchers.draft", flag.clone())
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
            "SELECT drafted_via FROM base_watcher_versions WHERE watcher_id=$1 AND number=$2",
            &[&r3, &(v3 as i32)],
        )
        .await?
        .get(0);
    check(
        via == "agent",
        "the version records that an agent drafted it",
    );
    for (parameters, path) in [
        (json!(r#"{"note":"big"}"#), "do.0.input"),
        (Value::Null, "do.0.input"),
        (json!([]), "do.0.input"),
        (json!(true), "do.0.input"),
        (json!(42), "do.0.input"),
        (json!("scalar"), "do.0.input"),
        (json!({"note": 42}), "do.0.input.note"),
    ] {
        let malformed = json!({"name": "Invalid action parameters", "watch": OPENED,
            "do": [{"operation": "watcher_cases.cases.flag", "input": parameters}],
            "examples": [{"facts": "{}", "expect": "match"}]});
        check(
            proof
                .review(&agent, "base.watchers.draft", malformed.clone())
                .await
                .to_string()
                .contains(path),
            "preview rejects invalid action parameters at their field",
        );
        check(
            proof
                .write(&agent, "base.watchers.draft", malformed)
                .await
                .is_err_and(|why| why.contains(path)),
            "saving rejects invalid action parameters at their field",
        );
    }
    let omitted = json!({"name": "Every opened case", "watch": OPENED,
        "do": [{"operation": "watcher_cases.cases.flag"}]});
    let mut empty_filter = omitted.clone();
    empty_filter["if"] = json!({"match": "any", "conditions": []});
    check(
        proof
            .review(&agent, "base.watchers.draft", empty_filter.clone())
            .await
            .to_string()
            .contains("if.conditions"),
        "preview rejects a supplied empty filter at its field",
    );
    check(
        proof
            .write(&agent, "base.watchers.draft", empty_filter)
            .await
            .is_err_and(|why| why.contains("if.conditions")),
        "saving rejects a supplied empty filter",
    );
    let (every_watcher, every_version) = proof.draft(&one, omitted).await?;
    check(
        proof
            .approve(&one, every_watcher, every_version)
            .await
            .is_ok(),
        "a watcher with no if or examples is approved",
    );
    let ordinary = proof.open(&one, 1, "Every event", "low", "1.00").await?;
    proof.settled().await?;
    check(
        proof.case(ordinary).await?.2 == 1,
        "a watcher without if acts on an ordinary event",
    );
    proof
        .switch(&one, every_watcher, false)
        .await
        .map_err(|_| Error::Internal)?;
    let wrong = json!({"name": "Wrong example", "watch": OPENED, "if": {"conditions": [{"field": "amount", "op": "ge", "value": "10000"}]},
        "do": [{"operation": "watcher_cases.cases.flag", "input": json!({})}],
        "examples": [{"facts": json!({"amount": "5"}).to_string(), "expect": "match"}]});
    check(
        proof
            .write(&agent, "base.watchers.draft", wrong)
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
        "both approved watchers acted, with no AI call",
    );

    let imprecise = json!({"name": "Unrepresentable decimal", "watch": OPENED,
        "if": {"conditions": [{"field": "amount", "op": "eq", "value": "1.00000000000000000000000000001"}]},
        "do": [{"operation": "watcher_cases.cases.flag", "input": json!({})}],
        "examples": [{"facts": json!({"amount": "1.00"}).to_string(), "expect": "match"}]});
    check(
        proof
            .write(&one, "base.watchers.draft", imprecise)
            .await
            .is_err_and(|why| why.contains("representable exactly")),
        "an unrepresentable decimal is refused rather than rounded to one",
    );
    let exact = json!({"name": "Exact decimal boundary", "watch": OPENED,
        "if": {"conditions": [{"field": "amount", "op": "eq", "value": "1.0000000000000000000000000001"}]},
        "do": [{"operation": "watcher_cases.cases.flag", "input": json!({})}],
        "examples": [{"facts": json!({"amount": "1.00"}).to_string(), "expect": "no_match"},
            {"facts": json!({"amount": "1.0000000000000000000000000001"}).to_string(), "expect": "match"}]});
    let (exact_watcher, exact_version) = proof.draft(&one, exact).await?;
    check(
        proof
            .approve(&one, exact_watcher, exact_version)
            .await
            .is_ok(),
        "an exactly representable decimal retains its last digit",
    );
    let one_unit = proof.open(&one, 1, "Exact amount", "low", "1.00").await?;
    proof.settled().await?;
    check(
        proof.case(one_unit).await?.2 == 0,
        "one does not equal one plus 10 to the power minus 28",
    );
    proof
        .switch(&one, exact_watcher, false)
        .await
        .map_err(|_| Error::Internal)?;

    println!("\n== The Watchers page drafts with the same contract");
    // The page builds this input from its comboboxes and typed fields: an
    // `in` list as `values`, action parameters and example facts as JSON
    // text. A person drafts it, previews its review, and a version of the
    // same watcher follows with `watcher_id`; nothing else writes a watcher.
    let from_page = json!({
        "name": "Route by priority", "watch": OPENED, "if": {"match": "any", "conditions": [{"field": "priority", "op": "in", "values": ["urgent", "normal"]}]},
        "do": [{"operation": "watcher_cases.cases.route", "input": json!({"queue": "page"})}],
        "examples": [
            {"facts": json!({"priority": "urgent"}).to_string(), "expect": "match"},
            {"facts": json!({"priority": "low"}).to_string(), "expect": "no_match"}
        ],
        "explanation": "Send urgent and normal cases to the page queue."
    });
    let page_review = proof
        .review(&one, "base.watchers.draft", from_page.clone())
        .await;
    check(
        page_review["valid"] != json!(false)
            && page_review["preview"]["result"]["conditions"]
                .as_array()
                .is_some_and(|c| c.len() == 1)
            && page_review["preview"]["result"]["examples"]
                .as_array()
                .is_some_and(|e| e.len() == 2 && e.iter().all(|x| x["passes"] == json!(true))),
        "the page's review shows the conditions in words and each example passing",
    );
    let (page_watcher, page_v1) = proof.draft(&one, from_page.clone()).await?;
    let mut next = from_page;
    next["watcher_id"] = json!(page_watcher);
    next["if"]["conditions"] = json!([{"field": "priority", "op": "eq", "value": "urgent"}]);
    next["examples"] = json!([
        {"facts": json!({"priority": "urgent"}).to_string(), "expect": "match"},
        {"facts": json!({"priority": "normal"}).to_string(), "expect": "no_match"}
    ]);
    let (same_watcher, page_v2) = proof.draft(&one, next).await?;
    let via: String = proof
        .db
        .get()
        .await?
        .query_one(
            "SELECT drafted_via FROM base_watcher_versions WHERE watcher_id=$1 AND number=$2",
            &[&page_watcher, &(page_v2 as i32)],
        )
        .await?
        .get(0);
    check(
        same_watcher == page_watcher && page_v2 == page_v1 + 1 && via == "person",
        "the next version belongs to the same watcher and records that a person drafted it",
    );
    check(
        proof.approve(&one, page_watcher, page_v2).await.is_ok(),
        "a person approves the version the page drafted",
    );
    proof
        .switch(&one, page_watcher, false)
        .await
        .map_err(|_| Error::Internal)?;

    println!("\n== A denied capability stops the effect");
    proof
        .grant(&one, "watchers", "watcher-cases.case.route", Some(false))
        .await?;
    let denied = proof
        .open(&one, 1, "Network down", "urgent", "1500.00")
        .await?;
    proof.settled().await?;
    proof.show(denied).await?;
    check(
        proof.case(denied).await?.0.is_none(),
        "with route denied to the watchers principal the case was not routed",
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
        .grant(&one, "watchers", "watcher-cases.case.route", Some(true))
        .await?;

    println!("\n== A restricted field is never read");
    let margin = json!({"name": "Thin margins", "watch": OPENED, "if": {"conditions": [{"field": "margin", "op": "lt", "value": "500"}]},
        "do": [{"operation": "watcher_cases.cases.flag", "input": json!({})}],
        "examples": [{"facts": json!({"margin": "100"}).to_string(), "expect": "match"}]});
    let (r4, v4) = proof.draft(&one, margin.clone()).await?;
    check(
        proof.approve(&one, r4, v4).await.is_ok(),
        "a watcher on the margin is approved while nobody restricts it",
    );
    let historical_margin = proof
        .open(&one, 1, "Previously readable margin", "low", "800.00")
        .await?;
    proof.settled().await?;
    check(
        proof.case(historical_margin).await?.2 == 1,
        "the margin watcher records a successful match before restrictions change",
    );
    let restrictions = proof
        .services
        .get::<bilibi_authz::restrictions::FieldRestrictions>("base/authz.field_restrictions")?;
    restrictions
        .put(&one, "watcher_cases", "margin", &[2])
        .await?;
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
        "once the margin is restricted, the watcher is refused before it reads it",
    );
    check(proof.case(thin).await?.2 == 0, "and nothing was flagged");
    let (r5, v5) = proof.draft(&one, json!({"name": "Thin margins again", "watch": OPENED, "if": {"conditions": [{"field": "margin", "op": "lt", "value": "500"}]},
        "do": [{"operation": "watcher_cases.cases.flag", "input": json!({})}],
        "examples": [{"facts": json!({"margin": "100"}).to_string(), "expect": "match"}]})).await.unwrap_or((0, 0));
    let refused = proof.approve(&one, r5, v5).await;
    check(
        refused
            .as_ref()
            .is_err_and(|w| w.contains("hidden from automation"))
            || r5 == 0,
        "a new watcher on the restricted margin cannot be approved",
    );
    restrictions.remove(&one, "watcher_cases", "margin").await?;
    proof
        .switch(&one, r4, false)
        .await
        .map_err(|_| Error::Internal)?;
    custom_field_proof(proof, &restrictions).await?;
    linked_custom_field_proof(proof).await?;

    println!("\n== Cycles are refused at approval; a chain stops at the depth budget");
    let selfish = json!({"name": "Reroute forever", "watch": ROUTED,
        "do": [{"operation": "watcher_cases.cases.route", "input": json!({"queue": "again"})}],
        "examples": [{"facts": "{}", "expect": "match"}]});
    let (rs, vs) = proof.draft(&one, selfish).await?;
    check(
        proof
            .approve(&one, rs, vs)
            .await
            .is_err_and(|w| w.contains("would start itself")),
        "a watcher whose action records its own trigger is refused at approval",
    );
    let (ra, va) = proof.draft(&one, json!({"name": "Flag routed cases", "watch": ROUTED,
        "do": [{"operation": "watcher_cases.cases.flag", "input": json!({})}], "examples": [{"facts": "{}", "expect": "match"}]})).await?;
    let (rb, vb) = proof.draft(&one, json!({"name": "Route flagged cases", "watch": FLAGGED,
        "do": [{"operation": "watcher_cases.cases.route", "input": json!({"queue": "loop"})}], "examples": [{"facts": "{}", "expect": "match"}]})).await?;
    check(
        proof.approve(&one, ra, va).await.is_ok(),
        "\"flag routed cases\" is approved",
    );
    check(
        proof
            .approve(&one, rb, vb)
            .await
            .is_err_and(|why| why.contains("cycle")),
        "the second watcher of the routed-to-flagged-to-routed cycle is refused",
    );
    proof
        .setting(&one, "watchers.chain_depth", json!(1))
        .await?;
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
            "SELECT max(depth) FROM base_watcher_runs WHERE subject=$1",
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
    proof
        .setting(&one, "watchers.chain_depth", json!(3))
        .await?;

    println!("\n== An archived company stops the effect");
    proof.stop().await;
    let archived = proof
        .open(&one, 5, "Archived customer's case", "urgent", "1500.00")
        .await?;
    proof
        .services
        .get::<bilibi_company::CompanyService>(bilibi_company::SERVICE)?
        .archive_company(&one, 5, Some("Watchers walkthrough"))
        .await
        .map_err(|_| Error::Internal)?;
    proof.start()?;
    proof.settled().await?;
    proof.show(archived).await?;
    check(
        proof.case(archived).await?.0.is_none(),
        "the case of a company archived before the watcher acted was not routed",
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
            .authenticate(&**client, 1, 6, "watchers".into())
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
                operator.mutation_context("watchers.walkthrough.tenant"),
            )
            .await?;
        let moved = tenants
            .move_in(&mut tx, &operator, 2, change, "Watchers walkthrough")
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
                operator.mutation_context("watchers.walkthrough.tenant"),
            )
            .await?;
        let _ = tenants
            .move_in(
                &mut tx,
                &operator,
                2,
                bilibi_tenancy::Move::Reactivate,
                "Watchers walkthrough",
            )
            .await?;
        tx.commit().await?;
    }

    println!("\n== A stale version and a switched-off watcher, and a threshold change");
    let waiting = proof.hold_matching_case(&one, "Held back").await?;
    let (r1b, v1b) = proof.draft(&one, json!({"watcher_id": r1, "name": "Urgent cases to the priority desk",
        "watch": OPENED, "if": {"conditions": [{"field": "priority", "op": "eq", "value": "urgent"}, {"field": "amount", "op": "gt", "value": "5000"}]},
        "do": [{"operation": "watcher_cases.cases.route", "input": json!({"queue": "priority-desk"})}],
        "examples": [{"facts": json!({"priority": "urgent", "amount": "6000"}).to_string(), "expect": "match"}, {"facts": json!({"priority": "urgent", "amount": "3000"}).to_string(), "expect": "no_match"}],
        "explanation": "Raise the threshold to 5000."})).await?;
    check(
        r1b == r1 && v1b == 2,
        "the threshold change is the watcher's version 2, data only",
    );
    check(
        proof.approve(&one, r1, v1b).await.is_ok(),
        "version 2 is approved, with no release",
    );
    check(
        proof.switch(&one, r3, false).await.is_ok(),
        "\"flag big cases\" is switched off",
    );
    check(
        proof.resume_watcher_jobs(&one, 0).await? == 1,
        "the held job is still scheduled with its original attempt budget",
    );
    proof.start()?;
    proof.settled().await?;
    let held = proof.receipts(waiting).await?;
    check(
        held.iter()
            .any(|r| r.0 == r1 && r.1 == "refused" && r.2.as_deref() == Some("stale_version")),
        "the pinned old version is refused as stale",
    );
    check(
        held.iter()
            .any(|r| r.0 == r3 && r.1 == "refused" && r.2.as_deref() == Some("watcher_off")),
        "the switched-off watcher is refused",
    );
    check(
        proof.case(waiting).await? == (None, 0, 0),
        "the held case was neither routed nor flagged",
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
    check(
        proof.restore(&one, r1, 1).await.is_ok(),
        "an earlier approved version is restored after its checks pass again",
    );
    let restored = proof
        .open(&one, 1, "Restored threshold", "urgent", "3000.00")
        .await?;
    proof.settled().await?;
    check(
        proof.case(restored).await?.0.as_deref() == Some("priority-desk"),
        "the restored version 1 is active",
    );
    check(
        proof.restore(&one, r1, 2).await.is_ok(),
        "version 2 can be restored after version 1",
    );
    let current = proof
        .open(&one, 1, "Current threshold again", "urgent", "3000.00")
        .await?;
    proof.settled().await?;
    check(
        proof.case(current).await?.0.is_none(),
        "restoring version 2 makes its higher threshold active again",
    );

    println!("\n== Watchers share a tenant bucket and resume in order");
    proof.stop().await;
    proof
        .setting(&one, "watchers.runs_per_minute", json!(12))
        .await?;
    proof
        .switch(&one, r3, true)
        .await
        .map_err(|_| Error::Internal)?;
    let first = proof
        .open(&one, 1, "First in the burst", "urgent", "12000.00")
        .await?;
    let client = proof.db.get().await?;
    client.execute(
        "INSERT INTO base_watcher_buckets (tenant_id,tokens,refilled_at) VALUES ($1,1,clock_timestamp())
            ON CONFLICT (tenant_id) DO UPDATE SET tokens=1,refilled_at=EXCLUDED.refilled_at",
        &[&one.tenant()],
    ).await?;
    let token_due: std::time::SystemTime = client.query_one(
        "SELECT refilled_at + interval '5 seconds' FROM base_watcher_buckets WHERE tenant_id=$1",
        &[&one.tenant()],
    ).await?.get(0);
    drop(client);
    proof.start()?;
    let first_subject = first.to_string();
    let head = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row = proof
                .db
                .get()
                .await?
                .query_opt(
                    "SELECT q.id,q.available_at,q.attempt,clock_timestamp()
                 FROM base_queue_jobs q JOIN base_watcher_runs r
                    ON r.tenant_id=q.tenant_id AND r.event_id::text=q.args->>'event_id'
                 WHERE q.tenant_id=$1 AND q.worker_id='base/watchers-admit' AND q.state='scheduled'
                    AND r.subject=$2 AND r.watcher_id=$3 AND r.started_at IS NULL",
                    &[&one.tenant(), &first_subject, &r3],
                )
                .await?;
            if let Some(row) = row {
                return Ok::<_, Error>(row);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| Error::Internal)??;
    let head_id: i64 = head.get(0);
    let wake: std::time::SystemTime = head.get(1);
    let observed: std::time::SystemTime = head.get(3);
    check(
        head.get::<_, i32>(2) == 0 && observed < token_due
            && wake.duration_since(token_due).is_ok_and(|delta| delta <= Duration::from_millis(10)),
        "the original job waits without spending an attempt until one token refills at five seconds",
    );

    let second = proof
        .open(&one, 1, "Second in the burst", "urgent", "12000.00")
        .await?;
    let other = proof
        .open(&two, 100, "Meanwhile in tenant 2", "urgent", "1500.00")
        .await?;
    let subjects = [first_subject, second.to_string()];
    let starts = tokio::time::timeout(Duration::from_secs(19), async {
        loop {
            let starts = proof
                .db
                .get()
                .await?
                .query(
                    "SELECT subject,watcher_id,started_at FROM base_watcher_runs
                 WHERE tenant_id=$1 AND subject=ANY($2) AND state='done' ORDER BY started_at,id",
                    &[&one.tenant(), &&subjects[..]],
                )
                .await?;
            if starts.len() == 4 {
                return Ok::<_, Error>(starts);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| Error::Internal)??;
    let order = [
        (&subjects[0], r1),
        (&subjects[0], r3),
        (&subjects[1], r1),
        (&subjects[1], r3),
    ];
    check(
        starts.iter().zip(&order).all(|(row, &(subject, watcher))| {
            row.get::<_, String>(0) == *subject && row.get::<_, i64>(1) == watcher
        }),
        "each refill admits the next run in event order across both watchers",
    );
    let started: std::time::SystemTime = starts[1].get(2);
    let timings = proof
        .db
        .get()
        .await?
        .query_one(
            "SELECT q.state='completed',q.attempt,
            EXISTS(SELECT 1 FROM base_watcher_runs r WHERE r.tenant_id=$3 AND r.subject=$4
                AND r.state='done' AND r.finished_at < $5)
         FROM base_queue_jobs q WHERE q.tenant_id=$1 AND q.id=$2",
            &[
                &one.tenant(),
                &head_id,
                &two.tenant(),
                &other.to_string(),
                &token_due,
            ],
        )
        .await?;
    check(
        timings.get::<_, bool>(0)
            && timings.get::<_, i32>(1) == 1
            && started
                .duration_since(token_due)
                .is_ok_and(|late| late <= Duration::from_secs(2)),
        "the same job resumes on the next token within the polling tolerance",
    );
    check(
        timings.get::<_, bool>(2) && proof.case(other).await?.0.as_deref() == Some("vip-line"),
        "another tenant runs while the first tenant waits for a refill",
    );
    for case in [first, second] {
        check(
            proof.case(case).await? == (Some("priority-desk".into()), 1, 1),
            "the original deferred case is routed and flagged exactly once",
        );
    }
    proof
        .switch(&one, r3, false)
        .await
        .map_err(|_| Error::Internal)?;
    proof
        .setting(&one, "watchers.runs_per_minute", json!(10000))
        .await?;
    proof
        .db
        .get()
        .await?
        .execute(
            "DELETE FROM base_watcher_buckets WHERE tenant_id=$1",
            &[&one.tenant()],
        )
        .await?;
    println!("\n== A receipt shows a record's values only to who may read the record");
    proof
        .services
        .get::<Grants>("base/authz.grants")?
        .set_direct(
            &one,
            2,
            "admin.watchers.view",
            Some(bilibi_authz::grants::Direct::Allow(
                bilibi_authz::Reach::All,
            )),
        )
        .await?;
    let client = proof.db.get().await?;
    let viewer = proof
        .services
        .get::<Authentication>("base/tenancy.authentication")?
        .authenticate(&**client, 1, 2, "watchers".into())
        .await?;
    let theirs = proof
        .watchers
        .runs(&**client, &viewer, Some(r1), 50)
        .await?;
    let mine = proof.watchers.runs(&**client, &one, Some(r1), 50).await?;
    drop(client);
    check(
        !theirs.is_empty()
            && theirs.iter().all(|r| {
                r.facts == json!({"restricted": true}) && r.outcomes == json!({"restricted": true})
            }),
        "a reader of watchers who may not read cases sees each receipt's facts as restricted",
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
            "watcher-cases.case.view",
            Some(bilibi_authz::grants::Direct::Allow(
                bilibi_authz::Reach::All,
            )),
        )
        .await?;
    let client = proof.db.get().await?;
    let visible = proof
        .watchers
        .runs(&**client, &viewer, Some(r4), 50)
        .await?;
    check(
        visible.iter().any(|run| {
            run.subject == historical_margin.to_string() && run.facts.get("margin").is_some()
        }),
        "the receipt is visible while its field is readable",
    );
    drop(client);
    restrictions
        .put(&one, "watcher_cases", "margin", &[2])
        .await?;
    let client = proof.db.get().await?;
    let hidden = proof
        .watchers
        .runs(&**client, &viewer, Some(r4), 50)
        .await?;
    check(
        hidden.iter().any(|run| {
            run.state == "done"
                && run.facts == json!({"restricted": true})
                && run.outcomes == json!({"restricted": true})
        }),
        "a later field restriction hides historical facts and outcomes despite the record read grant",
    );
    drop(client);
    restrictions.remove(&one, "watcher_cases", "margin").await?;
    let before = proof.case(historical_margin).await?;
    for (operation, input) in [
        (
            "watcher_cases.cases.route",
            json!({"case_id": historical_margin, "expected_version": 1, "queue": "stale destination"}),
        ),
        (
            "watcher_cases.cases.flag",
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
        let mut draft = urgent_watcher("Pending threshold review", "priority-desk", "1000");
        draft["watcher_id"] = json!(r1);
        proof.draft(&one, draft).await?;
    }
    let client = proof.db.get().await?;
    let view = proof.watchers.get(&**client, &one, r1).await?;
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
    drop(client);
    proof
        .setting(&one, "watchers.runs_per_minute", json!(10000))
        .await?;
    let optional = json!({"name": "Cases without a parent title", "watch": OPENED,
        "if": {"conditions": [{"field": "parent.title", "op": "empty"}, {"field": "parent_edge.title", "op": "empty"}, {"field": "parent.parent.title", "op": "empty"}]},
        "do": [{"operation": "watcher_cases.cases.flag", "input": json!({})}],
        "examples": [{"facts": json!({"parent.title": null, "parent_edge.title": null, "parent.parent.title": null}).to_string(), "expect": "match"}]});
    let (optional_watcher, optional_version) = proof.draft(&one, optional).await?;
    check(
        proof
            .approve(&one, optional_watcher, optional_version)
            .await
            .is_ok(),
        "an empty condition on optional links passes approval",
    );
    let absent = proof.open(&one, 1, "No parent", "low", "1.00").await?;
    proof.settled().await?;
    check(
        proof.case(absent).await?.2 == 1,
        "absent direct, edge and chained links evaluate as null and allow the action",
    );
    proof
        .services
        .get::<Grants>("base/authz.grants")?
        .set_direct(
            &one,
            2,
            "watcher-cases.case.open",
            Some(bilibi_authz::grants::Direct::Allow(
                bilibi_authz::Reach::All,
            )),
        )
        .await?;
    let child = proof.write(&viewer, "watcher_cases.cases.open", json!({"company_id": 1, "title": "Reader-owned child", "priority": "low", "amount": "1.00", "parent_id": absent})).await.map_err(|_| Error::Invalid)?["case_id"].as_i64().ok_or(Error::Invalid)?;
    let unlinked = proof
        .open(&viewer, 1, "Reader-owned unlinked case", "low", "1.00")
        .await?;
    proof.settled().await?;
    proof
        .services
        .get::<Grants>("base/authz.grants")?
        .set_direct(
            &one,
            2,
            "watcher-cases.case.view",
            Some(bilibi_authz::grants::Direct::Allow(
                bilibi_authz::Reach::Own,
            )),
        )
        .await?;
    let client = proof.db.get().await?;
    let receipts = proof
        .watchers
        .runs(&**client, &viewer, Some(optional_watcher), 50)
        .await?;
    let child_run: i64 = client
        .query_one(
            "SELECT id FROM base_watcher_runs WHERE tenant_id=1 AND watcher_id=$1 AND subject=$2",
            &[&optional_watcher, &child.to_string()],
        )
        .await?
        .get(0);
    check(
        receipts.iter().any(|run| {
            run.id == child_run
                && run.facts == json!({"restricted": true})
                && run.outcomes == json!({"restricted": true})
        }),
        "a present parent outside the reader's reach keeps the receipt restricted",
    );
    check(
        receipts.iter().any(|run| {
            run.subject == unlinked.to_string()
                && run.state == "done"
                && run.facts.get("parent.title") == Some(&Value::Null)
        }),
        "an absent parent stays readable under own-record reach",
    );
    let catalog = bilibi_base::grid::Catalog::for_scope(&**client, &viewer)
        .await
        .map_err(Error::from)?;
    let source = catalog
        .table("watcher_cases")
        .ok_or(Error::Forbidden)?
        .scoped_source();
    for path in ["parent.title", "parent_edge.title", "parent.parent.title"] {
        let access = bilibi_base::grid::scalar_access(&catalog, "watcher_cases", path)
            .map_err(|_| Error::Invalid)?;
        for (id, expected) in [(child, false), (unlinked, true)] {
            let allowed: bool = client.query_one(&format!("SELECT {access} FROM {source} r WHERE r.id=$3 AND $1::bigint IS NOT NULL AND $2::bigint IS NOT NULL"), &[&viewer.tenant(), &viewer.user(), &id]).await?.get(0);
            check(
                allowed == expected,
                "each direct, edge and chained access predicate distinguishes absence from denied reach",
            );
        }
    }
    drop(client);
    let grants = proof.services.get::<Grants>("base/authz.grants")?;
    for key in ["admin.watchers.approve", "watcher-cases.case.flag"] {
        grants
            .set_direct(
                &one,
                2,
                key,
                Some(bilibi_authz::grants::Direct::Allow(
                    bilibi_authz::Reach::All,
                )),
            )
            .await?;
    }
    let broad = json!({"name": "Flag amounts across owners", "watch": OPENED,
        "if": {"conditions": [{"field": "amount", "op": "gt", "value": "0"}]},
        "do": [{"operation": "watcher_cases.cases.flag", "input": json!({})}],
        "examples": [{"facts": json!({"amount": "1.00"}).to_string(), "expect": "match"}]});
    let (broad_watcher, broad_version) = proof.draft(&one, broad).await?;
    for reach in [bilibi_authz::Reach::Own, bilibi_authz::Reach::Team] {
        grants
            .set_direct(
                &one,
                2,
                "watcher-cases.case.view",
                Some(bilibi_authz::grants::Direct::Allow(reach)),
            )
            .await?;
        let reviewed = proof
            .review(
                &viewer,
                "base.watchers.approve",
                json!({"watcher_id": broad_watcher, "version": broad_version}),
            )
            .await;
        check(
            reviewed.to_string().contains("access to all records"),
            "approval review explains that partial record reach cannot authorize tenant-wide reads",
        );
        check(
            proof
                .approve(&viewer, broad_watcher, broad_version)
                .await
                .is_err_and(|why| why.contains("access to all records")),
            "an approver limited to Own or Team cannot delegate reads of other owners' cases",
        );
    }
    grants
        .set_direct(
            &one,
            2,
            "watcher-cases.case.view",
            Some(bilibi_authz::grants::Direct::Allow(
                bilibi_authz::Reach::All,
            )),
        )
        .await?;
    check(
        proof
            .approve(&viewer, broad_watcher, broad_version)
            .await
            .is_ok(),
        "an approver with All reach can approve the same watcher",
    );
    proof
        .switch(&viewer, broad_watcher, false)
        .await
        .map_err(|_| Error::Internal)?;
    grants
        .set_direct(
            &one,
            2,
            "watcher-cases.case.view",
            Some(bilibi_authz::grants::Direct::Allow(
                bilibi_authz::Reach::Own,
            )),
        )
        .await?;
    check(
        proof.switch(&viewer, broad_watcher, true).await.is_err(),
        "Own reach cannot switch tenant-wide automation back on",
    );
    grants
        .set_direct(
            &one,
            2,
            "watcher-cases.case.view",
            Some(bilibi_authz::grants::Direct::Allow(
                bilibi_authz::Reach::All,
            )),
        )
        .await?;
    check(
        proof.switch(&viewer, broad_watcher, true).await.is_ok(),
        "All reach can switch the approved watcher back on",
    );
    proof
        .switch(&one, broad_watcher, false)
        .await
        .map_err(|_| Error::Internal)?;
    proof.setting(&one, "watchers.facts_read", json!(1)).await?;
    let fact_limited = proof
        .open(&one, 1, "Lowered fact budget", "urgent", "6000.00")
        .await?;
    proof.settled().await?;
    let fact_receipts = proof.receipts(fact_limited).await?;
    check(
        fact_receipts
            .iter()
            .any(|run| run.2.as_deref() == Some("watcher_limit"))
            && fact_receipts.iter().all(|run| {
                run.1 == "refused"
                    && matches!(run.2.as_deref(), Some("watcher_limit" | "watcher_off"))
            }),
        "a lowered fact budget refuses already-approved watchers at runtime",
    );
    check(
        proof.case(fact_limited).await? == (None, 0, 0),
        "the fact budget stops effects before reading condition facts",
    );
    proof
        .setting(&one, "watchers.facts_read", json!(21))
        .await?;
    let attempt_limited = proof
        .hold_matching_case(&one, "Exhausted attempt budget")
        .await?;
    proof
        .setting(&one, "watchers.max_attempts", json!(1))
        .await?;
    check(
        proof.resume_watcher_jobs(&one, 1).await? == 1,
        "the held job resumes with its allowed attempt already spent",
    );
    proof.start()?;
    proof.settled().await?;
    let attempt_receipts = proof.receipts(attempt_limited).await?;
    check(attempt_receipts.iter().any(|run| run.2.as_deref() == Some("attempt_limit")) && attempt_receipts.iter().all(|run| run.1 == "refused" && matches!(run.2.as_deref(), Some("attempt_limit" | "watcher_off"))), "the existing ending hook refuses waiting runs when the configured attempt budget is exhausted");
    check(
        proof.case(attempt_limited).await? == (None, 0, 0),
        "the attempt limit stops all effects on the matching case",
    );
    proof
        .setting(&one, "watchers.max_attempts", json!(25))
        .await?;
    Ok(())
}

#[path = "watchers/observations.rs"]
mod observations;
#[path = "watchers/scripts.rs"]
mod scripts;
