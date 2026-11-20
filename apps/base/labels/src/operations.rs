//! The label operations agents and the page call (`base.labels.*`): thin
//! adapters over the `Labels` facade with the sealed scope the registry
//! proved. Reads list what may be renamed with its current label; every write
//! is a reviewed safe write whose preview shares the facade's checks without
//! writing and whose commit writes through the plan's audited transaction.
use crate::store::{Edit, Kind, Labels, Plan, MAX_LABEL};
use crate::{MANAGE, VIEW};
use bilibi_agent_api::{
    json::{Data, Output},
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
};

static BOUND: OnceLock<Arc<Labels>> = OnceLock::new();
pub(crate) fn bind(labels: Arc<Labels>) -> bilibi_module_registry::Result<()> {
    BOUND
        .set(labels)
        .map_err(|_| RegistryError::Service("duplicate labels binding".into()))
}
fn bound() -> bilibi_agent_api::Result<&'static Arc<Labels>> {
    BOUND.get().ok_or(Error::Internal.into())
}
fn found(error: Error) -> bilibi_agent_api::Error {
    match error {
        Error::Refused(message) => bilibi_agent_api::invalid("subject", &message),
        other => other.into(),
    }
}
fn kind(input: &Value) -> bilibi_agent_api::Result<Kind> {
    Kind::from_key(input["kind"].as_str().unwrap_or_default())
        .ok_or_else(|| bilibi_agent_api::invalid("kind", "must be menu, record or field"))
}
fn subject(input: &Value) -> bilibi_agent_api::Result<&str> {
    input["subject"]
        .as_str()
        .filter(|subject| !subject.is_empty())
        .ok_or_else(|| bilibi_agent_api::invalid("subject", "is required"))
}
pub(crate) fn version(input: &Value) -> bilibi_agent_api::Result<(i64, i64)> {
    let token = input
        .get("version")
        .ok_or_else(|| bilibi_agent_api::invalid("version", "is required"))?;
    let (id, revision) = token
        .as_str()
        .and_then(|token| token.split_once(':'))
        .ok_or_else(|| {
            bilibi_agent_api::invalid("version", "must be the identity:version token you read")
        })?;
    let id = id.parse::<i64>().map_err(|_| Error::Invalid)?;
    let revision = revision.parse::<i64>().map_err(|_| Error::Invalid)?;
    if !((id == 0 && revision == 0) || (id > 0 && revision > 0)) {
        return Err(Error::Invalid.into());
    }
    Ok((id, revision))
}
fn record(value: Value) -> Data {
    Data::restricted(value, &HashSet::new())
}
fn plan_json(plan: &Plan) -> Value {
    json!({"kind": plan.kind.key(), "subject": plan.subject, "default": plan.default, "before": plan.before, "after": plan.after})
}

enum Read {
    List,
}
impl ReadHandler for Read {
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
            let labels = bound()?;
            let wanted = input["kind"].as_str().and_then(Kind::from_key);
            let search = input["search"].as_str().unwrap_or_default().to_lowercase();
            let stored = Labels::stored(client, scope).await?;
            let rows: Vec<Value> = labels
                .subjects()?
                .into_iter()
                .filter(|s| wanted.is_none_or(|kind| kind == s.kind))
                .filter(|s| {
                    search.is_empty()
                        || s.default.to_lowercase().contains(&search)
                        || s.id.contains(&search)
                        || stored
                            .get(&(s.kind, s.id.clone()))
                            .is_some_and(|stored| stored.label.to_lowercase().contains(&search))
                })
                .map(|s| {
                    let own = stored.get(&(s.kind, s.id.clone()));
                    json!({"kind": s.kind.key(), "subject": s.id, "default": s.default, "trail": s.trail, "page": s.page, "record_kinds": s.record_kinds,
                        "label": own.map(|own| own.label.clone()), "version": own.map_or_else(|| "0:0".to_owned(), |own| format!("{}:{}", own.id, own.version))})
                })
                .collect();
            Ok(Output::Data(record(Value::Array(rows))))
        })
    }
}

enum Write {
    Set,
    Clear,
}
impl Write {
    fn text<'a>(&self, input: &'a Value) -> &'a str {
        match self {
            Self::Set => input["label"].as_str().unwrap_or_default(),
            Self::Clear => "",
        }
    }
}
impl Write {
    fn edit<'a>(&self, input: &'a Value) -> bilibi_agent_api::Result<Edit<'a>> {
        Ok(Edit {
            kind: kind(input)?,
            subject: subject(input)?,
            text: self.text(input),
            version: version(input)?,
        })
    }
}
impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        "base/labels"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let labels = bound()?;
            if matches!(self, Self::Set) && self.text(input).trim().is_empty() {
                return Err(bilibi_agent_api::invalid(
                    "label",
                    "is required; clear the override to restore the module's word",
                ));
            }
            let plan = labels
                .plan(client, scope, self.edit(input)?, false)
                .await
                .map_err(found)?;
            let summary = match &plan.after {
                Some(label) => format!(
                    "Show “{label}” for {} (the module says “{}”)",
                    plan.subject, plan.default
                ),
                None => format!(
                    "Restore the module's word “{}” for {}",
                    plan.default, plan.subject
                ),
            };
            Ok(json!({"summary": summary, "plan": plan_json(&plan)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let labels = bound()?;
            let plan = labels
                .apply(tx, scope, self.edit(input)?)
                .await
                .map_err(found)?;
            Ok(Output::Data(record(plan_json(&plan))))
        })
    }
}

fn kind_schema() -> Value {
    json!({"type":"string","enum":["menu","record","field"],"description":"menu: a menu item by its menu ID. record: a record type by its Grid table ID. field: a field by table.field (Grid table ID, a dot, Grid field ID)."})
}
fn subject_schema() -> Value {
    json!({"type":"string","maxLength":200,"description":"The stable ID, from base.labels.list."})
}
fn version_schema() -> Value {
    json!({"type":"string","description":"The identity:version token you read (0:0 when there was no override); a replaced or changed label is refused."})
}
fn list_schema() -> Value {
    json!({"type":"object","properties":{"kind":kind_schema(),"search":{"type":"string","maxLength":120,"description":"Only words holding this text, in any case."}}})
}
fn set_schema() -> Value {
    json!({"type":"object","required":["kind","subject","label","version"],"properties":{"kind":kind_schema(),"subject":subject_schema(),
        "label":{"type":"string","maxLength":MAX_LABEL,"description":"The plain text to show instead."},"version":version_schema()}})
}
fn clear_schema() -> Value {
    json!({"type":"object","required":["kind","subject","version"],"properties":{"kind":kind_schema(),"subject":subject_schema(),"version":version_schema()}})
}
fn list_output() -> Value {
    json!({"type":"array","description":"Each word that may be renamed: kind, subject, default (the module's word), trail, page (the address a menu item opens, or null), record_kinds (the Grid table and record types a record or field belongs to), label (the tenant's, or null) and version."})
}
fn write_output() -> Value {
    json!({"type":"object","description":"kind, subject, default, before and after (null when none)."})
}

inventory::submit! { Contribution::new(DEFINITIONS, "base/labels.agent", Provenance { module: "base/labels", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "base.labels.list", title: "List labels", summary: "The menu items, record types and fields a tenant may rename, with the module's own word and the tenant's label.", keywords: &["rename", "label", "menu", "terminology"], capability: VIEW, input: list_schema, output: list_output, handler: Handler::Read(&Read::List), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.labels.set", title: "Rename a menu item, record type or field", summary: "Show your own word for a menu item, record type or field, such as Clients for Companies; operation keys, routes and permissions never change.", keywords: &["rename", "label", "menu", "terminology"], capability: MANAGE, input: set_schema, output: write_output, handler: Handler::SafeWrite(&Write::Set), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.labels.clear", title: "Restore a label", summary: "Remove the tenant's label so the module's own word shows again.", keywords: &["restore", "label", "menu"], capability: MANAGE, input: clear_schema, output: write_output, handler: Handler::SafeWrite(&Write::Clear), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[Guide { key: "base.labels", title: "Tenant labels", summary: "Rename menu items, record types and fields for your tenant without changing keys, routes or permissions.", keywords: &["rename", "label", "terminology"], capability: VIEW,
        body: "A label is plain text your tenant shows instead of a module's own word for a menu item, a record type or a field: Clients for Companies, say. Read what may be renamed with base.labels.list (kind menu, record or field; a field's subject is table.field; each row has a stable subject ID, the module's default and your current label). Rename with base.labels.set and restore with base.labels.clear, each with the version you read. A label never renames an operation key, a route or a permission key, so everything you call by key keeps its name. Labels are 1 to 60 characters of plain text and apply to your tenant only. For each write, run the dry-run check, show the complete preview, wait for explicit approval, then commit the unchanged operation." }],
}) }
