//! Watcher configuration only, with no runtime evidence or approval state.
use crate::{
    definition::{examples_json, Problems},
    review::Draft,
    Watchers, MANAGE, VIEW,
};
use bilibi_database::{Error, MutationTransaction, Result};
use bilibi_grid::{Catalog, CustomTable, FieldType};
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
}

pub enum WatcherTransfer {
    Same,
    Add(Vec<String>),
    Collision,
    Missing(Vec<String>),
    Invalid(Vec<(String, String)>, Vec<String>),
}

const LATEST: &str = "FROM base_watchers w JOIN LATERAL (
        SELECT definition,approved_at,examples FROM base_watcher_versions
        WHERE tenant_id=w.tenant_id AND watcher_id=w.id ORDER BY number DESC LIMIT 1
    ) v ON true WHERE w.tenant_id=$1";

fn setup(row: &Row) -> WatcherSetup {
    WatcherSetup {
        name: row.get(0),
        watch: row.get(1),
        definition: row.get(2),
    }
}

impl WatcherSetup {
    fn parse(&self, examples: &Value) -> (Draft, Problems) {
        let mut input = self.definition.clone();
        if !input.is_object() {
            return (
                Draft::parse(&Value::Null, &mut Vec::new()),
                vec![("definition".into(), "Supply a watcher definition.".into())],
            );
        }
        input["name"] = json!(self.name);
        input["watch"] = json!(self.watch);
        input["examples"] = examples.clone();
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
            || (!examples.is_null() && examples_json(&draft.examples) != *examples)
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
            .query_opt(&format!("SELECT w.name,w.trigger,v.definition,v.approved_at IS NOT NULL,v.examples {LATEST} AND w.id=$2"), &[&scope.tenant(), &id])
            .await?
            .ok_or(Error::NotFound)?;
        if row.get::<_, bool>(3) {
            return Err(Error::Refused(
                "Select a watcher whose latest version is a draft.".into(),
            ));
        }
        let setup = setup(&row);
        let (draft, problems) = setup.parse(&row.get(4));
        let catalog = Catalog::for_scope(tx, scope).await.map_err(Error::from)?;
        let (review, _) = self
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
        examples: &Value,
        fields: &[CustomTable],
        lock: bool,
    ) -> Result<WatcherTransfer> {
        bilibi_authz::authorize_key(tx, scope, MANAGE).await?;
        bilibi_authz::authorize_key(tx, scope, VIEW).await?;
        let (draft, problems) = incoming.parse(&Value::Null);
        let Some(definition) = &draft.definition else {
            return Ok(WatcherTransfer::Invalid(problems, Vec::new()));
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
        if !problems.is_empty() {
            return Ok(WatcherTransfer::Invalid(problems, Vec::new()));
        }
        if lock {
            self.lock_admission(tx, scope).await?;
        }
        let rows = tx
            .query(
                &format!("SELECT w.name,w.trigger,v.definition {LATEST} AND w.name=$2 ORDER BY w.id LIMIT 2"),
                &[&scope.tenant(), &incoming.name],
            )
            .await?;
        match rows.as_slice() {
            [] => {}
            [row] if &setup(row) == incoming => return Ok(WatcherTransfer::Same),
            _ => return Ok(WatcherTransfer::Collision),
        }
        let (draft, problems) = incoming.parse(examples);
        let catalog = Catalog::for_setup_preview(tx, scope, fields)
            .await
            .map_err(Error::from)?;
        let (problems, warnings) = self
            .check_transfer(tx, scope, &catalog, &draft, problems)
            .await?;
        Ok(if problems.is_empty() {
            WatcherTransfer::Add(warnings)
        } else {
            WatcherTransfer::Invalid(problems, warnings)
        })
    }

    async fn check_transfer(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        catalog: &Catalog,
        draft: &Draft,
        mut problems: Problems,
    ) -> Result<(Problems, Vec<String>)> {
        let trigger = self.trigger(&draft.trigger).ok_or(Error::Invalid)?;
        let definition = draft.definition.as_ref().ok_or(Error::Invalid)?;
        let mut warnings = Vec::new();
        for condition in &definition.conditions {
            let Some(column) = catalog
                .table(trigger.definition.table)
                .and_then(|table| catalog.resolve(table, &condition.field).ok())
            else {
                continue;
            };
            if catalog.table(column.table).is_some_and(|table| {
                column.field.is_some_and(|field| {
                    table.custom_reference(field.id).is_some()
                        || field.id == table.key()
                        || record_id(field.id)
                        || field.personal.is_some()
                        || table
                            .record_fields()
                            .iter()
                            .any(|record| record.id == field.id && record.personal.is_some())
                })
            }) {
                problems.push((
                    "if".into(),
                    "A condition on a record identifier or personal field cannot travel.".into(),
                ));
            }
            if column.ty == FieldType::String && !condition.value.is_null() {
                warnings.push(format!(
                    "Condition on {} contains free text. Review the literal and remove the condition if it contains source record data.",
                    condition.field
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
                    } else if contains_free_text(&offered.input["properties"][key], value) {
                        warnings.push(format!(
                            "Action {} parameter {key} contains free text. Review it for source record data.",
                            action.operation
                        ));
                    }
                }
            }
        }
        let review = self
            .review_new_draft(tx, scope, draft, problems, catalog, trigger)
            .await?;
        Ok((review.problems, warnings))
    }

    pub async fn import_draft(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        incoming: &WatcherSetup,
        examples: &Value,
    ) -> Result<()> {
        let (draft, problems) = incoming.parse(examples);
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

fn contains_free_text(schema: &Value, value: &Value) -> bool {
    match value {
        Value::String(_) => schema.get("enum").is_none() && schema.get("const").is_none(),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| contains_free_text(&schema["properties"][key], value)),
        Value::Array(items) => items
            .iter()
            .any(|value| contains_free_text(&schema["items"], value)),
        _ => false,
    }
}
