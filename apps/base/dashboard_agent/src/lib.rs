//! Agent and Customisations door for the Dashboard owner's arrangement.
//! Session and performance widgets depend on Dashboard, so the connector
//! contributes here without placing Agent API below the dashboard catalogue.
use bilibi_agent_api::{
    invalid, Channels, Definitions, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS, SIGNED_IN,
};
use bilibi_dashboard::{
    ArrangementWrite, ArrangementWriter, Arrangements, DashboardPage, Placement, ARRANGEMENTS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance, RegistryError};
use bilibi_tenancy::Scope;
use bilibi_ui::page::{LiveRequest, PageService};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

const ID: &str = "base/dashboard_agent";
const PAGE: &str = "base/dashboard_agent.pages";
const READ: &str = "base.dashboard_agent.read";
const SET: &str = "base.dashboard_agent.set";
const RESTORE: &str = "base.dashboard_agent.restore";
const UNDO: &str = "customisation.dashboard_restore";
static ARRANGEMENT: OnceLock<Arc<Arrangements>> = OnceLock::new();

macro_rules! provenance {
    () => {
        Provenance {
            module: ID,
            source: file!(),
            line: line!(),
        }
    };
}
fn owner() -> bilibi_agent_api::Result<&'static Arrangements> {
    ARRANGEMENT
        .get()
        .map(Arc::as_ref)
        .ok_or(Error::Internal.into())
}
inventory::submit! { Module {
    id: ID, layer: Layer::Base,
    dependencies: &["base/agent_api","base/dashboard","base/database","base/labels","base/module_registry","base/tenancy","base/ui"],
    migrations: &[], services: &[bilibi_module_registry::Service { id: PAGE, factory: |services| {
        Ok(Arc::new(PageService::<Scope, Error>(Arc::new(DashboardPage::new(
            services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
            services.get(ARRANGEMENTS)?,
            Arc::new(PageWriter(services.get(bilibi_agent_api::page::SERVICE)?)),
        )))))
    }}],
} }
inventory::submit! { bilibi_module_registry::Ready {
    module: ID,
    check: |services| ARRANGEMENT.set(services.get(ARRANGEMENTS)?)
        .map_err(|_| RegistryError::Contribution("dashboard operations already bound".into())),
} }
inventory::submit! { Contribution::new(bilibi_module_registry::contributions::ROUTES, "dashboard.index", provenance!(),
    &bilibi_module_registry::contributions::Route {
        company_scope: bilibi_module_registry::contributions::CompanyScope::Company,
        method: bilibi_module_registry::contributions::Method::Get, path: "/dashboard",
        service: PAGE, operation: "index", access: bilibi_module_registry::contributions::Access::SignedIn, framed: true,
    })
}
inventory::submit! { Contribution::new(bilibi_module_registry::contributions::PAGE_WRITES, PAGE, provenance!(),
    &bilibi_module_registry::contributions::PageWrites {
        service: PAGE,
        contracts: &[("add_widget", SET), ("remove_widget", SET), ("move_up", SET), ("move_down", SET),
            ("reorder_widgets", SET), ("add_section", SET), ("remove_section", SET),
            ("move_section_up", SET), ("move_section_down", SET)],
        pending: bilibi_module_registry::contributions::Pending::None,
    })
}

struct PageWriter(Arc<bilibi_agent_api::page::PageContracts>);
impl ArrangementWriter for PageWriter {
    fn set<'a>(
        &'a self,
        request: &'a LiveRequest<'_, Scope>,
        event: &'a str,
        input: Value,
    ) -> ArrangementWrite<'a> {
        Box::pin(async move {
            let call = bilibi_agent_api::page::Call {
                scope: request.scope,
                credential: request.credential,
                service: PAGE,
                event,
                contract: SET,
            };
            self.0
                .submit(&call, input)
                .await
                .map(|_| ())
                .map_err(|error| match error {
                    bilibi_agent_api::Error::Backend(error) => error,
                    bilibi_agent_api::Error::Forbidden => Error::Forbidden,
                    bilibi_agent_api::Error::InvalidInput(_) => Error::Invalid,
                    _ => Error::Internal,
                })
        })
    }
}

struct Read;
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        ID
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        _: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            Ok(owner()?
                .read(client.sql_client().await?, scope)
                .await?
                .into())
        })
    }
}

enum Write {
    Set,
    Restore,
}
impl Write {
    fn input<'a>(
        &self,
        value: &'a Value,
    ) -> bilibi_agent_api::Result<(Placement, &'a str, Option<Vec<String>>)> {
        let placement = match value["region"].as_str() {
            Some("grid") => Placement::Grid,
            Some("sections") => Placement::Section,
            _ => return Err(invalid("region", "choose grid or sections")),
        };
        let version = value["version"].as_str().ok_or(Error::Invalid)?;
        let ids = match self {
            Self::Set => Some(
                serde_json::from_value(value["entry_ids"].clone()).map_err(|_| Error::Invalid)?,
            ),
            Self::Restore => None,
        };
        Ok((placement, version, ids))
    }
}

fn refusal(error: Error) -> bilibi_agent_api::Error {
    match error {
        Error::Invalid => invalid(
            "entry_ids",
            "choose distinct available entries for this region, up to the dashboard panel limit",
        ),
        Error::Refused(message) => invalid("region", message),
        other => other.into(),
    }
}

impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        ID
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (placement, version, ids) = self.input(input)?;
            Ok(owner()?
                .preview(
                    client.sql_client().await?,
                    scope,
                    placement,
                    version,
                    ids.as_deref(),
                )
                .await
                .map_err(refusal)?
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
            let (placement, version, ids) = self.input(input)?;
            Ok(owner()?
                .apply(tx, scope, placement, version, ids.as_deref())
                .await
                .map_err(refusal)?
                .into())
        })
    }
}

fn empty() -> Value {
    json!({})
}
fn empty_schema() -> Value {
    json!({"type":"object","properties":{}})
}
fn restore_schema() -> Value {
    json!({"type":"object","required":["region","version"],"properties":{
        "region":{"type":"string","enum":["grid","sections"]},
        "version":{"type":"string","maxLength":64,"description":"Version of this region from read."}
    }})
}
fn set_schema() -> Value {
    let mut schema = restore_schema();
    schema["required"] = json!(["region", "version", "entry_ids"]);
    schema["properties"]["entry_ids"] = json!({
        "type":"array","items":{"type":"string","maxLength":90},
        "description":"Complete ordered list of visible entry IDs for the chosen region. Invisible saved entries keep their positions."
    });
    schema
}
fn output() -> Value {
    json!({"type":"object"})
}

inventory::submit! { Contribution::new(DEFINITIONS, "base/dashboard_agent.arrangement", provenance!(), &Definitions {
    operations: &[
        Operation { key:READ, title:"Read my dashboard arrangement", summary:"Read the grid widgets and full-width sections you can arrange, including placed, available and default entries.", keywords:&["dashboard","arrangement","widget","panel"], capability:SIGNED_IN, input:empty_schema, output, handler:Handler::Read(&Read), channels:Channels::Every, step_up:&[] },
        Operation { key:SET, title:"Set my dashboard arrangement", summary:"Choose and order the visible entries in one of your dashboard regions.", keywords:&["dashboard","arrangement","widget","panel"], capability:SIGNED_IN, input:set_schema, output, handler:Handler::SafeWrite(&Write::Set), channels:Channels::Every, step_up:&[] },
        Operation { key:RESTORE, title:"Restore my dashboard defaults", summary:"Clear the saved arrangement for your dashboard grid or full-width sections.", keywords:&["dashboard","arrangement","widget","restore"], capability:SIGNED_IN, input:restore_schema, output, handler:Handler::SafeWrite(&Write::Restore), channels:Channels::Every, step_up:&[] },
    ],
    guides: &[],
}) }
fn rows(reply: &Value) -> Vec<bilibi_labels::CustomisationRow> {
    [
        ("grid", "Widgets", 1),
        ("sections", "Full-width sections", 2),
    ]
    .into_iter()
    .filter_map(|(key, label, id)| {
        let region = &reply[key];
        if region["customised"] != true {
            return None;
        }
        let labels = region["placed"]
            .as_array()?
            .iter()
            .filter_map(|item| item["label"].as_str())
            .collect::<Vec<_>>();
        let description = if labels.is_empty() {
            format!("{label} · None shown")
        } else {
            format!("{label} · {}", labels.join(", "))
        };
        Some(bilibi_labels::CustomisationRow {
            description,
            audit_kind: "user_dashboard",
            audit_id: id,
            revision: 0,
            input: json!({"region":key,"version":region["version"]}),
        })
    })
    .collect()
}
inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/dashboard_agent.customisations", provenance!(), &bilibi_labels::CustomisationSection {
    title:"My dashboard",
    undo: Some(bilibi_labels::Undo { read: READ, read_input: empty, action: Some(bilibi_labels::UndoAction { operation: RESTORE, event: UNDO, label: "Restore default", detail: "The chosen dashboard region will return to its original entries." }), rows }),
    changes:None,
}) }
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.dashboard_agent.customisations_restore", provenance!(), &bilibi_agent_api::page::PageBinding {
    service:"base/labels.customisations", event:UNDO, contract:RESTORE,
}) }
