//! Tokio supervision edge: one per server process. It claims due readings,
//! runs each reader in its own task, renews eligible leases in one update
//! per beat, and hands them back on shutdown. PostgreSQL leases give
//! one owner per key; they do not make a reader's effects exactly-once.
use crate::{
    reading::{Ended, ReaderService, Reading},
    registry::valid_code,
    store, Residents, SERVICE,
};
use bilibi_authz::{SystemAuthentication, SystemScope};
use bilibi_database::{Db, Error, MutationBoundary, Result};
use bilibi_module_registry::Services;
use bilibi_tenancy::Authentication;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{sync::watch, task::JoinSet, time::Instant};

/// Readings one poll claims: a process that starts beside others takes its
/// share over a few polls instead of everything in the first.
const CLAIM_BATCH: usize = 32;

#[derive(Clone, Debug)]
pub struct Config {
    /// Readings one process holds at once.
    pub capacity: usize,
    /// How often a process looks for due readings and expired leases.
    pub poll_interval: Duration,
    /// Lease length; the heartbeat is a third of it. A dead owner's readings
    /// become eligible for takeover when their leases expire.
    pub lease_seconds: i32,
    /// How long a cancelled reader may take to return before it is dropped.
    /// At most a heartbeat, so a reader that can no longer renew has stopped
    /// before its lease runs out.
    pub grace: Duration,
    /// Wait before a reading that ended with `Ended::Closed` starts again.
    pub restart_delay: Duration,
    /// Longest wait after a failure: `min(max_backoff, 2^failures seconds)`.
    pub max_backoff: Duration,
    /// A run this long clears the failure count.
    pub healthy_after: Duration,
    /// How often a running reader's tenant, principal and grant are proven again.
    pub reproof: Duration,
    /// The label operators see for this process; its host name and process
    /// ID when `None`.
    pub owner: Option<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            capacity: 512,
            poll_interval: Duration::from_secs(1),
            lease_seconds: 30,
            grace: Duration::from_secs(5),
            restart_delay: Duration::from_secs(1),
            max_backoff: Duration::from_secs(300),
            healthy_after: Duration::from_secs(60),
            reproof: Duration::from_secs(300),
            owner: None,
        }
    }
}
impl Config {
    fn validate(self) -> Result<Self> {
        let beat = Duration::from_secs(self.lease_seconds.max(0) as u64) / 3;
        if !(1..=10_000).contains(&self.capacity)
            || !(3..=3600).contains(&self.lease_seconds)
            || self.poll_interval < Duration::from_millis(10)
            || self.poll_interval > Duration::from_secs(60)
            || self.grace.is_zero()
            || self.grace > beat
            || self.restart_delay > self.max_backoff
            || self.max_backoff.is_zero()
            || self.max_backoff > Duration::from_secs(86_400)
            || self.reproof < Duration::from_millis(100)
            || self.owner.as_ref().is_some_and(|owner| {
                owner.is_empty()
                    || owner.len() > 200
                    || !owner.chars().all(|c| c.is_ascii_graphic())
            })
        {
            return Err(Error::Invalid);
        }
        Ok(self)
    }
}
/// `host:pid`, from the kernel's host name; nothing about a person.
fn process_label() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|name| {
            name.trim()
                .chars()
                .filter(char::is_ascii_graphic)
                .take(150)
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "server".into());
    format!("{host}:{}", std::process::id())
}

struct Bound {
    service: Arc<ReaderService>,
    module: &'static str,
    id: &'static str,
    principal: &'static str,
    capability: &'static str,
}
/// A reading this process holds, as the heartbeat sees it.
struct Held {
    token: String,
    /// When the lease was last known to run a full length.
    renewed_at: Instant,
    /// Tells the reading's task that its lease is gone.
    lost: watch::Sender<bool>,
}
/// Removes a reading from the heartbeat when its task ends, however it ends.
struct Registration {
    supervisor: Arc<Supervisor>,
    id: i64,
    token: String,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let mut held = self.supervisor.held();
        // A later claim of the same row holds another token: leave it.
        if held
            .get(&self.id)
            .is_some_and(|entry| entry.token == self.token)
        {
            held.remove(&self.id);
        }
    }
}
/// Why a run is over, as its supervising task sees it.
enum Finish {
    Closed,
    Failed(&'static str),
    Lost,
    /// The process is shutting down.
    Stopped,
}
#[derive(Clone, Copy, Debug)]
enum Loop {
    Claims,
    Heartbeats,
}
pub struct Supervisor {
    db: Db,
    authentication: Arc<Authentication>,
    boundary: Arc<MutationBoundary>,
    readers: BTreeMap<&'static str, Bound>,
    /// The reader IDs this process may claim.
    ids: Vec<String>,
    owner: String,
    config: Config,
    held: Mutex<BTreeMap<i64, Held>>,
}
impl Supervisor {
    /// Call after `Services::install`: owners bind their reader services
    /// during boot. A reader whose service, principal or capability is
    /// missing or belongs to another module is refused before any reading
    /// starts.
    pub fn from_services(services: &Services, config: Config) -> Result<Arc<Self>> {
        let residents = services.get::<Residents>(SERVICE)?;
        let vocabulary = bilibi_authz::vocabulary()?;
        let mut readers = BTreeMap::new();
        for reader in residents.readers().iter() {
            let declared = reader.definition;
            // The owner's own principal, deciding a key it declared: a module
            // can never read as another module's principal.
            if vocabulary.principal_module(declared.principal) != Some(reader.module)
                || !vocabulary.capabilities.contains_key(declared.capability)
                || !vocabulary
                    .system_principals
                    .get(declared.principal)
                    .is_some_and(|principal| principal.capabilities.contains(&declared.capability))
            {
                return Err(Error::Invalid);
            }
            readers.insert(
                reader.id,
                Bound {
                    service: services.get(declared.service)?,
                    module: reader.module,
                    id: reader.id,
                    principal: declared.principal,
                    capability: declared.capability,
                },
            );
        }
        let config = config.validate()?;
        Ok(Arc::new(Self {
            db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
            authentication: services.get("base/tenancy.authentication")?,
            boundary: services.get("base/database.mutations")?,
            ids: readers.keys().map(|id| (*id).to_owned()).collect(),
            readers,
            owner: config.owner.clone().unwrap_or_else(process_label),
            config,
            held: Mutex::new(BTreeMap::new()),
        }))
    }
    /// The label this process writes as the owner of the readings it holds.
    pub fn owner(&self) -> &str {
        &self.owner
    }
    fn held(&self) -> MutexGuard<'_, BTreeMap<i64, Held>> {
        // The map holds plain facts; a panic elsewhere leaves them usable.
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    fn lease(&self) -> Duration {
        Duration::from_secs(self.config.lease_seconds as u64)
    }
    /// Runs until `shutdown` turns true. Then every reader is cancelled and
    /// waited for (at most the grace period), then handback is attempted
    /// within another grace period. Success makes it due at the next poll;
    /// failure leaves lease expiry as the recovery path. The
    /// heartbeat keeps running until the last reader has stopped. A process
    /// that is killed instead leaves its leases to expire. A loop that ends
    /// while the process runs, by a panic, is started again after a poll.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let (quiet, quieted) = watch::channel(false);
        let mut loops = JoinSet::new();
        let mut running = BTreeMap::new();
        self.start(&mut loops, &mut running, Loop::Claims, &shutdown, false);
        self.start(&mut loops, &mut running, Loop::Heartbeats, &quieted, false);
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                change = shutdown.changed() => { if change.is_err() || *shutdown.borrow() { break; } }
                ended = loops.join_next_with_id() => {
                    let Some(ended) = ended else { break };
                    let id = match &ended { Ok((id, _)) => *id, Err(error) => error.id() };
                    let Some(which) = running.remove(&id) else { continue };
                    if *shutdown.borrow() { break; }
                    bilibi_module_registry::server_log!(Error,"resident {which:?} loop ended; restarting it");
                    let stop = match which { Loop::Claims => &shutdown, Loop::Heartbeats => &quieted };
                    self.start(&mut loops, &mut running, which, stop, true);
                }
            }
        }
        // The readings first: they stop under a live heartbeat.
        while running.values().any(|which| matches!(which, Loop::Claims)) {
            let Some(ended) = loops.join_next_with_id().await else {
                break;
            };
            running.remove(&match &ended {
                Ok((id, _)) => *id,
                Err(error) => error.id(),
            });
        }
        let _ = quiet.send(true);
        while loops.join_next().await.is_some() {}
        Ok(())
    }
    fn start(
        self: &Arc<Self>,
        loops: &mut JoinSet<()>,
        running: &mut BTreeMap<tokio::task::Id, Loop>,
        which: Loop,
        stop: &watch::Receiver<bool>,
        delayed: bool,
    ) {
        let supervisor = self.clone();
        let mut stop = stop.clone();
        let handle = loops.spawn(async move {
            if delayed {
                tokio::select! {
                    _ = tokio::time::sleep(supervisor.config.poll_interval) => {}
                    _ = stop.changed() => {}
                }
            }
            match which {
                Loop::Claims => supervisor.claims(stop).await,
                Loop::Heartbeats => supervisor.heartbeats(stop).await,
            }
        });
        running.insert(handle.id(), which);
    }
    /// Claims due readings and starts a task for each. On shutdown it waits
    /// for those tasks, each of which stops its reader and ends its lease.
    async fn claims(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        let mut readings = JoinSet::new();
        while !*stop.borrow() {
            while readings.try_join_next().is_some() {}
            let free = self.config.capacity.saturating_sub(readings.len());
            // Infrastructure errors expose no SQL and never end the loop;
            // readings that stay unclaimed are simply due at the next poll.
            let claimed_at = Instant::now();
            let claims = tokio::select! {
                _ = stop.wait_for(|stop| *stop) => break,
                result = tokio::time::timeout(self.lease() / 3, store::claim(
                &self.db,
                &self.owner,
                &self.ids,
                self.config.lease_seconds,
                free.min(CLAIM_BATCH) as i64,
                )) => result,
            };
            match claims {
                Ok(Ok(claims)) => {
                    for claim in claims {
                        readings.spawn(self.clone().hold(claim, claimed_at, stop.clone()));
                    }
                }
                _ => bilibi_module_registry::server_log!(
                    Error,
                    "resident claim infrastructure unavailable"
                ),
            }
            tokio::select! {
                _ = tokio::time::sleep(self.config.poll_interval) => {}
                changed = stop.changed() => { if changed.is_err() { break; } }
            }
        }
        while readings.join_next().await.is_some() {}
    }
    /// Renews eligible leases with one update per beat and tells the
    /// reading whose lease did not renew. A beat that cannot reach the
    /// database fails once without stopping anything; a reading stops before
    /// the lease it last proved can run out.
    async fn heartbeats(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        let lease = self.lease();
        let beat = lease / 3;
        let mut tick = tokio::time::interval(beat);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                changed = stop.changed() => { if changed.is_err() || *stop.borrow() { return; } }
            }
            let (ids, tokens, deadline) = {
                let held = self.held();
                let deadline = held
                    .values()
                    .filter(|entry| !*entry.lost.borrow())
                    .map(|entry| entry.renewed_at + lease - self.config.grace)
                    .min();
                let (ids, tokens): (Vec<i64>, Vec<String>) = held
                    .iter()
                    .filter(|(_, entry)| !*entry.lost.borrow())
                    .map(|(id, entry)| (*id, entry.token.clone()))
                    .unzip();
                (ids, tokens, deadline)
            };
            if ids.is_empty() {
                continue;
            }
            let asked = Instant::now();
            // Bounded: a hung statement must not hide that leases are running out.
            let answer = tokio::time::timeout_at(
                deadline.unwrap_or(asked + beat).min(asked + beat),
                store::heartbeat(&self.db, &ids, &tokens, self.config.lease_seconds),
            )
            .await;
            let answer = match answer {
                Ok(Ok(answer)) => Some(answer),
                _ => {
                    bilibi_module_registry::server_log!(
                        Error,
                        "resident heartbeat infrastructure unavailable"
                    );
                    None
                }
            };
            let mut held = self.held();
            for (id, token) in ids.iter().zip(&tokens) {
                let Some(entry) = held.get_mut(id).filter(|entry| &entry.token == token) else {
                    continue;
                };
                let renewed = answer
                    .as_ref()
                    .is_some_and(|beat| beat.renewed.contains(id));
                let unknown = answer.as_ref().is_none_or(|beat| beat.busy.contains(id));
                if renewed {
                    entry.renewed_at = asked;
                } else if !unknown || entry.renewed_at.elapsed() + beat + self.config.grace >= lease
                {
                    // Gone, or not provable before the lease can run out.
                    let _ = entry.lost.send(true);
                }
            }
        }
    }
    /// One reading from claim to ending: the reader in its own task, so its
    /// panic fails this reading and nothing else.
    async fn hold(
        self: Arc<Self>,
        claim: store::Claim,
        claimed_at: Instant,
        mut stop: watch::Receiver<bool>,
    ) {
        if claimed_at.elapsed() + self.config.grace >= self.lease() {
            return;
        }
        let (lost_sender, mut lost) = watch::channel(false);
        let (cancel, cancelled) = watch::channel(false);
        self.held().insert(
            claim.id,
            Held {
                token: claim.token.clone(),
                renewed_at: claimed_at,
                lost: lost_sender,
            },
        );
        let registration = Registration {
            supervisor: self.clone(),
            id: claim.id,
            token: claim.token.clone(),
        };
        let claim = Arc::new(claim);
        let started = Instant::now();
        let mut child = JoinSet::new();
        child.spawn(self.clone().read(claim.clone(), cancelled));
        let mut reproof =
            tokio::time::interval_at(started + self.config.reproof, self.config.reproof);
        reproof.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let finish = loop {
            if *stop.borrow() {
                break Finish::Stopped;
            }
            let cancel_at = self
                .held()
                .get(&claim.id)
                .map(|entry| entry.renewed_at + self.lease() - self.config.grace)
                .unwrap_or_else(Instant::now);
            if *lost.borrow() || Instant::now() >= cancel_at {
                break Finish::Lost;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(cancel_at) => continue,
                result = child.join_next() => break match result {
                    Some(Ok(finish)) => finish,
                    _ => Finish::Failed("reader_panicked"),
                },
                _ = lost.changed() => break Finish::Lost,
                changed = stop.changed() => { if changed.is_err() || *stop.borrow() { break Finish::Stopped; } }
                _ = reproof.tick() => {
                    // Only a refusal stops a running reader; a database error
                    // or a slow answer leaves it to the next proof and to
                    // `Reading::begin`. Bounded, so shutdown and a lost lease
                    // are never kept waiting behind it.
                    let proof = tokio::select! {
                        _ = lost.changed() => break Finish::Lost,
                        _ = stop.wait_for(|stop| *stop) => break Finish::Stopped,
                        proof = tokio::time::timeout_at(
                            cancel_at.min(Instant::now() + self.config.grace),
                            self.prove(&claim),
                        ) => proof,
                    };
                    if let Ok(Err(Error::Unauthorized | Error::Forbidden)) = proof {
                        break Finish::Failed("actor_refused");
                    }
                }
            }
        };
        if !child.is_empty() {
            // Still reading: ask it to stop, wait the grace period, then drop it.
            let _ = cancel.send(true);
            let expires_at = self
                .held()
                .get(&claim.id)
                .map(|entry| entry.renewed_at + self.lease())
                .unwrap_or_else(Instant::now);
            if tokio::time::timeout_at(
                expires_at.min(Instant::now() + self.config.grace),
                child.join_next(),
            )
            .await
            .is_err()
            {
                child.abort_all();
            }
            while child.join_next().await.is_some() {}
        }
        drop(registration);
        let kept = if started.elapsed() >= self.config.healthy_after {
            0
        } else {
            claim.failures
        };
        let (delay, failures, code) = match finish {
            Finish::Lost => {
                bilibi_module_registry::server_log!(
                    Error,
                    "resident reader {} reading {} lost its lease",
                    claim.reader,
                    claim.id
                );
                (Duration::ZERO, claim.failures, None)
            }
            Finish::Stopped => (Duration::ZERO, claim.failures, None),
            Finish::Closed => (self.config.restart_delay, kept, None),
            Finish::Failed(code) => {
                let failures = kept.saturating_add(1);
                bilibi_module_registry::server_log!(
                    Error,
                    "resident reader {} reading {} failed: {code}",
                    claim.reader,
                    claim.id
                );
                (self.backoff(failures), failures, Some(code))
            }
        };
        // `Ok(false)`: released or taken over while the reader was ending.
        if !matches!(
            tokio::time::timeout(
                self.config.grace,
                store::end(&self.db, claim.id, &claim.token, delay, failures, code),
            )
            .await,
            Ok(Ok(_))
        ) {
            // The lease expires on its own and the next poll takes it over.
            bilibi_module_registry::server_log!(
                Error,
                "resident reader {} reading {} could not hand its lease back",
                claim.reader,
                claim.id
            );
        }
    }
    fn backoff(&self, failures: i32) -> Duration {
        Duration::from_secs(1u64 << failures.clamp(0, 20) as u32).min(self.config.max_backoff)
    }
    /// The live tenant, the principal's declaration and its grant.
    async fn prove(&self, claim: &store::Claim) -> Result<()> {
        let bound = self
            .readers
            .get(claim.reader.as_str())
            .ok_or(Error::Invalid)?;
        let client = self.db.get().await?;
        let system = SystemScope::rehydrate(&**client, claim.tenant, Some(bound.principal)).await?;
        bilibi_authz::authorize_system(&**client, &system, bound.capability).await
    }
    /// Proves the principal, seals its scope and runs the owner's reader.
    async fn read(
        self: Arc<Self>,
        claim: Arc<store::Claim>,
        mut cancel: watch::Receiver<bool>,
    ) -> Finish {
        let Some(bound) = self
            .readers
            .get(claim.reader.as_str())
            .filter(|bound| bound.module == claim.module)
        else {
            return Finish::Failed("reader_refused");
        };
        let cancelled = cancel.clone();
        let prepared = tokio::select! {
            biased;
            _ = cancel.wait_for(|cancel| *cancel) => return Finish::Stopped,
            result = tokio::time::timeout(self.lease() / 3, self.prepare(&claim, bound, cancelled)) => result,
        };
        let reading = match prepared {
            Ok(Ok(reading)) => reading,
            Ok(Err(Error::Unauthorized | Error::Forbidden)) => {
                return Finish::Failed("actor_refused");
            }
            _ => return Finish::Failed("prepare_unavailable"),
        };
        match bound.service.0.read(&reading).await {
            Ended::Closed => Finish::Closed,
            Ended::Failed(code) if valid_code(code) => Finish::Failed(code),
            Ended::Failed(_) => Finish::Failed("invalid_reader_result"),
        }
    }
    async fn prepare(
        &self,
        claim: &store::Claim,
        bound: &Bound,
        cancel: watch::Receiver<bool>,
    ) -> Result<Reading> {
        let client = self.db.get().await?;
        // Declaration and durable grant are read live; a tenant that is not
        // active, or a principal without its grant, is refused.
        let system = SystemScope::rehydrate(&**client, claim.tenant, Some(bound.principal)).await?;
        bilibi_authz::authorize_system(&**client, &system, bound.capability).await?;
        let scope = Box::new(self.authentication.for_system(&**client, &system).await?);
        Ok(Reading {
            id: claim.id,
            key: claim.key.clone(),
            reader: bound.id,
            system,
            scope,
            token: claim.token.clone(),
            capability: bound.capability,
            boundary: self.boundary.clone(),
            cancel,
        })
    }
}
