//! Watcher configuration only, with no runtime evidence or approval state.
use crate::{
    definition::{examples_json, Problems},
    review::Draft,
    Watchers, MANAGE, VIEW,
};
use bilibi_database::{Error, MutationTransaction, Result};
use bilibi_grid::{Catalog, CustomTable};
use bilibi_tenancy::Scope;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_postgres::{Row, Transaction};

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WatcherSetup {
    pub name: String,
    pub watch: String,
    pub definition: Value,
    pub examples: Value,
}

pub enum WatcherTransfer {
    Same,
    Add,
    Collision,
    Missing(Vec<String>),
    Invalid(Vec<(String, String)>),
}

const LATEST: &str = "SELECT w.name,w.trigger,v.definition,v.examples,v.approved_at IS NOT NULL
    FROM base_watchers w JOIN LATERAL (
        SELECT definition,examples,approved_at FROM base_watcher_versions
        WHERE tenant_id=w.tenant_id AND watcher_id=w.id ORDER BY number DESC LIMIT 1
    ) v ON true WHERE w.tenant_id=$1";

fn setup(row: &Row) -> WatcherSetup {
    WatcherSetup {
        name: row.get(0),
        watch: row.get(1),
        definition: row.get(2),
        examples: row.get(3),
    }
}

impl WatcherSetup {
    fn parse(&self) -> (Draft, Problems) {
        let mut input = self.definition.clone();
        if !input.is_object() {
            return (
                Draft::parse(&Value::Null, &mut Vec::new()),
                vec![("definition".into(), "Supply a watcher definition.".into())],
            );
        }
        input["name"] = json!(self.name);
        input["watch"] = json!(self.watch);
        input["examples"] = self.examples.clone();
        let mut problems = Vec::new();
        let mut draft = Draft::parse(&input, &mut problems);
        draft.watcher_id = None;
        draft.explanation = None;
        if draft.name != self.name
            || draft.trigger != self.watch
            || draft
                .definition
                .as_ref()
                .is_some_and(|definition| definition.to_json() != self.definition)
            || examples_json(&draft.examples) != self.examples
        {
            problems.push((
                "definition".into(),
                "Supply a canonical watcher definition and examples.".into(),
            ));
        }
        (draft, problems)
    }
}

impl Watchers {
    pub async fn export_draft(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        id: i64,
    ) -> Result<WatcherSetup> {
        bilibi_authz::authorize_key(tx, scope, VIEW).await?;
        let row = tx
            .query_opt(&format!("{LATEST} AND w.id=$2"), &[&scope.tenant(), &id])
            .await?
            .ok_or(Error::NotFound)?;
        if row.get::<_, bool>(4) {
            return Err(Error::Refused(
                "Select a watcher whose latest version is a draft.".into(),
            ));
        }
        let setup = setup(&row);
        let (draft, problems) = setup.parse();
        let catalog = Catalog::for_scope(tx, scope).await.map_err(Error::from)?;
        let review = self
            .check_transfer(tx, scope, &catalog, &draft, problems)
            .await?;
        if !review.is_empty() {
            return Err(Error::Refused(
                "The draft contains unavailable, restricted or nonportable inputs.".into(),
            ));
        }
        Ok(setup)
    }

    pub async fn compare_draft(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        incoming: &WatcherSetup,
        fields: &[CustomTable],
        lock: bool,
    ) -> Result<WatcherTransfer> {
        bilibi_authz::authorize_key(tx, scope, MANAGE).await?;
        let (draft, problems) = incoming.parse();
        let Some(definition) = &draft.definition else {
            return Ok(WatcherTransfer::Invalid(problems));
        };
        let mut missing = Vec::new();
        if self.trigger(&draft.trigger).is_none() {
            missing.push(format!("Trigger “{}” is unavailable.", draft.trigger));
        }
        for action in &definition.actions {
            if self.action(&action.operation).is_none() {
                missing.push(format!("Action “{}” is unavailable.", action.operation));
            }
        }
        if !missing.is_empty() {
            return Ok(WatcherTransfer::Missing(missing));
        }
        let catalog = Catalog::for_setup_preview(tx, scope, fields)
            .await
            .map_err(Error::from)?;
        let problems = self
            .check_transfer(tx, scope, &catalog, &draft, problems)
            .await?;
        if !problems.is_empty() {
            return Ok(WatcherTransfer::Invalid(problems));
        }
        if lock {
            self.lock_admission(tx, scope).await?;
        }
        let rows = tx
            .query(
                &format!("{LATEST} AND lower(w.name)=lower($2) ORDER BY w.id LIMIT 2"),
                &[&scope.tenant(), &incoming.name],
            )
            .await?;
        Ok(match rows.as_slice() {
            [] => WatcherTransfer::Add,
            [row]
                if {
                    let mut current = setup(row);
                    current.name.clone_from(&incoming.name);
                    &current == incoming
                } =>
            {
                WatcherTransfer::Same
            }
            _ => WatcherTransfer::Collision,
        })
    }

    async fn check_transfer(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        catalog: &Catalog,
        draft: &Draft,
        mut problems: Problems,
    ) -> Result<Problems> {
        let trigger = self.trigger(&draft.trigger).ok_or(Error::Invalid)?;
        let definition = draft.definition.as_ref().ok_or(Error::Invalid)?;
        // The stored rule literals and authored examples are configuration.
        // A record reference or an action parameter declared secret is not portable.
        for condition in &definition.conditions {
            if catalog
                .table(trigger.definition.table)
                .and_then(|table| catalog.resolve(table, &condition.field).ok())
                .is_some_and(|column| {
                    catalog.table(column.table).is_some_and(|table| {
                        column.field.is_some_and(|field| {
                            table.custom_reference(field.id).is_some()
                                || field.id == table.key()
                                || record_id(field.id)
                                || field.personal.is_some()
                                || table.record_fields().iter().any(|record| {
                                    record.id == field.id && record.personal.is_some()
                                })
                        })
                    })
                })
            {
                problems.push((
                    "if".into(),
                    "A condition on a record identifier or personal field cannot travel.".into(),
                ));
            }
        }
        for action in &definition.actions {
            if let Some(offered) = self.action(&action.operation) {
                for (key, value) in &action.input {
                    if !portable_input(&offered.input["properties"][key], key, value) {
                        problems.push((
                            "do".into(),
                            "Use only declared action parameters that do not identify a record or hold a secret.".into(),
                        ));
                    }
                }
            }
        }
        Ok(self
            .review_new_draft(tx, scope, draft, problems, catalog, trigger)
            .await?
            .problems)
    }

    pub async fn import_draft(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        incoming: &WatcherSetup,
    ) -> Result<()> {
        let (draft, problems) = incoming.parse();
        self.draft(tx, scope, draft, problems).await?;
        Ok(())
    }
}

fn record_id(key: &str) -> bool {
    key == "id" || key.ends_with("_id")
}

fn portable_input(schema: &Value, key: &str, value: &Value) -> bool {
    schema.is_object()
        && schema["writeOnly"] != true
        && !record_id(key)
        && value.as_object().is_none_or(|object| {
            object
                .iter()
                .all(|(key, value)| portable_input(&schema["properties"][key], key, value))
        })
        && value.as_array().is_none_or(|items| {
            items
                .iter()
                .all(|value| portable_input(&schema["items"], key, value))
        })
}
