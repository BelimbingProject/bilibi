//! Bounded, non-blocking diagnostics from static server log templates.
use crate::recorder::Recorder;
use bilibi_database::{Error, Result};
use bilibi_module_registry::{LogEvent, LogLevel};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::timeout,
};

pub const KEEP_DAYS: &str = "diagnostics.keep_days";

fn normalized(template: &str) -> String {
    let template = template
        .strip_prefix("error: ")
        .or_else(|| template.strip_prefix("warning: "))
        .unwrap_or(template);
    let mut text = String::new();
    let mut closing = None;
    for ch in template.chars() {
        if let Some(end) = closing {
            if ch == end {
                closing = None;
            }
            continue;
        }
        match ch {
            '{' => closing = Some('}'),
            '"' | '\x27' | '`' => closing = Some(ch),
            '0'..='9' => {}
            _ => text.push(ch),
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '.');
    if text.is_empty() {
        "A server operation reported a problem.".to_owned()
    } else {
        text.to_owned()
    }
}

pub(crate) struct Ingress {
    sender: mpsc::Sender<LogEvent>,
    receiver: Mutex<Option<mpsc::Receiver<LogEvent>>>,
}
impl Ingress {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = mpsc::channel(1000);
        Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
        }
    }
    pub(crate) fn take(&self) -> Result<mpsc::Receiver<LogEvent>> {
        self.receiver
            .lock()
            .map_err(|_| Error::Internal)?
            .take()
            .ok_or(Error::Invalid)
    }
}
impl Recorder {
    pub fn capture_server_logs(&self) {
        let sender = self.diagnostics.sender.clone();
        bilibi_module_registry::install_log_sink(move |item| {
            let _ = sender.try_send(item);
        });
    }

    pub(crate) async fn write_diagnostics(
        self: Arc<Self>,
        mut receiver: mpsc::Receiver<LogEvent>,
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
    async fn save_diagnostic(&self, item: LogEvent) -> Result<()> {
        let severity = match item.level {
            LogLevel::Warning => "warning",
            LogLevel::Error => "error",
        };
        let source = item.source;
        let message = normalized(item.template);
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
        client.execute("INSERT INTO base_perf_diagnostics (severity, source, kind, message, count, first_seen, last_seen) VALUES ($1, $2, $3, $4, 1, now(), now()) ON CONFLICT (severity, source, kind) DO UPDATE SET message = excluded.message, count = base_perf_diagnostics.count + 1, last_seen = excluded.last_seen", &[&severity, &source, &message, &message]).await?;
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
