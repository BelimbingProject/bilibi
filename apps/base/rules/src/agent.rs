//! The rule contracts. The Rules pages call the same ones (ADR 01): an agent
//! drafts a rule from a person's plain words through `base.rules.draft`,
//! whose review shows the conditions, actions and how each example comes
//! out; an administrator approves one fixed version through
//! `base.rules.approve`; `base.rules.switch` turns it off and on. Nothing an
//! agent drafts runs until a person approves it, and the approved version
//! runs with no AI.
use crate::review::{Draft, Review};
use crate::store::{RuleSummary, Rules, RunView, VersionView};
use crate::{APPROVE, MANAGE, VIEW};
use bilibi_agent_api::{
    json::{Data, Output},
    Channels, Definitions, Error as AgentError, Guide, Handler, Operation, OperationClient,
    OperationFuture, ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

static RULES: OnceLock<Arc<Rules>> = OnceLock::new();
/// Bound once at boot by the `base/rules.agent` service.
pub(crate) fn bind(rules: Arc<Rules>) -> bool {
    RULES.set(rules).is_ok()
}
fn rules() -> bilibi_agent_api::Result<&'static Arc<Rules>> {
    RULES.get().ok_or(Error::Internal.into())
}

pub(crate) fn summary_json(rule: &RuleSummary) -> Value {
    json!({
        "rule_id": rule.id, "name": rule.name, "trigger": rule.trigger, "trigger_title": rule.trigger_title,
        "state": rule.state.key(), "running_version": rule.running, "latest_version": rule.latest,
        "awaiting_approval": rule.pending, "last_run": rule.last_run.map(|at| at.to_string()),
        "path": format!("{}/{}", crate::web::INDEX, rule.id),
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
        "run_id": run.id, "rule_id": run.rule_id, "version": run.version, "event_id": run.event_id,
        "event": run.event_name, "subject": run.subject, "depth": run.depth, "state": run.state,
        "reason": run.reason, "facts": run.facts, "outcomes": run.outcomes,
        "at": run.created_at.to_string(), "finished_at": run.finished_at.map(|at| at.to_string()),
        "effects": run.effects.iter().map(|e| json!({"position": e.position, "operation": e.operation, "state": e.state, "reason": e.reason, "result": e.result})).collect::<Vec<_>>(),
    })
}

/// A refused review as the agent doors answer it: each problem on its field.
fn refused(review: &Review) -> AgentError {
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
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "base/rules"
    }
    fn call<'a>(
        &'a self,
        _client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let rules = rules()?;
            let client = rules.db.get().await.map_err(Error::from)?;
            let data = match self {
                Self::Vocabulary => rules.vocabulary(&**client, scope).await?,
                Self::List => {
                    let search = bilibi_agent_api::text_field(input, "search");
                    let (rows, total) = rules.list(&**client, scope, &search, 100, 0).await?;
                    json!({"rules": rows.iter().map(summary_json).collect::<Vec<_>>(), "total": total})
                }
                Self::Get => {
                    let view = rules
                        .get(
                            &**client,
                            scope,
                            input["rule_id"].as_i64().unwrap_or_default(),
                        )
                        .await?;
                    let mut rule = summary_json(&view.summary);
                    rule["versions"] = view.versions.iter().map(version_json).collect();
                    rule["runs"] = view.runs.iter().map(run_json).collect();
                    rule
                }
                Self::Runs => {
                    let runs = rules
                        .runs(
                            &**client,
                            scope,
                            input["rule_id"].as_i64(),
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

struct DraftRule;
impl WriteHandler for DraftRule {
    fn owner(&self) -> &'static str {
        "base/rules"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let rules = rules()?;
            let (draft, problems) = parse_draft(input);
            // The plan's own transaction when the door gives one: a preview
            // never asks the pool for a second connection.
            let review = match client.transaction() {
                Some(tx) => rules.review_draft(tx, scope, &draft, problems).await?,
                None => {
                    let pooled = rules.db.get().await.map_err(Error::from)?;
                    rules
                        .review_draft(&**pooled, scope, &draft, problems)
                        .await?
                }
            };
            if !review.ok() {
                return Err(refused(&review));
            }
            let mut facts = review.to_json();
            facts["name"] = json!(draft.name);
            facts["rule_id"] = json!(draft.rule_id);
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
            let rules = rules()?;
            let (draft, problems) = parse_draft(input);
            match rules.draft(tx, scope, draft, problems.clone()).await {
                Ok((id, number, review)) => {
                    let mut result = review.to_json();
                    result["rule_id"] = json!(id);
                    result["version"] = json!(number);
                    result["path"] = json!(format!("{}/{id}", crate::web::INDEX));
                    Ok(result.into())
                }
                Err(Error::Invalid) => {
                    let (draft, problems) = parse_draft(input);
                    let review = rules
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
        "base/rules"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let rules = rules()?;
            let (rule, number) = (
                input["rule_id"].as_i64().unwrap_or_default(),
                version(input),
            );
            let review = match client.transaction() {
                Some(tx) => rules.review_approval(tx, scope, rule, number).await?,
                None => {
                    let pooled = rules.db.get().await.map_err(Error::from)?;
                    rules
                        .review_approval(&**pooled, scope, rule, number)
                        .await?
                }
            };
            if !review.ok() {
                return Err(refused(&review));
            }
            let mut facts = review.to_json();
            facts["summary"] = json!(format!(
                "Approve version {number}: from now on it runs on every new \u{201c}{}\u{201d}, without AI.",
                review.trigger_title
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
            let rules = rules()?;
            let (rule, number) = (
                input["rule_id"].as_i64().unwrap_or_default(),
                version(input),
            );
            match rules.approve(tx, scope, rule, number).await {
                Ok(review) => {
                    let mut result = review.to_json();
                    result["rule_id"] = json!(rule);
                    result["version"] = json!(number);
                    result["state"] = json!("on");
                    Ok(result.into())
                }
                Err(Error::Invalid) => {
                    let review = rules.review_approval(tx.sql(), scope, rule, number).await?;
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
        let rules = rules()?;
        let (rule, enabled) = (
            input["rule_id"].as_i64().unwrap_or_default(),
            input["enabled"].as_bool().unwrap_or_default(),
        );
        match rules.set_enabled(tx, scope, rule, enabled).await {
            Ok(_) => Ok(json!({"rule_id": rule, "state": if enabled {"on"} else {"off"}})),
            Err(Error::Conflict) => Err(bilibi_agent_api::invalid("rule_id", "has no approved version yet; approve one first")),
            Err(Error::Invalid) => Err(bilibi_agent_api::invalid("enabled", "the running version no longer passes its approval checks; review and approve a new version")),
            Err(error) => Err(error.into()),
        }
    }
}
impl WriteHandler for Switch {
    fn owner(&self) -> &'static str {
        "base/rules"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let rules = rules()?;
            let id = input["rule_id"].as_i64().unwrap_or_default();
            let rule = match client.transaction() {
                Some(tx) => {
                    bilibi_authz::authorize_key(tx, scope, APPROVE).await?;
                    rules.get(tx, scope, id).await?
                }
                None => {
                    let pooled = rules.db.get().await.map_err(Error::from)?;
                    bilibi_authz::authorize_key(&**pooled, scope, APPROVE).await?;
                    rules.get(&**pooled, scope, id).await?
                }
            };
            let enabled = input["enabled"].as_bool().unwrap_or_default();
            Ok(json!({
                "rule_id": rule.summary.id, "name": rule.summary.name, "from": rule.summary.state.key(),
                "to": if enabled {"on"} else {"off"},
                "summary": if enabled {
                    format!("Switch \u{201c}{}\u{201d} on: its approved version runs on new events again.", rule.summary.name)
                } else {
                    format!("Switch \u{201c}{}\u{201d} off: no new runs, and queued ones are refused when they come to act.", rule.summary.name)
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
    json!({"type":"object","properties":{"search":{"type":"string","maxLength":120,"description":"Part of a rule's name"}}})
}
fn rule_schema() -> Value {
    json!({"type":"object","required":["rule_id"],"properties":{"rule_id":{"type":"integer","minimum":1}}})
}
fn runs_schema() -> Value {
    json!({"type":"object","properties":{"rule_id":{"type":"integer","minimum":1,"description":"One rule's runs; all rules' when absent"},"limit":{"type":"integer","minimum":1,"maximum":200}}})
}
fn scalar() -> Value {
    json!({"type":"string","maxLength":500,"description":"Text, a number written as text (\"1000.50\") or true/false written as text"})
}
fn draft_schema() -> Value {
    json!({"type":"object","required":["name","trigger","conditions","actions","examples"],"properties":{
        "rule_id":{"type":"integer","minimum":1,"description":"The rule this is the next version of; leave out for a new rule"},
        "name":{"type":"string","maxLength":120},
        "trigger":{"type":"string","maxLength":200,"description":"An installed trigger, from base.rules.vocabulary"},
        "match":{"type":"string","enum":["all","any"],"description":"all (default): every condition must hold; any: one is enough"},
        "conditions":{"type":"array","items":{"type":"object","required":["field","op"],"properties":{
            "field":{"type":"string","maxLength":160,"description":"A field of the trigger's record, from base.rules.vocabulary"},
            "op":{"type":"string","enum":crate::definition::Op::ALL},
            "value":{"type":"string","maxLength":500,"nullable":true,"description":"One value for eq, ne, lt, le, gt, ge; leave out for empty and not_empty"},
            "values":{"type":"array","items":scalar(),"description":"The values for in and not_in"}
        }}},
        "actions":{"type":"array","items":{"type":"object","required":["operation"],"properties":{
            "operation":{"type":"string","maxLength":200,"description":"An installed action, from base.rules.vocabulary"},
            "input":{"type":"string","maxLength":4000,"description":"The action's parameters as a JSON object, without its subject field (the runner binds the trigger's record)"}
        }}},
        "examples":{"type":"array","items":{"type":"object","required":["facts","expect"],"properties":{
            "facts":{"type":"string","maxLength":4000,"description":"A record's field values as a JSON object (field → value)"},
            "expect":{"type":"string","enum":["match","no_match"]}
        }}},
        "explanation":{"type":"string","maxLength":2000,"description":"The person's request in their own words"}
    }})
}
fn approve_schema() -> Value {
    json!({"type":"object","required":["rule_id","version"],"properties":{"rule_id":{"type":"integer","minimum":1},"version":{"type":"integer","minimum":1,"description":"The version number reviewed"}}})
}
fn switch_schema() -> Value {
    json!({"type":"object","required":["rule_id","enabled"],"properties":{"rule_id":{"type":"integer","minimum":1},"enabled":{"type":"boolean"}}})
}
fn output() -> Value {
    json!({"type":"object","description":"Rules, their versions and receipts; reviews list conditions, actions, example results, warnings and problems."})
}

/// The agent schema carries nested JSON (action inputs, example facts) as
/// text, which the schema dialect can bound; the contract reads it back
/// into the stored shape before any check.
pub(crate) fn normalise(input: &Value) -> Value {
    let mut input = input.clone();
    let object = |text: &Value| match text {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or(Value::Null),
        other => other.clone(),
    };
    if let Some(conditions) = input["conditions"].as_array_mut() {
        for condition in conditions {
            if let Some(values) = condition.get("values").cloned() {
                condition["value"] = values;
                condition.as_object_mut().map(|o| o.remove("values"));
            }
        }
    }
    if let Some(actions) = input["actions"].as_array_mut() {
        for action in actions {
            if action.get("input").is_some() {
                action["input"] = object(&action["input"]);
            }
        }
    }
    if let Some(examples) = input["examples"].as_array_mut() {
        for example in examples {
            example["facts"] = object(&example["facts"]);
        }
    }
    input
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
        Box::pin(async move { self.0.preview(client, scope, &normalise(input)).await })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move { self.0.commit(tx, scope, &normalise(input)).await })
    }
}
static DRAFT: Normalised = Normalised(&DraftRule);

const GUIDE: &str = "A rule is data: when a trigger event happens to a record, if its conditions hold, run its actions. Read base.rules.vocabulary first: it lists the installed triggers with the fields their record offers, the actions with their parameters, the operators and the tenant's limits. Turn the person's plain words into one draft: a name, the trigger, match (all or any), conditions (field, op, value; values for in and not_in), actions (operation and its input as a JSON object, leaving out the record field the runner binds) and 1 to 10 examples of records that should and should not match (facts as a JSON object of the fields the conditions read, expect match or no_match). Plan base.rules.draft and show the person its review: the conditions in words, the actions, how each example comes out and any warnings. A draft never runs. Tell the person an administrator approves the version on the Rules page (or with base.rules.approve after reviewing it there); the approved version then runs on every new event without AI. To change a running rule, draft its next version with rule_id; it runs only once approved. Read base.rules.runs to explain what a rule did: each receipt says which conditions held, the facts read and each action's outcome.";

inventory::submit! { Contribution::new(DEFINITIONS, "base/rules", Provenance { module: "base/rules", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key:"base.rules.vocabulary", title:"Show what rules can use", summary:"The installed rule triggers with the fields of their record, the actions with their parameters, the operators and this tenant's limits.", keywords:&["rule","automation","trigger","action","condition","workflow"], capability:VIEW, input:empty_schema, output, handler:Handler::Read(&Read::Vocabulary), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.rules.list", title:"List rules", summary:"This tenant's rules: trigger, whether they are on, the running and latest version, and the last run.", keywords:&["rule","automation","list"], capability:VIEW, input:list_schema, output, handler:Handler::Read(&Read::List), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.rules.get", title:"Get a rule", summary:"One rule with its versions (conditions, actions, examples, who drafted and approved each) and its latest receipts.", keywords:&["rule","automation","version"], capability:VIEW, input:rule_schema, output, handler:Handler::Read(&Read::Get), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.rules.runs", title:"List rule runs", summary:"Receipts of rule runs: which conditions held, the facts read, each action's outcome, or why the run was refused.", keywords:&["rule","run","receipt","why","automation"], capability:VIEW, input:runs_schema, output, handler:Handler::Read(&Read::Runs), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.rules.draft", title:"Draft a rule", summary:"Save a rule, or a rule's next version, for an administrator to approve. The review shows the conditions, the actions and how each example comes out. Nothing runs until approved.", keywords:&["rule","automation","draft","when","then"], capability:MANAGE, input:draft_schema, output, handler:Handler::SafeWrite(&DRAFT), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.rules.approve", title:"Approve a rule version", summary:"Approve one fixed version of a rule; from then on it runs on every new trigger event, without AI, as the rules automation principal.", keywords:&["rule","approve","activate","automation"], capability:APPROVE, input:approve_schema, output, handler:Handler::SafeWrite(&Approve), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.rules.switch", title:"Switch a rule on or off", summary:"Stop a rule's approved version from running, or let it run again.", keywords:&["rule","disable","enable","off","on","automation"], capability:APPROVE, input:switch_schema, output, handler:Handler::SafeWrite(&Switch), channels:Channels::Every, step_up:&[] },
    ],
    guides: &[Guide { key:"base.rules", title:"Tenant rules", summary:"Draft a \u{201c}when this happens, do that\u{201d} rule from a person's words for an administrator to approve.", keywords:&["rule","automation","when","then","trigger"], capability:VIEW, body:GUIDE }],
}) }
