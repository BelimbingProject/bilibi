use crate::{
    store::{sibling_groups, sibling_ids, version_token, MenuOrder, Orders},
    MANAGE, VIEW,
};
use bilibi_agent_api::{
    json::{Data, Output},
    AudiencePreview, Channels, Definitions, Guide, Handler, Operation, OperationClient,
    OperationFuture, ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
};

static BOUND: OnceLock<Arc<MenuOrder>> = OnceLock::new();
pub(crate) fn bind(order: Arc<MenuOrder>) -> bilibi_module_registry::Result<()> {
    BOUND
        .set(order)
        .map_err(|_| RegistryError::Service("duplicate menu order binding".into()))
}
fn bound() -> bilibi_agent_api::Result<&'static Arc<MenuOrder>> {
    BOUND.get().ok_or(Error::Internal.into())
}
fn data(value: Value) -> Output {
    Output::Data(Data::restricted(value, &HashSet::new()))
}
fn parent(input: &Value) -> bilibi_agent_api::Result<&str> {
    input.get("parent").and_then(Value::as_str).ok_or_else(|| {
        bilibi_agent_api::invalid("parent", "is required; use an empty string for the root")
    })
}
fn version(input: &Value) -> bilibi_agent_api::Result<&str> {
    input
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| bilibi_agent_api::invalid("version", "must be the version token you read"))
}
fn items(input: &Value) -> bilibi_agent_api::Result<Vec<String>> {
    let array = input
        .get("item_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| bilibi_agent_api::invalid("item_ids", "must be an array of sibling IDs"))?;
    if array.len() > 5000 {
        return Err(bilibi_agent_api::invalid(
            "item_ids",
            "at most 5000 siblings",
        ));
    }
    array
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| bilibi_agent_api::invalid("item_ids", "each ID must be a string"))
        })
        .collect()
}
fn found(error: Error) -> bilibi_agent_api::Error {
    match error {
        Error::Refused(message) => bilibi_agent_api::invalid("item_ids", &message),
        other => other.into(),
    }
}
fn nodes(tree: &[bilibi_menu::Node], labels: &bilibi_labels::LabelSet) -> Value {
    Value::Array(tree.iter().map(|n| json!({"id": n.item.item.id, "label": labels.menu(n.item.item.id, n.item.item.label), "children": nodes(&n.children, labels)})).collect())
}
async fn preview_tree(
    service: &MenuOrder,
    client: &dyn OperationClient,
    scope: &Scope,
    mut tree: Vec<bilibi_menu::Node>,
    orders: Orders,
) -> bilibi_agent_api::Result<Value> {
    orders.apply(&mut tree);
    let labels = service
        .labels
        .labels(client.sql_client().await?, scope)
        .await?;
    Ok(nodes(&tree, &labels))
}

enum Read {
    List,
    Preview,
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "base/menu_order"
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let service = bound()?;
            let tree = service.visible_tree(client, scope).await?;
            match self {
                Self::List => {
                    let stored = service.stored(client, scope).await?;
                    let rows: Vec<_> = sibling_groups(&tree)
                        .into_iter()
                        .map(|(parent, nodes)| {
                            let saved = stored.get(parent);
                            let siblings = sibling_ids(nodes);
                            let product: Vec<_> = nodes
                                .iter()
                                .map(|n| json!({"id":n.item.item.id,"label":n.item.item.label}))
                                .collect();
                            let mut effective: Vec<_> =
                                nodes.iter().map(|node| node.item.item.id).collect();
                            if let Some(saved) = saved {
                                saved.order_ids(&mut effective);
                            }
                            json!({
                                "parent": parent,
                                "product": product,
                                "effective_item_ids": effective,
                                "item_ids": saved.map(|s| s.visible_items(&siblings)),
                                "version": version_token(parent, &siblings, saved),
                            })
                        })
                        .collect();
                    Ok(data(json!(rows)))
                }
                Self::Preview => {
                    let mut orders = service.orders(client, scope).await?;
                    if input.get("parent").is_some() || input.get("item_ids").is_some() {
                        let parent = parent(input)?;
                        let proposed = items(input)?;
                        MenuOrder::validate(&tree, parent, &proposed).map_err(found)?;
                        orders.propose(parent, proposed);
                    }
                    Ok(data(
                        preview_tree(service, client, scope, tree, orders).await?,
                    ))
                }
            }
        })
    }
}
enum Write {
    Set,
    Restore,
}
impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        "base/menu_order"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let ids = match self {
                Self::Set => Some(items(input)?),
                Self::Restore => None,
            };
            let service = bound()?;
            if let Some(id) = input["receipt"].as_i64() {
                check_receipt(client.sql_client().await?, scope, parent(input)?, id).await?;
            }
            let (plan, tree) = service
                .plan(
                    client,
                    scope,
                    parent(input)?,
                    ids.as_deref(),
                    version(input)?,
                    false,
                )
                .await
                .map_err(found)?;
            let mut orders = service.orders(client, scope).await?;
            plan.propose(&mut orders);
            let menu = preview_tree(service, client, scope, tree, orders).await?;
            let audience_preview = AudiencePreview::everyone().review(menu);
            Ok(json!({"summary": if ids.is_some() { "Set the order of menu siblings" } else { "Restore the product order of menu siblings" }, "plan":plan.review(), "audience_preview":audience_preview}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let ids = match self {
                Self::Set => Some(items(input)?),
                Self::Restore => None,
            };
            let parent = parent(input)?;
            bilibi_authz::authorize_key(tx.sql(), scope, MANAGE).await?;
            let lock = format!("base/menu_order:{}:{parent}", scope.tenant());
            tx.sql()
                .execute(
                    "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                    &[&lock],
                )
                .await
                .map_err(Error::from)?;
            if let Some(id) = input["receipt"].as_i64() {
                check_receipt(tx.sql().client(), scope, parent, id).await?;
            }
            let plan = bound()?
                .apply(tx, scope, parent, ids.as_deref(), version(input)?)
                .await
                .map_err(found)?;
            Ok(data(plan.review()))
        })
    }
}
fn empty_schema() -> Value {
    json!({"type":"object","properties":{}})
}
fn preview_schema() -> Value {
    json!({"type":"object","properties":{"parent":{"type":"string","description":"Optional sibling parent; empty string means root. Supply with item_ids."},"item_ids":{"type":"array","items":{"type":"string"},"description":"Optional proposed order for this reader; supply with parent. An empty array previews product order."}}})
}
fn set_schema() -> Value {
    json!({"type":"object","required":["parent","item_ids","version"],"properties":{"receipt":{"type":"integer","minimum":1},"parent":{"type":"string","description":"Parent ID; empty string means the root."},"item_ids":{"type":"array","items":{"type":"string"},"description":"Sibling IDs in desired order. Unlisted items follow in product order."},"version":{"type":"string","description":"Version token for this group from list."}}})
}
fn restore_schema() -> Value {
    json!({"type":"object","required":["parent","version"],"properties":{"receipt":{"type":"integer","minimum":1},"parent":{"type":"string"},"version":{"type":"string","description":"Version token for this group from list."}}})
}
fn list_output() -> Value {
    json!({"type":"array","description":"Sibling groups or the reader’s visible menu tree."})
}
fn output() -> Value {
    json!({"type":"object","description":"The before and after order."})
}
inventory::submit! { Contribution::new(DEFINITIONS, "base/menu_order.agent", Provenance { module:"base/menu_order", source:file!(), line:line!() }, &Definitions {
    operations: &[
        Operation { key:"base.menu_order.list", title:"List menu sibling groups", summary:"Read product, saved and effective order with version tokens.", keywords:&["menu","order"], capability:VIEW, input:empty_schema, output:list_output, handler:Handler::Read(&Read::List), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.menu_order.preview", title:"Preview my menu", summary:"Preview the menu visible to the current reader in the tenant's order.", keywords:&["menu","preview"], capability:VIEW, input:preview_schema, output:list_output, handler:Handler::Read(&Read::Preview), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.menu_order.set", title:"Order menu siblings", summary:"Set a tenant's order for siblings by stable menu item ID.", keywords:&["menu","order"], capability:MANAGE, input:set_schema, output, handler:Handler::SafeWrite(&Write::Set), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.menu_order.restore", title:"Restore product menu order", summary:"Restore product order for the visible siblings in one group.", keywords:&["menu","restore"], capability:MANAGE, input:restore_schema, output, handler:Handler::SafeWrite(&Write::Restore), channels:Channels::Every, step_up:&[] },
    ],
    guides:&[Guide { key:"base.menu_order", title:"Menu order", summary:"Arrange a tenant's menu siblings and restore product order.", keywords:&["menu","order","navigation"], capability:VIEW, body:"Use base.menu_order.list to find visible sibling IDs, the parent ID and version token. The empty parent is the staff menu root. Set an ordered list of visible sibling IDs with base.menu_order.set. Unlisted and newly contributed visible siblings follow in product order, and removed items disappear. Use base.menu_order.preview to see the current reader's permitted menu. Optionally pass parent and item_ids to preview a proposed order without saving. Use base.menu_order.restore with the parent and current version to return visible siblings to product order. Both writes preserve saved IDs for siblings outside your view. Each write requires a dry run, a complete preview and explicit approval before committing the unchanged operation." }],
}) }

/// What may be changed: every sibling group the caller sees, in product
/// order or the tenant's own.
fn customisation_choices(reads: &[Value]) -> Vec<bilibi_labels::Choice> {
    let groups = reads[0].as_array().into_iter().flatten();
    groups
        .filter_map(|group| {
            let parent = group["parent"].as_str()?;
            let product: Vec<Value> = group["product"]
                .as_array()?
                .iter()
                .map(|item| item["id"].clone())
                .collect();
            let saved = group["item_ids"].as_array().filter(|ids| !ids.is_empty());
            Some(bilibi_labels::Choice {
                keywords: "menu order reorder arrange navigation sidebar move restore",
                subject: if parent.is_empty() { "root" } else { parent }.to_owned(),
                title: if parent.is_empty() {
                    "Menu order: the top level".to_owned()
                } else {
                    format!("Menu order: items under {parent}")
                },
                value: Some(group["effective_item_ids"].clone()),
                default: Some(json!(product)),
                source: Some(if saved.is_some() { "tenant" } else { "default" }.into()),
                customised: saved.is_some(),
                version: Some(group["version"].clone()),
                input: json!({"parent": parent, "version": group["version"]}),
                verify_input: no_input(),
                ..Default::default()
            })
        })
        .collect()
}
fn no_input() -> Value {
    json!({})
}
inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/menu_order.customisations", Provenance { module:"base/menu_order", source:file!(), line:line!() }, &bilibi_labels::CustomisationSection {
    title: "Menu order",
    undo: None,
    changes: Some(bilibi_labels::Changes {
        purpose: "Arrange the staff menu's items among their siblings for everyone in this tenant.",
        limits: "Only the order of siblings under one parent, by stable menu item ID. An order never grants access, renames an item or moves it to another parent, and the customer portal menu is not ordered here.",
        guide: "base.menu_order",
        reads: &[bilibi_labels::Read { operation: "base.menu_order.list", input: no_input }],
        choices: customisation_choices,
        change: &["base.menu_order.set"],
        preview: Some("base.menu_order.preview"),
        verify: "base.menu_order.preview",
        restore: Some("base.menu_order.restore"),
    }),
}) }

fn receipt_order<'a>(client: &'a dyn OperationClient, scope: &'a Scope) -> OperationFuture<'a> {
    Box::pin(async move { Read::List.call(client, scope, &Value::Null).await })
}

fn project_receipt<'a>(
    client: &'a dyn OperationClient,
    scope: &'a Scope,
    change: &'a mut Value,
    rows: &'a Value,
) -> bilibi_labels::ReceiptFuture<'a> {
    Box::pin(async move {
        let side = if change["after"].is_null() {
            &change["before"]
        } else {
            &change["after"]
        };
        let Some(current) = rows
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| row["parent"] == side["parent"])
        else {
            return Ok(false);
        };
        let latest = bilibi_audit::latest_customisation_mutation(
            client.sql_client().await?,
            scope,
            "menu_order",
            &json!({"parent": current["parent"]}),
        )
        .await?;
        let visible: HashSet<_> = current["product"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["id"].as_str())
            .collect();
        for key in ["before", "after"] {
            if let Some(items) = change[key]
                .get_mut("item_ids")
                .and_then(Value::as_array_mut)
            {
                items.retain(|id| id.as_str().is_some_and(|id| visible.contains(id)));
            }
        }
        change["changed_since"] = json!(latest != change["mutation_id"].as_i64());
        change["effective"] = current.clone();
        Ok(true)
    })
}

fn receipt_correction(change: &Value, current: &Value) -> Option<bilibi_labels::ReceiptCorrection> {
    let old = &change["before"]["item_ids"];
    let items = old.as_array().filter(|items| !items.is_empty());
    let after = change["after"]["item_ids"]
        .as_array()
        .filter(|items| !items.is_empty());
    if items == after {
        return None;
    }
    let mut input = json!({"parent": current["parent"], "version": current["version"], "receipt": change["mutation_id"]});
    let (contract, event) = if let Some(items) = items {
        input["item_ids"] = json!(items);
        ("base.menu_order.set", "customisation.receipt_menu_set")
    } else {
        (
            "base.menu_order.restore",
            "customisation.receipt_menu_restore",
        )
    };
    Some(bilibi_labels::ReceiptCorrection {
        contract,
        event,
        input,
    })
}

async fn check_receipt(
    sql: &tokio_postgres::Client,
    scope: &Scope,
    parent: &str,
    receipt: i64,
) -> bilibi_database::Result<()> {
    bilibi_authz::authorize_key(sql, scope, bilibi_labels::CUSTOMISATIONS_VIEW).await?;
    bilibi_authz::authorize_key(sql, scope, VIEW).await?;
    if bilibi_audit::latest_customisation_mutation(
        sql,
        scope,
        "menu_order",
        &json!({"parent":parent}),
    )
    .await?
        != Some(receipt)
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

inventory::submit! {
    Contribution::new(bilibi_labels::RECEIPT_SOURCES, "base/menu_order.receipts",
        Provenance { module: "base/menu_order", source: file!(), line: line!() },
        &bilibi_labels::ReceiptSource { kind: "menu_order", capability: VIEW,
            undo: Some(bilibi_labels::ReceiptUndo { capability: MANAGE, correction: receipt_correction }),
            read: Some(receipt_order), project: project_receipt })
}
inventory::submit! {
    Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.menu_order.receipt_set",
        Provenance { module: "base/menu_order", source: file!(), line: line!() },
        &bilibi_agent_api::page::PageBinding { service: "base/labels.customisations", event: "customisation.receipt_menu_set", contract: "base.menu_order.set" })
}
inventory::submit! {
    Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.menu_order.receipt_restore",
        Provenance { module: "base/menu_order", source: file!(), line: line!() },
        &bilibi_agent_api::page::PageBinding { service: "base/labels.customisations", event: "customisation.receipt_menu_restore", contract: "base.menu_order.restore" })
}
