//! Reviewed self-service operations for navigation preferences.
use crate::{
    preferences::{apply_pins, list_pins, lock_pins, pin_route_is_served, Pin, PinDraft, MAX_PINS},
    UserService,
};
use bilibi_agent_api::{
    invalid, Channels, Definitions, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS, SIGNED_IN,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_security::Scope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
};
use tokio_postgres::GenericClient;

const PINS_READ: &str = "core.user.pins.read";
const PINS_RESTORE: &str = "core.user.pins.restore";
const PINS_UNDO: &str = "customisation.pins_restore";
pub(crate) const LANDING_READ: &str = "core.user.landing.read";
pub(crate) const LANDING_SET: &str = "core.user.landing.set";
pub(crate) const LANDING_RESTORE: &str = "core.user.landing.restore";
const LANDING_UNDO: &str = "customisation.landing_restore";
static USERS: OnceLock<Arc<UserService>> = OnceLock::new();

macro_rules! provenance {
    () => {
        Provenance {
            module: "core/user",
            source: file!(),
            line: line!(),
        }
    };
}

pub(crate) fn bind(users: Arc<UserService>) -> Result<(), RegistryError> {
    USERS
        .set(users)
        .map_err(|_| RegistryError::Contribution("navigation preferences already bound".into()))
}

fn users() -> bilibi_agent_api::Result<&'static UserService> {
    USERS.get().map(Arc::as_ref).ok_or(Error::Internal.into())
}
fn version(scope: &Scope, value: &Value) -> String {
    Sha256::digest(
        json!([scope.tenant(), scope.user(), value])
            .to_string()
            .as_bytes(),
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}
fn check(input: &Value, scope: &Scope, value: &Value) -> bilibi_agent_api::Result<()> {
    if scope.impersonator().is_some() {
        return Err(Error::Forbidden.into());
    }
    if input["version"] != version(scope, value) {
        return Err(Error::Conflict.into());
    }
    Ok(())
}

enum Read {
    Pins,
    Landing,
}

impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "core/user"
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        _: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let client = client.sql_client().await?;
            let reply = match self {
                Self::Pins => {
                    let pins = json!(list_pins(client, scope).await?);
                    json!({"version": version(scope, &pins), "pins": pins})
                }
                Self::Landing => {
                    let users = users()?;
                    let selected = users.landing_choice(client, scope).await?;
                    let pages = users.landing_pages(client, scope).await?;
                    let label = pages
                        .iter()
                        .find(|page| page.id == selected)
                        .map(|page| page.label);
                    let value = json!(selected);
                    json!({
                        "version": version(scope, &value), "menu_id": selected, "label": label,
                        "default_url": "/workspace",
                        "pages": pages.iter().map(|page| json!({"id":page.id,"label":page.label,"url":page.route})).collect::<Vec<_>>(),
                    })
                }
            };
            Ok(reply.into())
        })
    }
}

enum Write {
    SetPins,
    RestorePins,
    SetLanding,
    RestoreLanding,
}

impl Write {
    fn pins(&self) -> bool {
        matches!(self, Self::SetPins | Self::RestorePins)
    }

    fn plan_pins(
        &self,
        scope: &Scope,
        input: &Value,
        before: &[Pin],
    ) -> bilibi_agent_api::Result<Vec<PinDraft>> {
        check(input, scope, &json!(before))?;
        if matches!(self, Self::RestorePins) {
            let Some(id) = input.get("pin_id") else {
                return Ok(Vec::new());
            };
            let id = id
                .as_i64()
                .filter(|id| *id > 0)
                .ok_or_else(|| invalid("pin_id", "choose a pin ID from read"))?;
            if !before.iter().any(|pin| pin.id == id) {
                return Err(bilibi_agent_api::Error::NotFound);
            }
            return Ok(before
                .iter()
                .filter(|pin| pin.id != id)
                .map(PinDraft::from)
                .collect());
        }

        let pins = input["pins"].as_array().ok_or(Error::Invalid)?;
        if pins.len() as i64 > MAX_PINS {
            return Err(invalid("pins", "choose at most 100 pins"));
        }
        let routes = users()?.pin_routes();
        let mut seen = HashSet::new();
        pins.iter()
            .enumerate()
            .map(|(index, value)| {
                let path = format!("pins[{index}]");
                let pin: PinDraft =
                    serde_json::from_value(value.clone()).map_err(|_| Error::Invalid)?;
                if !pin.url.starts_with('/') {
                    return Err(invalid(&path, "use a site-relative path beginning with /"));
                }
                let pin =
                    PinDraft::clean(&pin.label, &pin.url, pin.icon.as_deref()).map_err(|_| {
                        invalid(
                            &path,
                            "use a valid page path, nonempty label and known icon",
                        )
                    })?;
                if !before.iter().any(|saved| saved.url == pin.url)
                    && !pin_route_is_served(routes, &pin.url)
                {
                    return Err(invalid(&path, "choose a page served by this installation"));
                }
                if !seen.insert(pin.url.clone()) {
                    return Err(invalid(&path, "this page is already pinned"));
                }
                Ok(pin)
            })
            .collect()
    }

    async fn plan_landing<'a>(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        input: &'a Value,
    ) -> bilibi_agent_api::Result<(String, &'a str)> {
        let before = users()?.landing_choice(client, scope).await?;
        check(input, scope, &json!(before))?;
        let value = if matches!(self, Self::SetLanding) {
            input["menu_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| invalid("menu_id", "choose a menu page ID from read"))?
        } else {
            ""
        };
        match users()?.check_landing(client, scope, value).await {
            Err(Error::Invalid) => return Err(invalid("menu_id", "choose a page you can open")),
            result => result?,
        }
        Ok((before, value))
    }
}

impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        "core/user"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let sql = client.sql_client().await?;
            if self.pins() {
                let before = list_pins(sql, scope).await?;
                let after = self.plan_pins(scope, input, &before)?;
                Ok(json!({"before": before, "after": after}).into())
            } else {
                let (before, after) = self.plan_landing(sql, scope, input).await?;
                Ok(json!({"before": before, "after": after, "default_url": "/workspace"}).into())
            }
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            if self.pins() {
                lock_pins(tx, scope).await?;
                let before = list_pins(tx.sql(), scope).await?;
                let after = self.plan_pins(scope, input, &before)?;
                let pins = json!(apply_pins(tx, scope, before, after).await?);
                Ok(json!({"version": version(scope, &pins), "pins": pins}).into())
            } else {
                UserService::lock_landing(tx, scope).await?;
                let (before, after) = self.plan_landing(tx.sql(), scope, input).await?;
                users()?.apply_landing(tx, scope, after).await?;
                Ok(json!({"before": before, "after": after, "version": version(scope, &json!(after)), "default_url": "/workspace"}).into())
            }
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
    json!({"type":"object","required":["version"],"properties":{"version":{"type":"string","maxLength":64,"description":"Version from read."}}})
}
fn pins_restore_schema() -> Value {
    let mut schema = restore_schema();
    schema["properties"]["pin_id"] = json!({"type":"integer","minimum":1,"maximum":i64::MAX,"description":"Remove only this pin from read. Omit to restore the empty default list."});
    schema
}
fn pins_schema() -> Value {
    let mut schema = restore_schema();
    schema["required"] = json!(["version", "pins"]);
    schema["properties"]["pins"] = json!({
        "type":"array","description":"Complete list in display order. Omitted pins are removed.",
        "items":{"type":"object","required":["label","url"],"properties":{
            "label":{"type":"string","maxLength":150},
            "url":{"type":"string","maxLength":8192,"description":"Site-relative page path with optional query."},
            "icon":{"type":"string","maxLength":100,"nullable":true}
        }}
    });
    schema
}
fn landing_schema() -> Value {
    let mut schema = restore_schema();
    schema["required"] = json!(["version", "menu_id"]);
    schema["properties"]["menu_id"] =
        json!({"type":"string","maxLength":200,"description":"Reachable menu page ID from read."});
    schema
}
fn output() -> Value {
    json!({"type":"object"})
}

inventory::submit! { Contribution::new(DEFINITIONS, "core/user.navigation_preferences", provenance!(), &Definitions {
    operations: &[
        Operation { key:PINS_READ, title:"Read my sidebar pins", summary:"Read your pins in order and the version to use when changing them.", keywords:&["sidebar","pins","workspace"], capability:SIGNED_IN, input:empty_schema, output, handler:Handler::Read(&Read::Pins), channels:Channels::Every, step_up:&[] },
        Operation { key:"core.user.pins.set", title:"Set my sidebar pins", summary:"Set your complete ordered list of pins by page path, label and optional icon.", keywords:&["sidebar","pins","workspace"], capability:SIGNED_IN, input:pins_schema, output, handler:Handler::SafeWrite(&Write::SetPins), channels:Channels::Every, step_up:&[] },
        Operation { key:PINS_RESTORE, title:"Restore my sidebar pins", summary:"Remove one pin or clear your list to restore the default.", keywords:&["sidebar","pins","unpin","restore"], capability:SIGNED_IN, input:pins_restore_schema, output, handler:Handler::SafeWrite(&Write::RestorePins), channels:Channels::Every, step_up:&[] },
        Operation { key:LANDING_READ, title:"Read my landing page", summary:"Read your chosen page after sign-in and the menu pages you can choose.", keywords:&["landing","sign-in","start page","workspace"], capability:SIGNED_IN, input:empty_schema, output, handler:Handler::Read(&Read::Landing), channels:Channels::Every, step_up:&[] },
        Operation { key:LANDING_SET, title:"Set my landing page", summary:"Choose which permitted menu page opens after sign-in.", keywords:&["landing","sign-in","start page"], capability:SIGNED_IN, input:landing_schema, output, handler:Handler::SafeWrite(&Write::SetLanding), channels:Channels::Every, step_up:&[] },
        Operation { key:LANDING_RESTORE, title:"Restore my default landing page", summary:"Clear your landing-page choice so sign-in opens the workspace again.", keywords:&["landing","sign-in","restore"], capability:SIGNED_IN, input:restore_schema, output, handler:Handler::SafeWrite(&Write::RestoreLanding), channels:Channels::Every, step_up:&[] },
    ],
    guides: &[],
}) }

fn pin_rows(reply: &Value) -> Vec<bilibi_labels::CustomisationRow> {
    reply["pins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pin| {
            let id = pin["id"].as_i64()?;
            Some(bilibi_labels::CustomisationRow {
                description: format!("{} · {}", pin["label"].as_str()?, pin["url"].as_str()?),
                audit_kind: "user_pin",
                audit_id: id,
                revision: 0,
                input: json!({"pin_id":id,"version":reply["version"]}),
            })
        })
        .collect()
}
fn landing_rows(reply: &Value) -> Vec<bilibi_labels::CustomisationRow> {
    let Some(id) = reply["menu_id"].as_str().filter(|id| !id.is_empty()) else {
        return Vec::new();
    };
    vec![bilibi_labels::CustomisationRow {
        description: reply["label"].as_str().unwrap_or(id).into(),
        audit_kind: "user_landing",
        audit_id: 0,
        revision: 0,
        input: json!({"version":reply["version"]}),
    }]
}

inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "core/user.pins_customisations", provenance!(), &bilibi_labels::CustomisationSection {
    title:"My pins",
    undo: Some(bilibi_labels::Undo { read: PINS_READ, read_input: empty, action: Some(bilibi_labels::UndoAction { operation: PINS_RESTORE, event: PINS_UNDO, label: "Remove pin", detail: "This removes the shortcut from your sidebar." }), rows: pin_rows }),
    changes:None,
}) }
inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "core/user.landing_customisations", provenance!(), &bilibi_labels::CustomisationSection {
    title:"My landing page",
    undo: Some(bilibi_labels::Undo { read: LANDING_READ, read_input: empty, action: Some(bilibi_labels::UndoAction { operation: LANDING_RESTORE, event: LANDING_UNDO, label: "Restore default", detail: "The workspace will open after sign-in." }), rows: landing_rows }),
    changes:None,
}) }
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "core.user.pins_customisations_restore", provenance!(), &bilibi_agent_api::page::PageBinding {
    service:"base/labels.customisations", event:PINS_UNDO, contract:PINS_RESTORE,
}) }
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "core.user.landing_customisations_restore", provenance!(), &bilibi_agent_api::page::PageBinding {
    service:"base/labels.customisations", event:LANDING_UNDO, contract:LANDING_RESTORE,
}) }
