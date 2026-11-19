use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_menu::{Menu, Node};
use bilibi_tenancy::{ProofClient, ReadClient, Scope};
use serde_json::{json, Value};
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
            let positions: BTreeMap<_, _> = order
                .iter()
                .enumerate()
                .map(|(i, id)| (id.as_str(), i))
                .collect();
            nodes.sort_by_key(|node| {
                positions
                    .get(node.item.item.id)
                    .copied()
                    .unwrap_or(usize::MAX)
            });
        }
        for node in nodes {
            self.apply_children(&mut node.children, node.item.item.id);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub id: i64,
    pub version: i64,
    pub items: Vec<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub parent: String,
    pub before: Option<Stored>,
    pub after: Option<Vec<String>>,
}

pub struct MenuOrder {
    pub(crate) menu: Menu,
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
    pub(crate) fn validate(&self, parent: &str, items: &[String]) -> Result<()> {
        if !parent.is_empty() && (self.menu.fetch(parent).is_none() || self.menu.in_portal(parent))
        {
            return Err(Error::Refused("Unknown staff menu parent.".into()));
        }
        let known: BTreeMap<_, _> = self
            .menu
            .items()
            .iter()
            .map(|item| (item.item.id, item.item))
            .collect();
        let mut seen = BTreeSet::new();
        for id in items {
            let item = known
                .get(id.as_str())
                .ok_or_else(|| Error::Refused(format!("Unknown menu item {id}.")))?;
            if id.as_str() == bilibi_menu::PORTAL_ROOT
                || item.parent.unwrap_or("") != parent
                || !seen.insert(id)
            {
                return Err(Error::Refused(format!(
                    "Menu item {id} is repeated or is not a child of this parent."
                )));
            }
        }
        Ok(())
    }
    pub(crate) async fn plan(
        &self,
        sql: &dyn ProofClient,
        scope: &Scope,
        parent: &str,
        items: Option<&[String]>,
        version: (i64, i64),
        lock: bool,
    ) -> Result<Plan> {
        bilibi_authz::authorize_proof(sql, scope, crate::MANAGE).await?;
        if let Some(items) = items {
            self.validate(parent, items)?;
        }
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
        if before.as_ref().map_or((0, 0), |s| (s.id, s.version)) != version {
            return Err(Error::Conflict);
        }
        Ok(Plan {
            parent: parent.to_owned(),
            before,
            after: items.map(<[String]>::to_vec),
        })
    }
    pub(crate) async fn apply(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        parent: &str,
        items: Option<&[String]>,
        version: (i64, i64),
    ) -> Result<Plan> {
        let plan = self
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
