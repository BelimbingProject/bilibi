//! Observing a piece of work. Adapters (Axum for requests and live events, the
//! Queue for jobs) wrap their work with `Recorder::observe`; the work is
//! polled exactly as it would be without it, and the observation is handed to
//! `submit` afterwards, so nothing here can fail, wait or change the result.
use crate::{
    recorder::Recorder,
    sample::{valid_identity, Kind, Outcome, Process, Sample, SizeClass},
};
use bilibi_database::timing::{self, Counters};
use std::{future::Future, sync::Arc, time::Instant, time::SystemTime};

/// What the adapter concluded about the finished work.
#[derive(Clone, Copy, Debug)]
pub struct Finish {
    pub outcome: Outcome,
    /// Response body bytes where known; only the size class is stored.
    pub response_bytes: Option<u64>,
}
impl Finish {
    pub fn outcome(outcome: Outcome) -> Self {
        Self {
            outcome,
            response_bytes: None,
        }
    }
}

/// Records `cancelled` (or `error` while panicking) when the observed future is
/// dropped before it finishes: a client that went away, a lease timeout, a
/// shutdown.
struct Pending<'a> {
    recorder: &'a Recorder,
    kind: Kind,
    identity: &'a str,
    started: Instant,
    counters: Arc<Counters>,
    finished: bool,
}
impl Pending<'_> {
    fn record(&mut self, finish: Finish) {
        self.finished = true;
        self.recorder.submit(Sample {
            kind: self.kind,
            identity: self.identity.to_owned(),
            outcome: finish.outcome,
            duration_ms: millis(self.started),
            db_duration_ms: i64::try_from(self.counters.millis()).unwrap_or(i64::MAX),
            db_count: i32::try_from(self.counters.count()).unwrap_or(i32::MAX),
            response_size_class: finish.response_bytes.map(SizeClass::of),
            process: Process::default(),
            observed_at: SystemTime::now(),
        });
    }
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let outcome = if std::thread::panicking() {
                Outcome::Error
            } else {
                Outcome::Cancelled
            };
            self.record(Finish::outcome(outcome));
        }
    }
}
fn millis(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

impl Recorder {
    /// Runs `work`, attributing its wall time and its database time (see
    /// `bilibi_database::timing`) to `identity`. A missing or non-conforming
    /// identity (an unmatched route, a name with a query string) runs the work
    /// unobserved: nothing unbounded or identifying is ever recorded.
    ///
    /// `work` is boxed before the async block: an `async` wrapper that owns a
    /// future keeps it twice (the capture and the awaited value), and on the
    /// live path those copies overflowed an unoptimised build's worker stack
    /// (#965).
    pub fn observe<'a, T: 'a>(
        &'a self,
        kind: Kind,
        identity: Option<&'a str>,
        work: impl Future<Output = T> + 'a,
        finish: impl FnOnce(&T) -> Finish + 'a,
    ) -> impl Future<Output = T> + 'a {
        let work = Box::pin(work);
        async move {
            let Some(identity) =
                identity.filter(|identity| kind != Kind::Runtime && valid_identity(kind, identity))
            else {
                return work.await;
            };
            let counters = Arc::new(Counters::default());
            let mut pending = Pending {
                recorder: self,
                kind,
                identity,
                started: Instant::now(),
                counters: counters.clone(),
                finished: false,
            };
            let result = timing::scope(counters, work).await;
            pending.record(finish(&result));
            result
        }
    }
}
