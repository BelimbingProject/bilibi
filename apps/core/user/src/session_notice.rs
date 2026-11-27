//! The durable notice that an account's session ended: `core/user.session_ended`
//! on Base Events. Base Session decides when a session ends and calls this
//! inside the ending transaction (it cannot depend on Base Events itself); the
//! event row, its deliveries and the delivery job commit or roll back with the
//! ending. The payload names the account and why, never the token, its digest
//! or the session ID; the event's tenant is the ending transaction's.
use bilibi_database::MutationTransaction;
use bilibi_events::{EventType, Events, NewEvent, EVENTS};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_session::{EndNotice, Ended, NoticeFuture};
use serde_json::json;
use std::sync::Arc;

pub(crate) const SESSION_ENDED: &str = "core/user.session_ended";

inventory::submit! { Contribution::new(EVENTS, SESSION_ENDED, Provenance { module: "core/user", source: file!(), line: line!() }, &EventType { version: 1 }) }

pub(crate) struct SessionNotice(pub(crate) Arc<Events>);
impl EndNotice for SessionNotice {
    fn record<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        ended: &'a [Ended],
    ) -> NoticeFuture<'a> {
        Box::pin(async move {
            for notice in ended {
                // Each ending is its own fact: the key is fresh, never derived
                // from the session.
                let key = format!("session_ended:{}", uuid::Uuid::new_v4().simple());
                self.0
                    .record(
                        tx,
                        NewEvent {
                            name: SESSION_ENDED,
                            subject: &format!("account:{}", notice.account()),
                            key: &key,
                            payload: json!({
                                "account": notice.account(),
                                "reason": notice.reason().as_str(),
                                "sessions": notice.sessions(),
                                "still_signed_in": notice.still_signed_in(),
                            }),
                        },
                    )
                    .await?;
            }
            Ok(())
        })
    }
}
