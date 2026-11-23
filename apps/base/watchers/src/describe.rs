//! A stored watcher version in words, for the pages that read it.
use crate::definition::{self, Definition};
use crate::store::Watchers;
use bilibi_grid::Catalog;
use serde_json::{Value, json};

/// A stored version in words, for reading: conditions with their fields'
/// labels, actions with their titles and parameters, examples. Direct linked
/// custom fields use the reader's labels, such as `Company › Segment`.
pub(crate) struct Described {
    pub scripted: bool,
    pub matching: &'static str,
    pub conditions: Vec<String>,
    pub actions: Vec<String>,
    pub examples: Vec<String>,
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
            crate::review::custom_path(path)
                .and_then(|(link, _)| link)
                .and(reader)
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
                matching: "all",
                conditions: Vec::new(),
                actions: Vec::new(),
                examples: Vec::new(),
            };
        };
        let conditions = parsed
            .conditions
            .iter()
            .map(|c| definition::sentence(c, &label(&c.field)))
            .collect();
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
                    .map(|(field, value)| format!("{} {}", label(field), definition::shown(value)))
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
            matching: parsed.matching.key(),
            conditions,
            actions,
            examples,
        }
    }
}
