//! Reverse the creation of a draft through the Customisations page. An
//! approved watcher or one with subsequent edits needs its ordinary workflow.
use crate::{agent::watchers, Watchers, MANAGE};
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
const EVENT: &str = "customisation.watcher_draft";
const KIND: &str = "base/watchers.watcher";

impl Watchers {
    async fn discardable(
        &self,
        sql: &(impl GenericClient + Sync),
        scope: &Scope,
        id: i64,
    ) -> Result<String> {
        bilibi_authz::authorize_key(sql, scope, MANAGE).await?;
        sql.query_opt(
            "SELECT name FROM base_watchers WHERE tenant_id=$1 AND id=$2 AND version=1 AND active_version_id IS NULL",
            &[&scope.tenant(), &id],
        )
        .await?
        .map(|row| row.get(0))
        .ok_or_else(|| Error::Refused("Only the first, still unapproved draft can be discarded here.".into()))
    }

    async fn discard_initial(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
    ) -> Result<()> {
        self.lock_admission(tx.sql(), scope).await?;
        let name = self.discardable(tx.sql(), scope, id).await?;
        let tenant = scope.tenant();
        tx.mutate(move |sql| Box::pin(async move {
            let version = sql.query_one(
                "DELETE FROM base_watcher_versions WHERE tenant_id=$1 AND watcher_id=$2 AND number=1 AND approved_at IS NULL RETURNING id",
                &[&tenant, &id],
            ).await?.get(0);
            sql.execute("DELETE FROM base_watchers WHERE tenant_id=$1 AND id=$2", &[&tenant, &id]).await?;
            Ok(((), vec![
                Change { kind: "base/watchers.version".into(), id: version, old: json!({"watcher":id,"number":1}), new:Value::Null },
                Change { kind: KIND.into(), id, old:json!({"name":name, "state":"draft"}), new:Value::Null },
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
            let name = watchers()?
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
fn list_input() -> Value {
    json!({})
}

fn rows(result: &Value) -> Vec<bilibi_labels::CustomisationRow> {
    if result["can_discard"] != true {
        return Vec::new();
    }
    result["watchers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|watcher| {
            if watcher["latest_version"] != 1
                || watcher["awaiting_approval"] != true
                || !watcher["running_version"].is_null()
            {
                return None;
            }
            let id = watcher["watcher_id"].as_i64()?;
            Some(bilibi_labels::CustomisationRow {
                description: watcher["name"].as_str()?.into(),
                audit_kind: KIND,
                audit_id: id,
                revision: 1,
                input: json!({"watcher_id":id}),
            })
        })
        .collect()
}

inventory::submit! { Contribution::new(DEFINITIONS, DISCARD, Provenance { module: "base/watchers", source: file!(), line: line!() }, &Definitions {
    operations: &[Operation {
        key: DISCARD, title: "Discard a watcher draft",
        summary: "Discard an initial draft that has never been approved or revised.",
        keywords: &["watcher", "discard", "undo"], capability: MANAGE,
        input, output, handler: Handler::SafeWrite(&Discard), channels: Channels::Every, step_up: &[],
    }], guides: &[],
}) }

inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/watchers.customisations", Provenance { module: "base/watchers", source: file!(), line: line!() },
    &bilibi_labels::CustomisationSection {
        title: "Watcher drafts",
        undo: Some(bilibi_labels::Undo {
            read: "base.watchers.list", read_input: list_input, rows, operation: DISCARD, event: EVENT,
            action: "Discard draft", detail: "The draft has never run.",
        }),
        changes: None,
    }
) }

inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, DISCARD, Provenance { module: "base/watchers", source: file!(), line: line!() },
    &bilibi_agent_api::page::PageBinding { service: "base/labels.customisations", event: EVENT, contract: DISCARD }
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
        let current = client.query_opt(
            "SELECT name,version,active_version_id IS NOT NULL,disabled_at IS NOT NULL FROM base_watchers WHERE tenant_id=$1 AND id=$2",
            &[&scope.tenant(), &id],
        ).await?;
        change["effective"] = current.as_ref().map_or(Value::Null, |row| {
            let state = match (row.get::<_, bool>(2), row.get::<_, bool>(3)) {
                (true, true) => "off",
                (true, false) => "approved",
                _ => "draft",
            };
            json!({"name":row.get::<_,String>(0), "state":state})
        });
        change["changed_since"] = json!(if created {
            current.is_none_or(|row| row.get::<_, i32>(1) != 1)
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
            kind: KIND, capability: crate::VIEW, subject_filter: None, undo: None,
            read: None, project: project_draft_receipt,
        })
}
