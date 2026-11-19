use crate::{
    store::{MenuOrder, Plan},
    MANAGE, VIEW,
};
use bilibi_agent_api::{
    json::{Data, Output},
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
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
fn version(input: &Value) -> bilibi_agent_api::Result<(i64, i64)> {
    let (id, revision) = input
        .get("version")
        .and_then(Value::as_str)
        .and_then(|s| s.split_once(':'))
        .ok_or_else(|| {
            bilibi_agent_api::invalid("version", "must be the identity:version token you read")
        })?;
    let id = id.parse::<i64>().map_err(|_| Error::Invalid)?;
    let revision = revision.parse::<i64>().map_err(|_| Error::Invalid)?;
    if !((id == 0 && revision == 0) || (id > 0 && revision > 0)) {
        return Err(Error::Invalid.into());
    }
    Ok((id, revision))
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
fn plan_json(plan: &Plan) -> Value {
    json!({"parent":plan.parent,"before":plan.before.as_ref().map(|s| &s.items),"after":plan.after})
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
fn groups(nodes: &[bilibi_menu::Node], parent: &str, result: &mut BTreeMap<String, Vec<Value>>) {
    result.insert(
        parent.to_owned(),
        nodes
            .iter()
            .filter(|n| n.item.item.id != bilibi_menu::PORTAL_ROOT)
            .map(|n| json!({"id":n.item.item.id,"label":n.item.item.label}))
            .collect(),
    );
    for node in nodes
        .iter()
        .filter(|n| n.item.item.id != bilibi_menu::PORTAL_ROOT)
    {
        if !node.children.is_empty() {
            groups(&node.children, node.item.item.id, result);
        }
    }
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
            match self {
                Self::List => {
                    let stored = service.stored(client, scope).await?;
                    let mut all = BTreeMap::new();
                    groups(&service.menu.tree(), "", &mut all);
                    for parent in stored.keys() {
                        all.entry(parent.clone()).or_default();
                    }
                    let rows: Vec<_> = all.into_iter().map(|(parent, product)| {
                        let saved = stored.get(&parent);
                        json!({"parent":parent,"product":product,"item_ids":saved.map(|s| &s.items),"version":saved.map_or_else(|| "0:0".to_owned(), |s| format!("{}:{}",s.id,s.version))})
                    }).collect();
                    Ok(data(json!(rows)))
                }
                Self::Preview => {
                    let sql = client.sql_client().await?;
                    let capabilities = bilibi_authz::effective_capabilities(sql, scope).await?;
                    let owner = match bilibi_tenancy::prove_owner(sql, scope).await {
                        Ok(_) => true,
                        Err(Error::Forbidden) => false,
                        Err(e) => return Err(e.into()),
                    };
                    let mut tree = service.menu.visible_tree(&|key| {
                        if key == bilibi_menu::OWNER_STANDING {
                            owner
                        } else {
                            capabilities.contains(key)
                        }
                    });
                    let mut orders = service.orders(client, scope).await?;
                    if input.get("parent").is_some() || input.get("item_ids").is_some() {
                        let parent = parent(input)?;
                        let proposed = items(input)?;
                        service.validate(parent, &proposed).map_err(found)?;
                        orders.propose(parent, proposed);
                    }
                    orders.apply(&mut tree);
                    let labels = service.labels.labels(sql, scope).await?;
                    Ok(data(nodes(&tree, &labels)))
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
            let plan = bound()?
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
            Ok(json!({"summary": if ids.is_some() { "Set the order of menu siblings" } else { "Restore the product order of menu siblings" }, "plan":plan_json(&plan)}).into())
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
            Ok(data(plan_json(&plan)))
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
    json!({"type":"object","required":["parent","item_ids","version"],"properties":{"parent":{"type":"string","description":"Parent ID; empty string means the root."},"item_ids":{"type":"array","items":{"type":"string"},"description":"Sibling IDs in desired order. Unlisted items follow in product order."},"version":{"type":"string","description":"Identity:version from list."}}})
}
fn restore_schema() -> Value {
    json!({"type":"object","required":["parent","version"],"properties":{"parent":{"type":"string"},"version":{"type":"string"}}})
}
fn list_output() -> Value {
    json!({"type":"array","description":"Sibling groups or the reader’s visible menu tree."})
}
fn output() -> Value {
    json!({"type":"object","description":"The before and after order."})
}
inventory::submit! { Contribution::new(DEFINITIONS, "base/menu_order.agent", Provenance { module:"base/menu_order", source:file!(), line:line!() }, &Definitions {
    operations: &[
        Operation { key:"base.menu_order.list", title:"List menu sibling groups", summary:"Read product and saved order with version tokens.", keywords:&["menu","order"], capability:VIEW, input:empty_schema, output:list_output, handler:Handler::Read(&Read::List), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.menu_order.preview", title:"Preview my menu", summary:"Preview the menu visible to the current reader in the tenant's order.", keywords:&["menu","preview"], capability:VIEW, input:preview_schema, output:list_output, handler:Handler::Read(&Read::Preview), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.menu_order.set", title:"Order menu siblings", summary:"Set a tenant's order for siblings by stable menu item ID.", keywords:&["menu","order"], capability:MANAGE, input:set_schema, output, handler:Handler::SafeWrite(&Write::Set), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.menu_order.restore", title:"Restore product menu order", summary:"Remove one sibling group's override.", keywords:&["menu","restore"], capability:MANAGE, input:restore_schema, output, handler:Handler::SafeWrite(&Write::Restore), channels:Channels::Every, step_up:&[] },
    ],
    guides:&[Guide { key:"base.menu_order", title:"Menu order", summary:"Arrange a tenant's menu siblings and restore product order.", keywords:&["menu","order","navigation"], capability:VIEW, body:"Use base.menu_order.list to find sibling IDs, the parent ID and version token. The empty parent is the staff menu root. Set an ordered list of sibling IDs with base.menu_order.set; unlisted and newly contributed siblings follow in product order, and removed items disappear. Use base.menu_order.preview to see the current reader's permitted menu; optionally pass parent and item_ids to preview a proposed order without saving. It never grants access. Use base.menu_order.restore with the parent and current version to return that group to product order. Each write requires a dry run, a complete preview and explicit approval before committing the unchanged operation." }],
}) }
