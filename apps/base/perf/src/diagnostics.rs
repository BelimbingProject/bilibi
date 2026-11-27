//! Bounded, non-blocking operational diagnostics. Only predefined messages are
//! accepted: error strings and request data must never reach the store.
use crate::recorder::Recorder;
use bilibi_database::{Error, Result};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::timeout,
};

pub const KEEP_DAYS: &str = "diagnostics.keep_days";

#[derive(Clone, Copy)]
pub enum Diagnostic {
    RequestError,
    LiveError,
    JobError,
    InfrastructureWarning,
}
impl Diagnostic {
    fn facts(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::RequestError => (
                "error",
                "Request",
                "A server request failed. Check the server log for details.",
            ),
            Self::LiveError => (
                "error",
                "Live page",
                "A live page operation failed. Check the server log for details.",
            ),
            Self::JobError => (
                "error",
                "Job",
                "A background job failed. Check the server log for details.",
            ),
            Self::InfrastructureWarning => (
                "warning",
                "Server",
                "The server reported a warning. Check the server log for details.",
            ),
        }
    }
}

pub(crate) struct Ingress {
    sender: mpsc::Sender<Diagnostic>,
    receiver: Mutex<Option<mpsc::Receiver<Diagnostic>>>,
}
impl Ingress {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = mpsc::channel(1000);
        Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
        }
    }
    pub(crate) fn record(&self, item: Diagnostic) {
        let _ = self.sender.try_send(item);
    }
    pub(crate) fn take(&self) -> Result<mpsc::Receiver<Diagnostic>> {
        self.receiver
            .lock()
            .map_err(|_| Error::Internal)?
            .take()
            .ok_or(Error::Invalid)
    }
}
impl Recorder {
    /// Record a predefined diagnostic without blocking the work being observed.
    pub fn diagnostic(&self, item: Diagnostic) {
        self.diagnostics.record(item);
    }

    pub(crate) async fn write_diagnostics(
        self: Arc<Self>,
        mut receiver: mpsc::Receiver<Diagnostic>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        loop {
            let item = tokio::select! {
                changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break; } else { continue; } },
                item = receiver.recv() => match item { Some(item) => item, None => break },
            };
            let _ = timeout(Duration::from_secs(5), self.save_diagnostic(item)).await;
        }
        while let Ok(item) = receiver.try_recv() {
            let _ = timeout(Duration::from_secs(5), self.save_diagnostic(item)).await;
        }
    }
    async fn save_diagnostic(&self, item: Diagnostic) -> Result<()> {
        let (severity, source, message) = item.facts();
        let client = self.db.get().await?;
        let keep = self
            .settings
            .get_global(&**client, KEEP_DAYS)
            .await?
            .as_i64()
            .filter(|n| (1..=3650).contains(n))
            .ok_or(Error::Invalid)?;
        client
            .execute(
                "DELETE FROM base_perf_diagnostics WHERE last_seen < now() - $1::bigint * interval '1 day'",
                &[&keep],
            )
            .await?;
        client.execute("INSERT INTO base_perf_diagnostics (severity, source, message, count, first_seen, last_seen) VALUES ($1, $2, $3, 1, now(), now()) ON CONFLICT (severity, source) DO UPDATE SET message = excluded.message, count = base_perf_diagnostics.count + 1, last_seen = excluded.last_seen", &[&severity, &source, &message]).await?;
        Ok(())
    }
}

/// A safe, installation-wide fact supplied by a diagnostics provider.
#[derive(Clone, Debug)]
pub struct Row {
    pub severity: String,
    pub source: String,
    pub message: String,
    pub count: i64,
    pub first_seen: std::time::SystemTime,
    pub last_seen: std::time::SystemTime,
}
pub type ReadFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Row>>> + Send + 'a>>;
/// Providers return bounded, non-sensitive facts; the caller checks operator access.
pub struct Provider {
    pub read: for<'a> fn(&'a bilibi_database::Db, &'a bilibi_settings::Settings) -> ReadFuture<'a>,
}
pub const PROVIDERS: &str = "base/perf.diagnostic_providers";
inventory::submit! { bilibi_module_registry::Consumer::new::<Provider>(PROVIDERS, "base/perf", |_| Ok(())) }
inventory::submit! { bilibi_module_registry::Contribution::new(PROVIDERS, "base/perf.errors", bilibi_module_registry::Provenance { module: "base/perf", source: file!(), line: line!() }, &Provider { read: read_errors }) }
fn read_errors<'a>(
    db: &'a bilibi_database::Db,
    settings: &'a bilibi_settings::Settings,
) -> ReadFuture<'a> {
    Box::pin(async move {
        let client = db.get().await?;
        let keep = settings
            .get_global(&**client, KEEP_DAYS)
            .await?
            .as_i64()
            .filter(|n| (1..=3650).contains(n))
            .ok_or(Error::Invalid)?;
        let rows = client.query("SELECT severity, source, message, count, first_seen, last_seen FROM base_perf_diagnostics WHERE last_seen >= now() - $1::bigint * interval '1 day' ORDER BY last_seen DESC", &[&keep]).await?;
        Ok(rows
            .iter()
            .map(|r| Row {
                severity: r.get(0),
                source: r.get(1),
                message: r.get(2),
                count: r.get(3),
                first_seen: r.get(4),
                last_seen: r.get(5),
            })
            .collect())
    })
}
