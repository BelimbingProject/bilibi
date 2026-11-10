//! Reference subscriber to `core/user.session_ended`, the shape a
//! contact-centre Domain takes when an agent who signs out should go offline
//! at once. It subscribes by the event name only: its Cargo manifest names
//! Core User so the publisher is linked (ADR 25), and no code calls it. It
//! keeps its own copy of each notice, and asks Base Session whether the
//! account still holds a live session as it handles the notice. Mount it only
//! to prove the notice (`apps/web/examples/session-ended-walkthrough.rs`).
use bilibi_authz::SystemScope;
use bilibi_database::{Change, Error};
use bilibi_events::{Event, On, Subscribe, Subscriber, SubscriberFuture, SUBSCRIBERS};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance, RegistryError, Service};
use bilibi_session::Sessions;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use tokio_postgres::Transaction;

const MODULE: &str = "session_listener/presence";
// Core User's documented event name and payload; its crate is never named.
const SESSION_ENDED: &str = "core/user.session_ended";
static SESSIONS: OnceLock<Arc<Sessions>> = OnceLock::new();

inventory::submit! {
    Module {
        id: MODULE, layer: Layer::Domains,
        dependencies: &["base/authz", "base/database", "base/events", "base/session"],
        migrations: &[("session-listener-presence-001-notices", include_str!("../migrations/001-notices.sql"))],
        services: &[Service { id: "session_listener/presence.sessions", factory: |services| {
            SESSIONS.set(services.get("base/session.sessions")?)
                .map_err(|_| RegistryError::Service("duplicate presence sessions binding".into()))?;
            Ok(Arc::new(()))
        } }],
    }
}
inventory::submit! { Contribution::new(SUBSCRIBERS, "session_listener/presence.sessions", Provenance { module: MODULE, source: file!(), line: line!() }, &Subscriber {
    events: &[On { event: SESSION_ENDED, versions: &[1] }],
    handler: &Presence,
}) }

struct Presence;
impl Subscribe for Presence {
    fn handle<'a>(
        &'a self,
        tx: &'a Transaction<'_>,
        scope: &'a SystemScope,
        event: &'a Event,
    ) -> SubscriberFuture<'a> {
        Box::pin(async move {
            let payload = &event.payload;
            let account = payload
                .get("account")
                .and_then(Value::as_i64)
                .ok_or(Error::Invalid)?;
            let reason = payload
                .get("reason")
                .and_then(Value::as_str)
                .ok_or(Error::Invalid)?;
            let sessions = payload
                .get("sessions")
                .and_then(Value::as_i64)
                .ok_or(Error::Invalid)?;
            let still = payload
                .get("still_signed_in")
                .and_then(Value::as_bool)
                .ok_or(Error::Invalid)?;
            let now = SESSIONS
                .get()
                .ok_or(Error::Internal)?
                .signed_in(tx, scope, account)
                .await?;
            // The event key makes a redelivered notice a no-op.
            let copied = tx.execute(
                "INSERT INTO session_listener_notices(tenant_id,account,reason,sessions,still_signed_in,signed_in_now,event_key,payload) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
                &[&scope.tenant(), &account, &reason, &sessions, &still, &now, &event.key, payload],
            ).await?;
            if copied == 0 {
                return Ok(Vec::new());
            }
            Ok(vec![Change {
                kind: "session_listener/presence.notice".into(),
                id: event.id,
                old: Value::Null,
                new: json!({"account": account, "reason": reason, "offline": !now}),
            }])
        })
    }
}
