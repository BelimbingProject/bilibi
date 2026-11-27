use bilibi_agent_api::OperationClient;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_menu::{Menu, Node};
use bilibi_tenancy::{ReadClient, Scope};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Orders, keyed by parent ID; the empty string names the staff root.
#[derive(Clone, Debug, Default)]
pub struct Orders(BTreeMap<String, Vec<String>>);
impl Orders {
    /// Reorder a permission-filtered tree, retaining product order for new items.
    pub fn apply(&self, nodes: &mut [Node]) {
        self.apply_children(nodes, "");
    }
    pub(crate) fn propose(&mut self, parent: &str, items: Vec<String>) {
        self.0.insert(parent.to_owned(), items);
    }
    fn apply_children(&self, nodes: &mut [Node], parent: &str) {
        if let Some(order) = self.0.get(parent) {
            order_siblings(nodes, order, |node| node.item.item.id);
        }
        for node in nodes {
            self.apply_children(&mut node.children, node.item.item.id);
        }
    }
}

fn order_siblings<T>(items: &mut [T], order: &[String], id: impl Fn(&T) -> &'static str) {
    let positions: BTreeMap<_, _> = order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    items.sort_by_key(|item| positions.get(id(item)).copied().unwrap_or(usize::MAX));
}

#[derive(Clone, Debug)]
pub(crate) struct Stored {
    id: i64,
    version: i64,
    items: Vec<String>,
}
impl Stored {
    pub(crate) fn order_ids(&self, ids: &mut [&'static str]) {
        order_siblings(ids, &self.items, |id| id);
    }

    pub(crate) fn visible_items(&self, siblings: &BTreeSet<&str>) -> Vec<&str> {
        visible_items(&self.items, siblings)
    }
}

fn visible_items<'a>(items: &'a [String], siblings: &BTreeSet<&str>) -> Vec<&'a str> {
    items
        .iter()
        .map(String::as_str)
        .filter(|id| siblings.contains(id))
        .collect()
}

pub(crate) fn sibling_ids(nodes: &[Node]) -> BTreeSet<&'static str> {
    nodes.iter().map(|node| node.item.item.id).collect()
}

pub(crate) fn version_token(
    parent: &str,
    siblings: &BTreeSet<&str>,
    saved: Option<&Stored>,
) -> String {
    let (id, version) = saved.map_or((0, 0), |row| (row.id, row.version));
    let mut hash = Sha256::new();
    hash.update(id.to_be_bytes());
    hash.update(version.to_be_bytes());
    for part in std::iter::once(parent).chain(siblings.iter().copied()) {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn sibling_groups(tree: &[Node]) -> BTreeMap<&'static str, &[Node]> {
    fn collect<'a>(
        nodes: &'a [Node],
        parent: &'static str,
        groups: &mut BTreeMap<&'static str, &'a [Node]>,
    ) {
        groups.insert(parent, nodes);
        for node in nodes.iter().filter(|node| !node.children.is_empty()) {
            collect(&node.children, node.item.item.id, groups);
        }
    }
    let mut groups = BTreeMap::new();
    collect(tree, "", &mut groups);
    groups
}

#[derive(Clone, Debug)]
pub(crate) struct Plan {
    parent: String,
    before: Option<Stored>,
    after: Option<Vec<String>>,
    siblings: BTreeSet<&'static str>,
}
impl Plan {
    pub(crate) fn propose(&self, orders: &mut Orders) {
        orders.propose(&self.parent, self.after.clone().unwrap_or_default());
    }

    pub(crate) fn review(&self) -> Value {
        json!({
            "parent": self.parent,
            "before": self.before.as_ref().map(|saved| saved.visible_items(&self.siblings)),
            "after": self.after.as_ref().map(|ids| visible_items(ids, &self.siblings)),
        })
    }
}

pub struct MenuOrder {
    menu: Menu,
    pub(crate) labels: std::sync::Arc<bilibi_labels::Labels>,
}
impl MenuOrder {
    pub(crate) fn new(menu: Menu, labels: std::sync::Arc<bilibi_labels::Labels>) -> Self {
        Self { menu, labels }
    }

    /// Read once per render, from the reader's tenant. This grants no visibility.
    pub async fn orders(&self, client: &dyn ReadClient, scope: &Scope) -> Result<Orders> {
        let rows = client
            .query(
                "SELECT parent,item_ids FROM base_menu_order WHERE tenant_id=$1",
                &[&scope.tenant()],
            )
            .await?;
        Ok(Orders(
            rows.iter().map(|row| (row.get(0), row.get(1))).collect(),
        ))
    }
    pub(crate) async fn stored(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
    ) -> Result<BTreeMap<String, Stored>> {
        let rows = client
            .query(
                "SELECT parent,id,version,item_ids FROM base_menu_order WHERE tenant_id=$1",
                &[&scope.tenant()],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    row.get(0),
                    Stored {
                        id: row.get(1),
                        version: row.get(2),
                        items: row.get(3),
                    },
                )
            })
            .collect())
    }
    pub(crate) async fn visible_tree(
        &self,
        client: &dyn OperationClient,
        scope: &Scope,
    ) -> Result<Vec<Node>> {
        let sql = client.sql_client().await?;
        let capabilities = bilibi_authz::effective_capabilities(sql, scope).await?;
        let owner = match bilibi_tenancy::prove_owner(sql, scope).await {
            Ok(_) => true,
            Err(Error::Forbidden) => false,
            Err(error) => return Err(error),
        };
        Ok(self.menu.visible_staff_tree(&capabilities, owner))
    }
    pub(crate) fn validate(
        tree: &[Node],
        parent: &str,
        items: &[String],
    ) -> Result<BTreeSet<&'static str>> {
        let groups = sibling_groups(tree);
        let nodes = groups
            .get(parent)
            .ok_or_else(|| Error::Refused("Unknown staff menu parent.".into()))?;
        let siblings = sibling_ids(nodes);
        let mut seen = BTreeSet::new();
        if items
            .iter()
            .any(|id| !siblings.contains(id.as_str()) || !seen.insert(id))
        {
            return Err(Error::Refused(
                "Item IDs must be unique visible children of this parent.".into(),
            ));
        }
        Ok(siblings)
    }
    pub(crate) async fn plan(
        &self,
        sql: &dyn OperationClient,
        scope: &Scope,
        parent: &str,
        items: Option<&[String]>,
        version: &str,
        lock: bool,
    ) -> Result<(Plan, Vec<Node>)> {
        bilibi_authz::authorize_proof(sql, scope, crate::MANAGE).await?;
        let tree = self.visible_tree(sql, scope).await?;
        let siblings = Self::validate(&tree, parent, items.unwrap_or_default())?;
        if items.is_some_and(|items| items.is_empty()) {
            return Err(Error::Refused(
                "Provide at least one item, or restore product order.".into(),
            ));
        }
        let query = if lock {
            "SELECT id,version,item_ids FROM base_menu_order WHERE tenant_id=$1 AND parent=$2 FOR UPDATE"
        } else {
            "SELECT id,version,item_ids FROM base_menu_order WHERE tenant_id=$1 AND parent=$2"
        };
        let before = sql
            .query_opt(query, &[&scope.tenant(), &parent])
            .await?
            .map(|row| Stored {
                id: row.get(0),
                version: row.get(1),
                items: row.get(2),
            });
        if version_token(parent, &siblings, before.as_ref()) != version {
            return Err(Error::Conflict);
        }
        let mut proposed = items.unwrap_or_default().iter().cloned();
        let mut after = Vec::new();
        if let Some(saved) = &before {
            for id in &saved.items {
                if siblings.contains(id.as_str()) {
                    after.extend(proposed.next());
                } else if id != bilibi_menu::PORTAL_ROOT
                    && self
                        .menu
                        .fetch(id)
                        .is_some_and(|item| item.item.parent.unwrap_or("") == parent)
                {
                    after.push(id.clone());
                }
            }
        }
        after.extend(proposed);
        Ok((
            Plan {
                parent: parent.to_owned(),
                before,
                after: (!after.is_empty()).then_some(after),
                siblings,
            },
            tree,
        ))
    }
    pub(crate) async fn apply(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        parent: &str,
        items: Option<&[String]>,
        version: &str,
    ) -> Result<Plan> {
        let (plan, _) = self
            .plan(tx.sql(), scope, parent, items, version, true)
            .await?;
        if plan.before.as_ref().map(|s| &s.items) == plan.after.as_ref() {
            return Ok(plan);
        }
        let tenant = scope.tenant();
        let parent = plan.parent.clone();
        let before = plan.before.clone();
        let after = plan.after.clone();
        tx.mutate(move |sql| Box::pin(async move {
            let id: i64 = match &after {
                Some(items) if before.is_some() => sql.query_one("UPDATE base_menu_order SET item_ids=$3,version=version+1 WHERE tenant_id=$1 AND parent=$2 RETURNING id", &[&tenant,&parent,items]).await?.get(0),
                Some(items) => sql.query_opt("INSERT INTO base_menu_order(tenant_id,parent,item_ids) VALUES($1,$2,$3) ON CONFLICT (tenant_id,parent) DO NOTHING RETURNING id", &[&tenant,&parent,items]).await?.ok_or(Error::Conflict)?.get(0),
                None => sql.query_one("DELETE FROM base_menu_order WHERE tenant_id=$1 AND parent=$2 RETURNING id", &[&tenant,&parent]).await?.get(0),
            };
            let old = before.map_or(Value::Null, |s| json!({"parent":parent,"item_ids":s.items}));
            let new = after.map_or(Value::Null, |items| json!({"parent":parent,"item_ids":items}));
            Ok(((), vec![Change { kind: "menu_order".into(), id, old, new }]))
        })).await?;
        Ok(plan)
    }
}
