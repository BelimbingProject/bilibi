//! The watcher contracts. The Watchers pages call the same ones (ADR 01): an agent
//! drafts a watcher from a person's plain words through `base.watchers.draft`,
//! whose review shows the conditions, actions and how each example comes
//! out; an administrator approves one fixed version through
//! `base.watchers.approve`; `base.watchers.switch` turns it off and on. Nothing an
//! agent drafts runs until a person approves it, and the approved version
//! runs with no AI.
use crate::review::{Draft, Review};
use crate::store::{RunView, VersionView, WatcherSummary, Watchers};
use crate::{APPROVE, MANAGE, VIEW};
use bilibi_agent_api::{
    json::{Data, Output},
    Channels, Definitions, Error as AgentError, Guide, Handler, Operation, OperationClient,
    OperationFuture, ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::{Channel, Scope};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

static WATCHERS: OnceLock<Arc<Watchers>> = OnceLock::new();
/// Bound once at boot by the `base/watchers.watchers` service.
pub(crate) fn bind(watchers: Arc<Watchers>) -> bool {
    WATCHERS.set(watchers).is_ok()
}
pub(crate) fn watchers() -> bilibi_agent_api::Result<&'static Arc<Watchers>> {
    WATCHERS.get().ok_or(Error::Internal.into())
}

fn approved_field_uses<'a>(
    sql: &'a tokio_postgres::Client,
    scope: &'a Scope,
    table: &'a str,
    key: &'a str,
    write: bool,
) -> bilibi_custom_fields::DependencyFuture<'a> {
    Box::pin(async move {
        let watchers = WATCHERS.get().ok_or(Error::Internal)?;
        if write {
            watchers.lock_admission(sql, scope).await?;
        }
        watchers.approved_field_uses(sql, scope, table, key).await
    })
}

inventory::submit! {
    Contribution::new(bilibi_custom_fields::RECEIPT_DEPENDENCIES, "base/watchers.custom_fields",
        Provenance { module: "base/watchers", source: file!(), line: line!() },
        &bilibi_custom_fields::ReceiptDependency { approved: approved_field_uses })
}

pub(crate) fn summary_json(watcher: &WatcherSummary) -> Value {
    json!({
        "watcher_id": watcher.id, "name": watcher.name, "watch": watcher.trigger, "watch_title": watcher.trigger_title,
        "state": watcher.state.key(), "running_version": watcher.running, "latest_version": watcher.latest,
        "awaiting_approval": watcher.pending, "last_run": watcher.last_run.map(|at| at.to_string()),
        "path": format!("{}/{}", crate::web::INDEX, watcher.id),
    })
}
fn version_json(version: &VersionView) -> Value {
    json!({
        "number": version.number, "definition": version.definition, "examples": version.examples,
        "explanation": version.explanation, "digest": version.digest, "drafted_via": version.drafted_via,
        "drafted_by": version.drafted_by, "drafted_at": version.drafted_at.to_string(),
        "approved_by": version.approved_by, "approved_at": version.approved_at.map(|at| at.to_string()),
    })
}
pub(crate) fn run_json(run: &RunView) -> Value {
    json!({
        "run_id": run.id, "watcher_id": run.watcher_id, "version": run.version, "event_id": run.event_id,
        "event": run.event_name, "subject": run.subject, "depth": run.depth, "state": run.state,
        "reason": run.reason, "facts": run.facts, "outcomes": run.outcomes, "observation": run.observation,
        "at": run.created_at.to_string(), "finished_at": run.finished_at.map(|at| at.to_string()),
        "effects": run.effects.iter().map(|e| json!({"position": e.position, "operation": e.operation, "state": e.state, "reason": e.reason, "result": e.result})).collect::<Vec<_>>(),
    })
}

/// A refused review as the agent doors answer it: each problem on its field.
pub(crate) fn refused(review: &Review) -> AgentError {
    let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (field, why) in &review.problems {
        fields.entry(field.clone()).or_default().push(why.clone());
    }
    AgentError::InvalidInput(fields)
}
fn parse_draft(input: &Value) -> (Draft, crate::definition::Problems) {
    let mut problems = Vec::new();
    let draft = Draft::parse(input, &mut problems);
    (draft, problems)
}

enum Read {
    Vocabulary,
    List,
    Get,
    Runs,
    Trial,
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "base/watchers"
    }
    fn call<'a>(
        &'a self,
        operation_client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let watchers = watchers()?;
            if matches!(self, Self::Trial) {
                if !operation_client.detached_read() {
                    return Err(AgentError::ChannelNotAllowed);
                }
                let days = input["days"].as_i64().unwrap_or(0);
                let limit = input["limit"].as_i64().unwrap_or(0);
                let draft_input = normalise(&input["draft"])?;
                let (draft, problems) = parse_draft(&draft_input);
                let data = watchers.trial(scope, draft, problems, days, limit).await?;
                return Ok(Output::Data(Data::Json(data)));
            }
            let client = watchers.db.get().await.map_err(Error::from)?;
            let data = match self {
                Self::Trial => return Err(Error::Internal.into()),
                Self::Vocabulary => watchers.vocabulary(&**client, scope).await?,
                Self::List => {
                    let search = bilibi_agent_api::text_field(input, "search");
                    let (rows, total) = watchers.list(&**client, scope, &search, 100, 0).await?;
                    json!({"watchers": rows.iter().map(summary_json).collect::<Vec<_>>(), "total": total})
                }
                Self::Get => {
                    let view = watchers
                        .get(
                            &**client,
                            scope,
                            input["watcher_id"].as_i64().unwrap_or_default(),
                        )
                        .await?;
                    let mut watcher = summary_json(&view.summary);
                    watcher["versions"] = view.versions.iter().map(version_json).collect();
                    watcher["runs"] = view.runs.iter().map(run_json).collect();
                    watcher
                }
                Self::Runs => {
                    let runs = watchers
                        .runs(
                            &**client,
                            scope,
                            input["watcher_id"].as_i64(),
                            input["limit"].as_i64().unwrap_or(50),
                        )
                        .await?;
                    json!({"runs": runs.iter().map(run_json).collect::<Vec<_>>()})
                }
            };
            Ok(Output::Data(Data::Json(data)))
        })
    }
}

struct DraftWatcher;
impl WriteHandler for DraftWatcher {
    fn owner(&self) -> &'static str {
        "base/watchers"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let watchers = watchers()?;
            let (draft, problems) = parse_draft(input);
            // The plan's own transaction when the door gives one: a preview
            // never asks the pool for a second connection.
            let review = match client.transaction() {
                Some(tx) => watchers.review_draft(tx, scope, &draft, problems).await?,
                None => {
                    let pooled = watchers.db.get().await.map_err(Error::from)?;
                    watchers
                        .review_draft(&**pooled, scope, &draft, problems)
                        .await?
                }
            };
            if !review.ok() {
                return Err(refused(&review));
            }
            let mut facts = review.to_json();
            facts["name"] = json!(draft.name);
            facts["watcher_id"] = json!(draft.watcher_id);
            facts["drafted_via"] = json!(if scope.agent().is_some() {
                "agent"
            } else {
                "person"
            });
            facts["summary"] = json!(format!(
                "Save \u{201c}{}\u{201d} as a new version for review. Nothing runs until an administrator approves it.",
                draft.name
            ));
            Ok(facts.into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let watchers = watchers()?;
            let (draft, problems) = parse_draft(input);
            match watchers.draft(tx, scope, draft, problems.clone()).await {
                Ok((id, number, review)) => {
                    let mut result = review.to_json();
                    result["watcher_id"] = json!(id);
                    result["version"] = json!(number);
                    result["path"] = json!(format!("{}/{id}", crate::web::INDEX));
                    Ok(result.into())
                }
                Err(Error::Invalid) => {
                    let (draft, problems) = parse_draft(input);
                    let review = watchers
                        .review_draft(tx.sql(), scope, &draft, problems)
                        .await?;
                    Err(refused(&review))
                }
                Err(error) => Err(error.into()),
            }
        })
    }
}

struct Approve;
impl WriteHandler for Approve {
    fn owner(&self) -> &'static str {
        "base/watchers"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let watchers = watchers()?;
            let (watcher, number) = (
                input["watcher_id"].as_i64().unwrap_or_default(),
                version(input),
            );
            let review = match client.transaction() {
                Some(tx) => watchers.review_approval(tx, scope, watcher, number).await?,
                None => {
                    let pooled = watchers.db.get().await.map_err(Error::from)?;
                    watchers
                        .review_approval(&**pooled, scope, watcher, number)
                        .await?
                }
            };
            if !review.ok() {
                return Err(refused(&review));
            }
            let mut facts = review.to_json();
            facts["summary"] = json!(match &review.observation {
                None => format!(
                    "Approve version {number}: from now on it runs on every new \u{201c}{}\u{201d}, without AI.",
                    review.trigger_title
                ),
                Some(observation) => format!(
                    "Approve version {number}: from now on it runs on every new \u{201c}{}\u{201d}. {observation} The answer only decides whether the approved actions run.",
                    review.trigger_title
                ),
            });
            Ok(facts.into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let watchers = watchers()?;
            let (watcher, number) = (
                input["watcher_id"].as_i64().unwrap_or_default(),
                version(input),
            );
            match watchers.approve(tx, scope, watcher, number).await {
                Ok(review) => {
                    let mut result = review.to_json();
                    result["watcher_id"] = json!(watcher);
                    result["version"] = json!(number);
                    result["state"] = json!("on");
                    Ok(result.into())
                }
                Err(Error::Invalid) => {
                    let review = watchers
                        .review_approval(tx.sql(), scope, watcher, number)
                        .await?;
                    Err(refused(&review))
                }
                Err(error) => Err(error.into()),
            }
        })
    }
}
fn version(input: &Value) -> i32 {
    input["version"]
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or_default()
}

struct Switch;
impl Switch {
    async fn apply(
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &Value,
    ) -> bilibi_agent_api::Result<Value> {
        let watchers = watchers()?;
        let (watcher, enabled) = (
            input["watcher_id"].as_i64().unwrap_or_default(),
            input["enabled"].as_bool().unwrap_or_default(),
        );
        match watchers.set_enabled(tx, scope, watcher, enabled).await {
            Ok(_) => Ok(json!({"watcher_id": watcher, "state": if enabled {"on"} else {"off"}})),
            Err(Error::Conflict) => Err(bilibi_agent_api::invalid("watcher_id", "has no approved version yet; approve one first")),
            Err(Error::Invalid) => Err(bilibi_agent_api::invalid("enabled", "the running version no longer passes its approval checks; review and approve a new version")),
            Err(error) => Err(error.into()),
        }
    }
}
impl WriteHandler for Switch {
    fn owner(&self) -> &'static str {
        "base/watchers"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let watchers = watchers()?;
            let id = input["watcher_id"].as_i64().unwrap_or_default();
            let watcher = match client.transaction() {
                Some(tx) => {
                    bilibi_authz::authorize_key(tx, scope, APPROVE).await?;
                    watchers.get(tx, scope, id).await?
                }
                None => {
                    let pooled = watchers.db.get().await.map_err(Error::from)?;
                    bilibi_authz::authorize_key(&**pooled, scope, APPROVE).await?;
                    watchers.get(&**pooled, scope, id).await?
                }
            };
            let enabled = input["enabled"].as_bool().unwrap_or_default();
            Ok(json!({
                "watcher_id": watcher.summary.id, "name": watcher.summary.name, "from": watcher.summary.state.key(),
                "to": if enabled {"on"} else {"off"},
                "summary": if enabled {
                    format!("Switch \u{201c}{}\u{201d} on: its approved version runs on new events again.", watcher.summary.name)
                } else {
                    format!("Switch \u{201c}{}\u{201d} off: no new runs, and queued ones are refused when they come to act.", watcher.summary.name)
                },
            }).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move { Ok(Switch::apply(tx, scope, input).await?.into()) })
    }
}

fn empty_schema() -> Value {
    json!({"type":"object","properties":{}})
}
fn list_schema() -> Value {
    json!({"type":"object","properties":{"search":{"type":"string","maxLength":120,"description":"Part of a watcher's name"}}})
}
fn watcher_schema() -> Value {
    json!({"type":"object","required":["watcher_id"],"properties":{"watcher_id":{"type":"integer","minimum":1}}})
}
fn runs_schema() -> Value {
    json!({"type":"object","properties":{"watcher_id":{"type":"integer","minimum":1,"description":"One watcher's runs; all watchers' when absent"},"limit":{"type":"integer","minimum":1,"maximum":200}}})
}
fn scalar() -> Value {
    json!({"type":"string","maxLength":500,"description":"Text, a number written as text (\"1000.50\") or true/false written as text"})
}
fn script_schema() -> Value {
    let call = json!({"type":"object","required":["operation"],"properties":{
        "operation":{"type":"string","maxLength":200},
        "input":{"type":"object","additionalProperties":true}
    }});
    json!({"type":"object","required":["name","source","fields","calls","examples"],"properties":{
        "name":{"type":"string","maxLength":120},
        "source":{"type":"string","maxLength":16384,"description":"JavaScript function body. Return an ordered array of declared calls using facts from the approved fields."},
        "fields":{"type":"array","items":{"type":"string","maxLength":160}},
        "calls":{"type":"array","items":call},
        "examples":{"type":"array","items":{"type":"object","required":["facts","expect"],"properties":{
            "facts":{"type":"object","additionalProperties":true},
            "expect":{"type":"array","items":call}
        }}},
        "language":{"type":"string","enum":["javascript"]}
    }})
}

fn observe_schema() -> Value {
    json!({"type":"object","required":["question","fields","labels"],"description":"One AI observation, asked once per event before the conditions, through the AI provider chosen in settings. Conditions read its answer as ai:label (one of labels); the answer can only decide whether the approved actions run.","properties":{
        "question":{"type":"string","maxLength":1000,"description":"What to ask about the record, e.g. Is the customer asking to cancel?"},
        "fields":{"type":"array","items":{"type":"string","maxLength":160},"description":"1 to 10 of the trigger record's fields, sent with the question; nothing else of the record is sent"},
        "labels":{"type":"array","items":{"type":"string","maxLength":40},"description":"2 to 20 unique labels of lower-case letters, digits and _, the only answers kept, e.g. cancellation, other"}
    }})
}

fn draft_schema() -> Value {
    json!({"type":"object","required":["name","watch","do"],"properties":{
        "observe":observe_schema(),
        "watcher_id":{"type":"integer","minimum":1,"description":"The watcher this is the next version of; leave out for a new watcher"},
        "name":{"type":"string","maxLength":120},
        "watch":{"type":"string","maxLength":200,"description":"An installed trigger, from base.watchers.vocabulary"},
        "if":{"type":"object","description":"Omit to act on every event of the watched kind.","required":["conditions"],"properties":{"match":{"type":"string","enum":["all","any"],"description":"all (default): every condition must hold; any: one is enough"},
        "conditions":{"type":"array","description":"At least one condition.","items":{"type":"object","required":["field","op"],"properties":{
            "field":{"type":"string","maxLength":160,"description":"A field of the trigger's record, from base.watchers.vocabulary, or ai:label when the draft has observe"},
            "op":{"type":"string","enum":crate::definition::Op::ALL},
            "value":{"type":"string","maxLength":500,"nullable":true,"description":"One value for eq, ne, lt, le, gt, ge; leave out for empty and not_empty"},
            "values":{"type":"array","items":scalar(),"description":"The values for in and not_in"}
        }}}}},
        "do":{"type":"array","items":{"type":"object","description":"Supply either an operation with its input, or a script.","properties":{
            "script":script_schema(),
            "operation":{"type":"string","maxLength":200,"description":"An installed action, from base.watchers.vocabulary"},
            "input":{"type":"object","additionalProperties":true,"description":"The action's parameters as a JSON object, without its subject field (the runner binds the trigger's record)"}
        }}},
        "examples":{"type":"array","items":{"type":"object","required":["facts","expect"],"properties":{
            "facts":{"type":"string","maxLength":4000,"description":"A record's field values as a JSON object (field → value); with observe, ai:label stands for the AI's answer"},
            "expect":{"type":"string","enum":["match","no_match"]}
        }}},
        "explanation":{"type":"string","maxLength":2000,"description":"The person's request in their own words"}
    }})
}
fn trial_schema() -> Value {
    json!({"type":"object","required":["draft"],"properties":{
        "draft":draft_schema(),
        "days":{"type":"integer","minimum":1,"maximum":365,"description":"Past days to examine; default 30, subject to tenant limit"},
        "limit":{"type":"integer","minimum":1,"maximum":500,"description":"Maximum events to examine; default 100, subject to tenant limit"}
    }})
}
fn approve_schema() -> Value {
    json!({"type":"object","required":["watcher_id","version"],"properties":{"watcher_id":{"type":"integer","minimum":1},"version":{"type":"integer","minimum":1,"description":"The version number reviewed"}}})
}
fn switch_schema() -> Value {
    json!({"type":"object","required":["watcher_id","enabled"],"properties":{"watcher_id":{"type":"integer","minimum":1},"enabled":{"type":"boolean"}}})
}
fn output() -> Value {
    json!({"type":"object","description":"Watchers, their versions and receipts; reviews list conditions, actions, example results, warnings and problems."})
}

/// Read example facts from JSON text and fold list values into the stored shape.
fn normalise(input: &Value) -> bilibi_agent_api::Result<Value> {
    let mut input = input.clone();
    let decode = |value: &Value, field: &str| match value {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .map_err(|_| bilibi_agent_api::invalid(field, "must be a JSON object")),
        other => Ok(other.clone()),
    };
    if let Some(conditions) = input
        .get_mut("if")
        .and_then(|v| v.get_mut("conditions"))
        .and_then(Value::as_array_mut)
    {
        for (i, condition) in conditions.iter_mut().enumerate() {
            let op = condition
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let has_value = condition.get("value").is_some();
            let has_values = condition.get("values").is_some();
            match op {
                "in" | "not_in" => {
                    if has_value {
                        return Err(bilibi_agent_api::invalid(
                            &format!("if.conditions.{i}.value"),
                            "use values for this operator",
                        ));
                    }
                    let Some(values) = condition.get("values").cloned() else {
                        return Err(bilibi_agent_api::invalid(
                            &format!("if.conditions.{i}.values"),
                            "is required",
                        ));
                    };
                    condition["value"] = values;
                    condition.as_object_mut().map(|o| o.remove("values"));
                }
                "empty" | "not_empty" => {
                    if has_value || has_values {
                        return Err(bilibi_agent_api::invalid(
                            &format!(
                                "if.conditions.{i}.{}",
                                if has_value { "value" } else { "values" }
                            ),
                            "takes no value",
                        ));
                    }
                }
                _ => {
                    if has_values {
                        return Err(bilibi_agent_api::invalid(
                            &format!("if.conditions.{i}.values"),
                            "use value for this operator",
                        ));
                    }
                }
            }
        }
    }
    if let Some(examples) = input["examples"].as_array_mut() {
        for (i, example) in examples.iter_mut().enumerate() {
            example["facts"] = decode(&example["facts"], &format!("examples.{i}.facts"))?;
        }
    }
    Ok(input)
}

struct Normalised(&'static dyn WriteHandler);
impl WriteHandler for Normalised {
    fn owner(&self) -> &'static str {
        self.0.owner()
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move { self.0.preview(client, scope, &normalise(input)?).await })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move { self.0.commit(tx, scope, &normalise(input)?).await })
    }
}
static DRAFT: Normalised = Normalised(&DraftWatcher);

const GUIDE: &str = "A watcher is data: when a trigger event happens to a record, if its conditions hold, run its actions. Read base.watchers.vocabulary first: it lists the installed triggers with the fields their record offers, the actions with their parameters, the operators and the tenant's limits. Turn the person's plain words into one draft: a name, watch (the event), optional if (match all or any, conditions with field, op, value; values for in and not_in), do (operation and its input as a JSON object, leaving out the record field the runner binds) and up to 10 examples of records that should and should not match (facts as a JSON object of the fields the conditions read, expect match or no_match). When a condition needs judgement a field cannot give (\u{201c}the customer seems to ask to cancel\u{201d}), add observe: a question, the fewest record fields the question needs, and 2 to 20 labels; conditions then read ai:label, and examples can supply it as a fact. Only the listed fields are sent, approval shows them, the answer is kept only when it is one of the labels, and it only decides whether the approved actions run. Each event asks once, within the tenant's daily AI budget; AI for automation must be on. Use base.watchers.trial to examine past events before saving; its event evidence and current record facts have different times, and it takes no action. Plan base.watchers.draft and show the person its review: the conditions in words, the actions, how each example comes out and any warnings. Without if, every event of the watched kind matches. A draft never runs. Tell the person an administrator approves the version on the Watchers page (or with base.watchers.approve after reviewing it there); the approved version then runs on every new event, with no AI beyond its declared observation. To change a running watcher, draft its next version with watcher_id; it runs only once approved. Read base.watchers.runs to explain what a watcher did: each receipt says which conditions held, the facts read and each action's outcome.";

inventory::submit! { Contribution::new(DEFINITIONS, "base/watchers", Provenance { module: "base/watchers", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key:"base.watchers.vocabulary", title:"Show what watchers can use", summary:"The installed watcher triggers with the fields of their record, the actions with their parameters, the operators and this tenant's limits.", keywords:&["watcher","automation","trigger","action","condition","workflow"], capability:VIEW, input:empty_schema, output, handler:Handler::Read(&Read::Vocabulary), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.watchers.list", title:"List watchers", summary:"This tenant's watchers: trigger, whether they are on, the running and latest version, and the last run.", keywords:&["watcher","automation","list"], capability:VIEW, input:list_schema, output, handler:Handler::Read(&Read::List), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.watchers.get", title:"Get a watcher", summary:"One watcher with its versions (conditions, actions, examples, who drafted and approved each) and its latest receipts.", keywords:&["watcher","automation","version"], capability:VIEW, input:watcher_schema, output, handler:Handler::Read(&Read::Get), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.watchers.runs", title:"List watcher runs", summary:"Receipts of watcher runs: which conditions held, the facts read, each action's outcome, or why the run was refused.", keywords:&["watcher","run","receipt","why","automation"], capability:VIEW, input:runs_schema, output, handler:Handler::Read(&Read::Runs), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.watchers.trial", title:"Trial a draft watcher", summary:"Evaluate a draft against bounded past events and preview its actions without acting. Current record facts are marked separately from event evidence.", keywords:&["watcher","trial","preview","replay"], capability:VIEW, input:trial_schema, output, handler:Handler::Read(&Read::Trial), channels:Channels::Only { channels:&[Channel::Api,Channel::Mcp,Channel::Web], reason:"The audited trial requires a detached read." }, step_up:&[] },
        Operation { key:"base.watchers.draft", title:"Draft a watcher", summary:"Save a watcher, or a watcher's next version, for an administrator to approve. The review shows the conditions, the actions and how each example comes out. Nothing runs until approved.", keywords:&["watcher","automation","draft","when","then"], capability:MANAGE, input:draft_schema, output, handler:Handler::SafeWrite(&DRAFT), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.watchers.approve", title:"Approve a watcher version", summary:"Approve one fixed version of a watcher; from then on it runs on every new trigger event as the watchers automation principal, with no AI beyond the version's declared observation.", keywords:&["watcher","approve","activate","automation"], capability:APPROVE, input:approve_schema, output, handler:Handler::SafeWrite(&Approve), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.watchers.switch", title:"Switch a watcher on or off", summary:"Stop a watcher's approved version from running, or let it run again.", keywords:&["watcher","disable","enable","off","on","automation"], capability:APPROVE, input:switch_schema, output, handler:Handler::SafeWrite(&Switch), channels:Channels::Every, step_up:&[] },
    ],
    guides: &[Guide { key:"base.watchers", title:"Tenant watchers", summary:"Draft a \u{201c}when this happens, do that\u{201d} watcher from a person's words for an administrator to approve.", keywords:&["watcher","automation","when","then","trigger"], capability:VIEW, body:GUIDE }],
}) }
