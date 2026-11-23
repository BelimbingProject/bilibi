//! A watcher version owns the source, its declared reads and the complete set
//! of calls its script may request. The host binds the subject of every call.
use crate::definition::{ActionCall, MAX_ACTIONS, MAX_EXAMPLES, MAX_FACTS};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Script {
    pub name: String,
    pub source: String,
    pub fields: Vec<String>,
    pub calls: Vec<ActionCall>,
    pub examples: Vec<ScriptExample>,
    /// Explicit run permission for this script version.
    pub runner: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScriptExample {
    pub facts: Map<String, Value>,
    pub expect: Vec<ActionCall>,
}

impl Script {
    pub(crate) fn validate(&self) -> bool {
        !self.name.trim().is_empty()
            && self.name.len() <= 120
            && !self.source.trim().is_empty()
            && self.source.len() <= 16 * 1024
            && self.runner == crate::PRINCIPAL
            && self.fields.len() <= MAX_FACTS
            && self
                .fields
                .iter()
                .all(|field| bilibi_grid::column::parse(field).is_ok())
            && (1..=MAX_ACTIONS).contains(&self.calls.len())
            && self.calls.iter().all(|call| !call.operation.is_empty())
            && (1..=MAX_EXAMPLES).contains(&self.examples.len())
            && self.examples.iter().all(|example| {
                example.expect.len() <= MAX_ACTIONS
                    && example
                        .facts
                        .keys()
                        .all(|field| self.fields.contains(field))
            })
    }

    pub(crate) async fn plan(
        &self,
        facts: &Map<String, Value>,
        milliseconds: u64,
        heap_mib: u64,
    ) -> Result<Vec<ActionCall>, &'static str> {
        let input = self
            .fields
            .iter()
            .filter_map(|field| facts.get(field).map(|value| (field.clone(), value.clone())))
            .collect();
        let answer =
            crate::sandbox::evaluate(&self.source, Value::Object(input), milliseconds, heap_mib)
                .await?;
        let calls: Vec<ActionCall> = serde_json::from_value(answer).map_err(|_| "script_output")?;
        if calls.len() > MAX_ACTIONS || calls.iter().any(|call| !self.calls.contains(call)) {
            return Err("script_call_refused");
        }
        Ok(calls)
    }
}

impl crate::Watchers {
    pub(crate) async fn plan_steps(
        &self,
        sql: &(impl tokio_postgres::GenericClient + Sync),
        scope: &bilibi_tenancy::Scope,
        definition: &crate::definition::Definition,
        facts: &Map<String, Value>,
    ) -> bilibi_database::Result<Result<Vec<ActionCall>, &'static str>> {
        use crate::definition::Step;
        if definition.scripts().next().is_none() {
            return Ok(Ok(definition.actions().cloned().collect()));
        }
        if scope.principal() != Some(crate::PRINCIPAL)
            || !bilibi_authz::decide(sql, scope, crate::RUN_SCRIPT)
                .await?
                .allowed
        {
            return Ok(Err("script_run_refused"));
        }
        let (time, heap, limit) = self.script_limits(sql, scope).await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(time);
        let mut selected = Vec::new();
        for step in &definition.steps {
            match step {
                Step::Operation(call) => selected.push(call.clone()),
                Step::Script { script } => {
                    if scope.principal() != Some(script.runner.as_str()) {
                        return Ok(Err("script_run_refused"));
                    }
                    match tokio::time::timeout_at(deadline, script.plan(facts, time, heap)).await {
                        Ok(Ok(calls)) => selected.extend(calls),
                        Ok(Err(code)) => return Ok(Err(code)),
                        Err(_) => return Ok(Err("script_timeout")),
                    }
                }
            }
            if selected.len() > limit {
                return Ok(Err("action_limit"));
            }
        }
        Ok(Ok(selected))
    }

    pub(crate) async fn script_limits(
        &self,
        sql: &(impl tokio_postgres::GenericClient + Sync),
        scope: &bilibi_tenancy::Scope,
    ) -> bilibi_database::Result<(u64, u64, usize)> {
        Ok((
            self.limit(sql, scope, crate::SCRIPT_TIME).await? as u64,
            self.limit(sql, scope, crate::SCRIPT_HEAP).await? as u64,
            self.limit(sql, scope, crate::ACTIONS_PER_RUN).await? as usize,
        ))
    }

    pub(crate) async fn script_examples(
        &self,
        sql: &(impl tokio_postgres::GenericClient + Sync),
        scope: &bilibi_tenancy::Scope,
        definition: &crate::definition::Definition,
        problems: &mut crate::definition::Problems,
    ) -> bilibi_database::Result<()> {
        if definition.scripts().next().is_none() {
            return Ok(());
        }
        let (time, heap, limit) = self.script_limits(sql, scope).await?;
        for script in definition.scripts() {
            for (index, example) in script.examples.iter().enumerate() {
                match script.plan(&example.facts, time, heap).await {
                    Ok(selected) if selected == example.expect && selected.len() <= limit => {}
                    _ => {
                        problems.push(("do".into(), format!("{} example {} failed to produce the expected calls within the limits.", script.name,index+1)));
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }
}
