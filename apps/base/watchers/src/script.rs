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
    /// Approved terms for each outbound operation among `calls`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outbound: Vec<OutboundTerms>,
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

/// The one destination and the data fields an outbound operation selected by
/// this script may send. A new recipient or a broader payload is a new version.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutboundTerms {
    pub operation: String,
    pub destination: String,
    pub data: Vec<String>,
}

impl OutboundTerms {
    pub(crate) fn terms(&self) -> bilibi_agent_api::OutboundTerms {
        bilibi_agent_api::OutboundTerms {
            destination: self.destination.clone(),
            data: self.data.clone(),
        }
    }
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
        let sends: Vec<String> = self
            .outbound
            .iter()
            .map(|terms| {
                format!(
                    "{} sends only {} to {}",
                    terms.operation,
                    terms.data.join(", "),
                    terms.destination
                )
            })
            .collect();
        format!(
            "Run {} as {} (JavaScript, SHA-256 {}). Reads fields {} and lookups {}. Allowed calls {}. Outbound: {}. Source: {}",
            self.name,
            crate::PRINCIPAL,
            self.source_digest,
            serde_json::json!(self.fields),
            serde_json::json!(self.lookups),
            serde_json::json!(self.calls),
            if sends.is_empty() { "none".to_owned() } else { sends.join("; ") },
            self.source
        )
    }

    /// The approved terms for one outbound operation of this script.
    pub(crate) fn terms(&self, operation: &str) -> Option<bilibi_agent_api::OutboundTerms> {
        self.outbound
            .iter()
            .find(|terms| terms.operation == operation)
            .map(OutboundTerms::terms)
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
            && self.outbound.len() <= MAX_ACTIONS
            && self.outbound.iter().enumerate().all(|(index, terms)| {
                self.calls
                    .iter()
                    .any(|call| call.operation == terms.operation)
                    && !self.outbound[..index]
                        .iter()
                        .any(|earlier| earlier.operation == terms.operation)
                    && !terms.destination.trim().is_empty()
                    && terms.destination.len() <= 320
                    && (1..=20).contains(&terms.data.len())
                    && terms.data.iter().enumerate().all(|(at, field)| {
                        !field.trim().is_empty()
                            && field.len() <= 120
                            && !terms.data[..at].contains(field)
                    })
            })
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

pub(crate) const PERSON_CALLER: &str = "person";

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
    async fn callable_script(
        &self,
        sql: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        watcher: i64,
        version: i32,
        position: usize,
    ) -> Result<(Script, ScriptPermit, &OfferedTrigger)> {
        let row = sql
            .query_opt(
                "SELECT v.id,v.definition,w.trigger FROM base_watcher_versions v JOIN base_watchers w ON w.tenant_id=v.tenant_id AND w.id=v.watcher_id WHERE v.tenant_id=$1 AND v.watcher_id=$2 AND v.number=$3 AND v.approved_at IS NOT NULL",
                &[&scope.tenant(), &watcher, &version],
            )
            .await?
            .ok_or(bilibi_database::Error::NotFound)?;
        let version_id: i64 = row.get(0);
        let definition = Definition::parse(&row.get::<_, Value>(1), &mut Vec::new())
            .ok_or(bilibi_database::Error::Invalid)?;
        let trigger = self
            .trigger(row.get::<_, String>(2).as_str())
            .ok_or(bilibi_database::Error::Invalid)?;
        let script = definition
            .scripts()
            .nth(position)
            .cloned()
            .ok_or(bilibi_database::Error::NotFound)?;
        let caller = scope.principal().unwrap_or(PERSON_CALLER);
        let digest = script.permission_digest();
        if sql
            .query_opt(
                "SELECT 1 FROM base_watcher_script_permissions WHERE tenant_id=$1 AND version_id=$2 AND script_digest=$3 AND principal=$4",
                &[&scope.tenant(), &version_id, &digest, &caller],
            )
            .await?
            .is_none()
        {
            return Err(bilibi_database::Error::Refused(
                "script_permission_refused".into(),
            ));
        }
        Ok((script, ScriptPermit { digest }, trigger))
    }

    async fn selected_script_calls(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        scope: &Scope,
        watcher: i64,
        version: i32,
        position: usize,
        subject: &str,
        started: Instant,
    ) -> Result<Vec<bilibi_agent_api::ScriptCall>> {
        let (script, permit, trigger) = self
            .callable_script(tx, scope, watcher, version, position)
            .await?;
        let mut fields = script.fields.clone();
        if !fields
            .iter()
            .any(|field| field == trigger.definition.version)
        {
            fields.push(trigger.definition.version.to_owned());
        }
        let (mut facts, types) = match self
            .read_facts(tx, scope, trigger.definition.table, subject, &fields)
            .await?
        {
            Ok(facts) => facts,
            Err(reason) => return Err(bilibi_database::Error::Refused(reason.into())),
        };
        let revision = crate::run::record_revision(facts.get(trigger.definition.version))?;
        for (path, ty) in types {
            let fact = facts
                .get_mut(&path)
                .ok_or(bilibi_database::Error::Invalid)?;
            *fact = crate::definition::script_fact(ty, &[], fact)
                .map_err(|_| bilibi_database::Error::Refused("script_input".into()))?;
        }
        let budget = self.script_deadline(tx, scope, true, started).await?;
        let replies = budget
            .within(async {
                let mut replies = Vec::with_capacity(script.lookups.len());
                for lookup in &script.lookups {
                    if self.lookup(&lookup.operation).is_none() {
                        return Err(bilibi_database::Error::Refused(
                            "script_lookup_refused".into(),
                        ));
                    }
                    let answer = self
                        .registry
                        .call_in(
                            tx,
                            scope,
                            &lookup.operation,
                            &Value::Object(lookup.input.clone()),
                        )
                        .await
                        .map_err(|_| {
                            bilibi_database::Error::Refused("script_lookup_refused".into())
                        })?;
                    replies.push(LookupValue {
                        operation: lookup.operation.clone(),
                        input: lookup.input.clone(),
                        reply: answer.get("result").cloned().ok_or_else(|| {
                            bilibi_database::Error::Refused("script_lookup_refused".into())
                        })?,
                    });
                }
                Ok::<_, bilibi_database::Error>(replies)
            })
            .await??;
        let (heap, limit) = self.script_limits(tx, scope).await?;
        let time = budget
            .deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64
            })
            .unwrap_or_default();
        if time == 0 {
            return Err(bilibi_database::Error::Refused("script_timeout".into()));
        }
        let selected = script
            .capture(&permit, &facts, &replies, time, heap)
            .await
            .map_err(|reason| bilibi_database::Error::Refused(reason.into()))?;
        if selected.len() > limit || !script.permits(&selected) {
            return Err(bilibi_database::Error::Refused(
                "script_call_refused".into(),
            ));
        }
        let mut calls = Vec::with_capacity(selected.len());
        for call in selected {
            let action = self
                .action(&call.operation)
                .filter(|action| action.script_callable(&script))
                .ok_or_else(|| bilibi_database::Error::Refused("script_call_refused".into()))?;
            let input = crate::run::action_input(
                action.definition,
                &action.input,
                subject,
                &call.input,
                revision,
            )
            .ok_or_else(|| bilibi_database::Error::Refused("script_input".into()))?;
            calls.push(bilibi_agent_api::ScriptCall {
                terms: script.terms(&call.operation),
                key: call.operation,
                input,
            });
        }
        budget.check()?;
        Ok(calls)
    }

    pub(crate) async fn preview_script_call(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        scope: &Scope,
        request: &ScriptCall,
    ) -> Result<Vec<Value>> {
        let calls = self
            .selected_script_calls(
                tx,
                scope,
                request.watcher,
                request.version,
                request.position,
                &request.subject,
                Instant::now(),
            )
            .await?;
        self.registry
            .preview_script_calls(tx, scope, &calls)
            .await
            .map_err(agent_error)
    }

    pub(crate) async fn commit_script_call(
        &self,
        tx: &mut bilibi_database::MutationTransaction<'_>,
        scope: &Scope,
        request: &ScriptCall,
    ) -> Result<Vec<Value>> {
        let calls = self
            .selected_script_calls(
                tx.sql(),
                scope,
                request.watcher,
                request.version,
                request.position,
                &request.subject,
                Instant::now(),
            )
            .await?;
        self.registry
            .commit_script_calls(tx, scope, &calls)
            .await
            .map_err(agent_error)
    }

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

    /// Every call a script may select is either certified local by its owner
    /// or outbound under that script's approved terms.
    pub(crate) fn local_script_plan(&self, definition: &Definition) -> bool {
        definition.scripts().all(|script| {
            script.calls.iter().all(|call| {
                self.action(&call.operation)
                    .is_some_and(|action| action.script_callable(script))
            })
        })
    }

    /// The approved terms for an outbound action of this definition: the
    /// definition's scripts declare one set per operation (review refuses
    /// differing ones), and an operation step never sends outside.
    pub(crate) fn outbound_terms(
        definition: &Definition,
        operation: &str,
    ) -> Option<bilibi_agent_api::OutboundTerms> {
        definition
            .scripts()
            .find_map(|script| script.terms(operation))
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

pub(crate) struct ScriptCall {
    pub watcher: i64,
    pub version: i32,
    pub position: usize,
    pub subject: String,
}

fn agent_error(error: bilibi_agent_api::Error) -> bilibi_database::Error {
    match error {
        bilibi_agent_api::Error::PlanRejected(reason) if reason["reason"].is_string() => {
            bilibi_database::Error::Refused(reason["reason"].as_str().unwrap_or_default().into())
        }
        bilibi_agent_api::Error::Backend(error) => error,
        bilibi_agent_api::Error::Forbidden => bilibi_database::Error::Forbidden,
        bilibi_agent_api::Error::NotFound => bilibi_database::Error::NotFound,
        bilibi_agent_api::Error::InvalidInput(_) => bilibi_database::Error::Invalid,
        other => bilibi_database::Error::Refused(other.to_string()),
    }
}
