//! Reverse the creation of a draft through the Customisations page. An
//! approved watcher or one with subsequent edits needs its ordinary workflow.
use crate::{agent::watchers, WatcherState, Watchers, APPROVE, MANAGE, VIEW};
use bilibi_agent_api::{
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, WriteHandler,
    DEFINITIONS,
};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::GenericClient;

const DISCARD: &str = "base.watchers.discard_draft";
const EVENT: &str = "customisation.receipt_watcher_draft";
const KIND: &str = "base/watchers.watcher";
const ACTIVATION_KIND: &str = "base/watchers.activation";

impl Watchers {
    async fn discardable(
        &self,
        sql: &(impl GenericClient + Sync),
        scope: &Scope,
        id: i64,
    ) -> Result<(String, String)> {
        bilibi_authz::authorize_key(sql, scope, MANAGE).await?;
        bilibi_authz::authorize_key(sql, scope, VIEW).await?;
        sql.query_opt(
            "SELECT name,trigger FROM base_watchers WHERE tenant_id=$1 AND id=$2 AND version=1 AND active_version_id IS NULL",
            &[&scope.tenant(), &id],
        )
        .await?
        .map(|row| (row.get(0), row.get(1)))
        .ok_or_else(|| Error::Refused("Only the first, still unapproved draft can be discarded here.".into()))
    }

    async fn discard_initial(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
    ) -> Result<()> {
        self.lock_admission(tx.sql(), scope).await?;
        let (name, trigger) = self.discardable(tx.sql(), scope, id).await?;
        let tenant = scope.tenant();
        tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_one(
                "DELETE FROM base_watcher_versions WHERE tenant_id=$1 AND watcher_id=$2 AND number=1 AND approved_at IS NULL RETURNING id,definition,examples",
                &[&tenant, &id],
            ).await?;
            let version = row.get(0);
            let summary = draft_summary(&row.get(1), &row.get(2));
            sql.execute("DELETE FROM base_watchers WHERE tenant_id=$1 AND id=$2", &[&tenant, &id]).await?;
            Ok(((), vec![
                Change { kind: "base/watchers.version".into(), id: version, old: json!({"watcher":id,"number":1}), new:Value::Null },
                Change { kind: KIND.into(), id, old:json!({"name":name, "state":"draft", "watch":trigger, "rule":summary}), new:Value::Null },
            ]))
        })).await
    }
}

struct Discard;

impl WriteHandler for Discard {
    fn owner(&self) -> &'static str {
        "base/watchers"
    }

    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let id = input["watcher_id"].as_i64().ok_or(Error::Invalid)?;
            let (name, _) = watchers()?
                .discardable(client.sql_client().await?, scope, id)
                .await?;
            Ok(json!({"summary":format!("Discard the draft “{name}”.")}).into())
        })
    }

    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let id = input["watcher_id"].as_i64().ok_or(Error::Invalid)?;
            watchers()?.discard_initial(tx, scope, id).await?;
            Ok(json!({"discarded":true}).into())
        })
    }
}

fn input() -> Value {
    json!({"type":"object","required":["watcher_id"],"properties":{"watcher_id":{"type":"integer","minimum":1}}})
}
fn output() -> Value {
    json!({"type":"object"})
}
pub(crate) fn draft_summary(definition: &Value, examples: &Value) -> Value {
    let conditions: Vec<_> = definition["if"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|condition| json!({"field":condition["field"], "operator":condition["op"]}))
        .collect();
    let actions: Vec<_> = definition["do"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|action| match action.get("script") {
            Some(script) => json!({"script":script["name"],"language":script["language"],"source_digest":script["source_digest"],"fields":script["fields"],"operations":script["calls"].as_array().into_iter().flatten().map(|call| &call["operation"]).collect::<Vec<_>>()}),
            None => action["operation"].clone(),
        })
        .collect();
    let examples: Vec<_> = examples
        .as_array()
        .into_iter()
        .flatten()
        .map(|example| &example["expect"])
        .collect();
    json!({"match":definition["if"]["match"], "conditions":conditions, "actions":actions, "example outcomes":examples})
}

fn correction(change: &Value, effective: &Value) -> Option<bilibi_labels::ReceiptCorrection> {
    if !change["before"].is_null() || effective["state"] != "draft" {
        return None;
    }
    let id: i64 = change["id"].as_str()?.parse().ok()?;
    Some(bilibi_labels::ReceiptCorrection {
        contract: DISCARD,
        event: EVENT,
        input: json!({"watcher_id":id}),
    })
}

inventory::submit! { Contribution::new(DEFINITIONS, DISCARD, Provenance { module: "base/watchers", source: file!(), line: line!() }, &Definitions {
    operations: &[Operation {
        key: DISCARD, title: "Discard a watcher draft",
        summary: "Discard an initial draft that has never been approved or revised.",
        keywords: &["watcher", "discard", "undo"], capability: MANAGE,
        input, output, handler: Handler::SafeWrite(&Discard), channels: Channels::Every, step_up: &[],
    }], guides: &[],
}) }

inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, DISCARD, Provenance { module: "base/watchers", source: file!(), line: line!() },
    &bilibi_agent_api::page::PageBinding { service: "base/labels.customisations", event: EVENT, contract: DISCARD }
) }
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.watchers.restore", Provenance { module: "base/watchers", source: file!(), line: line!() },
    &bilibi_agent_api::page::PageBinding { service: "base/labels.customisations", event: "customisation.receipt_watcher_restore", contract: "base.watchers.restore" }
) }

fn project_draft_receipt<'a>(
    client: &'a dyn OperationClient,
    scope: &'a Scope,
    change: &'a mut Value,
    _: &'a Value,
) -> bilibi_labels::ReceiptFuture<'a> {
    Box::pin(async move {
        // The watcher page owns approvals, subsequent versions and run history.
        let created = change["before"].is_null() && change["after"]["version"] == 1;
        let discarded = change["after"].is_null() && change["before"]["state"] == "draft";
        if !created && !discarded {
            return Ok(false);
        }
        let id: i64 = change["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .ok_or(Error::Internal)?;
        let current = client
            .query_opt(
                "SELECT w.name,w.version,w.active_version_id IS NOT NULL,w.disabled_at IS NOT NULL,
                    w.trigger,v.definition,v.examples,v.approved_at IS NOT NULL,w.needs_review
             FROM base_watchers w JOIN LATERAL (
                 SELECT definition,examples,approved_at FROM base_watcher_versions
                 WHERE tenant_id=w.tenant_id AND watcher_id=w.id
                 ORDER BY (id=w.active_version_id) DESC NULLS LAST, number DESC LIMIT 1
             ) v ON true WHERE w.tenant_id=$1 AND w.id=$2",
                &[&scope.tenant(), &id],
            )
            .await?;
        change["effective"] = current.as_ref().map_or(Value::Null, |row| {
            let state = WatcherState::from_flags(row.get(2), row.get(3), row.get(8)).key();
            json!({"name":row.get::<_,String>(0), "state":state, "watch":row.get::<_,String>(4), "rule":draft_summary(&row.get(5), &row.get(6))})
        });
        change["changed_since"] = json!(if created {
            current.is_none_or(|row| {
                row.get::<_, i32>(1) != 1 || row.get::<_, bool>(2) || row.get::<_, bool>(7)
            })
        } else {
            current.is_some()
        });
        Ok(true)
    })
}

inventory::submit! {
    Contribution::new(bilibi_labels::RECEIPT_SOURCES, "base/watchers.draft_receipts",
        Provenance { module: "base/watchers", source: file!(), line: line!() },
        &bilibi_labels::ReceiptSource {
            kind: KIND, capability: crate::VIEW, subject_filter: None,
            undo: Some(bilibi_labels::ReceiptUndo { capability: MANAGE, correction }),
            read: None, project: project_draft_receipt,
        })
}

fn restore_correction(
    change: &Value,
    effective: &Value,
) -> Option<bilibi_labels::ReceiptCorrection> {
    let number = change["before"]["number"].as_i64()?;
    if effective["active_version"] != change["after"]["active_version"] {
        return None;
    }
    if effective["activation"] != change["after"]["activation"] || effective["enabled"] != true {
        return None;
    }
    let watcher = change["id"].as_str()?.parse::<i64>().ok()?;
    Some(bilibi_labels::ReceiptCorrection {
        contract: "base.watchers.restore",
        event: "customisation.receipt_watcher_restore",
        input: json!({"watcher_id":watcher,"version":number,"expected_activation":change["after"]["activation"]}),
    })
}

fn project_activation<'a>(
    client: &'a dyn OperationClient,
    scope: &'a Scope,
    change: &'a mut Value,
    _: &'a Value,
) -> bilibi_labels::ReceiptFuture<'a> {
    Box::pin(async move {
        let watcher = change["id"]
            .as_str()
            .and_then(|id| id.parse::<i64>().ok())
            .ok_or(Error::Internal)?;
        let current = client
            .query_opt(
                "SELECT w.name,w.active_version_id,v.number,w.version,w.disabled_at IS NULL FROM base_watchers w LEFT JOIN base_watcher_versions v ON v.tenant_id=w.tenant_id AND v.id=w.active_version_id WHERE w.tenant_id=$1 AND w.id=$2",
                &[&scope.tenant(), &watcher],
            )
            .await?;
        let Some(current) = current else {
            return Ok(false);
        };
        change["effective"] = json!({
            "name": current.get::<_, String>(0),
            "active_version": current.get::<_, Option<i64>>(1),
            "number": current.get::<_, Option<i32>>(2),
            "activation": current.get::<_, i32>(3),
            "enabled": current.get::<_, bool>(4),
        });
        change["changed_since"] = json!(
            change["effective"]["active_version"] != change["after"]["active_version"]
                || change["effective"]["activation"] != change["after"]["activation"]
                || change["effective"]["enabled"] != true
        );
        Ok(true)
    })
}

inventory::submit! {
    Contribution::new(bilibi_labels::RECEIPT_SOURCES, "base/watchers.activation_receipts",
        Provenance { module: "base/watchers", source: file!(), line: line!() },
        &bilibi_labels::ReceiptSource {
            kind: ACTIVATION_KIND, capability: VIEW, subject_filter: None,
            undo: Some(bilibi_labels::ReceiptUndo { capability: APPROVE, correction: restore_correction }),
            read: None, project: project_activation,
        })
}
