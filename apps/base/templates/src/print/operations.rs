use super::{preview, revisions::AUDIT_KIND, PrintField, PrintKind, PrintValue};
use crate::{
    operations::{owner, selection},
    store::Selection,
    MANAGE, MODULE, VIEW,
};
use bilibi_agent_api::page::{PageBinding, PAGE_BINDINGS};
use bilibi_agent_api::{
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::Contribution;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};

const LIST: &str = "base.templates.print.list";
const GET: &str = "base.templates.print.get";
const OVERRIDES: &str = "base.templates.print.overrides";
const PREVIEW: &str = "base.templates.print.preview";
const PUBLISH: &str = "base.templates.print.publish";
const RESTORE: &str = "base.templates.print.restore";
const UNDO_EVENT: &str = "customisation.print";

enum Read {
    List,
    Get,
    Overrides,
    Preview,
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
            let response = match self {
                Self::List => json!(templates.print_contracts().map(|contract| json!({
                    "key":contract.key,"label":contract.label,"version":contract.version,
                    "languages":contract.defaults.iter().map(|d| d.language).collect::<Vec<_>>(),
                    "states":contract.states,
                })).collect::<Vec<_>>()),
                Self::Overrides => json!(templates.print_overrides(client, scope).await?),
                Self::Get => {
                    let target = selection(input)?;
                    let edit = templates.edit_print(client, scope, &target, false).await?;
                    let contract = templates.print_contract(target.key)?;
                    json!({"print":edit, "fields":fields(contract.fields), "contract_version":contract.version})
                }
                Self::Preview => {
                    let target = selection(input)?;
                    let draft = input["layout"].as_str();
                    let edit = templates
                        .edit_print(client, scope, &target, draft.is_some())
                        .await?;
                    let (language, layout) = draft.map_or(
                        (
                            edit.effective.language.as_str(),
                            edit.effective.layout.as_ref(),
                        ),
                        |layout| (target.language, layout),
                    );
                    let contract = templates.print_contract(target.key)?;
                    let preview = preview::service()?
                        .stage(scope, target.company, contract, language, layout)
                        .await?;
                    json!({"preview_path":preview.path(), "revision":edit.revision, "language":language})
                }
            };
            Ok(response.into())
        })
    }
}

enum Write {
    Publish,
    Restore,
}

struct Revision<'a> {
    selection: Selection<'a>,
    expected: &'a str,
    layout: Option<&'a str>,
}

impl Write {
    fn revision<'a>(&self, input: &'a Value) -> bilibi_agent_api::Result<Revision<'a>> {
        let selection = selection(input)?;
        let expected = input["review"].as_str().ok_or(Error::Invalid)?;
        let layout = match self {
            Self::Publish => Some(input["layout"].as_str().ok_or(Error::Invalid)?),
            Self::Restore => None,
        };
        Ok(Revision {
            selection,
            expected,
            layout,
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
            let revision = self.revision(input)?;
            let target = &revision.selection;
            let before = templates.edit_print(client, scope, target, true).await?;
            before.check(revision.expected, revision.layout.is_none())?;
            let Some(layout) = revision.layout else {
                let inherited = match before.inherited.origin {
                    crate::Origin::Packaged { version } => {
                        format!("the packaged layout (version {version})")
                    }
                    crate::Origin::Tenant { revision } => {
                        format!("the workspace layout (revision {revision})")
                    }
                    crate::Origin::Company { revision, .. } => {
                        format!("the company layout (revision {revision})")
                    }
                };
                return Ok(
                    json!({ "summary": format!("New documents will use {inherited}.") }).into(),
                );
            };
            templates
                .validate_print(target, layout)
                .map_err(|problem| bilibi_agent_api::invalid(&problem.field, &problem.message))?;
            Ok(json!({
                "key":target.key, "company_id":target.company, "language":target.language,
                "revision":before.revision, "before":before.effective.origin, "layout":layout,
                "preview_operation":PREVIEW,
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
            let revision = self.revision(input)?;
            let id = templates
                .change_print(
                    tx,
                    scope,
                    &revision.selection,
                    revision.expected,
                    revision.layout,
                )
                .await?;
            Ok(json!({"revision":id}).into())
        })
    }
}

fn sample(value: PrintValue<'_>) -> Value {
    match value {
        PrintValue::Text(value) => json!(value),
        PrintValue::Integer(value) => json!(value),
        PrintValue::Decimal(value) => json!(value.to_string()),
        PrintValue::Money { amount, currency } => {
            json!({"amount":amount.to_string(),"currency":currency})
        }
        PrintValue::Date(value) => json!(value.to_string()),
        PrintValue::Boolean(value) => json!(value),
        PrintValue::Address(value) => json!(value),
        PrintValue::Rows(rows) => json!(rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(key, value)| (key.to_string(), sample(*value)))
                    .collect::<serde_json::Map<_, _>>()
            })
            .collect::<Vec<_>>()),
    }
}

fn fields(items: &[PrintField]) -> Vec<Value> {
    items.iter().map(|field| {
        let (kind, columns) = match field.kind {
            PrintKind::Text => ("text", Vec::new()),
            PrintKind::Integer => ("integer", Vec::new()),
            PrintKind::Decimal => ("decimal", Vec::new()),
            PrintKind::Money => ("money", Vec::new()),
            PrintKind::Date => ("date", Vec::new()),
            PrintKind::Boolean => ("boolean", Vec::new()),
            PrintKind::Address => ("address", Vec::new()),
            PrintKind::Rows(columns) => ("rows", fields(columns)),
        };
        json!({
            "id":field.id,"type":kind,"optional":field.optional,"required_output":field.required_output,
            "labels":field.labels,"columns":columns,"sample":sample(field.sample),
        })
    }).collect()
}

fn empty() -> Value {
    json!({ "type":"object","properties":{} })
}
fn array() -> Value {
    json!({ "type":"array","items":{"type":"object"} })
}
fn result() -> Value {
    json!({ "type":"object" })
}
fn get() -> Value {
    json!({"type":"object","required":["key","language"],"properties":{
        "key":{"type":"string","maxLength":128},
        "language":{"type":"string","maxLength":80},
        "company_id":{"type":"integer","nullable":true,"minimum":1}
    }})
}
fn preview_schema() -> Value {
    let mut schema = get();
    schema["properties"]["layout"] = json!({"type":"string","maxLength":49152});
    schema
}
fn restore() -> Value {
    let mut schema = get();
    schema["required"] = json!(["key", "language", "review"]);
    schema["properties"]["review"] = json!({"type":"string","minLength":64,"maxLength":64});
    schema
}
fn publish() -> Value {
    let mut schema = restore();
    schema["required"] = json!(["key", "language", "review", "layout"]);
    schema["properties"]["layout"] = json!({"type":"string","maxLength":49152});
    schema
}

inventory::submit! { Contribution::new(DEFINITIONS, "base/templates.print.agent", crate::provenance!(), &Definitions {
    operations: &[
        Operation { key: LIST, title: "List print layouts", summary: "Find the document types and languages available for print customisation.", keywords: &["print","document","layout"], capability: VIEW, input: empty, output: array, handler: Handler::Read(&Read::List), channels: Channels::Every, step_up: &[] },
        Operation { key: GET, title: "Read a print layout", summary: "See a print layout, its fields, its inherited source and the current revision.", keywords: &["print","layout"], capability: VIEW, input: get, output: result, handler: Handler::Read(&Read::Get), channels: Channels::Every, step_up: &[] },
        Operation { key: OVERRIDES, title: "List published print layouts", summary: "Find current print overrides for the workspace and companies you can access.", keywords: &["print","overrides"], capability: VIEW, input: empty, output: array, handler: Handler::Read(&Read::Overrides), channels: Channels::Every, step_up: &[] },
        Operation { key: PREVIEW, title: "Preview a print layout", summary: "Render the selected or draft layout to a PDF with synthetic sample facts.", keywords: &["print","preview","pdf"], capability: VIEW, input: preview_schema, output: result, handler: Handler::Read(&Read::Preview), channels: Channels::Every, step_up: &[] },
        Operation { key: PUBLISH, title: "Publish a print layout", summary: "Publish a reviewed layout for one language and company or the workspace.", keywords: &["print","publish"], capability: MANAGE, input: publish, output: result, handler: Handler::SafeWrite(&Write::Publish), channels: Channels::Every, step_up: &[] },
        Operation { key: RESTORE, title: "Restore inherited print layout", summary: "Restore the layout inherited from the workspace or the packaged default.", keywords: &["print","restore"], capability: MANAGE, input: restore, output: result, handler: Handler::SafeWrite(&Write::Restore), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[Guide { key: "base.templates.print", title: "Customise a business print layout", summary: "Read, preview and publish a print layout for a workspace or company.", keywords: &["print","pdf","template"], capability: VIEW, body: "List document types, then read the target key, language and optional company. Use the current layout as an example. The layout is a JSON string containing paper and blocks. Arrange the owner fields and tables, preserving required blocks and leaving their labels to the contract. Preview the draft PDF with synthetic facts. Review and approve publication using the review token returned by the read operation. You can view and restore the result in Administration > Customisations." }],
}) }

fn rows(input: &Value) -> Vec<bilibi_labels::CustomisationRow> {
    input.as_array().into_iter().flatten().filter_map(|row| {
        let revision = row["revision"].as_i64()?;
        Some(bilibi_labels::CustomisationRow {
            description: row["description"].as_str()?.to_owned(),
            audit_kind: AUDIT_KIND,
            audit_id: revision,
            revision,
            input: json!({"key":row["key"],"language":row["language"],"company_id":row["company_id"],"review":row["review"]}),
        })
    }).collect()
}

fn override_rows() -> Value {
    json!({})
}
fn preview_path(row: &bilibi_labels::CustomisationRow) -> Option<String> {
    Some(format!(
        "/admin/customisations/print-layouts/{}/preview",
        row.revision
    ))
}

inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/templates.print-layouts", crate::provenance!(), &bilibi_labels::CustomisationSection {
    title: "Print layouts",
    undo: Some(bilibi_labels::Undo {
        read: OVERRIDES, read_input: override_rows,
        action: Some(bilibi_labels::UndoAction { operation: RESTORE, event: UNDO_EVENT, label: "Restore inherited layout", detail: "The current revision stays in history." }),
        rows,
    }),
    changes: None,
}) }

inventory::submit! { Contribution::new(bilibi_labels::PREVIEW_LINKS, "base/templates.print-layouts", crate::provenance!(), &bilibi_labels::PreviewLink {
    read: OVERRIDES, label: "Preview PDF", path: preview_path,
}) }

inventory::submit! { Contribution::new(PAGE_BINDINGS, "base/templates.print-restore", crate::provenance!(), &PageBinding {
    service: "base/labels.customisations", event: UNDO_EVENT, contract: RESTORE,
}) }
