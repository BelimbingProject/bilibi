use crate::catalog::OfferedTrigger;
use crate::definition::{ActionCall, Definition, MAX_ACTIONS, MAX_EXAMPLES, MAX_FACTS};
use bilibi_database::Result;
use bilibi_tenancy::Scope;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
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

    pub(crate) fn parse(value: &Value, stored: bool) -> Option<Self> {
        if value.get("source_digest").is_some() != stored {
            return None;
        }
        let mut script: Self = serde_json::from_value(value.clone()).ok()?;
        let digest = script.digest();
        if (stored && script.source_digest != digest) || !script.validate() {
            return None;
        }
        script.source_digest = digest;
        Some(script)
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "Run {} as {} (JavaScript, SHA-256 {}). Reads {}. Allowed calls {}. Source: {}",
            self.name,
            crate::PRINCIPAL,
            self.source_digest,
            serde_json::json!(self.fields),
            serde_json::json!(self.calls),
            self.source
        )
    }

    fn validate(&self) -> bool {
        !self.name.trim().is_empty()
            && self.name.len() <= 120
            && !self.source.trim().is_empty()
            && self.source.len() <= 16 * 1024
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

pub(crate) const MAX_SCRIPT_TIME: Duration = Duration::from_millis(1000);

pub(crate) struct ScriptBudget {
    pub deadline: Option<Instant>,
    committing: AtomicBool,
}

impl ScriptBudget {
    pub(crate) fn new(deadline: Option<Instant>) -> Self {
        Self {
            deadline,
            committing: AtomicBool::new(false),
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(bilibi_database::Error::Refused("script_timeout".into()));
        }
        Ok(())
    }

    pub(crate) async fn within<T>(&self, work: impl Future<Output = T>) -> Result<T> {
        let Some(deadline) = self.deadline else {
            return Ok(work.await);
        };
        self.check()?;
        tokio::pin!(work);
        tokio::select! {
            biased;
            result = &mut work => Ok(result),
            _ = tokio::time::sleep_until(deadline) => {
                if self.committing.load(Relaxed) {
                    Ok(work.await)
                } else {
                    Err(bilibi_database::Error::Refused("script_timeout".into()))
                }
            }
        }
    }

    fn begin_commit(&self) -> Result<()> {
        self.check()?;
        self.committing.store(true, Relaxed);
        Ok(())
    }

    pub(crate) async fn commit(&self, tx: bilibi_database::MutationTransaction<'_>) -> Result<()> {
        self.check()?;
        let result = tx.commit_checked(|| self.begin_commit()).await;
        self.committing.store(false, Relaxed);
        result
    }

    pub(crate) async fn commit_trial(&self, tx: tokio_postgres::Transaction<'_>) -> Result<()> {
        self.begin_commit()?;
        let result = tx.commit().await;
        self.committing.store(false, Relaxed);
        Ok(result?)
    }
}

pub(crate) fn timed_out(error: &bilibi_database::Error) -> bool {
    matches!(error, bilibi_database::Error::Refused(reason) if reason == "script_timeout")
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
        scripted: bool,
        start: Instant,
    ) -> Result<ScriptBudget> {
        if !scripted {
            return Ok(ScriptBudget::new(None));
        }
        let ms = ScriptBudget::new(Some(start + MAX_SCRIPT_TIME))
            .within(self.limit(sql, scope, crate::SCRIPT_TIME))
            .await?? as u64;
        Ok(ScriptBudget::new(Some(start + Duration::from_millis(ms))))
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
        let (mut facts, types) = match self
            .read_facts(sql, scope, trigger.definition.table, subject, &fields)
            .await?
        {
            Ok(facts) => facts,
            Err(reason) => return Ok(Err(reason)),
        };
        if crate::run::record_revision(facts.get(trigger.definition.version))? != revision {
            return Ok(Err("record_changed"));
        }
        for (path, ty) in types {
            let fact = facts
                .get_mut(&path)
                .ok_or(bilibi_database::Error::Invalid)?;
            let Ok(value) = crate::definition::script_fact(ty, &[], fact) else {
                return Ok(Err("script_input"));
            };
            *fact = value;
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
