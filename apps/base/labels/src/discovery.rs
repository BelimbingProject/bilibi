//! "What can I change here?" for agents: one paged read over the sections
//! owners contribute for the Customisations page. Each section is read
//! through its owner's operation on the caller's connection, so the owner's
//! permission checks, field restrictions and tenant scope decide what shows.
use crate::{Changes, Choice, CustomisationSection};
use bilibi_agent_api::{
    json::{Data, Output, Page},
    reach, text_field, Channels, Definitions, Error as AgentError, Handler, Kind, Operation,
    OperationClient, OperationFuture, ReadHandler, Registry, DEFINITIONS, SIGNED_IN,
};
use bilibi_database::Error;
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};

pub(crate) const KEY: &str = "base.labels.customisations.discover";
const DEFAULT_PAGE_SIZE: u64 = 25;

struct Bound {
    registry: Arc<Registry>,
    sections: Vec<(&'static str, &'static CustomisationSection)>,
}
static BOUND: OnceLock<Bound> = OnceLock::new();

/// Checks every section's declared operations against the registry, then
/// serves them. A section that names a missing or wrong kind of operation
/// stops the boot.
pub(crate) fn bind(
    registry: Arc<Registry>,
    sections: Vec<(&'static str, &'static CustomisationSection)>,
) -> bilibi_module_registry::Result<()> {
    let kind = |key: &str| registry.operations.get(key).map(|op| op.definition.kind());
    let safe_write = |key: &str| {
        registry
            .operations
            .get(key)
            .is_some_and(|op| op.definition.safe_write())
    };
    for (id, section) in &sections {
        let Some(changes) = &section.changes else {
            continue;
        };
        let reads_direct = !changes.reads.is_empty()
            && changes.reads.iter().all(|read| {
                registry.operations.get(read.operation).is_some_and(|op| {
                    matches!(op.definition.handler, Handler::Read(handler) if !handler.requires_transaction())
                })
            });
        let valid = !changes.purpose.is_empty()
            && !changes.limits.is_empty()
            && reads_direct
            && kind(changes.verify) == Some(Kind::Read)
            && changes
                .preview
                .is_none_or(|key| kind(key) == Some(Kind::Read))
            && !changes.change.is_empty()
            && changes.change.iter().all(|key| safe_write(key))
            && changes.restore.is_none_or(safe_write)
            && registry.guides.contains_key(changes.guide);
        if !valid {
            return Err(RegistryError::Contribution(format!(
                "customisation section {id} names an operation or guide that is missing or of the wrong kind"
            )));
        }
    }
    BOUND
        .set(Bound { registry, sections })
        .map_err(|_| RegistryError::Service("duplicate customisation discovery binding".into()))
}

impl Bound {
    fn available(&self, key: &str, scope: &Scope, held: &BTreeSet<String>) -> bool {
        self.registry.operations.get(key).is_some_and(|op| {
            op.definition.channels.allows(scope.channel())
                && reach::allows(op.definition.capability, scope, held)
        })
    }
    fn operation(&self, key: &str, scope: &Scope, held: &BTreeSet<String>) -> Value {
        let title = self
            .registry
            .operations
            .get(key)
            .map(|op| op.definition.title);
        json!({"key": key, "title": title, "available": self.available(key, scope, held)})
    }
    fn available_writes(
        &self,
        changes: &Changes,
        choice: &Choice,
        scope: &Scope,
        held: &BTreeSet<String>,
    ) -> Vec<&'static str> {
        changes
            .change
            .iter()
            .copied()
            .chain(changes.restore)
            .filter(|key| {
                choice
                    .writes
                    .as_ref()
                    .is_none_or(|writes| writes.contains(key))
                    && self.available(key, scope, held)
            })
            .collect()
    }

    fn section(
        &self,
        id: &str,
        section: &CustomisationSection,
        changes: &Changes,
        matches: usize,
        scope: &Scope,
        held: &BTreeSet<String>,
    ) -> Value {
        let op = |key: &str| self.operation(key, scope, held);
        json!({
            "section": id,
            "title": section.title,
            "purpose": changes.purpose,
            "target": "tenant",
            "limits": changes.limits,
            "guide": changes.guide,
            "matches": matches,
            "operations": {
                "read": changes.reads.iter().map(|read| op(read.operation)).collect::<Vec<_>>(),
                "change": changes.change.iter().map(|key| op(key)).collect::<Vec<_>>(),
                "preview": changes.preview.map(op),
                "verify": op(changes.verify),
                "restore": changes.restore.map(op),
            },
        })
    }
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
}

struct Filter<'a> {
    on_page: Option<&'a str>,
    record_kind: Option<&'a str>,
    purpose: Vec<String>,
}
impl<'a> Filter<'a> {
    fn new(input: &'a Value) -> Self {
        Self {
            on_page: input["on_page"].as_str(),
            record_kind: input["record_kind"].as_str(),
            purpose: words(&text_field(input, "purpose").to_lowercase())
                .filter(|word| !matches!(*word, "a" | "an" | "the"))
                .map(str::to_owned)
                .collect(),
        }
    }
    fn matches(&self, choice: &Choice) -> bool {
        self.on_page
            .is_none_or(|page| choice.page.as_deref() == Some(page))
            && self
                .record_kind
                .is_none_or(|kind| choice.record_kinds.iter().any(|id| id == kind))
            && self.matches_purpose(choice)
    }

    fn matches_purpose(&self, choice: &Choice) -> bool {
        if self.purpose.is_empty() {
            return true;
        }
        let text =
            format!("{} {} {}", choice.title, choice.subject, choice.keywords).to_lowercase();
        let choice_words: BTreeSet<_> = words(&text).collect();
        self.purpose
            .iter()
            .all(|word| choice_words.contains(word.as_str()))
    }
}

fn choice_data(id: &str, choice: Choice, available_writes: Vec<&'static str>) -> Data {
    let mut row: BTreeMap<String, Data> = BTreeMap::new();
    let mut put = |key: &str, value: Value| {
        row.insert(key.to_owned(), Data::Json(value));
    };
    put("section", json!(id));
    put("subject", json!(choice.subject));
    put("title", json!(choice.title));
    put("on_page", json!(choice.page));
    put("record_kinds", json!(choice.record_kinds));
    put("target", json!("tenant"));
    put("default", json!(choice.default));
    put("source", json!(choice.source));
    put("customised", json!(choice.customised));
    put("version", json!(choice.version));
    put("input", choice.input);
    put("verify_input", choice.verify_input);
    put("limits", choice.limits);
    put("available_writes", json!(available_writes));
    row.insert(
        "value".to_owned(),
        choice.value.map_or(Data::Restricted, Data::Json),
    );
    Data::Object(row)
}

struct Discover;
impl ReadHandler for Discover {
    fn owner(&self) -> &'static str {
        "base/labels"
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let bound = BOUND.get().ok_or(Error::Internal)?;
            let filter = Filter::new(input);
            // One snapshot decides which owners to ask, so a section the
            // caller cannot read adds no refused decision to the log.
            let held = reach::capabilities(client.sql_client().await?, scope).await?;
            let (mut sections, mut choices) = (Vec::new(), Vec::new());
            for (id, section) in &bound.sections {
                let Some(changes) = &section.changes else {
                    continue;
                };
                if !changes
                    .reads
                    .iter()
                    .all(|read| bound.available(read.operation, scope, &held))
                {
                    continue;
                }
                let mut results = Vec::with_capacity(changes.reads.len());
                for read in changes.reads {
                    let called = bound
                        .registry
                        .call_on(client, scope, read.operation, &(read.input)())
                        .await;
                    match called {
                        Ok(mut value) => results.push(value["result"].take()),
                        Err(AgentError::Forbidden | AgentError::Backend(Error::Forbidden)) => break,
                        Err(error) => return Err(error),
                    }
                }
                if results.len() < changes.reads.len() {
                    continue;
                }
                let matched: Vec<Choice> = (changes.choices)(&results)
                    .into_iter()
                    .filter(|choice| filter.matches(choice))
                    .collect();
                if matched.is_empty() {
                    continue;
                }
                sections.push(bound.section(id, section, changes, matched.len(), scope, &held));
                choices.extend(matched.into_iter().map(|choice| (*id, changes, choice)));
            }
            let page = input["page"].as_u64().unwrap_or(1).max(1);
            let page_size = input["page_size"].as_u64().unwrap_or(DEFAULT_PAGE_SIZE);
            let total = choices.len() as u64;
            let shown: Vec<Data> = choices
                .into_iter()
                .skip(usize::try_from((page - 1).saturating_mul(page_size)).unwrap_or(usize::MAX))
                .take(page_size as usize)
                .map(|(id, changes, choice)| {
                    let writes = bound.available_writes(changes, &choice, scope, &held);
                    choice_data(id, choice, writes)
                })
                .collect();
            let entries = Data::Object(BTreeMap::from([
                ("choices".to_owned(), Data::Array(shown)),
                ("sections".to_owned(), Data::Json(json!(sections))),
            ]));
            Ok(Output::Page(Page {
                entries,
                page,
                page_size,
                total,
            }))
        })
    }
}

fn input() -> Value {
    json!({"type":"object","properties":{
        "on_page":{"type":"string","maxLength":200,"description":"The exact page path, such as /companies."},
        "record_kind":{"type":"string","maxLength":120,"description":"A kind of record by its stable ID, such as company. Given with on_page, the choice must match both."},
        "purpose":{"type":"string","maxLength":120,"description":"What the person wants to do, in a few words, such as rename a column. Match words in the choice name, ID or purpose terms."},
        "page":{"type":"integer","minimum":1,"maximum":1000000,"description":"Left out: 1."},
        "page_size":{"type":"integer","minimum":1,"maximum":100,"description":"Choices per page. Left out: 25."}
    }})
}
fn output() -> Value {
    json!({"type":"object","description":"choices: one page of tenant customisation choices, each with section, subject, title, on_page, record_kinds, target, effective value (restricted when unreadable), default, source, customised, version, choice-specific limits, write input, verify_input and the available_writes for this choice. sections: matching sections with their purpose, target, limits, guide and owner operations. Section operation availability covers only capability and channel. Write plans preview the change. meta.page counts the choices.","properties":{
        "choices":{"type":"array","items":{"type":"object"}},
        "sections":{"type":"array","items":{"type":"object"}}
    }})
}

inventory::submit! { Contribution::new(DEFINITIONS, "base/labels.customisations.agent", Provenance { module: "base/labels", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: KEY, title: "What can be changed here", summary: "Everything this tenant can customise that you may read, by page, kind of record or purpose: each choice with or without an override, its value and source, version, limits and the operations that change, preview, verify and restore it. Start here before customising.", keywords: &["customise", "customize", "customisation", "change", "configure", "setup", "tailor", "rename", "settings", "what can I change"], capability: SIGNED_IN, input, output, handler: Handler::Read(&Discover), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[],
}) }
