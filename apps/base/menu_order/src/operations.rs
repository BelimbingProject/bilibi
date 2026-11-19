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
    Ok(AudiencePreview::everyone().review(nodes(&tree, &labels)))
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
                            json!({
                                "parent": parent,
                                "product": product,
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
            let audience_preview = preview_tree(service, client, scope, tree, orders).await?;
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
            let plan = bound()?
                .apply(tx, scope, parent(input)?, ids.as_deref(), version(input)?)
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
    json!({"type":"object","required":["parent","item_ids","version"],"properties":{"parent":{"type":"string","description":"Parent ID; empty string means the root."},"item_ids":{"type":"array","items":{"type":"string"},"description":"Sibling IDs in desired order. Unlisted items follow in product order."},"version":{"type":"string","description":"Version token for this group from list."}}})
}
fn restore_schema() -> Value {
    json!({"type":"object","required":["parent","version"],"properties":{"parent":{"type":"string"},"version":{"type":"string","description":"Version token for this group from list."}}})
}
fn list_output() -> Value {
    json!({"type":"array","description":"Visible sibling groups and their saved order."})
}
fn preview_output() -> Value {
    json!({"type":"object","properties":{
        "audience":{"type":"object","properties":{"everyone":{"type":"boolean"},"name":{"type":"string"}}},
        "visible_to_current_reader":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"label":{"type":"string"},"children":{"type":"array"}}}},
        "notice":{"type":"string"},
    }})
}
fn output() -> Value {
    json!({"type":"object","description":"The before and after order."})
}
inventory::submit! { Contribution::new(DEFINITIONS, "base/menu_order.agent", Provenance { module:"base/menu_order", source:file!(), line:line!() }, &Definitions {
    operations: &[
        Operation { key:"base.menu_order.list", title:"List menu sibling groups", summary:"Read product and saved order with version tokens.", keywords:&["menu","order"], capability:VIEW, input:empty_schema, output:list_output, handler:Handler::Read(&Read::List), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.menu_order.preview", title:"Preview my menu", summary:"Preview the menu visible to the current reader in the tenant's order.", keywords:&["menu","preview"], capability:VIEW, input:preview_schema, output:preview_output, handler:Handler::Read(&Read::Preview), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.menu_order.set", title:"Order menu siblings", summary:"Set a tenant's order for siblings by stable menu item ID.", keywords:&["menu","order"], capability:MANAGE, input:set_schema, output, handler:Handler::SafeWrite(&Write::Set), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.menu_order.restore", title:"Restore product menu order", summary:"Restore product order for the visible siblings in one group.", keywords:&["menu","restore"], capability:MANAGE, input:restore_schema, output, handler:Handler::SafeWrite(&Write::Restore), channels:Channels::Every, step_up:&[] },
    ],
    guides:&[Guide { key:"base.menu_order", title:"Menu order", summary:"Arrange a tenant's menu siblings and restore product order.", keywords:&["menu","order","navigation"], capability:VIEW, body:"Use base.menu_order.list to find visible sibling IDs, the parent ID and version token. The empty parent is the staff menu root. Set an ordered list of visible sibling IDs with base.menu_order.set. Unlisted and newly contributed visible siblings follow in product order, and removed items disappear. Use base.menu_order.preview to see the current reader's permitted menu. Optionally pass parent and item_ids to preview a proposed order without saving. Use base.menu_order.restore with the parent and current version to return visible siblings to product order. Both writes preserve saved IDs for siblings outside your view. Each write requires a dry run, a complete preview and explicit approval before committing the unchanged operation." }],
}) }
