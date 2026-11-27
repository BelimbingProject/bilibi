//! The agent edge's gate for traffic that proves no credential (`api.rs`).
//!
//! Each sender has two budgets of `agent_access.oauth_requests_per_minute`
//! a minute. A request without a bearer token spends one of the first before
//! it is dispatched, and is answered `429` when none is left. A request whose
//! bearer token is refused (`401`) spends one of the second afterwards; once
//! that is gone, every request with a token from the sender is answered `429`
//! before dispatch, so a flood of made-up tokens cannot keep reaching the
//! token lookup. The budgets are apart so that sign-in traffic (discovery,
//! device-code polls) from an office address never blocks the valid tokens
//! of the agents behind it. The setting is read once per minute, when that
//! minute's window opens.
//!
//! Refusals that prove nothing (`401`, `404`, `405` and `429` with no sealed
//! scope) write no exchange row each: a caller with no credential could
//! otherwise add rows without limit. They are counted by endpoint group and
//! status, and each minute's counts become one audit action
//! `agent_edge.refused` per group and status with a `count`, as the webhook
//! boundary does for its absorbed refusals (`webhooks.rs`).
use crate::App;
use axum::http::StatusCode;
use bilibi_agent::OAUTH_REQUESTS_PER_MINUTE;
use bilibi_base::audit::record_action;
use bilibi_database::{Error, Result};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const EVENT: &str = "agent_edge.refused";
/// Sender budgets counted in one minute; a new sender without a token beyond
/// this is refused until the minute ends.
const SENDER_KEYS: usize = 65_536;

/// Refusals by endpoint group and status.
type Counts = BTreeMap<(&'static str, u16), u64>;

#[derive(Default)]
pub(crate) struct Gate {
    window: Mutex<Window>,
}
#[derive(Default)]
struct Window {
    minute: u64,
    limit: u32,
    /// Spent budget by whether the requests carried a token, and sender.
    spent: HashMap<(bool, String), u32>,
    refused: Counts,
}
fn minute() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 60)
}
impl Gate {
    fn state(&self) -> MutexGuard<'_, Window> {
        self.window
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    /// Opens this minute's window when the last one has passed: reads the
    /// limit once and hands the closed window's counts to the audit log.
    pub(crate) async fn open(&self, app: &App) -> Result<()> {
        let now = minute();
        if self.state().minute == now {
            return Ok(());
        }
        let limit = {
            let client = app.db.get().await?;
            app.api
                .settings
                .get_global(&**client, OAUTH_REQUESTS_PER_MINUTE)
                .await?
                .as_u64()
                .and_then(|limit| u32::try_from(limit).ok())
                .ok_or(Error::Invalid)?
        };
        let closed = {
            let mut window = self.state();
            if window.minute == now {
                return Ok(());
            }
            let closed = std::mem::take(&mut window.refused);
            *window = Window {
                minute: now,
                limit,
                ..Window::default()
            };
            closed
        };
        flush(app, closed);
        // A quiet edge still reports this minute's refusals when it ends.
        let app = app.clone();
        tokio::spawn(async move {
            let into = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs() % 60);
            tokio::time::sleep(Duration::from_secs(60 - into)).await;
            let closed = {
                let mut window = app.api.gate.state();
                if window.minute != now {
                    return;
                }
                std::mem::take(&mut window.refused)
            };
            flush(&app, closed);
        });
        Ok(())
    }
    /// Whether `sender` may be dispatched now. A request without a token
    /// spends one of its budget here; one with a token spends nothing unless
    /// it is refused ([`Gate::spend`]).
    pub(crate) fn admit(&self, sender: &str, bearer: bool) -> bool {
        let mut window = self.state();
        let limit = window.limit;
        let full = window.spent.len() >= SENDER_KEYS;
        match window.spent.get_mut(&(bearer, sender.to_owned())) {
            Some(spent) if *spent >= limit => false,
            Some(spent) => {
                if !bearer {
                    *spent += 1;
                }
                true
            }
            None if bearer => true,
            None if full => false,
            None => {
                window.spent.insert((false, sender.to_owned()), 1);
                true
            }
        }
    }
    /// A refused token spends one of its sender's token budget.
    pub(crate) fn spend(&self, sender: &str) {
        let mut window = self.state();
        let full = window.spent.len() >= SENDER_KEYS;
        match window.spent.get_mut(&(true, sender.to_owned())) {
            Some(spent) => *spent = spent.saturating_add(1),
            None if full => {}
            None => {
                window.spent.insert((true, sender.to_owned()), 1);
            }
        }
    }
    /// Counts one refusal instead of recording it.
    pub(crate) fn absorb(&self, endpoint: &str, status: StatusCode) {
        *self
            .state()
            .refused
            .entry((group(endpoint), status.as_u16()))
            .or_insert(0) += 1;
    }
}
/// Whether a refusal with no sealed scope joins the minute's counts instead
/// of writing its own exchange row.
pub(crate) fn absorbed(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED
            | StatusCode::NOT_FOUND
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::TOO_MANY_REQUESTS
    )
}
/// The edge's fixed endpoint groups, so a flood of made-up paths still
/// counts under a handful of keys.
fn group(endpoint: &str) -> &'static str {
    if endpoint == "/api/v1" || endpoint.starts_with("/api/v1/") {
        "/api/v1"
    } else if endpoint == "/mcp" {
        "/mcp"
    } else if endpoint.starts_with("/oauth/") {
        "/oauth"
    } else {
        "/.well-known"
    }
}
fn flush(app: &App, closed: Counts) {
    if closed.is_empty() {
        return;
    }
    let db = app.db.clone();
    tokio::spawn(async move {
        for ((endpoint, status), count) in closed {
            let payload = json!({"endpoint": endpoint, "status": status, "result": "refused", "count": count});
            let written = match db.get().await {
                Ok(client) => {
                    record_action(
                        &**client,
                        &crate::webhooks::guest(),
                        None,
                        EVENT,
                        payload,
                        false,
                    )
                    .await
                }
                Err(error) => Err(error.into()),
            };
            if let Err(error) = written {
                eprintln!("warning: aggregated agent edge refusals for {endpoint} were not audited: {error}");
            }
        }
    });
}
