//! Read model for the operator page, the dashboard strip and System info. All of
//! it is bounded SQL over redacted rows plus this process's recorder counters;
//! an unreachable store is reported as unavailable, never as a quiet zero.
use crate::{
    recorder::{Recorder, Stats, SLOW_THRESHOLD_MS},
    sample::{Kind, Outcome, Process, Sample, SizeClass},
};
use bilibi_database::{Connection, Error, Result};
use std::{
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime},
};
use tokio::time::{timeout_at, Instant};
use tokio_postgres::{Client, Row};

pub const PAGE_SIZES: [i64; 4] = [25, 50, 100, 300];
const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct Filter {
    pub page: i64,
    pub page_size: i64,
    pub kind: Option<Kind>,
    /// Exact stable identity: a route template or worker id.
    pub identity: Option<String>,
    pub outcome: Option<Outcome>,
    /// Inclusive start, exclusive end.
    pub from: Option<SystemTime>,
    pub to: Option<SystemTime>,
}
impl Default for Filter {
    fn default() -> Self {
        Self {
            page: 1,
            page_size: 25,
            kind: None,
            identity: None,
            outcome: None,
            from: None,
            to: None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Recorded {
    pub id: i64,
    pub sample: Sample,
}
#[derive(Clone, Debug)]
pub struct Page {
    pub entries: Vec<Recorded>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Health {
    Available,
    /// Samples have been lost, or the queue is full right now.
    Degraded,
    /// The writer is not running in this process.
    Unavailable,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Store {
    Available,
    Unavailable,
}
#[derive(Clone, Debug)]
pub struct Diagnostics {
    pub recorder: Health,
    pub store: Store,
    /// Recording as last read from settings; `false` when the store is unreachable.
    pub recording: bool,
    pub samples: Option<i64>,
    pub last_observed_at: Option<SystemTime>,
    /// This process's counters. Other app processes keep their own.
    pub stats: Stats,
}

#[derive(Clone, Debug)]
pub struct Regression {
    pub kind: Kind,
    pub identity: String,
    pub baseline_samples: i64,
    pub current_samples: i64,
    pub baseline_average_ms: f64,
    pub current_average_ms: f64,
    pub current_p95_ms: f64,
    pub delta_percent: f64,
}
#[derive(Clone, Debug)]
pub struct Slowest {
    pub kind: Kind,
    pub identity: String,
    pub samples: i64,
    pub average_ms: f64,
    pub p95_ms: f64,
}
#[derive(Clone, Copy, Debug)]
pub struct Windows {
    pub baseline_from: SystemTime,
    pub baseline_to: SystemTime,
    pub current_from: SystemTime,
    pub current_to: SystemTime,
    /// A route or worker needs this many samples in both windows (5 to 100,000).
    pub min_samples: i64,
    /// 1 to 100.
    pub limit: i64,
}

fn recorded(row: &Row) -> Result<Recorded> {
    let kind: &str = row.get("kind");
    let outcome: &str = row.get("outcome");
    let size: Option<&str> = row.get("response_size_class");
    Ok(Recorded {
        id: row.get("id"),
        sample: Sample {
            kind: Kind::parse(kind).ok_or(Error::Internal)?,
            identity: row.get("identity"),
            outcome: Outcome::parse(outcome).ok_or(Error::Internal)?,
            duration_ms: row.get("duration_ms"),
            db_duration_ms: row.get("db_duration_ms"),
            db_count: row.get("db_count"),
            response_size_class: size.and_then(SizeClass::parse),
            process: Process {
                memory_bytes: row.get("memory_bytes"),
                run_queue: row.get("run_queue"),
                alive_tasks: row.get("alive_tasks"),
                threads: row.get("threads"),
                open_fds: row.get("open_fds"),
                cpu_ms: row.get("cpu_ms"),
                pool_in_use: row.get("pool_in_use"),
                pool_waiting: row.get("pool_waiting"),
            },
            observed_at: row.get("observed_at"),
        },
    })
}

const FILTERED: &str = "($1::text IS NULL OR kind=$1) AND ($2::text IS NULL OR identity=$2) AND ($3::text IS NULL OR outcome=$3) AND ($4::timestamptz IS NULL OR observed_at>=$4) AND ($5::timestamptz IS NULL OR observed_at<$5)";

impl Recorder {
    async fn bounded_read<T>(
        &self,
        connection: &mut Option<Connection>,
        query: impl for<'a> FnOnce(&'a Client) -> Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>,
    ) -> Result<T> {
        let deadline = Instant::now() + READ_TIMEOUT;
        let client = match connection {
            Some(client) => client,
            None => {
                let client = timeout_at(deadline, self.db.get())
                    .await
                    .map_err(|_| Error::Internal)??;
                connection.insert(client)
            }
        };
        let completed = timeout_at(deadline, async {
            let tx = client.build_transaction().read_only(true).start().await?;
            let result = async {
                let remaining = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis();
                let statement_ms = remaining
                    .checked_sub(500)
                    .filter(|ms| *ms > 0)
                    .ok_or(Error::Internal)?;
                let transaction_ms = remaining - 250;
                tx.batch_execute(&format!(
                    "SET LOCAL statement_timeout = '{statement_ms}ms'; \
                     SET LOCAL transaction_timeout = '{transaction_ms}ms'"
                ))
                .await?;
                query(tx.client()).await
            }
            .await;
            if result.is_ok() {
                tx.commit().await?;
            } else {
                tx.rollback().await?;
            }
            Ok::<_, Error>(result)
        })
        .await;
        let (value, reusable) = match completed {
            Ok(Ok(value)) => (value, !client.is_closed()),
            Ok(Err(error)) => (Err(error), false),
            Err(_) => (Err(Error::Internal), false),
        };
        if !reusable {
            drop(connection.take().map(Connection::take));
        }
        value
    }

    /// One bounded page, newest first. A page size outside `PAGE_SIZES`, a page
    /// below one or a malformed identity filter is refused, not clamped.
    pub async fn list_samples(&self, filter: &Filter) -> Result<Page> {
        self.list_samples_on(&mut None, filter).await
    }

    pub(crate) async fn list_samples_on(
        &self,
        connection: &mut Option<Connection>,
        filter: &Filter,
    ) -> Result<Page> {
        if filter.page < 1
            || filter.page > 1_000_000
            || !PAGE_SIZES.contains(&filter.page_size)
            || filter
                .identity
                .as_ref()
                .is_some_and(|identity| identity.is_empty() || identity.len() > 255)
        {
            return Err(Error::Invalid);
        }
        let filter = filter.clone();
        self.bounded_read(connection, move |client| Box::pin(async move {
            let kind = filter.kind.map(Kind::as_str);
            let outcome = filter.outcome.map(Outcome::as_str);
            let identity = filter.identity.as_deref();
            let total: i64 = client
                .query_one(
                    &format!("SELECT count(*) FROM base_perf_samples WHERE {FILTERED}"),
                    &[&kind, &identity, &outcome, &filter.from, &filter.to],
                )
                .await?
                .get(0);
            let offset = (filter.page - 1) * filter.page_size;
            let rows = client
                .query(
                    &format!("SELECT * FROM base_perf_samples WHERE {FILTERED} ORDER BY observed_at DESC, id DESC LIMIT $6 OFFSET $7"),
                    &[&kind, &identity, &outcome, &filter.from, &filter.to, &filter.page_size, &offset],
                )
                .await?;
            Ok::<_, Error>(Page {
                entries: rows.iter().map(recorded).collect::<Result<_>>()?,
                total,
                page: filter.page,
                page_size: filter.page_size,
            })
        })).await
    }

    /// Store and recorder health. Silence is not health: an empty history with
    /// a running writer reads as available with zero samples, and a missing
    /// writer or store says so.
    pub async fn diagnostics(&self) -> Diagnostics {
        self.diagnostics_on(&mut None).await
    }

    pub(crate) async fn diagnostics_on(&self, connection: &mut Option<Connection>) -> Diagnostics {
        let stats = self.stats();
        let recorder = if !stats.running {
            Health::Unavailable
        } else if stats.dropped > 0 || stats.pending >= stats.max_pending {
            Health::Degraded
        } else {
            Health::Available
        };
        let read = self.bounded_read(connection, |client| {
            Box::pin(async move {
                let row = client
                    .query_one(
                        "SELECT count(*)::bigint, max(observed_at) FROM base_perf_samples",
                        &[],
                    )
                    .await?;
                Ok::<_, Error>((row.get::<_, i64>(0), row.get::<_, Option<SystemTime>>(1)))
            })
        });
        let (store, samples, last_observed_at) = match read.await {
            Ok((samples, last)) => (Store::Available, Some(samples), last),
            Err(_) => (Store::Unavailable, None, None),
        };
        Diagnostics {
            recorder,
            store,
            recording: store == Store::Available && self.recording(),
            samples,
            last_observed_at,
            stats,
        }
    }
    /// The one-line status System info shows.
    pub async fn health_status(&self) -> String {
        let diagnostics = self.diagnostics().await;
        match (diagnostics.recorder, diagnostics.store) {
            (Health::Unavailable, _) | (_, Store::Unavailable) => "Unavailable".into(),
            (recorder, Store::Available) => format!(
                "{} ({} pending, {} dropped)",
                if recorder == Health::Degraded {
                    "Degraded"
                } else {
                    "Available"
                },
                diagnostics.stats.pending,
                diagnostics.stats.dropped
            ),
        }
    }

    /// Routes, events and jobs whose p95 in the current window is at or above
    /// `perf.slow_threshold_ms`, ranked by how much their mean grew against the
    /// baseline window. The windows must be ordered and must not overlap.
    pub async fn regressions(&self, windows: &Windows) -> Result<Vec<Regression>> {
        self.regressions_on(&mut None, windows).await
    }

    pub(crate) async fn regressions_on(
        &self,
        connection: &mut Option<Connection>,
        windows: &Windows,
    ) -> Result<Vec<Regression>> {
        if windows.baseline_from >= windows.baseline_to
            || windows.current_from >= windows.current_to
            || windows.baseline_to > windows.current_from
            || !(5..=100_000).contains(&windows.min_samples)
            || !(1..=100).contains(&windows.limit)
        {
            return Err(Error::Invalid);
        }
        let windows = *windows;
        let settings = self.settings.clone();
        self.bounded_read(connection, move |client| Box::pin(async move {
            let threshold = settings
                .get_global(client, SLOW_THRESHOLD_MS)
                .await?
                .as_f64()
                .ok_or(Error::Invalid)?;
            let rows = client
                .query(
                    "WITH window_stats AS ( \
                        SELECT kind, identity, \
                               CASE WHEN observed_at >= $3 THEN 'current' ELSE 'baseline' END AS span, \
                               count(*)::bigint AS samples, avg(duration_ms)::float8 AS average, \
                               (percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms))::float8 AS p95 \
                        FROM base_perf_samples \
                        WHERE kind <> 'runtime' AND ((observed_at >= $1 AND observed_at < $2) OR (observed_at >= $3 AND observed_at < $4)) \
                        GROUP BY kind, identity, span HAVING count(*) >= $5) \
                     SELECT c.kind, c.identity, b.samples AS baseline_samples, c.samples AS current_samples, \
                            b.average AS baseline_average, c.average AS current_average, c.p95 AS current_p95, \
                            CASE WHEN b.average = 0 THEN 0.0 ELSE ((c.average / b.average) - 1) * 100 END AS delta \
                     FROM window_stats c JOIN window_stats b ON b.kind = c.kind AND b.identity = c.identity AND b.span = 'baseline' \
                     WHERE c.span = 'current' AND c.p95 >= $6 \
                     ORDER BY delta DESC, c.kind, c.identity LIMIT $7",
                    &[&windows.baseline_from, &windows.baseline_to, &windows.current_from, &windows.current_to, &windows.min_samples, &threshold, &windows.limit],
                )
                .await?;
            rows.iter()
                .map(|row| {
                    let kind: &str = row.get("kind");
                    Ok(Regression {
                        kind: Kind::parse(kind).ok_or(Error::Internal)?,
                        identity: row.get("identity"),
                        baseline_samples: row.get("baseline_samples"),
                        current_samples: row.get("current_samples"),
                        baseline_average_ms: row.get("baseline_average"),
                        current_average_ms: row.get("current_average"),
                        current_p95_ms: row.get("current_p95"),
                        delta_percent: (row.get::<_, f64>("delta") * 10.0).round() / 10.0,
                    })
                })
                .collect::<Result<Vec<_>>>()
        })).await
    }

    /// Routes, events and jobs with the highest p95 duration among the samples
    /// observed in `[from, to)`. `limit` is 1 to 100.
    pub async fn slowest(
        &self,
        from: SystemTime,
        to: SystemTime,
        limit: i64,
    ) -> Result<Vec<Slowest>> {
        self.slowest_on(&mut None, from, to, limit).await
    }

    pub(crate) async fn slowest_on(
        &self,
        connection: &mut Option<Connection>,
        from: SystemTime,
        to: SystemTime,
        limit: i64,
    ) -> Result<Vec<Slowest>> {
        if from >= to || !(1..=100).contains(&limit) {
            return Err(Error::Invalid);
        }
        self.bounded_read(connection, move |client| Box::pin(async move {
            let rows = client
                .query(
                    "SELECT kind, identity, count(*)::bigint AS samples, avg(duration_ms)::float8 AS average, \
                            (percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms))::float8 AS p95 \
                     FROM base_perf_samples \
                     WHERE kind <> 'runtime' AND observed_at >= $1 AND observed_at < $2 \
                     GROUP BY kind, identity \
                     ORDER BY p95 DESC, kind, identity LIMIT $3",
                    &[&from, &to, &limit],
                )
                .await?;
            rows.iter()
                .map(|row| {
                    let kind: &str = row.get("kind");
                    Ok(Slowest {
                        kind: Kind::parse(kind).ok_or(Error::Internal)?,
                        identity: row.get("identity"),
                        samples: row.get("samples"),
                        average_ms: row.get("average"),
                        p95_ms: row.get("p95"),
                    })
                })
                .collect::<Result<Vec<_>>>()
        })).await
    }
}
