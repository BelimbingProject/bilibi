//! Drafting a watcher on the page: "New watcher" on `/watchers` and "Draft next
//! version" on `/watchers/{id}`. Both open this editor, which builds the input of
//! `base.watchers.draft` (the contract an AI agent calls) from comboboxes and
//! typed fields of `base.watchers.vocabulary`, shows the contract's own review
//! (conditions in words, actions, how each example comes out, warnings and
//! problems) and saves with the same contract. It never writes a watcher table.
//!
//! Every row is its own small live form, so a watcher with 20 conditions, 10
//! actions and 10 examples never meets the 64-field cap of one form event;
//! the server keeps the draft between events, and any edit discards the
//! review, so only what was reviewed is saved.
use super::*;
use crate::definition::Op;
use bilibi_agent_api::page::Call;
use bilibi_ui::{Choice, Combobox, Field, Form, InputKind, Modal, ModalWidth};
use std::collections::BTreeMap;

const EVENT_LIMIT: usize = 4_000;
const NAME_MAX: usize = 120;
const EXPLANATION_MAX: usize = 2_000;
const VALUE_MAX: usize = 500;

#[derive(Template)]
#[template(
    source = r#"<div class="space-y-6">{% for part in parts %}{{ part|safe }}{% endfor %}</div>"#,
    ext = "html"
)]
struct Stack {
    parts: Vec<Html>,
}
#[derive(Template)]
#[template(
    source = r#"<div class="space-y-3">{% for part in parts %}{{ part|safe }}{% endfor %}</div>"#,
    ext = "html"
)]
struct Rows {
    parts: Vec<Html>,
}

/// What `base.watchers.vocabulary` offers this reader.
struct FieldV {
    id: String,
    label: String,
    ty: String,
    values: Vec<String>,
}
struct TriggerV {
    event: String,
    title: String,
    table: String,
    fields: Vec<FieldV>,
}
struct Param {
    key: String,
    ty: String,
    description: String,
    choices: Vec<String>,
    required: bool,
}
struct ActionV {
    operation: String,
    title: String,
    table: String,
    params: Vec<Param>,
}
struct Vocab {
    triggers: Vec<TriggerV>,
    actions: Vec<ActionV>,
    conditions: usize,
    action_rows: usize,
    examples: usize,
}
fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}
fn humanise(key: &str) -> String {
    let spaced = key.replace('_', " ");
    let mut chars = spaced.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}
impl Vocab {
    async fn load(pages: &Pages, scope: &Scope) -> std::result::Result<Self, ContractError> {
        let reply = pages
            .contracts
            .read(scope, "base.watchers.vocabulary", &json!({}))
            .await?;
        let data = reply.get("result").unwrap_or(&reply);
        let triggers = data["triggers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["readable"] == true)
            .map(|t| TriggerV {
                event: text(&t["trigger"]),
                title: text(&t["title"]),
                table: text(&t["table"]),
                fields: t["fields"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|f| f["masked"] != true)
                    .map(|f| FieldV {
                        id: text(&f["field"]),
                        label: text(&f["label"]),
                        ty: text(&f["type"]),
                        values: f["values"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(text)
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        let actions = data["actions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| {
                let required: Vec<&str> = a["input"]["required"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                ActionV {
                    operation: text(&a["operation"]),
                    title: text(&a["title"]),
                    table: text(&a["table"]),
                    params: a["input"]["properties"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(key, schema)| Param {
                            key: key.clone(),
                            ty: text(&schema["type"]),
                            description: text(&schema["description"]),
                            choices: schema["enum"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(text)
                                .collect(),
                            required: required.contains(&key.as_str()),
                        })
                        .collect(),
                }
            })
            .collect();
        let limit = |key: &str, fallback: usize| {
            data["limits"][key]
                .as_u64()
                .map_or(fallback, |n| n as usize)
        };
        Ok(Self {
            triggers,
            actions,
            conditions: limit("conditions", 20),
            action_rows: limit("actions", 10),
            examples: limit("examples", 10),
        })
    }
    fn trigger(&self, event: &str) -> Option<&TriggerV> {
        self.triggers.iter().find(|t| t.event == event)
    }
    fn field(&self, trigger: &str, field: &str) -> Option<&FieldV> {
        self.trigger(trigger)?.fields.iter().find(|f| f.id == field)
    }
    fn action(&self, operation: &str) -> Option<&ActionV> {
        self.actions.iter().find(|a| a.operation == operation)
    }
}

#[derive(Clone, Default)]
struct Cond {
    field: String,
    op: String,
    value: String,
}
#[derive(Clone, Default)]
struct Act {
    operation: String,
    params: BTreeMap<String, String>,
}
#[derive(Clone, Default)]
struct Example {
    matches: bool,
    facts: BTreeMap<String, String>,
}

/// The contract's review of the draft as it stands.
enum Reviewed {
    Facts(Value),
    Problems(Vec<(String, String)>),
}

pub(super) enum Outcome {
    Open,
    Saved { watcher_id: i64, version: i64 },
}

pub(super) struct Editor {
    watcher_id: Option<i64>,
    /// The watcher's name when drafting its next version, for the title.
    of: String,
    name: String,
    trigger: String,
    matching: String,
    explanation: String,
    conditions: Vec<Cond>,
    actions: Vec<Act>,
    examples: Vec<Example>,
    reviewed: Option<Reviewed>,
    flash: Option<(StatusKind, String)>,
}

fn op_choices(ty: &str) -> Vec<Choice> {
    Op::ALL
        .iter()
        .filter_map(|key| Op::parse(key))
        .filter(|op| match ty {
            "boolean" => matches!(op, Op::Eq | Op::Ne | Op::Empty | Op::NotEmpty),
            "integer" | "float" | "decimal" | "date" | "datetime" => true,
            _ => !op.ordered(),
        })
        .map(|op| Choice::new(op.words(), op.key()))
        .collect()
}
fn takes_list(op: &str) -> bool {
    matches!(op, "in" | "not_in")
}
fn takes_nothing(op: &str) -> bool {
    matches!(op, "empty" | "not_empty")
}
fn lines(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}
fn shown(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Where a problem's input path points, in words.
fn place(path: &str) -> String {
    let parts: Vec<&str> = path.split('.').collect();
    let number = |i: usize| {
        parts
            .get(i)
            .and_then(|n| n.parse::<usize>().ok())
            .map(|n| n + 1)
    };
    match (parts.first().copied(), number(1), parts.get(2)) {
        (Some("conditions"), Some(n), Some(part)) => format!("Condition {n}, {part}"),
        (Some("conditions"), Some(n), None) => format!("Condition {n}"),
        (Some("actions"), Some(n), Some(part)) => format!("Action {n}, {}", part.replace('_', " ")),
        (Some("actions"), Some(n), None) => format!("Action {n}"),
        (Some("examples"), Some(n), Some(part)) => format!("Example {n}, {part}"),
        (Some("examples"), Some(n), None) => format!("Example {n}"),
        (Some("name"), ..) => "Name".into(),
        (Some("trigger"), ..) => "Trigger".into(),
        (Some("match"), ..) => "Match".into(),
        (Some("explanation"), ..) => "Asked for".into(),
        (Some(other), ..) => humanise(other),
        _ => "Watcher".into(),
    }
}

impl Editor {
    pub(super) fn new() -> Self {
        Self {
            watcher_id: None,
            of: String::new(),
            name: String::new(),
            trigger: String::new(),
            matching: "all".into(),
            explanation: String::new(),
            conditions: Vec::new(),
            actions: Vec::new(),
            examples: Vec::new(),
            reviewed: None,
            flash: None,
        }
    }

    /// The next version of a watcher, starting from its latest version.
    pub(super) fn next_version(view: &WatcherView) -> Self {
        let mut editor = Self::new();
        editor.watcher_id = Some(view.summary.id);
        editor.of = view.summary.name.clone();
        editor.name = view.summary.name.clone();
        editor.trigger = view.summary.trigger.clone();
        let Some(version) = view.versions.first() else {
            return editor;
        };
        editor.explanation = version.explanation.clone().unwrap_or_default();
        let definition = &version.definition;
        editor.matching = definition["if"]["match"]
            .as_str()
            .unwrap_or("all")
            .to_owned();
        editor.conditions = definition["if"]["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Cond {
                field: text(&c["field"]),
                op: text(&c["op"]),
                value: match &c["value"] {
                    Value::Array(items) => items.iter().map(shown).collect::<Vec<_>>().join("\n"),
                    other => shown(other),
                },
            })
            .collect();
        editor.actions = definition["do"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| Act {
                operation: text(&a["operation"]),
                params: a["input"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(key, value)| (key.clone(), shown(value)))
                    .collect(),
            })
            .collect();
        editor.examples = version
            .examples
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| Example {
                matches: e["expect"] == "match",
                facts: e["facts"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(key, value)| (key.clone(), shown(value)))
                    .collect(),
            })
            .collect();
        editor
    }

    fn row(posted: &HashMap<String, String>, len: usize) -> Result<usize> {
        posted
            .get("row")
            .and_then(|row| row.parse::<usize>().ok())
            .filter(|row| *row < len)
            .ok_or(Error::Invalid)
    }
    fn posted(value: &Value, form: &str, limit: usize) -> Result<HashMap<String, String>> {
        params::form_values_checked(value, form, limit).map_err(Error::refused)
    }
    fn index(value: &Value) -> Result<usize> {
        value
            .get("value")
            .and_then(Value::as_str)
            .and_then(|v| v.parse().ok())
            .ok_or(Error::Invalid)
    }

    /// The facts the examples are written in: the distinct fields of the conditions.
    fn used(&self) -> Vec<String> {
        let mut used: Vec<String> = Vec::new();
        for condition in &self.conditions {
            if !condition.field.is_empty() && !used.contains(&condition.field) {
                used.push(condition.field.clone());
            }
        }
        used
    }

    /// The input of `base.watchers.draft`, in the shape its schema asks for.
    fn input(&self, vocab: &Vocab) -> Value {
        let conditions: Vec<Value> = self
            .conditions
            .iter()
            .map(|c| {
                let mut item = json!({"field": c.field, "op": c.op});
                if takes_list(&c.op) {
                    item["values"] = json!(lines(&c.value));
                } else if !takes_nothing(&c.op) {
                    item["value"] = json!(c.value.trim());
                }
                item
            })
            .collect();
        let actions: Vec<Value> = self
            .actions
            .iter()
            .map(|a| {
                let mut input = serde_json::Map::new();
                let params = vocab.action(&a.operation).map(|a| a.params.as_slice());
                for (key, raw) in &a.params {
                    let raw = raw.trim();
                    if raw.is_empty() {
                        continue;
                    }
                    let ty = params
                        .and_then(|p| p.iter().find(|p| &p.key == key))
                        .map_or("string", |p| p.ty.as_str());
                    let value = match ty {
                        "integer" => raw.parse::<i64>().map_or(json!(raw), |n| json!(n)),
                        "boolean" => match raw {
                            "true" => json!(true),
                            "false" => json!(false),
                            _ => json!(raw),
                        },
                        "string" => json!(raw),
                        _ => serde_json::from_str(raw).unwrap_or_else(|_| json!(raw)),
                    };
                    input.insert(key.clone(), value);
                }
                json!({"operation": a.operation, "input": Value::Object(input)})
            })
            .collect();
        let used = self.used();
        let examples: Vec<Value> = self
            .examples
            .iter()
            .map(|e| {
                let facts: serde_json::Map<String, Value> = e
                    .facts
                    .iter()
                    .filter(|(key, value)| used.contains(key) && !value.trim().is_empty())
                    .map(|(key, value)| (key.clone(), json!(value.trim())))
                    .collect();
                json!({
                    "facts": Value::Object(facts).to_string(),
                    "expect": if e.matches { "match" } else { "no_match" },
                })
            })
            .collect();
        let mut input = json!({
            "name": self.name.trim(),
            "watch": self.trigger,
            "if": if conditions.is_empty() { Value::Null } else { json!({"match": self.matching, "conditions": conditions}) },
            "do": actions,
            "examples": examples,
        });
        if conditions.is_empty() {
            input.as_object_mut().unwrap().remove("if");
        }
        if let Some(id) = self.watcher_id {
            input["watcher_id"] = json!(id);
        }
        if !self.explanation.trim().is_empty() {
            input["explanation"] = json!(self.explanation.trim());
        }
        input
    }

    /// Applies one editor event. `Open` keeps the editor on screen; the
    /// editor is closed (`*editor = None`) on cancel or after a save.
    pub(super) async fn event(
        editor: &mut Option<Editor>,
        pages: &Pages,
        call: Call<'_>,
        kind: &str,
        value: &Value,
    ) -> Result<Outcome> {
        let Some(current) = editor.as_mut() else {
            return Ok(Outcome::Open);
        };
        current.flash = None;
        let vocab = match Vocab::load(pages, call.scope).await {
            Ok(vocab) => vocab,
            Err(ContractError::Backend(error)) => return Err(error),
            Err(_) => {
                current.flash = Some((StatusKind::Error, DENIED_DRAFT.into()));
                return Ok(Outcome::Open);
            }
        };
        match kind {
            "draft_close" => {
                *editor = None;
                return Ok(Outcome::Open);
            }
            "draft_head" => {
                let posted = Self::posted(value, "h", EXPLANATION_MAX)?;
                let get = |key: &str| posted.get(key).cloned();
                if let Some(name) = get("name") {
                    current.name = name;
                }
                if let Some(explanation) = get("explanation") {
                    current.explanation = explanation;
                }
                if let Some(matching) = get("match").filter(|m| m == "all" || m == "any") {
                    current.matching = matching;
                }
                if current.watcher_id.is_none() {
                    if let Some(trigger) = get("trigger") {
                        if trigger != current.trigger {
                            current.trigger = trigger;
                            current.conditions.clear();
                            current.actions.clear();
                            current.examples.clear();
                        }
                    }
                }
            }
            "draft_cond" => {
                let posted = Self::posted(value, "c", EVENT_LIMIT)?;
                let row = Self::row(&posted, current.conditions.len())?;
                let trigger = current.trigger.clone();
                let condition = &mut current.conditions[row];
                let get = |key: &str| posted.get(key).cloned().unwrap_or_default();
                let field = get("field");
                if field != condition.field {
                    condition.field = field;
                    condition.value.clear();
                    let ty = vocab
                        .field(&trigger, &condition.field)
                        .map_or("string", |f| f.ty.as_str());
                    if !op_choices(ty).iter().any(|o| o.value == condition.op) {
                        condition.op = "eq".into();
                    }
                } else {
                    let op = get("op");
                    if op != condition.op && !op.is_empty() {
                        if takes_nothing(&op) || takes_list(&op) != takes_list(&condition.op) {
                            condition.value.clear();
                        }
                        condition.op = op;
                    }
                    if let Some(typed) = posted.get("value") {
                        if !takes_nothing(&condition.op) {
                            condition.value = typed.clone();
                        }
                    }
                }
            }
            "draft_cond_add" => {
                if current.conditions.len() < vocab.conditions {
                    current.conditions.push(Cond {
                        op: "eq".into(),
                        ..Cond::default()
                    });
                }
            }
            "draft_cond_remove" => {
                let row = Self::index(value)?;
                if row < current.conditions.len() {
                    current.conditions.remove(row);
                }
            }
            "draft_act" => {
                let posted = Self::posted(value, "a", EVENT_LIMIT)?;
                let row = Self::row(&posted, current.actions.len())?;
                let action = &mut current.actions[row];
                let operation = posted.get("operation").cloned().unwrap_or_default();
                if operation != action.operation {
                    action.operation = operation;
                    action.params.clear();
                } else {
                    for (key, value) in &posted {
                        if let Some(key) = key.strip_prefix("p_") {
                            action.params.insert(key.to_owned(), value.clone());
                        }
                    }
                }
            }
            "draft_act_add" => {
                if current.actions.len() < vocab.action_rows {
                    current.actions.push(Act::default());
                }
            }
            "draft_act_remove" => {
                let row = Self::index(value)?;
                if row < current.actions.len() {
                    current.actions.remove(row);
                }
            }
            "draft_ex" => {
                let posted = Self::posted(value, "e", EVENT_LIMIT)?;
                let row = Self::row(&posted, current.examples.len())?;
                let example = &mut current.examples[row];
                if let Some(expect) = posted.get("expect") {
                    example.matches = expect == "match";
                }
                for (key, value) in &posted {
                    if let Some(field) = key.strip_prefix("f_") {
                        example.facts.insert(field.to_owned(), value.clone());
                    }
                }
            }
            "draft_ex_add" => {
                if current.examples.len() < vocab.examples {
                    current.examples.push(Example {
                        matches: true,
                        ..Example::default()
                    });
                }
            }
            "draft_ex_remove" => {
                let row = Self::index(value)?;
                if row < current.examples.len() {
                    current.examples.remove(row);
                }
            }
            "draft_review" => {
                let input = current.input(&vocab);
                let call = Call {
                    contract: "base.watchers.draft",
                    event: kind,
                    ..call
                };
                current.reviewed = None;
                match pages.contracts.preview(&call, input).await {
                    Ok(reply) => {
                        let first = &reply["results"][0];
                        if first["valid"] == true {
                            let facts = &first["preview"];
                            current.reviewed = Some(Reviewed::Facts(
                                facts.get("result").unwrap_or(facts).clone(),
                            ));
                        } else if first["error"]["code"] == "invalid_input" {
                            let mut problems = Vec::new();
                            if let Some(fields) = first["error"]["fields"].as_object() {
                                for (path, why) in fields {
                                    for why in why.as_array().into_iter().flatten() {
                                        problems.push((path.clone(), text(why)));
                                    }
                                }
                            }
                            current.reviewed = Some(Reviewed::Problems(problems));
                        } else {
                            current.flash = Some((StatusKind::Error, DENIED_DRAFT.into()));
                        }
                    }
                    Err(error) => current.answer(error)?,
                }
                return Ok(Outcome::Open);
            }
            "draft_save" => {
                if !matches!(current.reviewed, Some(Reviewed::Facts(_))) {
                    current.flash = Some((
                        StatusKind::Warning,
                        "Review the draft first; it is saved as reviewed.".into(),
                    ));
                    return Ok(Outcome::Open);
                }
                let input = current.input(&vocab);
                let call = Call {
                    contract: "base.watchers.draft",
                    event: kind,
                    ..call
                };
                match pages.contracts.commit(&call, input).await {
                    Ok(reply) => {
                        let outer = &reply["results"][0]["result"];
                        let result = if outer["watcher_id"].is_null() {
                            outer.get("result").unwrap_or(outer)
                        } else {
                            outer
                        };
                        let watcher_id = result["watcher_id"]
                            .as_i64()
                            .or(current.watcher_id)
                            .unwrap_or(0);
                        let version = result["version"].as_i64().unwrap_or(0);
                        *editor = None;
                        return Ok(Outcome::Saved {
                            watcher_id,
                            version,
                        });
                    }
                    Err(ContractError::InvalidInput(fields)) => {
                        let problems = fields
                            .into_iter()
                            .flat_map(|(path, why)| why.into_iter().map(move |w| (path.clone(), w)))
                            .collect();
                        current.reviewed = Some(Reviewed::Problems(problems));
                    }
                    Err(error) => current.answer(error)?,
                }
                return Ok(Outcome::Open);
            }
            _ => return Err(Error::Invalid),
        }
        current.reviewed = None;
        Ok(Outcome::Open)
    }

    fn answer(&mut self, error: ContractError) -> Result<()> {
        match error {
            ContractError::Backend(
                error @ (Error::StepUpRequired { .. } | Error::Unauthorized),
            ) => return Err(error),
            ContractError::Forbidden | ContractError::Backend(Error::Forbidden) => {
                self.flash = Some((StatusKind::Error, DENIED_DRAFT.into()))
            }
            _ => {
                self.flash = Some((
                    StatusKind::Error,
                    "The watcher could not be saved; nothing was saved.".into(),
                ))
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ rendering

    fn typed(
        id: String,
        name: String,
        label: &str,
        field: Option<&FieldV>,
        value: &str,
    ) -> Result<Html> {
        let ty = field.map_or("string", |f| f.ty.as_str());
        let choices: &[String] = field.map_or(&[], |f| f.values.as_slice());
        if !choices.is_empty() {
            return render(
                Combobox::new(id, name)
                    .label(label)
                    .value(value)
                    .placeholder("Choose…")
                    .options(choices.iter().map(|c| Choice::new(c, c)).collect()),
            );
        }
        let base = Field::named(name).id(id).label(label).value(value);
        render(match ty {
            "boolean" => base
                .kind(InputKind::Select)
                .prompt("Choose…")
                .options(vec![Choice::new("Yes", "true"), Choice::new("No", "false")]),
            "date" => base.kind(InputKind::Date),
            "datetime" => base
                .placeholder("2026-11-16T09:00:00Z")
                .attr("maxlength", &VALUE_MAX.to_string()),
            "integer" | "float" | "decimal" => base
                .attr("inputmode", "decimal")
                .attr("maxlength", &VALUE_MAX.to_string()),
            _ => base.attr("maxlength", &VALUE_MAX.to_string()),
        })
    }

    fn hidden(form: &str, id: String, row: usize) -> Result<Html> {
        render(
            Field::named(format!("{form}[row]"))
                .id(id)
                .kind(InputKind::Hidden)
                .value(row.to_string()),
        )
    }
    fn remove(id: String, event: &str, row: usize) -> Result<Html> {
        render(Button {
            id: &id,
            button_type: "button",
            event,
            name: "value",
            value: &row.to_string(),
            body: Html::text("Remove"),
            ..Button::default()
        })
    }
    fn add(id: &str, label: &str, event: &str, disabled: bool) -> Result<Html> {
        render(Button {
            id,
            button_type: "button",
            event,
            disabled,
            body: Html::text(label),
            ..Button::default()
        })
    }
    fn section(
        title: &str,
        id: &str,
        description: &str,
        rows: Vec<Html>,
        add: Html,
    ) -> Result<Html> {
        let mut html = render(SectionHeading {
            title,
            id,
            description,
            ..SectionHeading::default()
        })?;
        if !rows.is_empty() {
            html.push(&render(Rows { parts: rows })?);
        }
        html.push(&add);
        Ok(html)
    }

    fn head(&self, vocab: &Vocab) -> Result<Html> {
        let mut body = render(
            Field::named("h[name]")
                .id("watcher-draft-name")
                .label("Name")
                .value(&self.name)
                .attr("maxlength", &NAME_MAX.to_string())
                .required(),
        )?;
        if self.watcher_id.is_some() {
            let title = vocab
                .trigger(&self.trigger)
                .map_or(self.trigger.as_str(), |t| t.title.as_str());
            body.push(&render(
                Field::named("h[trigger]")
                    .id("watcher-draft-trigger")
                    .label("When")
                    .value(title)
                    .hint("A watcher keeps its trigger; draft a new watcher for another one.")
                    .readonly(),
            )?);
        } else {
            body.push(&render(
                Combobox::new("watcher-draft-trigger", "h[trigger]")
                    .label("When this happens")
                    .value(&self.trigger)
                    .placeholder("Search what happens…")
                    .options(
                        vocab
                            .triggers
                            .iter()
                            .map(|t| Choice::new(&t.title, &t.event))
                            .collect(),
                    ),
            )?);
        }
        body.push(&render(
            Field::named("h[match]")
                .id("watcher-draft-match")
                .label("Run the actions when")
                .kind(InputKind::Select)
                .options(vec![
                    Choice::new("All of the conditions hold", "all"),
                    Choice::new("Any of the conditions holds", "any"),
                ])
                .value(&self.matching),
        )?);
        body.push(&render(
            Field::named("h[explanation]")
                .id("watcher-draft-explanation")
                .label("Asked for (optional)")
                .kind(InputKind::Textarea)
                .value(&self.explanation)
                .hint("The request in the words it was first made. It is kept with this version.")
                .attr("rows", "2")
                .attr("maxlength", &EXPLANATION_MAX.to_string()),
        )?);
        render(Form {
            id: "watcher-draft-head",
            event: "draft_head",
            change: true,
            body,
            ..Form::default()
        })
    }

    fn condition_rows(&self, vocab: &Vocab) -> Result<Vec<Html>> {
        let trigger = vocab.trigger(&self.trigger);
        let mut rows = Vec::new();
        for (i, condition) in self.conditions.iter().enumerate() {
            let dom = format!("watcher-draft-cond-{i}");
            let field = vocab.field(&self.trigger, &condition.field);
            let mut body = Self::hidden("c", format!("{dom}-row"), i)?;
            body.push(&render(
                Combobox::new(format!("{dom}-field"), "c[field]")
                    .label("Field")
                    .value(&condition.field)
                    .placeholder("Search fields…")
                    .options(
                        trigger
                            .map(|t| {
                                t.fields
                                    .iter()
                                    .map(|f| Choice::new(&f.label, &f.id))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    ),
            )?);
            if field.is_some() {
                body.push(&render(
                    Field::named("c[op]")
                        .id(format!("{dom}-op"))
                        .label("Is")
                        .kind(InputKind::Select)
                        .options(op_choices(field.map_or("string", |f| f.ty.as_str())))
                        .value(&condition.op),
                )?);
                if takes_list(&condition.op) {
                    body.push(&render(
                        Field::named("c[value]")
                            .id(format!("{dom}-value"))
                            .label("Values")
                            .kind(InputKind::Textarea)
                            .value(&condition.value)
                            .hint("One value on each line.")
                            .attr("rows", "3"),
                    )?);
                } else if !takes_nothing(&condition.op) {
                    body.push(&Self::typed(
                        format!("{dom}-value"),
                        "c[value]".into(),
                        "Value",
                        field,
                        &condition.value,
                    )?);
                }
            }
            let actions = Self::remove(format!("{dom}-remove"), "draft_cond_remove", i)?;
            rows.push(render(Form {
                id: &dom,
                event: "draft_cond",
                change: true,
                body,
                actions,
                ..Form::default()
            })?);
        }
        Ok(rows)
    }

    fn action_rows(&self, vocab: &Vocab) -> Result<Vec<Html>> {
        let table = vocab.trigger(&self.trigger).map(|t| t.table.as_str());
        let mut rows = Vec::new();
        for (i, action) in self.actions.iter().enumerate() {
            let dom = format!("watcher-draft-act-{i}");
            let mut body = Self::hidden("a", format!("{dom}-row"), i)?;
            body.push(&render(
                Combobox::new(format!("{dom}-operation"), "a[operation]")
                    .label("Do")
                    .value(&action.operation)
                    .placeholder("Search actions…")
                    .options(
                        vocab
                            .actions
                            .iter()
                            .filter(|a| Some(a.table.as_str()) == table)
                            .map(|a| Choice::new(&a.title, &a.operation))
                            .collect(),
                    ),
            )?);
            if let Some(chosen) = vocab.action(&action.operation) {
                for param in &chosen.params {
                    let value = action.params.get(&param.key).cloned().unwrap_or_default();
                    let label = if param.required {
                        humanise(&param.key)
                    } else {
                        format!("{} (optional)", humanise(&param.key))
                    };
                    let id = format!("{dom}-p-{}", param.key);
                    let name = format!("a[p_{}]", param.key);
                    if param.choices.is_empty() {
                        let mut field = Field::named(name).id(id).label(&label).value(&value);
                        if param.ty == "boolean" {
                            field = field
                                .kind(InputKind::Select)
                                .prompt("Choose…")
                                .options(vec![
                                    Choice::new("Yes", "true"),
                                    Choice::new("No", "false"),
                                ]);
                        } else {
                            field = field.attr("maxlength", &VALUE_MAX.to_string());
                        }
                        body.push(&render(field.hint(&param.description))?);
                    } else {
                        body.push(&render(
                            Combobox::new(id, name)
                                .label(&label)
                                .value(&value)
                                .placeholder("Choose…")
                                .options(param.choices.iter().map(|c| Choice::new(c, c)).collect())
                                .hint(&param.description),
                        )?);
                    }
                }
            }
            let actions = Self::remove(format!("{dom}-remove"), "draft_act_remove", i)?;
            rows.push(render(Form {
                id: &dom,
                event: "draft_act",
                change: true,
                body,
                actions,
                ..Form::default()
            })?);
        }
        Ok(rows)
    }

    fn example_rows(&self, vocab: &Vocab) -> Result<Vec<Html>> {
        let used = self.used();
        let mut rows = Vec::new();
        for (i, example) in self.examples.iter().enumerate() {
            let dom = format!("watcher-draft-ex-{i}");
            let mut body = Self::hidden("e", format!("{dom}-row"), i)?;
            body.push(&render(
                Field::named("e[expect]")
                    .id(format!("{dom}-expect"))
                    .label("A record like this")
                    .kind(InputKind::Select)
                    .options(vec![
                        Choice::new("Matches", "match"),
                        Choice::new("Does not match", "no_match"),
                    ])
                    .value(if example.matches { "match" } else { "no_match" }),
            )?);
            for path in &used {
                let field = vocab.field(&self.trigger, path);
                let label = field.map_or(path.as_str(), |f| f.label.as_str());
                body.push(&Self::typed(
                    format!("{dom}-f-{path}"),
                    format!("e[f_{path}]"),
                    label,
                    field,
                    example.facts.get(path).map_or("", String::as_str),
                )?);
            }
            let actions = Self::remove(format!("{dom}-remove"), "draft_ex_remove", i)?;
            rows.push(render(Form {
                id: &dom,
                event: "draft_ex",
                change: true,
                body,
                actions,
                ..Form::default()
            })?);
        }
        Ok(rows)
    }

    /// The contract's review, in the words the agent sees.
    fn review(&self) -> Result<Html> {
        let Some(reviewed) = &self.reviewed else {
            return Ok(Html::EMPTY);
        };
        match reviewed {
            Reviewed::Problems(problems) => {
                let mut lines: Vec<String> = problems
                    .iter()
                    .map(|(path, why)| format!("{}: {}.", place(path), why.trim_end_matches('.')))
                    .collect();
                if lines.is_empty() {
                    lines.push("The draft was refused.".into());
                }
                render(Alert {
                    kind: StatusKind::Error,
                    id: "watcher-draft-problems",
                    class: "",
                    body: render(Bullets { lines: &lines })?,
                })
            }
            Reviewed::Facts(facts) => {
                let list = |key: &str| -> Vec<String> {
                    facts[key]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(text)
                        .collect()
                };
                let conditions = list("conditions");
                let condition = if conditions.is_empty() {
                    Html::text("None: every event matches")
                } else {
                    let mut html = Html::text(if self.matching == "any" {
                        "Any of these holds"
                    } else {
                        "All of these hold"
                    });
                    html.push(&render(Bullets { lines: &conditions })?);
                    html
                };
                let examples: Vec<String> = facts["examples"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(i, e)| {
                        let result = if e["result"] == "match" {
                            "matches"
                        } else {
                            "does not match"
                        };
                        let expected = if e["expect"] == "match" {
                            "should match"
                        } else {
                            "should not match"
                        };
                        format!(
                            "Example {}: {expected}, and it {result}{}",
                            i + 1,
                            if e["passes"] == true {
                                "."
                            } else {
                                " (not as stated)."
                            }
                        )
                    })
                    .collect();
                let mut body = render(SectionHeading {
                    title: "Review",
                    id: "watcher-draft-review-heading",
                    description:
                        "What this version does. Nothing runs until an administrator approves it.",
                    ..SectionHeading::default()
                })?;
                body.push(&render(Facts {
                    id: "watcher-draft-review",
                    items: vec![
                        Fact {
                            title: "When",
                            id: "",
                            value: Html::text(&text(&facts["trigger"])),
                        },
                        Fact {
                            title: "If",
                            id: "",
                            value: condition,
                        },
                        Fact {
                            title: "Then",
                            id: "",
                            value: render(Bullets {
                                lines: &list("actions"),
                            })?,
                        },
                        Fact {
                            title: "Examples",
                            id: "",
                            value: render(Lines { lines: &examples })?,
                        },
                    ],
                })?);
                let warnings = list("warnings");
                if !warnings.is_empty() {
                    body.push(&render(Alert {
                        kind: StatusKind::Warning,
                        id: "watcher-draft-warnings",
                        class: "",
                        body: render(Bullets { lines: &warnings })?,
                    })?);
                }
                Ok(body)
            }
        }
    }

    pub(super) async fn dialog(&self, pages: &Pages, scope: &Scope) -> Result<Html> {
        let vocab = match Vocab::load(pages, scope).await {
            Ok(vocab) => vocab,
            Err(ContractError::Backend(error)) => return Err(error),
            Err(_) => return Ok(Html::EMPTY),
        };
        let trigger_chosen = vocab.trigger(&self.trigger).is_some();
        let mut parts = vec![self.head(&vocab)?];
        parts.push(Self::section(
            "If",
            "watcher-draft-if",
            if trigger_chosen {
                "Conditions on the record the event is about. Leave none for every event."
            } else {
                "Choose what happens first; the fields of its record appear here."
            },
            self.condition_rows(&vocab)?,
            Self::add(
                "watcher-draft-cond-add",
                "Add condition",
                "draft_cond_add",
                !trigger_chosen || self.conditions.len() >= vocab.conditions,
            )?,
        )?);
        parts.push(Self::section(
            "Then",
            "watcher-draft-then",
            if trigger_chosen {
                "What the watcher does to that record, in order."
            } else {
                "Choose what happens first; the actions that fit its record appear here."
            },
            self.action_rows(&vocab)?,
            Self::add(
                "watcher-draft-act-add",
                "Add action",
                "draft_act_add",
                !trigger_chosen || self.actions.len() >= vocab.action_rows,
            )?,
        )?);
        parts.push(Self::section(
            "Examples",
            "watcher-draft-examples",
            "Records that should and should not match. The review shows how each one comes out.",
            self.example_rows(&vocab)?,
            Self::add(
                "watcher-draft-ex-add",
                "Add example",
                "draft_ex_add",
                !trigger_chosen || self.examples.len() >= vocab.examples,
            )?,
        )?);
        parts.push(self.review()?);
        let saved_review = matches!(self.reviewed, Some(Reviewed::Facts(_)));
        let mut actions = render(Button {
            id: "watcher-draft-cancel",
            button_type: "button",
            event: "draft_close",
            body: Html::text("Cancel"),
            ..Button::default()
        })?;
        actions.push(&render(Button {
            id: "watcher-draft-review-button",
            variant: if saved_review {
                ButtonVariant::Secondary
            } else {
                ButtonVariant::Primary
            },
            button_type: "submit",
            working: "Reviewing…",
            body: Html::text("Review"),
            ..Button::default()
        })?);
        actions.push(&render(Button {
            id: "watcher-draft-save",
            variant: ButtonVariant::Primary,
            button_type: "button",
            event: "draft_save",
            working: "Saving…",
            disabled: !saved_review,
            body: Html::text("Save draft"),
            ..Button::default()
        })?);
        let footer = render(Form {
            id: "watcher-draft-review-form",
            event: "draft_review",
            body: Html::EMPTY,
            actions,
            ..Form::default()
        })?;
        parts.push(footer);
        let title = match self.watcher_id {
            Some(_) => format!("Draft next version of \u{201c}{}\u{201d}", self.of),
            None => "New watcher".into(),
        };
        render(Modal {
            id: "watcher-draft-dialog",
            title: &title,
            description:
                "Saved as a version for review. It runs only once an administrator approves it.",
            cancel_event: "draft_close",
            width: ModalWidth::Wide,
            flash: Some(flash_map(&self.flash)),
            body: render(Stack { parts })?,
            ..Modal::default()
        })
    }
}
