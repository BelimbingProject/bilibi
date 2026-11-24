//! A stored watcher version in words, for the pages that read it.
use crate::definition::{self, Definition};
use crate::review::is_linked_custom;
use crate::store::Watchers;
use bilibi_grid::Catalog;
use serde_json::{json, Value};

/// A stored version in words, for reading: conditions with their fields'
/// labels, actions with their titles and parameters, examples. Direct linked
/// custom fields use the reader's labels, such as `Company › Segment`.
pub(crate) struct Described {
    pub scripted: bool,
    /// The AI observation in words, with the fields it sends.
    pub observation: Option<String>,
    pub matching: &'static str,
    pub conditions: Vec<String>,
    pub actions: Vec<String>,
    pub examples: Vec<String>,
}
/// A condition's subject in words: a record field's label, or the AI's answer.
pub(crate) fn condition_label(path: &str, label: &dyn Fn(&str) -> String) -> String {
    match path {
        crate::observe::LABEL => "AI label".to_owned(),
        _ => label(path),
    }
}

pub(crate) fn needs_linked_labels(definition: &Value, examples: Option<&Value>) -> bool {
    let mut problems = Vec::new();
    let Some(parsed) = Definition::parse(definition, &mut problems) else {
        return false;
    };
    parsed.conditions.iter().any(|c| is_linked_custom(&c.field))
        || examples.is_some_and(|examples| {
            definition::parse_examples(examples, &mut problems)
                .iter()
                .any(|example| example.facts.keys().any(|path| is_linked_custom(path)))
        })
}

impl Watchers {
    pub(crate) fn describe(
        &self,
        reader: Option<&Catalog>,
        trigger: &str,
        definition: &Value,
        examples: &Value,
    ) -> Described {
        let mut problems = Vec::new();
        let parsed = Definition::parse(definition, &mut problems);
        let table = self
            .trigger(trigger)
            .and_then(|t| bilibi_grid::installed().ok()?.table(t.definition.table));
        let label = |path: &str| {
            reader
                .filter(|_| is_linked_custom(path))
                .and_then(|catalog| {
                    let root = catalog.table(table?.id())?;
                    catalog.resolve(root, path).ok().map(|column| column.label)
                })
                .or_else(|| {
                    table
                        .and_then(|t| t.field(path))
                        .map(|f| f.label.to_owned())
                })
                .unwrap_or_else(|| path.to_owned())
        };
        let Some(parsed) = parsed else {
            return Described {
                scripted: false,
                observation: None,
                matching: "all",
                conditions: Vec::new(),
                actions: Vec::new(),
                examples: Vec::new(),
            };
        };
        let conditions = parsed
            .conditions
            .iter()
            .map(|c| definition::sentence(c, &condition_label(&c.field, &label)))
            .collect();
        let observation = parsed
            .observe
            .as_ref()
            .map(|observe| observe.describe(label));
        let actions = parsed
            .steps
            .iter()
            .map(|step| {
                let a = match step {
                    crate::definition::Step::Script { script } => {
                        return script.describe();
                    }
                    crate::definition::Step::Operation(call) => call,
                };
                let title = self
                    .action(&a.operation)
                    .map_or(a.operation.as_str(), |o| o.title);
                let parameters: Vec<String> = a
                    .input
                    .iter()
                    .map(|(key, value)| format!("{key}: {}", definition::shown(value)))
                    .collect();
                if parameters.is_empty() {
                    title.to_owned()
                } else {
                    format!("{title} ({})", parameters.join(", "))
                }
            })
            .collect();
        let mut examples: Vec<String> = definition::parse_examples(examples, &mut problems)
            .iter()
            .map(|e| {
                let facts: Vec<String> = e
                    .facts
                    .iter()
                    .map(|(field, value)| {
                        format!(
                            "{} {}",
                            condition_label(field, &label),
                            definition::shown(value)
                        )
                    })
                    .collect();
                format!(
                    "{}: {}",
                    if e.expect {
                        "Matches"
                    } else {
                        "Does not match"
                    },
                    if facts.is_empty() {
                        "any record".to_owned()
                    } else {
                        facts.join(", ")
                    }
                )
            })
            .collect();
        for script in parsed.scripts() {
            for (index, example) in script.examples.iter().enumerate() {
                examples.push(format!(
                    "{} example {} with {} expects {}.",
                    script.name,
                    index + 1,
                    json!(example.facts),
                    json!(example.expect)
                ));
            }
        }
        let scripted = parsed.scripts().next().is_some();
        Described {
            scripted,
            observation,
            matching: parsed.matching.key(),
            conditions,
            actions,
            examples,
        }
    }
}
