//! Running proof of event retention and the subscriber watermark
//! (`base/events`, issue 547), separate from product code. It needs
//! BILIBI_DISPOSABLE_EVENTS_DEMO=yes and a fresh disposable DATABASE_URL:
//!
//! ```text
//! cargo run -p bilibi-web --example events-retention
//! ```
//!
//! No Domain is mounted, so the example contributes a reference event type
//! and a reference subscriber of its own under two mounted modules of
//! different layers. It records events through `Events::record`, lets the
//! queue deliver them, and writes two events without deliveries, as a
//! subscriber unmounted at the time would find them. It then moves recorded
//! times back and calls `Events::prune`, the call the daily job makes.
//!
//! It checks: the missed count and its first event come from the watermark,
//! which stops before a gap although a later event is delivered; at the keep
//! time one event goes and one a second younger stays; an event nobody reads
//! goes; an event with a failing delivery and an event a mounted subscriber
//! has not received stay past the keep time; a second run deletes nothing;
//! the tenant's own `events.retention_days` is used and a value under its
//! minimum is refused; after a replay the gap closes, the watermark moves on
//! and the replayed event can go. A failed check ends the run with an error.
use bilibi_authz::{grants::Grants, SystemScope};
use bilibi_base::events::{
    Event, EventType, Events, NewEvent, On, ReplayFrom, Subscribe, Subscriber, SubscriberFuture,
    DELIVER, EVENTS, PRINCIPAL, RETENTION_DAYS, SERVICE, SUBSCRIBERS,
};
use bilibi_base::queue::{Config, Executor};
use bilibi_base::schedule::Schedule;
use bilibi_base::settings::{Settings, Target};
use bilibi_database::{pool, Db, Error, MutationBoundary, Result};
use bilibi_module_registry::{verify_composition, Contribution, Provenance, Services};
use bilibi_tenancy::{Authentication, Scope};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio_postgres::Transaction;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const NOTED: &str = "core/user.walkthrough_noted";
const UNREAD: &str = "core/user.walkthrough_unread";
const SUBSCRIBER: &str = "base/authz_agent.walkthrough_notes";
const DAY: u64 = 86_400;

/// Receives a note, or refuses one whose payload says so.
struct Notes;
impl Subscribe for Notes {
    fn handle<'a>(
        &'a self,
        _: &'a Transaction<'_>,
        _: &'a SystemScope,
        event: &'a Event,
    ) -> SubscriberFuture<'a> {
        Box::pin(async move {
            if event.payload["refuse"] == json!(true) {
                Err(Error::Conflict)
            } else {
                Ok(Vec::new())
            }
        })
    }
}
inventory::submit! { Contribution::new(EVENTS, NOTED, Provenance { module: "core/user", source: file!(), line: line!() }, &EventType { version: 1 }) }
inventory::submit! { Contribution::new(EVENTS, UNREAD, Provenance { module: "core/user", source: file!(), line: line!() }, &EventType { version: 1 }) }
inventory::submit! { Contribution::new(SUBSCRIBERS, SUBSCRIBER, Provenance { module: "base/authz_agent", source: file!(), line: line!() }, &Subscriber {
    events: &[On { event: NOTED, versions: &[1] }],
    handler: &Notes,
}) }

struct Proof {
    db: Db,
    boundary: Arc<MutationBoundary>,
    events: Arc<Events>,
    settings: Arc<Settings>,
    admin: Scope,
    failures: u32,
}
impl Proof {
    fn check(&mut self, label: &str, ok: bool, detail: String) {
        if !ok {
            self.failures += 1;
        }
        println!("{} {label}: {detail}", if ok { "ok   " } else { "FAIL " });
    }
    /// Records one event as the admin, as a publisher's own write would.
    async fn record(&self, name: &str, number: u32, refuse: bool) -> Result<i64> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                self.admin.mutation_context("events.retention.walkthrough"),
            )
            .await?;
        let recorded = self
            .events
            .record(
                &mut tx,
                NewEvent {
                    name,
                    subject: &format!("note:{number}"),
                    key: &format!("noted:{number}"),
                    payload: json!({"refuse": refuse}),
                },
            )
            .await?;
        tx.commit().await?;
        Ok(recorded.id())
    }
    /// Writes an event with no delivery: what a subscriber that was not
    /// mounted when it was recorded finds afterwards.
    async fn unseen(&self, number: u32) -> Result<i64> {
        Ok(self.db.get().await?.query_one(
            "INSERT INTO base_events(tenant_id,publisher,name,version,subject,event_key,payload,trace_id) \
             VALUES(1,'core/user',$1,1,$2,$3,'{\"refuse\":false}'::jsonb,'walkthrough') RETURNING id",
            &[&NOTED, &format!("note:{number}"), &format!("noted:{number}")],
        ).await?.get(0))
    }
    /// Waits until the subscriber holds `delivered` receipts and `failing`
    /// deliveries that were refused at least once.
    async fn settled(&self, delivered: i64, failing: i64) -> Result<()> {
        for _ in 0..600 {
            let row = self.db.get().await?.query_one(
                "SELECT count(*) FILTER (WHERE delivered_at IS NOT NULL),count(*) FILTER (WHERE delivered_at IS NULL AND last_error IS NOT NULL) \
                 FROM base_events_deliveries WHERE subscriber=$1",
                &[&SUBSCRIBER],
            ).await?;
            if row.get::<_, i64>(0) == delivered && row.get::<_, i64>(1) == failing {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!("timed out waiting for {delivered} delivered and {failing} failing");
        Err(Error::Internal)
    }
    async fn age(&self, id: i64, recorded_at: SystemTime) -> Result<()> {
        self.db
            .get()
            .await?
            .execute(
                "UPDATE base_events SET recorded_at=$2 WHERE id=$1",
                &[&id, &recorded_at],
            )
            .await?;
        Ok(())
    }
    /// Which of `ids` are still recorded.
    async fn kept(&self, ids: &[i64]) -> Result<Vec<i64>> {
        Ok(self
            .db
            .get()
            .await?
            .query(
                "SELECT id FROM base_events WHERE id=ANY($1) ORDER BY id",
                &[&ids],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect())
    }
    /// How many deliveries the events of `ids` still have.
    async fn deliveries(&self, ids: &[i64]) -> Result<i64> {
        Ok(self
            .db
            .get()
            .await?
            .query_one(
                "SELECT count(*) FROM base_events_deliveries WHERE event_id=ANY($1)",
                &[&ids],
            )
            .await?
            .get(0))
    }
    async fn watermark(&self) -> Result<i64> {
        Ok(self
            .db
            .get()
            .await?
            .query_opt(
                "SELECT event_id FROM base_events_watermarks WHERE tenant_id=1 AND subscriber=$1",
                &[&SUBSCRIBER],
            )
            .await?
            .map_or(0, |row| row.get(0)))
    }
    /// The subscriber's missed count, first missed event, whether the count
    /// stopped early, and its pending and failing deliveries.
    async fn gap(&self) -> Result<(i64, Option<i64>, bool, i64, i64)> {
        let mut client = self.db.get().await?;
        let states = self
            .events
            .subscriber_states(&mut client, &self.admin)
            .await?;
        let state = states
            .iter()
            .find(|state| state.subscriber == SUBSCRIBER)
            .ok_or(Error::NotFound)?;
        Ok((
            state.missed,
            state.first_missed,
            state.missed_more,
            state.pending,
            state.failing,
        ))
    }
    async fn prune(&self, now: SystemTime) -> Result<u64> {
        let mut client = self.db.get().await?;
        self.events.prune(&mut client, now).await
    }
    /// Sets the tenant's own keep time, as the settings page would.
    async fn keep(&self, days: i64) -> Result<()> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                self.admin.mutation_context("events.retention.walkthrough"),
            )
            .await?;
        match self
            .settings
            .put(
                &mut tx,
                &self.admin,
                Target::Tenant,
                RETENTION_DAYS,
                json!(days),
            )
            .await
        {
            Ok(()) => tx.commit().await,
            Err(error) => {
                tx.rollback().await?;
                Err(error)
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_EVENTS_DEMO").as_deref() != Ok("yes") {
        eprintln!("refusing: set BILIBI_DISPOSABLE_EVENTS_DEMO=yes and a disposable DATABASE_URL");
        return Err(Error::Invalid);
    }
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
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide(
        "bootstrap.composition",
        std::sync::Arc::new(bilibi_web::composition()),
    )?;
    services.install(&modules)?;
    let grants = services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let events = services.get::<Events>(SERVICE)?;
    let client = db.get().await?;
    let admin = services
        .get::<Authentication>("base/tenancy.authentication")?
        .authenticate(&**client, 1, 4, "events".into())
        .await?;
    drop(client);
    // Declaration grants nothing: the tenant admin grants delivery once.
    grants
        .set_system(&admin, PRINCIPAL, DELIVER, Some(true))
        .await?;
    let registered = services
        .get::<Schedule>("base/schedule.store")?
        .definition("base/events-retention")
        .map(|registered| (registered.owner(), registered.definition().expression));
    let mut proof = Proof {
        db: db.clone(),
        boundary: services.get("base/database.mutations")?,
        events: events.clone(),
        settings: services.get("base/settings.store")?,
        admin,
        failures: 0,
    };
    proof.check(
        "the daily retention job is a contributed schedule definition",
        registered.is_some(),
        format!("{registered:?}"),
    );

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
    let result = run(&mut proof).await;
    let _ = stop.send(true);
    let _ = worker.await;
    result?;
    if proof.failures > 0 {
        println!("{} check(s) failed", proof.failures);
        return Err(Error::Internal);
    }
    Ok(())
}

async fn run(proof: &mut Proof) -> Result<()> {
    // Two delivered notes, one the subscriber refuses, one event nobody reads.
    let old = proof.record(NOTED, 1, false).await?;
    let young = proof.record(NOTED, 2, false).await?;
    let failing = proof.record(NOTED, 3, true).await?;
    let unread = proof.record(UNREAD, 4, false).await?;
    proof.settled(2, 1).await?;
    // Two notes recorded while the subscriber was not mounted, then one more
    // that is delivered: the newest receipt lies after the gap.
    let missed = proof.unseen(5).await?;
    let missed_later = proof.unseen(6).await?;
    let newest = proof.record(NOTED, 7, false).await?;
    proof.settled(3, 1).await?;

    let gap = proof.gap().await?;
    proof.check(
        "two events are missed, from the first one without a delivery, with one delivery failing",
        gap == (2, Some(missed), false, 1, 1),
        format!("{gap:?}, first missed event {missed}"),
    );
    let mark = proof.watermark().await?;
    proof.check(
        "the watermark stops before the gap although a later event is delivered",
        (failing..missed).contains(&mark),
        format!("watermark {mark}, gap from {missed}, newest delivered {newest}"),
    );

    // The default keep time is 90 days.
    let now = SystemTime::now();
    proof
        .age(old, now - Duration::from_secs(90 * DAY + 1))
        .await?;
    proof
        .age(young, now - Duration::from_secs(90 * DAY - 1))
        .await?;
    for id in [failing, unread, missed] {
        proof.age(id, now - Duration::from_secs(120 * DAY)).await?;
    }
    let all = [old, young, failing, unread, missed, missed_later, newest];
    let deleted = proof.prune(now).await?;
    let kept = proof.kept(&all).await?;
    proof.check(
        "at 90 days the delivered event a second past the keep time and the event nobody reads go; the one a second inside stays",
        deleted == 2 && kept == [young, failing, missed, missed_later, newest],
        format!("deleted {deleted}, kept {kept:?}"),
    );
    let receipts = proof.deliveries(&[old, unread]).await?;
    proof.check(
        "a deleted event's deliveries go with it",
        receipts == 0,
        format!("{receipts} left"),
    );
    proof.check(
        "a failing delivery and a missed event keep their events 30 days past the keep time",
        kept.contains(&failing) && kept.contains(&missed),
        format!("events {failing} and {missed}"),
    );
    let again = proof.prune(now).await?;
    proof.check(
        "a second run deletes nothing",
        again == 0,
        format!("deleted {again}"),
    );
    let gap = proof.gap().await?;
    proof.check(
        "the missed count is the same after the deletions",
        gap == (2, Some(missed), false, 1, 1),
        format!("{gap:?}"),
    );

    // The tenant's own setting.
    let refused = proof.keep(6).await;
    proof.check(
        "a keep time under 7 days is refused",
        matches!(refused, Err(Error::Invalid)),
        format!("{refused:?}"),
    );
    proof.keep(30).await?;
    let deleted = proof.prune(now).await?;
    let kept = proof.kept(&all).await?;
    proof.check(
        "with the tenant's keep time at 30 days the 90-day-old delivered event goes",
        deleted == 1 && kept == [failing, missed, missed_later, newest],
        format!("deleted {deleted}, kept {kept:?}"),
    );

    // A replay closes the gap; the watermark moves on and the event can go.
    let replay = proof
        .events
        .replay(&proof.admin, SUBSCRIBER, ReplayFrom::Event(missed), 1000)
        .await?;
    // The two deleted events took their receipts: one is left, two are new.
    proof.settled(3, 1).await?;
    let gap = proof.gap().await?;
    let mark = proof.watermark().await?;
    proof.check(
        "after a replay nothing is missed and the watermark reaches the newest event",
        replay.created == 2 && gap == (0, None, false, 1, 1) && mark == newest,
        format!("replay {replay:?}, {gap:?}, watermark {mark}"),
    );
    let deleted = proof.prune(now).await?;
    let kept = proof.kept(&all).await?;
    proof.check(
        "the replayed old event goes; the failing one is still kept",
        deleted == 1 && kept == [failing, missed_later, newest],
        format!("deleted {deleted}, kept {kept:?}"),
    );
    Ok(())
}
