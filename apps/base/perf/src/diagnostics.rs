//! Bounded, non-blocking operational diagnostics. Only predefined messages are
//! accepted: error strings and request data must never reach the store.
use crate::recorder::Recorder;
use bilibi_database::{Error, Result};
use bilibi_queue::JobFailure;
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
    RequestDatabase,
    RequestRegistry,
    RequestMigration,
    RequestSeed,
    RequestTransaction,
    RequestInternal,
    LiveShellRefresh,
    LivePanelUpdate,
    LiveUpload,
    LiveEvent,
    Job(JobFailure),
    MenuEntry,
    ApiExchange,
    AgentRefusalAudit,
    WebhookRefusalAudit,
    InstanceLookup,
    OperatorCompanyLookup,
    ProxySettingInvalid,
    ProxySettingRead,
}
impl Diagnostic {
    fn facts(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::RequestDatabase => (
                "error",
                "Request",
                "request_database",
                "The request could not access the database.",
            ),
            Self::RequestRegistry => (
                "error",
                "Request",
                "request_registry",
                "The request encountered a module registry error.",
            ),
            Self::RequestMigration => (
                "error",
                "Request",
                "request_migration",
                "The request encountered a migration definition error.",
            ),
            Self::RequestSeed => (
                "error",
                "Request",
                "request_seed",
                "The request encountered a seed definition error.",
            ),
            Self::RequestTransaction => (
                "error",
                "Request",
                "request_transaction",
                "The request transaction could not recover.",
            ),
            Self::RequestInternal => (
                "error",
                "Request",
                "request_internal",
                "The request failed with an internal error.",
            ),
            Self::LiveShellRefresh => (
                "error",
                "Live page",
                "live_shell_refresh",
                "A live status panel could not refresh.",
            ),
            Self::LivePanelUpdate => (
                "error",
                "Live page",
                "live_panel_update",
                "A live page panel could not update.",
            ),
            Self::LiveUpload => (
                "error",
                "Live page",
                "live_upload",
                "A live upload could not be staged.",
            ),
            Self::LiveEvent => (
                "error",
                "Live page",
                "live_event",
                "A live page event could not be processed.",
            ),
            Self::Job(failure) => {
                let (kind, message) = match failure {
                    JobFailure::Retry => (
                        "job_retry",
                        "A background job failed and requested a retry.",
                    ),
                    JobFailure::Preparation => {
                        ("job_preparation", "A background job could not be prepared.")
                    }
                    JobFailure::Panic => ("job_panic", "A background job panicked."),
                    JobFailure::Timeout => {
                        ("job_timeout", "A background job exceeded its time limit.")
                    }
                    JobFailure::InvalidResult => (
                        "job_invalid_result",
                        "A background job returned an invalid outcome.",
                    ),
                    JobFailure::EndingHook => {
                        ("job_ending_hook", "A background job ending hook failed.")
                    }
                    JobFailure::Loop => ("job_loop", "A queue loop ended unexpectedly."),
                    JobFailure::Execution => ("job_execution", "Queue execution was unavailable."),
                    JobFailure::Claim => ("job_claim", "Queue could not claim a job."),
                    JobFailure::EndingWrite => (
                        "job_ending_write",
                        "Queue could not record an ending hook failure.",
                    ),
                    JobFailure::EndingRead => (
                        "job_ending_read",
                        "Queue could not read pending ending hooks.",
                    ),
                };
                ("error", "Job", kind, message)
            }
            Self::MenuEntry => (
                "warning",
                "Server",
                "menu_entry",
                "A menu entry was dropped.",
            ),
            Self::ApiExchange => (
                "warning",
                "Server",
                "api_exchange",
                "An API exchange could not be recorded.",
            ),
            Self::AgentRefusalAudit => (
                "warning",
                "Server",
                "agent_refusal_audit",
                "Agent edge refusals could not be audited.",
            ),
            Self::WebhookRefusalAudit => (
                "warning",
                "Server",
                "webhook_refusal_audit",
                "Webhook refusals could not be audited.",
            ),
            Self::InstanceLookup => (
                "warning",
                "Server",
                "instance_lookup",
                "The server instance could not be loaded.",
            ),
            Self::OperatorCompanyLookup => (
                "warning",
                "Server",
                "operator_company_lookup",
                "The platform operator company could not be loaded.",
            ),
            Self::ProxySettingInvalid => (
                "warning",
                "Server",
                "proxy_setting_invalid",
                "The trusted proxy setting is invalid.",
            ),
            Self::ProxySettingRead => (
                "warning",
                "Server",
                "proxy_setting_read",
                "The trusted proxy setting could not be read.",
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
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stopping| *stopping) => {}
            _ = async {
                while let Some(item) = receiver.recv().await {
                    let _ = timeout(Duration::from_secs(5), self.save_diagnostic(item)).await;
                }
            } => {}
        }
        receiver.close();
        let _ = timeout(Duration::from_secs(5), async {
            while let Some(item) = receiver.recv().await {
                let _ = self.save_diagnostic(item).await;
            }
        })
        .await;
    }
    async fn save_diagnostic(&self, item: Diagnostic) -> Result<()> {
        let (severity, source, kind, message) = item.facts();
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
        client.execute("INSERT INTO base_perf_diagnostics (severity, source, kind, message, count, first_seen, last_seen) VALUES ($1, $2, $3, $4, 1, now(), now()) ON CONFLICT (severity, source, kind) DO UPDATE SET message = excluded.message, count = base_perf_diagnostics.count + 1, last_seen = excluded.last_seen", &[&severity, &source, &kind, &message]).await?;
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
