use crate::catalog::OfferedTrigger;
use crate::definition::{ActionCall, Definition, MAX_ACTIONS, MAX_EXAMPLES, MAX_FACTS};
use bilibi_database::Result;
use bilibi_tenancy::Scope;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::future::Future;
use tokio::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Language {
    #[default]
    JavaScript,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Script {
    pub name: String,
    pub source: String,
    pub fields: Vec<String>,
    pub calls: Vec<ActionCall>,
    pub examples: Vec<ScriptExample>,
    #[serde(default)]
    language: Language,
    #[serde(default)]
    source_digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScriptExample {
    pub facts: Map<String, Value>,
    pub expect: Vec<ActionCall>,
}

impl Script {
    fn digest(&self) -> String {
        Sha256::digest(self.source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub(crate) fn pin_source(&mut self) {
        self.source_digest = self.digest();
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "Run {} as {} (JavaScript, SHA-256 {}). {}",
            self.name,
            crate::PRINCIPAL,
            self.source_digest,
            self.source
        )
    }

    pub(crate) fn validate(&self) -> bool {
        !self.name.trim().is_empty()
            && self.name.len() <= 120
            && !self.source.trim().is_empty()
            && self.source.len() <= 16 * 1024
            && (self.source_digest.is_empty() || self.source_digest == self.digest())
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

    pub(crate) async fn capture(
        &self,
        facts: &Map<String, Value>,
        milliseconds: u64,
        heap_mib: u64,
    ) -> std::result::Result<Vec<ActionCall>, &'static str> {
        let input = self
            .fields
            .iter()
            .map(|field| {
                (
                    field.clone(),
                    facts.get(field).cloned().unwrap_or(Value::Null),
                )
            })
            .collect();
        let answer = match self.language {
            Language::JavaScript => {
                crate::sandbox::evaluate(&self.source, Value::Object(input), milliseconds, heap_mib)
                    .await?
            }
        };
        serde_json::from_value(answer).map_err(|_| "script_output")
    }

    pub(crate) fn permits(&self, calls: &[ActionCall]) -> bool {
        calls.len() <= MAX_ACTIONS && calls.iter().all(|call| self.calls.contains(call))
    }
}

pub(crate) async fn before_deadline<T>(
    deadline: Option<Instant>,
    work: impl Future<Output = T>,
) -> std::result::Result<T, tokio::time::error::Elapsed> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, work).await,
        None => Ok(work.await),
    }
}

pub(crate) async fn cancel_sql(client: &tokio_postgres::Client) {
    let _ = tokio::time::timeout(
        Duration::from_millis(250),
        client.cancel_token().cancel_query(tokio_postgres::NoTls),
    )
    .await;
}

impl crate::Watchers {
    pub(crate) async fn script_deadline(
        &self,
        sql: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        definition: &Definition,
    ) -> Result<Option<Instant>> {
        if definition.scripts().next().is_none() {
            return Ok(None);
        }
        let start = Instant::now();
        let ms = self.limit(sql, scope, crate::SCRIPT_TIME).await? as u64;
        Ok(Some(start + Duration::from_millis(ms)))
    }

    pub(crate) fn local_script_plan(&self, definition: &Definition) -> bool {
        definition.script_calls().all(|call| {
            self.action(&call.operation)
                .is_some_and(|action| action.local_script)
        })
    }

    pub(crate) async fn plan_steps(
        &self,
        sql: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        definition: &Definition,
        record: (&OfferedTrigger, &str, i64),
        deadline: Option<Instant>,
    ) -> Result<std::result::Result<Vec<ActionCall>, &'static str>> {
        use crate::definition::Step;
        let Some(deadline) = deadline else {
            return Ok(Ok(definition.actions().cloned().collect()));
        };
        if scope.principal() != Some(crate::PRINCIPAL)
            || !bilibi_authz::decide(sql, scope, crate::RUN_SCRIPT)
                .await?
                .allowed
        {
            return Ok(Err("script_run_refused"));
        }
        if !self.local_script_plan(definition) {
            return Ok(Err("script_call_refused"));
        }
        let (heap, limit) = self.script_limits(sql, scope).await?;
        let (trigger, subject, revision) = record;
        let fields = definition.script_fact_fields(trigger.definition.version);
        let facts = match self
            .read_facts(sql, scope, trigger.definition.table, subject, &fields)
            .await?
        {
            Ok((facts, _)) => facts,
            Err(reason) => return Ok(Err(reason)),
        };
        if crate::run::record_revision(facts.get(trigger.definition.version))? != revision {
            return Ok(Err("record_changed"));
        }
        let mut selected = Vec::new();
        for step in &definition.steps {
            match step {
                Step::Operation(call) => selected.push(call.clone()),
                Step::Script { script } => {
                    if !bilibi_authz::decide(sql, scope, crate::RUN_SCRIPT)
                        .await?
                        .allowed
                    {
                        return Ok(Err("script_run_refused"));
                    }
                    let time = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis() as u64;
                    if time == 0 {
                        return Ok(Err("script_timeout"));
                    }
                    match script.capture(&facts, time, heap).await {
                        Ok(calls) if script.permits(&calls) => selected.extend(calls),
                        Ok(_) => return Ok(Err("script_call_refused")),
                        Err(code) => return Ok(Err(code)),
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
        scope: &Scope,
    ) -> Result<(u64, usize)> {
        Ok((
            self.limit(sql, scope, crate::SCRIPT_HEAP).await? as u64,
            self.limit(sql, scope, crate::ACTIONS_PER_RUN).await? as usize,
        ))
    }
}
