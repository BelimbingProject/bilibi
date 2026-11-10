//! Operation contracts for the calendar feed key (`feed.rs`): read whether
//! the caller has one, make or replace it (the key is returned once and never
//! stored), and revoke it. The Settings page calls the same write contracts;
//! an administrator revokes anyone's with `user_id`.
use crate::agent_operations::{invalid, owner};
use crate::{FeedKey, FEED_PATH, STEP_UP_FEED_KEY, VIEW};
use bilibi_agent_api::{
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, ReadHandler,
    WriteHandler, DEFINITIONS,
};
use bilibi_database::MutationTransaction;
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::{Channel, Scope};
use serde_json::{json, Value};

const KEYWORDS: &[&str] = &[
    "activity",
    "calendar",
    "feed",
    "ical",
    "icalendar",
    "subscribe",
];

fn key_record(key: Option<&FeedKey>) -> Value {
    match key {
        Some(key) => json!({
            "active": true,
            "created_at": key.created_at.to_string(),
            "last_used_at": key.last_used_at.map(|at| at.to_string()),
        }),
        None => json!({"active": false}),
    }
}

struct GetFeedKey;
impl ReadHandler for GetFeedKey {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn call<'a>(
        &'a self,
        _: &'a dyn OperationClient,
        scope: &'a Scope,
        _: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let key = owner()?.feed_key(scope).await?;
            Ok(key_record(key.as_ref()).into())
        })
    }
}

struct CreateFeedKey;
impl WriteHandler for CreateFeedKey {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        _: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let owner = owner()?;
            owner
                .preview_create_feed_key(client, scope)
                .await
                .map_err(|e| invalid(e, ""))?;
            let replaces = owner.feed_key(scope).await?.is_some();
            Ok(json!({"creates_feed_key": true, "replaces_existing_link": replaces}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        _: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (key, token) = owner()?
                .create_feed_key(tx, scope)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({
                "key": token,
                "path": FEED_PATH.replace("{token}", &token),
                "created_at": key.created_at.to_string(),
                "notice": "This link is shown once. Anyone who has it can read your calendar until you revoke it.",
            })
            .into())
        })
    }
}

fn target(input: &Value) -> Option<i64> {
    input["user_id"].as_i64().filter(|id| *id > 0)
}

struct RevokeFeedKey;
impl WriteHandler for RevokeFeedKey {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let user = owner()?
                .preview_revoke_feed_key(client, scope, target(input))
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"revokes_feed_key_of_user": user}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let user = owner()?
                .revoke_feed_key(tx, scope, target(input))
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"revoked": true, "user_id": user}).into())
        })
    }
}

fn no_input() -> Value {
    json!({"type":"object","properties":{}})
}
fn revoke_schema() -> Value {
    json!({"type":"object","properties":{
        "user_id":{"type":"integer","minimum":1,"description":"Another person's account, to revoke their key: needs core.activity.feed.manage. Omitted revokes your own."}
    }})
}
fn key_output() -> Value {
    json!({"type":"object","description":"active, and when active created_at and last_used_at (null until the link is first read). The key itself is never shown again."})
}
fn review_output() -> Value {
    json!({"type":"object","description":"The review shows what will happen; the commit returns the result. Making a key returns the link once."})
}

static GET: GetFeedKey = GetFeedKey;
static CREATE: CreateFeedKey = CreateFeedKey;
static REVOKE: RevokeFeedKey = RevokeFeedKey;
inventory::submit! { Contribution::new(DEFINITIONS, "core/activity.feed_operations", Provenance { module: crate::MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "core.activity.feed_key", title: "Read your calendar feed link's status", summary: "Whether you have a calendar feed link and when it was made and last read. The link itself is never shown again.", keywords: KEYWORDS, capability: VIEW, input: no_input, output: key_output, handler: Handler::Read(&GET), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.create_feed_key", title: "Make or replace your calendar feed link", summary: "Make a read-only iCalendar link of the activities you take part in, for a calendar app to subscribe to. It replaces your old link, which stops working at once. Only the person's browser may make it: the link is shown once on My settings › Calendar feed.", keywords: KEYWORDS, capability: VIEW, input: no_input, output: review_output, handler: Handler::SafeWrite(&CREATE), channels: Channels::Only { channels: &[Channel::Web], reason: "The link is shown once, in the browser on My settings; an AI connection or a review card would keep it in model context and stored outcomes." }, step_up: &[STEP_UP_FEED_KEY] },
        Operation { key: "core.activity.revoke_feed_key", title: "Revoke a calendar feed link", summary: "Revoke your own calendar feed link, or with user_id and core.activity.feed.manage anyone's in the tenant. It stops working at once.", keywords: KEYWORDS, capability: VIEW, input: revoke_schema, output: review_output, handler: Handler::SafeWrite(&REVOKE), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[],
}) }
