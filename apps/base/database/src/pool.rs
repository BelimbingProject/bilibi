//! The process pool as a Bilibi type, so that one rule is checked where it
//! is broken: a task that holds a pooled connection must not ask the pool for
//! another. Once more callers than connections do that, each holds one, waits
//! out the pool for its second and is refused (#406, #539).
//!
//! `Db::get` notes which task holds each connection and looks before it
//! hands out the next. The note is keyed by the Tokio task, or by the thread
//! for the future a `block_on` drives (a walkthrough's `main`), so no caller
//! has to install a scope and a spawned task is covered like a request. The
//! note is dropped with the connection, wherever that happens.
use deadpool_postgres::{ClientWrapper, HookError, Object, Pool, PoolError, Status, Timeouts};
use std::{
    backtrace::{Backtrace, BacktraceStatus},
    collections::{HashMap, HashSet},
    future::Future,
    ops::{Deref, DerefMut},
    panic::Location,
    sync::{Arc, Mutex, MutexGuard},
};

type Site = &'static Location<'static>;

/// What `Db::get` does when the asking task already holds a pooled
/// connection. `Warn` (today's default) logs both call sites once and serves
/// the connection; `Refuse` logs and answers a pool error, so the page says
/// "Database unavailable" at once instead of after the pool's wait under load.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecondConnection {
    Warn,
    Refuse,
}
impl SecondConnection {
    pub const ENV: &'static str = "DATABASE_SECOND_CONNECTION";
    /// `DATABASE_SECOND_CONNECTION`: `warn` (default) or `refuse`. An
    /// environment value like the pool's limits, because the pool exists
    /// before a setting can be read. Any other value refuses the start.
    pub fn from_env() -> crate::Result<Self> {
        match std::env::var(Self::ENV) {
            Err(std::env::VarError::NotPresent) => Ok(Self::Warn),
            Ok(value) => match value.trim() {
                "" | "warn" => Ok(Self::Warn),
                "refuse" => Ok(Self::Refuse),
                _ => Self::invalid(),
            },
            Err(_) => Self::invalid(),
        }
    }
    fn invalid() -> crate::Result<Self> {
        eprintln!("{} must be warn or refuse.", Self::ENV);
        Err(crate::Error::Invalid)
    }
}

/// Who holds a connection: a task, or the thread of a `block_on` future.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Holder {
    Task(tokio::task::Id),
    Thread(std::thread::ThreadId),
}
impl Holder {
    fn current() -> Self {
        tokio::task::try_id().map_or_else(|| Self::Thread(std::thread::current().id()), Self::Task)
    }
}

#[derive(Default)]
struct Held {
    /// Where each holder took the connections it still has, oldest first.
    by: Mutex<HashMap<Holder, Vec<Site>>>,
    /// The pairs of call sites already logged, so a hot path logs one line.
    reported: Mutex<HashSet<(Site, Site)>>,
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
impl Held {
    fn ask(&self, holder: Holder, asked: Site, mode: SecondConnection) -> Result<(), PoolError> {
        let Some(held) = lock(&self.by)
            .get(&holder)
            .and_then(|sites| sites.first().copied())
        else {
            return Ok(());
        };
        let message = format!(
            "this task already holds a pooled connection (taken at {held}) and asked for another at {asked}. \
             Pass the one you hold (`&impl ProofClient`, or the `OperationClient` you were given), or release it first: \
             when more callers than connections do this, each waits out the pool and is refused (#406, #539). \
             A real exception takes `Db::get_detached(\"why\")`"
        );
        match mode {
            SecondConnection::Warn => {
                if lock(&self.reported).insert((held, asked)) {
                    eprintln!("warning: second pooled connection: {message}");
                    trace();
                }
                Ok(())
            }
            SecondConnection::Refuse => {
                eprintln!("error: second pooled connection refused: {message}");
                trace();
                Err(PoolError::PostCreateHook(HookError::message(message)))
            }
        }
    }
    fn mark(self: &Arc<Self>, holder: Holder, site: Site) -> Mark {
        lock(&self.by).entry(holder).or_default().push(site);
        Mark {
            held: self.clone(),
            holder,
            site,
        }
    }
}
/// The await chain between the two sites, when the developer asked for
/// backtraces (`RUST_BACKTRACE=1`); nothing otherwise.
fn trace() {
    let trace = Backtrace::capture();
    if trace.status() == BacktraceStatus::Captured {
        eprintln!("{trace}");
    }
}

/// One holder's note of one connection, erased when the connection goes.
struct Mark {
    held: Arc<Held>,
    holder: Holder,
    site: Site,
}
impl Drop for Mark {
    fn drop(&mut self) {
        let mut by = lock(&self.held.by);
        let Some(sites) = by.get_mut(&self.holder) else {
            return;
        };
        if let Some(at) = sites.iter().position(|site| *site == self.site) {
            sites.remove(at);
        }
        if sites.is_empty() {
            by.remove(&self.holder);
        }
    }
}

/// The process's connection pool. Cloning shares the pool and its notes.
#[derive(Clone)]
pub struct Db {
    pool: Pool,
    held: Arc<Held>,
    second: SecondConnection,
}
impl Db {
    pub(crate) fn new(pool: Pool, second: SecondConnection) -> Self {
        Self {
            pool,
            held: Arc::default(),
            second,
        }
    }
    /// A pooled connection, returned to the pool when dropped. A task that
    /// still holds one and asks again is logged with both call sites, or
    /// refused ([`SecondConnection`]): pass the connection you hold instead.
    #[track_caller]
    pub fn get(&self) -> impl Future<Output = Result<Connection, PoolError>> + Send + '_ {
        let site = Location::caller();
        async move {
            let holder = Holder::current();
            self.held.ask(holder, site, self.second)?;
            let object = self.pool.get().await?;
            Ok(Connection {
                object,
                mark: Some(self.held.mark(holder, site)),
            })
        }
    }
    /// A pooled connection the check leaves alone: it is neither refused when
    /// the task holds another, nor counted against the task's later asks.
    /// For the few places that must hold two at once, or that keep one for
    /// the life of a long task whose callees take their own. `why` is the
    /// reason, written for the reader of the call site; it still spends pool
    /// capacity, so say why the pool cannot be starved by it.
    pub async fn get_detached(&self, why: &'static str) -> Result<Connection, PoolError> {
        debug_assert!(!why.trim().is_empty(), "get_detached needs its reason");
        let object = self.pool.get().await?;
        Ok(Connection { object, mark: None })
    }
    pub fn status(&self) -> Status {
        self.pool.status()
    }
    pub fn timeouts(&self) -> Timeouts {
        self.pool.timeouts()
    }
    /// Changes how many connections the pool may hold (a walkthrough that
    /// proves behaviour on a full pool).
    pub fn resize(&self, size: usize) {
        self.pool.resize(size);
    }
}

/// A connection from [`Db`]. It reads as the PostgreSQL client it wraps.
pub struct Connection {
    object: Object,
    mark: Option<Mark>,
}
impl Connection {
    /// Takes the connection out of the pool for good: it is closed when
    /// dropped instead of being returned, the pool may open another in its
    /// place, and the task no longer counts as holding one. For a connection
    /// that carries session state (a session lock, a session setting) the
    /// next pooled caller must not inherit.
    pub fn take(this: Self) -> ClientWrapper {
        drop(this.mark);
        Object::take(this.object)
    }
}
impl Deref for Connection {
    type Target = ClientWrapper;
    fn deref(&self) -> &ClientWrapper {
        &self.object
    }
}
impl DerefMut for Connection {
    fn deref_mut(&mut self) -> &mut ClientWrapper {
        &mut self.object
    }
}
