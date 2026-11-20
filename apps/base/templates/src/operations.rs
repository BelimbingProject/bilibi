use crate::{
    catalogue,
    store::{Revision, Selection},
    Template, Templates, Text, MANAGE, MODULE, VIEW,
};
use bilibi_agent_api::{
    json::{Data, Output},
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::Contribution;
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

pub(crate) const LIST: &str = "base.templates.catalogue.list";
const OVERRIDES: &str = "base.templates.override.list";
pub(crate) const INSPECT: &str = "base.templates.template.get";
pub(crate) const PUBLISH: &str = "base.templates.template.publish";
pub(crate) const RESTORE: &str = "base.templates.template.restore";

static OWNER: OnceLock<Arc<Templates>> = OnceLock::new();

pub(crate) fn bind(owner: Arc<Templates>) -> bilibi_module_registry::Result<()> {
    OWNER.set(owner).map_err(|_| {
        bilibi_module_registry::RegistryError::Service(
            "duplicate template operation binding".into(),
        )
    })
}

fn owner() -> bilibi_agent_api::Result<&'static Templates> {
    Ok(OWNER.get().ok_or(Error::Internal)?)
}

fn metadata(entry: &Template) -> Value {
    json!({
        "key":entry.key,"label":entry.label,"audience":entry.audience,"outputs":entry.outputs,
        "base_language":entry.base_language,
        "languages":entry.defaults.iter().map(|d| d.language).collect::<Vec<_>>(),
        "slots":entry.slots.iter().map(|s| json!({
            "id":s.id,"label":s.label,"required":s.required,"max_bytes":s.max_bytes,
        })).collect::<Vec<_>>(),
        "variables":entry.variables.iter().map(|v| json!({
            "id":v.id,"label":v.label,"type":v.sample.kind(),"optional":v.optional,"slots":v.slots,
            "sample":match v.sample {
                crate::Value::Text(value) => json!(value),
                crate::Value::Date(value) => json!(value.to_string()),
            },
        })).collect::<Vec<_>>(),
    })
}

fn selection(input: &Value) -> bilibi_agent_api::Result<Selection<'_>> {
    let company = input["company_id"]
        .as_i64()
        .map(OrganisationId::try_from)
        .transpose()
        .map_err(|_| Error::Invalid)?;
    let key = input["key"].as_str().ok_or(Error::Invalid)?;
    let language = input["language"].as_str().ok_or(Error::Invalid)?;
    if catalogue::language(language).is_none() {
        return Err(bilibi_agent_api::invalid(
            "language",
            "Enter a language tag such as en or en-MY.",
        ));
    }
    Ok(Selection {
        key,
        company,
        language,
    })
}

enum Read {
    Overrides,
    List,
    Inspect,
}

impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        MODULE
    }

    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let templates = owner()?;
            let value = match self {
                Self::Overrides => json!(templates.overrides(client, scope).await?),
                Self::List => json!(templates.list().map(metadata).collect::<Vec<_>>()),
                Self::Inspect => {
                    let selection = selection(input)?;
                    let entry = templates.get(selection.key)?;
                    let edit = templates.edit(client, scope, &selection, false).await?;
                    json!({"template":metadata(entry),"wording":edit})
                }
            };
            Ok(Output::Data(Data::restricted(value, &Default::default())))
        })
    }
}

enum Write {
    Publish,
    Restore,
}

impl Write {
    fn revision<'a>(
        &self,
        templates: &Templates,
        input: &'a Value,
    ) -> bilibi_agent_api::Result<Revision<'a>> {
        let selection = selection(input)?;
        let expected = input["revision"].as_i64().ok_or(Error::Invalid)?;
        let slots = match self {
            Self::Publish => {
                let slots: Text = serde_json::from_value(input["slots"].clone()).map_err(|_| {
                    bilibi_agent_api::invalid("slots", "Enter text for each field.")
                })?;
                templates
                    .get(selection.key)?
                    .preview(&slots)
                    .map_err(|problem| {
                        bilibi_agent_api::invalid(&problem.field, &problem.message)
                    })?;
                Some(slots)
            }
            Self::Restore => None,
        };
        Ok(Revision {
            selection,
            expected,
            slots,
        })
    }
}

impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        MODULE
    }

    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let templates = owner()?;
            let revision = self.revision(templates, input)?;
            let before = templates
                .edit(client, scope, &revision.selection, true)
                .await?;
            before.check(&revision)?;
            let after = revision.slots.as_ref().unwrap_or(&before.inherited.slots);
            let sample = templates
                .get(revision.selection.key)?
                .preview(after)
                .map_err(|problem| bilibi_agent_api::invalid(&problem.field, &problem.message))?;
            Ok(json!({
                "key":revision.selection.key, "company_id":revision.selection.company,
                "language":revision.selection.language, "before":before.effective,
                "after":after,"sample":sample,
            })
            .into())
        })
    }

    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let templates = owner()?;
            let change = self.revision(templates, input)?;
            let revision = templates.change(tx, scope, change).await?;
            Ok(json!({"revision":revision}).into())
        })
    }
}

fn empty() -> Value {
    json!({"type":"object","properties":{}})
}
fn empty_input() -> Value {
    json!({})
}
fn list_output() -> Value {
    json!({"type":"array","items":{"type":"object"}})
}
fn output() -> Value {
    json!({"type":"object"})
}

fn inspect() -> Value {
    json!({"type":"object","required":["key","language"],"properties":{
        "key":{"type":"string","maxLength":128}, "language":{"type":"string","maxLength":80},
        "company_id":{"type":"integer","nullable":true,"minimum":1,"description":"Omit to use the workspace-wide wording."}
    }})
}
fn restore() -> Value {
    let mut schema = inspect();
    schema["required"] = json!(["key", "language", "revision"]);
    schema["properties"]["revision"] = json!({"type":"integer","minimum":0,"description":"Revision from base.templates.template.get, or zero before the first revision."});
    schema
}
fn publish() -> Value {
    let mut schema = restore();
    schema["required"] = json!(["key", "language", "revision", "slots"]);
    schema["properties"]["slots"] = json!({"type":"object","additionalProperties":true});
    schema
}

inventory::submit! { Contribution::new(DEFINITIONS, "base/templates.agent", crate::provenance!(), &Definitions {
    operations: &[
        Operation { key: OVERRIDES, title: "List published wording", summary: "Find current template overrides for the workspace and companies you can work in.", keywords: &["wording","templates"], capability: VIEW, input: empty, output: list_output, handler: Handler::Read(&Read::Overrides), channels: Channels::Every, step_up: &[] },
        Operation { key: LIST, title: "List templates", summary: "Find editable templates and the fields and variables each one offers.", keywords: &["wording","email","copy"], capability: VIEW, input: empty, output: list_output, handler: Handler::Read(&Read::List), channels: Channels::Every, step_up: &[] },
        Operation { key: INSPECT, title: "Read template wording", summary: "See the current and inherited wording and the revision for one language and company.", keywords: &["wording","email"], capability: VIEW, input: inspect, output, handler: Handler::Read(&Read::Inspect), channels: Channels::Every, step_up: &[] },
        Operation { key: PUBLISH, title: "Publish template wording", summary: "Publish plain-text wording for one language and company or the workspace.", keywords: &["wording","email"], capability: MANAGE, input: publish, output, handler: Handler::SafeWrite(&Write::Publish), channels: Channels::Every, step_up: &[] },
        Operation { key: RESTORE, title: "Restore inherited wording", summary: "Remove the custom wording for one language and company or the workspace.", keywords: &["wording","email"], capability: MANAGE, input: restore, output, handler: Handler::SafeWrite(&Write::Restore), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[Guide { key: "base.templates", title: "Edit message templates", summary: "Find and preview wording you can publish for a workspace or company.", keywords: &["templates","email","wording"], capability: VIEW, body: "List the templates, then read the chosen key, language and optional company. The read gives you all editable fields, sample variables, the current and inherited text and the revision. Place variables in double braces. Publish all editable fields together or restore inherited wording. Review the sample and approve the operation before committing." }],
}) }

const UNDO_EVENT: &str = "customisation.templates";

fn override_input(row: &Value) -> Value {
    json!({"key":row["key"], "company_id":row["company_id"], "language":row["language"], "revision":row["revision"]})
}

fn rows(value: &Value) -> Vec<bilibi_labels::CustomisationRow> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let revision = row["revision"].as_i64()?;
            Some(bilibi_labels::CustomisationRow {
                description: row["description"].as_str()?.into(),
                audit_kind: "base/templates.revision",
                audit_id: revision,
                revision,
                input: override_input(row),
            })
        })
        .collect()
}

fn choices(reads: &[Value]) -> Vec<bilibi_labels::Choice> {
    let mut choices = std::collections::BTreeMap::new();
    for row in reads[0].as_array().into_iter().flatten() {
        let (Some(key), Some(label)) = (row["key"].as_str(), row["label"].as_str()) else {
            continue;
        };
        for language in row["languages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let subject = format!("{key}/workspace/{language}");
            choices.insert(
                subject.clone(),
                bilibi_labels::Choice {
                    subject,
                    title: format!("{label} · Workspace · {language}"),
                    keywords: "templates wording email language",
                    page: Some(format!("/templates/{key}")),
                    record_kinds: vec!["template".into(), key.into()],
                    verify_input: json!({"key":key,"language":language}),
                    // Re-read the exact target before publishing to get its revision.
                    input: json!({"key":key,"language":language}),
                    limits: json!({"variables":row["variables"],"slots":row["slots"]}),
                    ..Default::default()
                },
            );
        }
    }
    for row in reads[1].as_array().into_iter().flatten() {
        let (Some(key), Some(language), Some(title)) = (
            row["key"].as_str(),
            row["language"].as_str(),
            row["description"].as_str(),
        ) else {
            continue;
        };
        let company = row["company_id"].as_i64();
        let scope = company.map_or_else(|| "workspace".into(), |id| id.to_string());
        let subject = format!("{key}/{scope}/{language}");
        choices.insert(
            subject.clone(),
            bilibi_labels::Choice {
                subject,
                title: title.into(),
                keywords: "templates wording email language",
                page: Some(format!("/templates/{key}")),
                record_kinds: vec!["template".into(), key.into()],
                value: Some(row["slots"].clone()),
                customised: true,
                version: Some(row["revision"].clone()),
                input: override_input(row),
                verify_input: json!({"key":key,"company_id":company,"language":language}),
                source: Some(
                    if company.is_some() {
                        "company"
                    } else {
                        "workspace"
                    }
                    .into(),
                ),
                ..Default::default()
            },
        );
    }
    choices.into_values().collect()
}

inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/templates.customisations", crate::provenance!(), &bilibi_labels::CustomisationSection {
    title: "Templates",
    undo: Some(bilibi_labels::Undo { read:OVERRIDES, read_input:empty_input, operation:RESTORE, event:UNDO_EVENT,
        action:"Restore inherited wording", detail:"Future messages will use the inherited wording.", rows }),
    changes: Some(bilibi_labels::Changes {
        purpose:"Change the wording people receive from a workspace or company.",
        limits:"Plain text with the variables each template offers. The sending feature chooses the recipient and attachments.",
        guide:"base.templates",
        reads:&[bilibi_labels::Read { operation:LIST, input:empty_input }, bilibi_labels::Read { operation:OVERRIDES, input:empty_input }],
        choices, change:&[PUBLISH], preview:None, verify:INSPECT, restore:Some(RESTORE),
    }),
}) }

inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base/templates.customisations", crate::provenance!(),
    &bilibi_agent_api::page::PageBinding { service:"base/labels.customisations", event:UNDO_EVENT, contract:RESTORE },
) }
