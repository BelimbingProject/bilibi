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
    #[serde(default)]
    pub lookups: Vec<LookupCall>,
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
    #[serde(default)]
    pub lookup_fixtures: Vec<LookupValue>,
    pub expect: Vec<ActionCall>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LookupCall {
    pub operation: String,
    #[serde(default)]
    pub input: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LookupValue {
    pub operation: String,
    #[serde(default)]
    pub input: Map<String, Value>,
    pub reply: Value,
}

impl Script {
    pub(crate) fn permission_digest(&self) -> String {
        let value = serde_json::to_vec(self).unwrap_or_default();
        Sha256::digest(value)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
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
            "Run {} as {} (JavaScript, SHA-256 {}). Reads fields {} and lookups {}. Allowed calls {}. Source: {}",
            self.name,
            crate::PRINCIPAL,
            self.source_digest,
            serde_json::json!(self.fields),
            serde_json::json!(self.lookups),
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
            && self.lookups.len() <= MAX_ACTIONS
            && self
                .lookups
                .iter()
                .all(|lookup| !lookup.operation.is_empty())
            && (1..=MAX_ACTIONS).contains(&self.calls.len())
            && self.calls.iter().all(|call| !call.operation.is_empty())
            && (1..=MAX_EXAMPLES).contains(&self.examples.len())
            && self.examples.iter().all(|example| {
                example.expect.len() <= MAX_ACTIONS
                    && example
                        .facts
                        .keys()
                        .all(|field| self.fields.contains(field))
                    && example.lookup_fixtures.len() == self.lookups.len()
                    && self
                        .lookups
                        .iter()
                        .zip(&example.lookup_fixtures)
                        .all(|(lookup, reply)| {
                            lookup.operation == reply.operation && lookup.input == reply.input
                        })
            })
    }

    pub(crate) async fn capture(
        &self,
        permit: &ScriptPermit,
        facts: &Map<String, Value>,
        replies: &[LookupValue],
        milliseconds: u64,
        heap_mib: u64,
    ) -> std::result::Result<Vec<ActionCall>, &'static str> {
        if permit.digest != self.permission_digest() {
            return Err("script_permission_refused");
        }
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
                crate::sandbox::evaluate(
                    &self.source,
                    Value::Object(input),
                    serde_json::to_value(replies).map_err(|_| "script_input")?,
                    milliseconds,
                    heap_mib,
                )
                .await?
            }
        };
        serde_json::from_value(answer).map_err(|_| "script_output")
    }

    pub(crate) fn permits(&self, calls: &[ActionCall]) -> bool {
        calls.len() <= MAX_ACTIONS && calls.iter().all(|call| self.calls.contains(call))
    }
}

pub(crate) struct ScriptPermit {
    digest: String,
}

pub(crate) struct ScriptPermits {
    digests: Vec<String>,
}

impl ScriptPermits {
    pub(crate) fn for_script(
        &self,
        script: &Script,
    ) -> std::result::Result<ScriptPermit, &'static str> {
        let digest = script.permission_digest();
        self.digests
            .contains(&digest)
            .then_some(ScriptPermit { digest })
            .ok_or("script_permission_refused")
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
    async fn lookup_replies(
        &self,
        client: &dyn bilibi_agent_api::OperationClient,
        scope: &Scope,
        script: &Script,
    ) -> std::result::Result<Vec<LookupValue>, &'static str> {
        let mut replies = Vec::with_capacity(script.lookups.len());
        for lookup in &script.lookups {
            if self.lookup(&lookup.operation).is_none() {
                return Err("script_lookup_refused");
            }
            let answer = self
                .registry
                .call_on(
                    client,
                    scope,
                    &lookup.operation,
                    &Value::Object(lookup.input.clone()),
                )
                .await
                .map_err(|_| "script_lookup_refused")?;
            replies.push(LookupValue {
                operation: lookup.operation.clone(),
                input: lookup.input.clone(),
                reply: answer
                    .get("result")
                    .cloned()
                    .ok_or("script_lookup_refused")?,
            });
        }
        Ok(replies)
    }

    pub(crate) async fn reviewed_script_permits(
        &self,
        sql: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        definition: &Definition,
    ) -> Result<std::result::Result<ScriptPermits, &'static str>> {
        if definition.scripts().next().is_none() {
            return Ok(Ok(ScriptPermits {
                digests: Vec::new(),
            }));
        }
        if !bilibi_authz::decide(sql, scope, crate::RUN_SCRIPT)
            .await?
            .allowed
        {
            return Ok(Err("script_run_refused"));
        }
        Ok(Ok(ScriptPermits {
            digests: definition
                .scripts()
                .map(Script::permission_digest)
                .collect(),
        }))
    }

    pub(crate) async fn approved_script_permits(
        &self,
        sql: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        version_id: i64,
        definition: &Definition,
    ) -> Result<std::result::Result<ScriptPermits, &'static str>> {
        let permits = match self.reviewed_script_permits(sql, scope, definition).await? {
            Ok(permits) => permits,
            Err(reason) => return Ok(Err(reason)),
        };
        for digest in &permits.digests {
            if sql
                .query_opt(
                    "SELECT 1 FROM base_watcher_script_permissions WHERE tenant_id=$1 AND version_id=$2 AND script_digest=$3 AND principal=$4",
                    &[&scope.tenant(), &version_id, digest, &crate::PRINCIPAL],
                )
                .await?
                .is_none()
            {
                return Ok(Err("script_permission_refused"));
            }
        }
        Ok(Ok(permits))
    }

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
        lookup_scope: &Scope,
        definition: &Definition,
        permits: &ScriptPermits,
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
                    let permit = match permits.for_script(script) {
                        Ok(permit) => permit,
                        Err(reason) => return Ok(Err(reason)),
                    };
                    let time = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis() as u64;
                    if time == 0 {
                        return Ok(Err("script_timeout"));
                    }
                    let replies = match self.lookup_replies(sql, lookup_scope, script).await {
                        Ok(replies) => replies,
                        Err(reason) => return Ok(Err(reason)),
                    };
                    match script.capture(&permit, &facts, &replies, time, heap).await {
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
