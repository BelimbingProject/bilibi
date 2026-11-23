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

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WatcherSetup {
    pub name: String,
    pub watch: String,
    pub definition: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

pub enum WatcherTransfer {
    Same,
    Add(Vec<String>),
    Collision(Vec<String>),
    Missing(Vec<String>, Vec<String>),
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
        warnings: Vec::new(),
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
        let mut setup = setup(&row);
        let (draft, problems) = setup.parse(&row.get(4));
        let catalog = Catalog::for_scope(tx, scope).await.map_err(Error::from)?;
        let mut problems = problems;
        let warnings = self.check_literals(&catalog, &draft, &mut problems);
        let trigger = self.trigger(&draft.trigger).ok_or(Error::Invalid)?;
        let review = self
            .review_new_draft(tx, scope, &draft, problems, &catalog, trigger)
            .await?;
        if !review.ok() {
            return Err(Error::Refused(
                "The draft contains unavailable, restricted or nonportable inputs.".into(),
            ));
        }
        setup.warnings = warnings;
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
        let (draft, mut problems) = incoming.parse(&Value::Null);
        let Some(definition) = &draft.definition else {
            return Ok(WatcherTransfer::Invalid(problems, Vec::new()));
        };
        let mut missing = Vec::new();
        if self.trigger(&draft.trigger).is_none() {
            missing.push(format!("Trigger “{}” is unavailable.", draft.trigger));
        }
        for action in definition.actions() {
            if self.action(&action.operation).is_none() {
                missing.push(format!("Action “{}” is unavailable.", action.operation));
            }
        }
        let catalog = Catalog::for_setup_preview(tx, scope, fields)
            .await
            .map_err(Error::from)?;
        let warnings = self.check_literals(&catalog, &draft, &mut problems);
        if !missing.is_empty() {
            return Ok(WatcherTransfer::Missing(missing, warnings));
        }
        if !problems.is_empty() {
            return Ok(WatcherTransfer::Invalid(problems, warnings));
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
            [row]
                if {
                    let current = setup(row);
                    current.watch == incoming.watch && current.definition == incoming.definition
                } =>
            {
                return Ok(WatcherTransfer::Same)
            }
            _ => return Ok(WatcherTransfer::Collision(warnings)),
        }
        let (draft, problems) = incoming.parse(examples);
        let trigger = self.trigger(&draft.trigger).ok_or(Error::Invalid)?;
        let review = self
            .review_new_draft(tx, scope, &draft, problems, &catalog, trigger)
            .await?;
        Ok(if review.ok() {
            WatcherTransfer::Add(warnings)
        } else {
            WatcherTransfer::Invalid(review.problems, warnings)
        })
    }

    fn check_literals(
        &self,
        catalog: &Catalog,
        draft: &Draft,
        problems: &mut Problems,
    ) -> Vec<String> {
        let Some(definition) = &draft.definition else {
            return Vec::new();
        };
        let table = self
            .trigger(&draft.trigger)
            .and_then(|trigger| catalog.table(trigger.definition.table));
        let mut warnings = Vec::new();
        for (index, condition) in definition.conditions.iter().enumerate() {
            let column = table.and_then(|table| catalog.resolve(table, &condition.field).ok());
            if column.as_ref().is_some_and(|column| {
                catalog.table(column.table).is_some_and(|table| {
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
                })
            }) {
                problems.push((
                    "if".into(),
                    "A condition on a record identifier or personal field cannot travel.".into(),
                ));
            }
            if column
                .as_ref()
                .is_none_or(|column| column.ty == FieldType::String)
            {
                let values = condition
                    .value
                    .as_array()
                    .map_or_else(|| std::slice::from_ref(&condition.value), Vec::as_slice);
                for (item, value) in values.iter().enumerate() {
                    if value.is_string() {
                        let path = format!("/if/conditions/{index}/value");
                        let path = if condition.value.is_array() {
                            format!("{path}/{item}")
                        } else {
                            path
                        };
                        warnings.push(format!(
                            "Condition on {} contains free text at {path}. Review the literal for source record data.",
                            condition.field
                        ));
                    }
                }
            }
        }
        for (index, action) in definition.actions().enumerate() {
            let offered = self.action(&action.operation);
            for (key, value) in &action.input {
                let schema =
                    offered.map_or(&Value::Null, |action| &action.input["properties"][key]);
                if offered.is_some() && !portable_input(schema, key, value) {
                    problems.push((
                        "do".into(),
                        "Use only declared action parameters that do not identify a record or hold a secret.".into(),
                    ));
                } else {
                    warn_action_text(
                        schema,
                        value,
                        &format!("/do/{index}/input/{}", pointer_key(key)),
                        &mut warnings,
                    );
                }
            }
        }
        warnings
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

fn pointer_key(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn warn_action_text(schema: &Value, value: &Value, path: &str, warnings: &mut Vec<String>) {
    match value {
        Value::String(_) if schema.get("enum").is_none() => {
            warnings.push(format!("Action parameter at {path} contains free text. Review the literal for source record data."));
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                warn_action_text(
                    &schema["properties"][key],
                    value,
                    &format!("{path}/{}", pointer_key(key)),
                    warnings,
                );
            }
        }
        Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                warn_action_text(
                    &schema["items"],
                    value,
                    &format!("{path}/{index}"),
                    warnings,
                );
            }
        }
        _ => {}
    }
}
