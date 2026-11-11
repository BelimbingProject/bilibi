//! Running specimen of producer-bound event types (`base/events`, "Binding an
//! event type to its producer"), separate from product code. Two modules live
//! only in this example and never name each other's crate: an Extension-shaped
//! connector that binds `event_producer/connector.call_observed` to its own
//! system principal, and a Domain-shaped intake that subscribes to it and
//! keeps its own configured sources. Run it once on a fresh disposable
//! DATABASE_URL:
//!
//! ```text
//! cargo run -p bilibi-web --example events-producer-reference
//! ```
//!
//! It prints each case and exits non-zero if any outcome is not the one
//! named: an unbound event type records as before; plain `record` on the
//! bound type is refused; `record_as` is refused without the tenant's grant,
//! under a denying grant, for another tenant, under impersonation, for an
//! actor or principal that is not the bound one; the granted producer
//! records, and the subscriber is handed the recorded producer; the same key
//! answers the same event, and conflicts when the payload or the recorded
//! producer differs; a subscriber refusal rolls its write back with the
//! receipt, and the retry hands over the same producer; a source that is
//! inactive, bound to another principal or owned by another tenant is refused
//! by the subscriber whatever the payload names.
//!
//! This proves the Base seam only. The connector here has no provider
//! transport: a real Extension authenticates its provider before it calls
//! `record_as`, and delivery stays at-least-once.
use bilibi_authz::{
    grants::Grants, CapabilityDefinition, SystemPrincipalDefinition, SystemScope,
    VocabularyContribution, VOCABULARY,
};
use bilibi_base::events::{
    Event, EventType, Events, NewEvent, On, Producer, Recorded, Subscribe, Subscriber,
    SubscriberFuture, DELIVER, EVENTS, PRINCIPAL, PRODUCERS, SERVICE, SUBSCRIBERS,
};
use bilibi_base::queue::{Config, Executor};
use bilibi_database::{pool, Change, Context, Db, Error, MutationBoundary, Principal, Result};
use bilibi_module_registry::{
    Catalog, Consumer, Contribution, Layer, Module, Provenance, Services,
};
use bilibi_tenancy::{Authentication, Scope};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio_postgres::Transaction;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const CONNECTOR: &str = "event_producer/connector";
const INTAKE: &str = "event_consumer/intake";
/// Bound to [`CONNECTOR_PRINCIPAL`] and [`PUBLISH`].
const OBSERVED: &str = "event_producer/connector.call_observed";
/// Not bound: recorded as every event type was before.
const NOTED: &str = "event_producer/connector.note_added";
const CONNECTOR_PRINCIPAL: &str = "event-producer-connector";
/// Declared by the same module with the same capability, but not the bound one.
const OTHER_PRINCIPAL: &str = "event-producer-other";
const PUBLISH: &str = "event-producer.connector.publish";
const SUBSCRIBER: &str = "event_consumer/intake.calls";
const ACTION: &str = "event_producer.connector.ingest";

macro_rules! connector {
    () => {
        Provenance {
            module: CONNECTOR,
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! { Module { id: CONNECTOR, layer: Layer::Extensions,
dependencies: &["base/authz", "base/database", "base/events"], migrations: &[], services: &[] } }
inventory::submit! { Contribution::new(VOCABULARY, "event_producer/connector.vocabulary", connector!(), &VocabularyContribution {
    domains: &["event-producer"], verbs: &["publish"],
    capabilities: &[CapabilityDefinition::tenant(PUBLISH)], roles: &[],
    system_principals: &[
        SystemPrincipalDefinition { name: CONNECTOR_PRINCIPAL, capabilities: &[PUBLISH] },
        SystemPrincipalDefinition { name: OTHER_PRINCIPAL, capabilities: &[PUBLISH] },
    ],
}) }
inventory::submit! { Contribution::new(EVENTS, OBSERVED, connector!(), &EventType { version: 1 }) }
inventory::submit! { Contribution::new(EVENTS, NOTED, connector!(), &EventType { version: 1 }) }
inventory::submit! { Contribution::new(PRODUCERS, OBSERVED, connector!(), &Producer { principal: CONNECTOR_PRINCIPAL, capability: PUBLISH }) }

inventory::submit! { Module { id: INTAKE, layer: Layer::Domains,
dependencies: &["base/authz", "base/database", "base/events"],
migrations: &[("event-consumer-intake-001-calls", "
        CREATE TABLE event_consumer_sources (tenant_id bigint NOT NULL, code text NOT NULL, principal text NOT NULL, active boolean NOT NULL, PRIMARY KEY (tenant_id, code));
        CREATE TABLE event_consumer_calls (tenant_id bigint NOT NULL, event_key text NOT NULL, source text, producer text, attempt integer NOT NULL, PRIMARY KEY (tenant_id, event_key));
    ")],
services: &[] } }
inventory::submit! { Contribution::new(SUBSCRIBERS, SUBSCRIBER, Provenance { module: INTAKE, source: file!(), line: line!() }, &Subscriber {
    events: &[On { event: OBSERVED, versions: &[1] }, On { event: NOTED, versions: &[1] }], handler: &Intake,
}) }

/// The Domain side. It writes its copy first and decides afterwards, so a
/// refusal shows the write rolling back with the receipt. For the bound event
/// the payload's `source` only selects a row: the tenant comes from the
/// delivery scope and the authority from the recorded producer.
struct Intake;
impl Subscribe for Intake {
    fn handle<'a>(
        &'a self,
        tx: &'a Transaction<'_>,
        scope: &'a SystemScope,
        event: &'a Event,
    ) -> SubscriberFuture<'a> {
        Box::pin(async move {
            let tenant = scope.tenant();
            let source = event.payload.get("source").and_then(Value::as_str);
            println!(
                "   intake: {} attempt {} in tenant {tenant}, delivery principal {:?}, recorded producer {:?}",
                event.key, event.attempt, scope.principal(), event.producer()
            );
            tx.execute(
                "INSERT INTO event_consumer_calls(tenant_id,event_key,source,producer,attempt) VALUES($1,$2,$3,$4,$5)",
                &[&tenant, &event.key, &source, &event.producer(), &event.attempt],
            )
            .await?;
            if event.name == OBSERVED
                && tx
                    .query_opt(
                        "SELECT 1 FROM event_consumer_sources WHERE tenant_id=$1 AND code=$2 AND principal=$3 AND active",
                        &[&tenant, &source, &event.producer()],
                    )
                    .await?
                    .is_none()
            {
                return Err(Error::Forbidden);
            }
            Ok(vec![Change {
                kind: "event_consumer/intake.call".into(),
                id: event.id,
                old: Value::Null,
                new: json!({"key": event.key, "producer": event.producer()}),
            }])
        })
    }
}

static FAILED: AtomicBool = AtomicBool::new(false);
fn expect(what: &str, ok: bool) {
    if ok {
        println!("ok   {what}");
    } else {
        FAILED.store(true, Ordering::Relaxed);
        println!("FAIL {what}");
    }
}

/// Boot builds the catalog from these contributions; show what it refuses
/// before any service is built.
fn boot_refusals(modules: &[&'static Module]) {
    static OUTSIDE: Contribution = Contribution::new(
        PRODUCERS,
        NOTED,
        Provenance {
            module: INTAKE,
            source: file!(),
            line: line!(),
        },
        &Producer {
            principal: CONNECTOR_PRINCIPAL,
            capability: PUBLISH,
        },
    );
    static UNNAMED: Contribution = Contribution::new(
        PRODUCERS,
        NOTED,
        Provenance {
            module: CONNECTOR,
            source: file!(),
            line: line!(),
        },
        &Producer {
            principal: "",
            capability: PUBLISH,
        },
    );
    for contribution in [&OUTSIDE, &UNNAMED] {
        let built = Catalog::build(
            modules,
            inventory::iter::<Consumer>.into_iter().collect(),
            inventory::iter::<Contribution>
                .into_iter()
                .chain([contribution])
                .collect(),
        );
        match built {
            Ok(_) => expect("boot refuses a malformed producer binding", false),
            Err(error) => println!("ok   boot refuses: {error}"),
        }
    }
}

struct Proof {
    db: Db,
    boundary: Arc<MutationBoundary>,
    events: Arc<Events>,
    grants: Arc<Grants>,
    admin: Scope,
}
enum Call<'a> {
    Record,
    RecordAs(&'a SystemScope),
}
impl Proof {
    async fn scope(&self, tenant: i64, principal: Option<&str>) -> Result<SystemScope> {
        SystemScope::rehydrate(&**self.db.get().await?, tenant, principal).await
    }
    /// One recording attempt in its own transaction: the facade's answer and
    /// whether the transaction could still commit.
    async fn attempt(
        &self,
        context: Context,
        call: Call<'_>,
        name: &str,
        key: &str,
        payload: Value,
    ) -> Result<(Result<Recorded>, bool)> {
        let mut client = self.db.get().await?;
        let mut tx = self.boundary.begin(&mut client, context).await?;
        let event = NewEvent {
            name,
            subject: &format!("call:{key}"),
            key,
            payload,
        };
        let recorded = match call {
            Call::Record => self.events.record(&mut tx, event).await,
            Call::RecordAs(scope) => self.events.record_as(&mut tx, scope, event).await,
        };
        let committed = tx.commit().await.is_ok();
        Ok((recorded, committed))
    }
    /// A refusal: the named error, a transaction that can no longer commit,
    /// and no event under the key in any tenant.
    async fn refused(
        &self,
        what: &str,
        context: Context,
        call: Call<'_>,
        key: &str,
        error: fn(&Error) -> bool,
    ) -> Result<()> {
        let (recorded, committed) = self
            .attempt(context, call, OBSERVED, key, json!({"source": "pbx-1"}))
            .await?;
        let rows: i64 = self
            .db
            .get()
            .await?
            .query_one(
                "SELECT count(*) FROM base_events WHERE event_key=$1",
                &[&key],
            )
            .await?
            .get(0);
        println!("     {recorded:?}, transaction committed: {committed}, events recorded: {rows}");
        expect(
            what,
            recorded.as_ref().is_err_and(error) && !committed && rows == 0,
        );
        Ok(())
    }
    async fn decision(&self, scope: &SystemScope) -> Result<&'static str> {
        let client = self.db.get().await?;
        Ok(bilibi_authz::decide_system(&**client, scope, PUBLISH)
            .await?
            .reason)
    }
    async fn source(&self, tenant: i64, code: &str, principal: &str, active: bool) -> Result<()> {
        println!("   intake configures source {code} in tenant {tenant}: principal {principal}, active {active}");
        self.db.get().await?.execute(
            "INSERT INTO event_consumer_sources(tenant_id,code,principal,active) VALUES($1,$2,$3,$4) ON CONFLICT (tenant_id,code) DO UPDATE SET principal=$3, active=$4",
            &[&tenant, &code, &principal, &active],
        ).await?;
        Ok(())
    }
    /// (stored producer, delivered, attempts, last failure, subscriber's copy rows).
    async fn delivery(
        &self,
        key: &str,
    ) -> Result<(Option<String>, bool, i32, Option<String>, i64)> {
        let client = self.db.get().await?;
        let row = client.query_one(
            "SELECT e.producer,d.delivered_at IS NOT NULL,d.attempts,d.last_error,(SELECT count(*) FROM event_consumer_calls c WHERE c.event_key=e.event_key) FROM base_events e JOIN base_events_deliveries d ON d.event_id=e.id AND d.subscriber=$2 WHERE e.event_key=$1",
            &[&key, &SUBSCRIBER],
        ).await?;
        Ok((row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
    }
    async fn until<F, Fut>(&self, what: &str, done: F) -> Result<()>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<bool>>,
    {
        for _ in 0..600 {
            if done().await? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!("timed out waiting for {what}");
        Err(Error::Internal)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    // A fixture binary enrolls no authenticator; grants still go through `Grants::set_system`.
    services
        .get::<bilibi_base::step_up::StepUp>(bilibi_base::step_up::SERVICE)?
        .set_enforced(false);
    let grants = services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let admin = services
        .get::<Authentication>("base/tenancy.authentication")?
        .authenticate(&**db.get().await?, 1, 4, "events".into())
        .await?;
    // Delivery is the tenant's own, separate consent.
    grants
        .set_system(&admin, PRINCIPAL, DELIVER, Some(true))
        .await?;
    let proof = Proof {
        db: db.clone(),
        boundary: services.get("base/database.mutations")?,
        events: services.get(SERVICE)?,
        grants,
        admin,
    };
    boot_refusals(&modules);
    let executor = Executor::from_services(
        &services,
        Config {
            concurrency: 4,
            poll_interval: Duration::from_millis(100),
            lease_seconds: 30,
            max_runtime: Duration::from_secs(60),
        },
    )?;
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(executor.run(shutdown));
    let result = cases(&proof).await;
    let _ = stop.send(true);
    let _ = worker.await;
    result?;
    if FAILED.load(Ordering::Relaxed) {
        println!("one or more cases FAILED");
        return Err(Error::Internal);
    }
    println!("every case had the named outcome");
    Ok(())
}

async fn cases(proof: &Proof) -> Result<()> {
    let forbidden: fn(&Error) -> bool = |error| matches!(error, Error::Forbidden);
    let connector = proof.scope(1, Some(CONNECTOR_PRINCIPAL)).await?;
    let other = proof.scope(1, Some(OTHER_PRINCIPAL)).await?;
    let anonymous = proof.scope(1, None).await?;
    let elsewhere = proof.scope(2, Some(CONNECTOR_PRINCIPAL)).await?;
    let user = || proof.admin.mutation_context(ACTION);
    let own = || connector.mutation_context(ACTION);

    println!("-- an unbound event type records as before");
    let (first, committed) = proof
        .attempt(
            user(),
            Call::Record,
            NOTED,
            "note-1",
            json!({"text": "hello"}),
        )
        .await?;
    let (again, _) = proof
        .attempt(
            user(),
            Call::Record,
            NOTED,
            "note-1",
            json!({"text": "hello"}),
        )
        .await?;
    println!("     {first:?} then {again:?}");
    expect(
        "record() of an unbound type by a signed-in user records once and answers the same event again",
        matches!((&first, &again), (Ok(Recorded::New(a)), Ok(Recorded::Existing(b))) if a == b) && committed,
    );
    let (bound, _) = proof
        .attempt(
            own(),
            Call::RecordAs(&connector),
            NOTED,
            "note-2",
            json!({"text": "x"}),
        )
        .await?;
    println!("     {bound:?}");
    expect(
        "record_as() on an unbound type is Invalid: a producer is never stamped on one",
        matches!(bound, Err(Error::Invalid)),
    );

    println!("-- plain record() on the bound type");
    proof
        .refused(
            "a signed-in user's record() is refused",
            user(),
            Call::Record,
            "plain-user",
            forbidden,
        )
        .await?;
    proof
        .refused(
            "record() under the connector's own context is refused too",
            own(),
            Call::Record,
            "plain-own",
            forbidden,
        )
        .await?;

    println!("-- record_as() before the tenant grants anything");
    println!(
        "     decision for {CONNECTOR_PRINCIPAL} in tenant 1: {}",
        proof.decision(&connector).await?
    );
    proof
        .refused(
            "the declared principal with no tenant grant is refused",
            own(),
            Call::RecordAs(&connector),
            "no-grant",
            forbidden,
        )
        .await?;

    println!("-- tenant 1's administrator grants {PUBLISH} to both declared principals");
    for principal in [CONNECTOR_PRINCIPAL, OTHER_PRINCIPAL] {
        proof
            .grants
            .set_system(&proof.admin, principal, PUBLISH, Some(true))
            .await?;
    }
    println!(
        "     decision for {CONNECTOR_PRINCIPAL} in tenant 1: {}",
        proof.decision(&connector).await?
    );
    proof.source(1, "pbx-1", CONNECTOR_PRINCIPAL, true).await?;
    let (recorded, committed) = proof
        .attempt(
            own(),
            Call::RecordAs(&connector),
            OBSERVED,
            "call-1",
            json!({"source": "pbx-1"}),
        )
        .await?;
    println!("     {recorded:?}");
    expect(
        "the granted, bound principal records",
        matches!(recorded, Ok(Recorded::New(_))) && committed,
    );
    proof
        .until("call-1 delivered", || async {
            Ok(proof.delivery("call-1").await?.1)
        })
        .await?;
    let (producer, delivered, attempts, _, copies) = proof.delivery("call-1").await?;
    println!("     stored producer {producer:?}, delivered {delivered}, attempts {attempts}, intake rows {copies}");
    expect(
        "the event row stores the producer and the subscriber applied it once",
        producer.as_deref() == Some(CONNECTOR_PRINCIPAL) && copies == 1,
    );

    println!("-- the transaction's context and the scope must both be the bound principal");
    let mut impersonated = own();
    impersonated.impersonator = Some(Principal::user(4));
    proof
        .refused(
            "an impersonated context is refused",
            impersonated,
            Call::RecordAs(&connector),
            "impersonated",
            forbidden,
        )
        .await?;
    proof
        .refused(
            "a user's transaction holding the connector's scope is refused",
            user(),
            Call::RecordAs(&connector),
            "user-actor",
            forbidden,
        )
        .await?;
    proof
        .refused(
            "a context naming another system actor is refused",
            Context::system(1, "not-the-connector", ACTION),
            Call::RecordAs(&connector),
            "other-actor",
            forbidden,
        )
        .await?;
    proof
        .refused(
            "another declared and granted principal of the same module is refused",
            other.mutation_context(ACTION),
            Call::RecordAs(&other),
            "other-principal",
            forbidden,
        )
        .await?;
    proof
        .refused(
            "an anonymous system scope is refused",
            own(),
            Call::RecordAs(&anonymous),
            "anonymous",
            forbidden,
        )
        .await?;

    println!("-- another tenant");
    proof
        .refused(
            "tenant 2's scope in tenant 1's transaction is refused",
            own(),
            Call::RecordAs(&elsewhere),
            "cross-scope",
            forbidden,
        )
        .await?;
    println!(
        "     decision for {CONNECTOR_PRINCIPAL} in tenant 2: {}",
        proof.decision(&elsewhere).await?
    );
    proof
        .refused(
            "tenant 1's grant admits nothing in tenant 2",
            elsewhere.mutation_context(ACTION),
            Call::RecordAs(&elsewhere),
            "cross-tenant",
            forbidden,
        )
        .await?;

    println!("-- the tenant denies, withdraws, then grants again");
    proof
        .grants
        .set_system(&proof.admin, CONNECTOR_PRINCIPAL, PUBLISH, Some(false))
        .await?;
    println!("     decision: {}", proof.decision(&connector).await?);
    proof
        .refused(
            "a denying grant refuses",
            own(),
            Call::RecordAs(&connector),
            "denied",
            forbidden,
        )
        .await?;
    proof
        .grants
        .set_system(&proof.admin, CONNECTOR_PRINCIPAL, PUBLISH, None)
        .await?;
    println!("     decision: {}", proof.decision(&connector).await?);
    proof
        .refused(
            "a withdrawn grant refuses",
            own(),
            Call::RecordAs(&connector),
            "withdrawn",
            forbidden,
        )
        .await?;
    let (replayed, _) = proof
        .attempt(
            own(),
            Call::RecordAs(&connector),
            OBSERVED,
            "call-1",
            json!({"source": "pbx-1"}),
        )
        .await?;
    println!("     {replayed:?}");
    expect(
        "without the grant even an exact replay of a recorded key is refused",
        matches!(replayed, Err(Error::Forbidden)),
    );
    proof
        .grants
        .set_system(&proof.admin, CONNECTOR_PRINCIPAL, PUBLISH, Some(true))
        .await?;

    println!("-- replays");
    let (same, _) = proof
        .attempt(
            own(),
            Call::RecordAs(&connector),
            OBSERVED,
            "call-1",
            json!({"source": "pbx-1"}),
        )
        .await?;
    let (changed, _) = proof
        .attempt(
            own(),
            Call::RecordAs(&connector),
            OBSERVED,
            "call-1",
            json!({"source": "pbx-9"}),
        )
        .await?;
    println!("     same event {same:?}; changed payload {changed:?}");
    expect(
        "the same key answers the event already recorded; a changed payload conflicts",
        matches!(same, Ok(Recorded::Existing(_))) && matches!(changed, Err(Error::Conflict)),
    );
    // Stored state only: rows as a deployment with a different binding (another
    // principal, or none yet) would have left them. One binary has one binding.
    let client = proof.db.get().await?;
    for (key, producer) in [("rebound", Some(OTHER_PRINCIPAL)), ("unbound", None)] {
        client.execute(
            "INSERT INTO base_events(tenant_id,publisher,name,version,subject,event_key,payload,trace_id,producer) VALUES(1,$1,$2,1,$3,$4,$5,'walkthrough',$6)",
            &[&CONNECTOR, &OBSERVED, &format!("call:{key}"), &key, &json!({"source": "pbx-1"}), &producer],
        ).await?;
        let (replay, _) = proof
            .attempt(
                own(),
                Call::RecordAs(&connector),
                OBSERVED,
                key,
                json!({"source": "pbx-1"}),
            )
            .await?;
        println!("     key {key} stored with producer {producer:?}: {replay:?}");
        expect(
            "an identical event under a key recorded with a different producer conflicts",
            matches!(replay, Err(Error::Conflict)),
        );
    }
    drop(client);

    println!("-- a refused delivery rolls the subscriber's write back; the retry hands over the same producer");
    proof.source(1, "pbx-2", CONNECTOR_PRINCIPAL, false).await?;
    proof
        .attempt(
            own(),
            Call::RecordAs(&connector),
            OBSERVED,
            "call-2",
            json!({"source": "pbx-2"}),
        )
        .await?
        .0?;
    proof
        .until("call-2 refused", || async {
            Ok(proof.delivery("call-2").await?.2 >= 1)
        })
        .await?;
    let (producer, delivered, attempts, error, copies) = proof.delivery("call-2").await?;
    println!("     stored producer {producer:?}, delivered {delivered}, attempts {attempts}, last failure {error:?}, intake rows {copies}");
    expect(
        "an inactive source is refused: no receipt and no subscriber row, though the handler wrote one",
        !delivered && copies == 0 && error.as_deref() == Some("subscriber_forbidden"),
    );
    proof.source(1, "pbx-2", CONNECTOR_PRINCIPAL, true).await?;
    proof
        .until("call-2 delivered", || async {
            Ok(proof.delivery("call-2").await?.1)
        })
        .await?;
    let (producer, _, attempts, _, copies) = proof.delivery("call-2").await?;
    let row = proof
        .db
        .get()
        .await?
        .query_one(
            "SELECT producer,attempt FROM event_consumer_calls WHERE event_key='call-2'",
            &[],
        )
        .await?;
    let (seen, attempt): (Option<String>, i32) = (row.get(0), row.get(1));
    println!("     delivered after {attempts} attempts; intake row written on attempt {attempt} with producer {seen:?}");
    expect(
        "the retry commits receipt and write together, with the producer recorded at the start",
        copies == 1
            && attempt >= 2
            && seen == producer
            && seen.as_deref() == Some(CONNECTOR_PRINCIPAL),
    );

    println!(
        "-- the payload's source only selects; the recorded producer and delivery tenant decide"
    );
    proof.source(1, "pbx-3", OTHER_PRINCIPAL, true).await?;
    proof.source(2, "pbx-4", CONNECTOR_PRINCIPAL, true).await?;
    for (key, source) in [
        ("call-3", "pbx-3"),
        ("call-4", "pbx-4"),
        ("call-5", "pbx-unknown"),
    ] {
        proof
            .attempt(
                own(),
                Call::RecordAs(&connector),
                OBSERVED,
                key,
                json!({"source": source}),
            )
            .await?
            .0?;
    }
    for (key, what) in [
        (
            "call-3",
            "a source bound to another principal refuses the connector's event",
        ),
        (
            "call-4",
            "a source configured only in tenant 2 refuses tenant 1's event",
        ),
        ("call-5", "an unknown source refuses"),
    ] {
        proof
            .until(key, || async { Ok(proof.delivery(key).await?.2 >= 1) })
            .await?;
        let (_, delivered, attempts, error, copies) = proof.delivery(key).await?;
        println!("     {key}: delivered {delivered}, attempts {attempts}, last failure {error:?}, intake rows {copies}");
        expect(
            what,
            !delivered && copies == 0 && error.as_deref() == Some("subscriber_forbidden"),
        );
    }

    println!("-- the unbound event reaches the subscriber with no producer");
    proof
        .until("note-1 delivered", || async {
            Ok(proof.delivery("note-1").await?.1)
        })
        .await?;
    let seen: Option<String> = proof
        .db
        .get()
        .await?
        .query_one(
            "SELECT producer FROM event_consumer_calls WHERE event_key='note-1'",
            &[],
        )
        .await?
        .get(0);
    expect(
        "an unbound event is delivered with producer None",
        seen.is_none(),
    );

    println!("-- events recorded in this run");
    for row in proof.db.get().await?.query(
        "SELECT e.id,e.tenant_id,e.name,e.event_key,e.producer,d.delivered_at IS NOT NULL,d.attempts,d.last_error FROM base_events e LEFT JOIN base_events_deliveries d ON d.event_id=e.id ORDER BY e.id",
        &[],
    ).await? {
        println!(
            "   event {} tenant {} {} {}: producer {:?}, delivered {:?}, attempts {:?}, last failure {:?}",
            row.get::<_, i64>(0), row.get::<_, i64>(1), row.get::<_, String>(2), row.get::<_, String>(3),
            row.get::<_, Option<String>>(4), row.get::<_, Option<bool>>(5), row.get::<_, Option<i32>>(6), row.get::<_, Option<String>>(7)
        );
    }
    Ok(())
}
