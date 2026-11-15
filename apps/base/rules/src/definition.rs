//! The rule record's shape and its pure evaluation: no I/O, no side effects,
//! no AI. A definition is a match mode, conditions over a trigger table's
//! fields and the actions to run when it matches. Values compare exactly:
//! numbers as decimals (never floating point), dates and times as instants,
//! text as written. A missing value matches only `empty`.
use bilibi_grid::FieldType;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};
use std::str::FromStr;

/// Bounds on one rule's size; the agent schema states the same.
pub(crate) const MAX_CONDITIONS: usize = 20;
pub(crate) const MAX_ACTIONS: usize = 10;
pub(crate) const MAX_EXAMPLES: usize = 10;
pub(crate) const MAX_CHOICES: usize = 50;
const MAX_TEXT: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Match {
    All,
    Any,
}
impl Match {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Op {
    Eq,
    Ne,
    In,
    NotIn,
    Lt,
    Le,
    Gt,
    Ge,
    Empty,
    NotEmpty,
}
impl Op {
    pub(crate) const ALL: [&'static str; 10] = [
        "eq",
        "ne",
        "in",
        "not_in",
        "lt",
        "le",
        "gt",
        "ge",
        "empty",
        "not_empty",
    ];
    fn parse(key: &str) -> Option<Self> {
        Some(match key {
            "eq" => Self::Eq,
            "ne" => Self::Ne,
            "in" => Self::In,
            "not_in" => Self::NotIn,
            "lt" => Self::Lt,
            "le" => Self::Le,
            "gt" => Self::Gt,
            "ge" => Self::Ge,
            "empty" => Self::Empty,
            "not_empty" => Self::NotEmpty,
            _ => return None,
        })
    }
    pub(crate) fn key(self) -> &'static str {
        Self::ALL[self as usize]
    }
    /// How a person reads it: "is", "is one of", "is above" …
    pub(crate) fn words(self) -> &'static str {
        match self {
            Self::Eq => "is",
            Self::Ne => "is not",
            Self::In => "is one of",
            Self::NotIn => "is none of",
            Self::Lt => "is below",
            Self::Le => "is at most",
            Self::Gt => "is above",
            Self::Ge => "is at least",
            Self::Empty => "is empty",
            Self::NotEmpty => "is not empty",
        }
    }
    fn ordered(self) -> bool {
        matches!(self, Self::Lt | Self::Le | Self::Gt | Self::Ge)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Condition {
    /// A Grid column path on the trigger's table (`priority`, `company.name`).
    pub field: String,
    pub op: Op,
    pub value: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct ActionCall {
    /// An installed action: an owner's write contract key.
    pub operation: String,
    /// Its approved parameters. The action's subject field is never here: the
    /// runner binds it to the trigger's record.
    pub input: Map<String, Value>,
}

#[derive(Clone, Debug)]
pub(crate) struct Definition {
    pub matching: Match,
    pub conditions: Vec<Condition>,
    pub actions: Vec<ActionCall>,
}

#[derive(Clone, Debug)]
pub(crate) struct Example {
    pub facts: Map<String, Value>,
    pub expect: bool,
}

/// One field problem, keyed by the input path the agent schema uses.
pub(crate) type Problems = Vec<(String, String)>;

impl Definition {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "match": self.matching.key(),
            "conditions": self.conditions.iter().map(|c| json!({"field": c.field, "op": c.op.key(), "value": c.value})).collect::<Vec<_>>(),
            "actions": self.actions.iter().map(|a| json!({"operation": a.operation, "input": a.input})).collect::<Vec<_>>(),
        })
    }
    /// Reads a stored or submitted definition; shape problems are listed by path.
    pub(crate) fn parse(value: &Value, problems: &mut Problems) -> Option<Self> {
        let matching = match value.get("match").and_then(Value::as_str).unwrap_or("all") {
            "all" => Match::All,
            "any" => Match::Any,
            _ => {
                problems.push(("match".into(), "must be all or any".into()));
                Match::All
            }
        };
        let mut conditions = Vec::new();
        let listed = value
            .get("conditions")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        if listed.len() > MAX_CONDITIONS {
            problems.push((
                "conditions".into(),
                format!("at most {MAX_CONDITIONS} conditions"),
            ));
        }
        for (i, item) in listed.iter().take(MAX_CONDITIONS).enumerate() {
            let at = |part: &str| format!("conditions.{i}.{part}");
            let field = item
                .get("field")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if bilibi_grid::column::parse(field).is_err() {
                problems.push((at("field"), "is not a field path".into()));
            }
            let Some(op) = item.get("op").and_then(Value::as_str).and_then(Op::parse) else {
                problems.push((at("op"), format!("must be one of {}", Op::ALL.join(", "))));
                continue;
            };
            let value = item.get("value").cloned().unwrap_or(Value::Null);
            match op {
                Op::Empty | Op::NotEmpty if !value.is_null() => {
                    problems.push((at("value"), "takes no value".into()))
                }
                Op::In | Op::NotIn => match value.as_array() {
                    Some(items)
                        if (1..=MAX_CHOICES).contains(&items.len()) && items.iter().all(scalar) => {
                    }
                    _ => problems.push((
                        at("value"),
                        format!("must be a list of 1 to {MAX_CHOICES} values"),
                    )),
                },
                Op::Empty | Op::NotEmpty => {}
                _ if !scalar(&value) => {
                    problems.push((at("value"), "must be one text, number or true/false".into()))
                }
                _ => {}
            }
            conditions.push(Condition {
                field: field.to_owned(),
                op,
                value,
            });
        }
        let mut actions = Vec::new();
        let listed = value
            .get("actions")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        if listed.is_empty() || listed.len() > MAX_ACTIONS {
            problems.push(("actions".into(), format!("give 1 to {MAX_ACTIONS} actions")));
        }
        for (i, item) in listed.iter().take(MAX_ACTIONS).enumerate() {
            let operation = item
                .get("operation")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let input = item
                .get("input")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if operation.is_empty() {
                problems.push((format!("actions.{i}.operation"), "is required".into()));
            }
            actions.push(ActionCall {
                operation: operation.to_owned(),
                input,
            });
        }
        Some(Self {
            matching,
            conditions,
            actions,
        })
    }

    /// Checks each condition's value against its field's type and choices.
    pub(crate) fn check_types(
        &self,
        field: impl Fn(&str) -> Option<(FieldType, &'static [&'static str])>,
        problems: &mut Problems,
    ) {
        for (i, condition) in self.conditions.iter().enumerate() {
            let at = |part: &str| format!("conditions.{i}.{part}");
            let Some((ty, choices)) = field(&condition.field) else {
                problems.push((
                    at("field"),
                    format!("{} is not a field this trigger offers", condition.field),
                ));
                continue;
            };
            if condition.op.ordered() && !ty.numeric() {
                problems.push((
                    at("op"),
                    format!("{} only applies to numbers and dates", condition.op.key()),
                ));
            }
            if ty == FieldType::Boolean
                && !matches!(condition.op, Op::Eq | Op::Ne | Op::Empty | Op::NotEmpty)
            {
                problems.push((at("op"), "true/false fields take is or is not".into()));
            }
            let values: Vec<&Value> = match &condition.value {
                Value::Array(items) => items.iter().collect(),
                Value::Null => Vec::new(),
                value => vec![value],
            };
            for value in values {
                if let Err(why) = typed(ty, choices, value) {
                    problems.push((at("value"), why));
                }
            }
        }
    }

    /// Each condition's outcome on `facts` (field path → value), and whether
    /// the definition matches.
    pub(crate) fn evaluate(
        &self,
        facts: &Map<String, Value>,
        types: &impl Fn(&str) -> FieldType,
    ) -> (bool, Vec<bool>) {
        let outcomes: Vec<bool> = self
            .conditions
            .iter()
            .map(|c| {
                holds(
                    c,
                    facts.get(&c.field).unwrap_or(&Value::Null),
                    types(&c.field),
                )
            })
            .collect();
        let matched = match self.matching {
            Match::All => outcomes.iter().all(|o| *o),
            Match::Any => outcomes.is_empty() || outcomes.iter().any(|o| *o),
        };
        (matched, outcomes)
    }

    /// The fields the conditions read, each once, in order.
    pub(crate) fn fields(&self) -> Vec<String> {
        let mut fields: Vec<String> = Vec::new();
        for condition in &self.conditions {
            if !fields.contains(&condition.field) {
                fields.push(condition.field.clone());
            }
        }
        fields
    }
}

pub(crate) fn parse_examples(value: &Value, problems: &mut Problems) -> Vec<Example> {
    let listed = value.as_array().map_or(&[][..], Vec::as_slice);
    if listed.is_empty() || listed.len() > MAX_EXAMPLES {
        problems.push((
            "examples".into(),
            format!(
                "give 1 to {MAX_EXAMPLES} examples of records that should and should not match"
            ),
        ));
    }
    listed
        .iter()
        .take(MAX_EXAMPLES)
        .enumerate()
        .filter_map(|(i, item)| {
            let facts = item.get("facts").and_then(Value::as_object).cloned();
            let expect = match item.get("expect").and_then(Value::as_str) {
                Some("match") => Some(true),
                Some("no_match") => Some(false),
                _ => None,
            };
            match (facts, expect) {
                (Some(facts), Some(expect)) if facts.values().all(|v| v.is_null() || scalar(v)) => {
                    Some(Example { facts, expect })
                }
                _ => {
                    problems.push((
                        format!("examples.{i}"),
                        "needs facts (field → value) and expect (match or no_match)".into(),
                    ));
                    None
                }
            }
        })
        .collect()
}
pub(crate) fn examples_json(examples: &[Example]) -> Value {
    Value::Array(
        examples
            .iter()
            .map(|e| json!({"facts": e.facts, "expect": if e.expect {"match"} else {"no_match"}}))
            .collect(),
    )
}

fn scalar(value: &Value) -> bool {
    match value {
        Value::String(text) => text.chars().count() <= MAX_TEXT,
        Value::Number(_) | Value::Bool(_) => true,
        _ => false,
    }
}

/// Whether `value` is a value of a field of type `ty`.
fn typed(ty: FieldType, choices: &[&str], value: &Value) -> Result<(), String> {
    match ty {
        FieldType::Integer | FieldType::Float | FieldType::Decimal => decimal(value)
            .map(|_| ())
            .ok_or_else(|| "must be a number representable exactly; write decimals as text".into()),
        FieldType::Boolean => boolean(value)
            .map(|_| ())
            .ok_or_else(|| "must be true or false".into()),
        FieldType::Date => value
            .as_str()
            .and_then(|text| jiff::civil::Date::from_str(text).ok())
            .map(|_| ())
            .ok_or_else(|| "must be a date (2026-11-16)".into()),
        FieldType::DateTime => value
            .as_str()
            .and_then(|text| jiff::Timestamp::from_str(text).ok())
            .map(|_| ())
            .ok_or_else(|| "must be a time with an offset (2026-11-16T09:00:00Z)".into()),
        FieldType::String | FieldType::Enum => match value.as_str() {
            Some(text) if choices.is_empty() || choices.contains(&text) => Ok(()),
            Some(_) => Err(format!("must be one of {}", choices.join(", "))),
            None => Err("must be text".into()),
        },
    }
}

/// `true`/`false`, as JSON or as the text the agent schema carries.
fn boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(text) => match text.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// An exact number: a JSON number or a decimal string, never a float.
fn decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            Decimal::from_str_exact(&number.to_string()).ok()
        }
        Value::String(text) => Decimal::from_str_exact(text.trim()).ok(),
        _ => None,
    }
}

fn holds(condition: &Condition, fact: &Value, ty: FieldType) -> bool {
    let present = !(fact.is_null() || fact.as_str().is_some_and(str::is_empty));
    match condition.op {
        Op::Empty => return !present,
        Op::NotEmpty => return present,
        _ if !present => return false,
        _ => {}
    }
    let compare = |expected: &Value| order(ty, fact, expected);
    match condition.op {
        Op::Eq => compare(&condition.value) == Some(std::cmp::Ordering::Equal),
        Op::Ne => compare(&condition.value).is_some_and(|o| o != std::cmp::Ordering::Equal),
        Op::In => condition.value.as_array().is_some_and(|items| {
            items
                .iter()
                .any(|v| compare(v) == Some(std::cmp::Ordering::Equal))
        }),
        Op::NotIn => condition.value.as_array().is_some_and(|items| {
            items
                .iter()
                .all(|v| compare(v).is_some_and(|o| o != std::cmp::Ordering::Equal))
        }),
        Op::Lt => compare(&condition.value) == Some(std::cmp::Ordering::Less),
        Op::Le => compare(&condition.value).is_some_and(|o| o != std::cmp::Ordering::Greater),
        Op::Gt => compare(&condition.value) == Some(std::cmp::Ordering::Greater),
        Op::Ge => compare(&condition.value).is_some_and(|o| o != std::cmp::Ordering::Less),
        Op::Empty | Op::NotEmpty => unreachable!("answered above"),
    }
}

/// `fact` compared with `expected` under the field's type; `None` when either
/// is not a value of that type (the condition is then false).
fn order(ty: FieldType, fact: &Value, expected: &Value) -> Option<std::cmp::Ordering> {
    match ty {
        FieldType::Integer | FieldType::Float | FieldType::Decimal => {
            Some(decimal(fact)?.cmp(&decimal(expected)?))
        }
        FieldType::Boolean => Some(boolean(fact)?.cmp(&boolean(expected)?)),
        FieldType::Date => {
            let date = |v: &Value| jiff::civil::Date::from_str(v.as_str()?.get(..10)?).ok();
            Some(date(fact)?.cmp(&date(expected)?))
        }
        FieldType::DateTime => {
            let at = |v: &Value| jiff::Timestamp::from_str(v.as_str()?).ok();
            Some(at(fact)?.cmp(&at(expected)?))
        }
        FieldType::String | FieldType::Enum => Some(fact.as_str()?.cmp(expected.as_str()?)),
    }
}

/// "Priority is urgent", "Amount is above 1000", "Queue is one of a, b".
pub(crate) fn sentence(condition: &Condition, label: &str) -> String {
    let value = match &condition.value {
        Value::Null => String::new(),
        Value::Array(items) => items.iter().map(shown).collect::<Vec<_>>().join(", "),
        value => shown(value),
    };
    if value.is_empty() {
        format!("{label} {}", condition.op.words())
    } else {
        format!("{label} {} {value}", condition.op.words())
    }
}
pub(crate) fn shown(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "—".into(),
        value => value.to_string(),
    }
}
