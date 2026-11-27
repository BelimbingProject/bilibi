//! The public site operations agents may call (`base.public.*`): thin
//! adapters over the [`Public`] facade with the sealed scope the registry
//! proved. Every write is a reviewed safe write whose preview shares the
//! facade's checks; the Public site page calls the same contracts.
use crate::{
    sites::{Checked, FieldError, LogoChange, SiteInput, MANAGE, VIEW},
    Public,
};
use bilibi_agent_api::{
    json::{Data, Output},
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

pub const SITE_GET: &str = "base.public.site.get";
pub const SITE_SET: &str = "base.public.site.set";
pub const HOSTNAME_ADD: &str = "base.public.hostname.add";
pub const HOSTNAME_REMOVE: &str = "base.public.hostname.remove";

static BOUND: OnceLock<Arc<Public>> = OnceLock::new();
pub(crate) fn bind(public: Arc<Public>) -> bilibi_module_registry::Result<()> {
    BOUND.set(public).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate public site binding".into())
    })
}
fn public() -> bilibi_agent_api::Result<&'static Public> {
    BOUND.get().map(Arc::as_ref).ok_or(Error::Internal.into())
}
fn checked<T>(checked: Checked<T>) -> bilibi_agent_api::Result<T> {
    checked.map_err(|errors: Vec<FieldError>| {
        let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for error in errors {
            fields
                .entry(error.field.to_owned())
                .or_default()
                .push(error.message);
        }
        bilibi_agent_api::Error::InvalidInput(fields)
    })
}
fn found(error: Error) -> bilibi_agent_api::Error {
    match error {
        Error::NotFound => bilibi_agent_api::Error::NotFound,
        other => other.into(),
    }
}
fn site_input(input: &Value) -> SiteInput {
    SiteInput {
        address: bilibi_agent_api::text_field(input, "address"),
        title: bilibi_agent_api::text_field(input, "title"),
        enabled: input["enabled"].as_bool(),
        logo: match (
            input["remove_logo"].as_bool(),
            input["logo_asset_id"].as_i64(),
        ) {
            (Some(true), _) => LogoChange::Remove,
            (_, Some(id)) => LogoChange::Set(id),
            _ => LogoChange::Keep,
        },
        version: input["version"].as_i64(),
    }
}
fn hostname(input: &Value) -> String {
    bilibi_agent_api::text_field(input, "hostname")
}

struct Read;
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "base/public"
    }
    fn call<'a>(
        &'a self,
        _client: &'a dyn OperationClient,
        scope: &'a Scope,
        _input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let public = public()?;
            let client = public.db.get().await.map_err(Error::from)?;
            let site = public.site_of(&**client, scope).await.map_err(found)?;
            Ok(Output::Data(Data::Json(json!({
                "site": site.map(|site| site.to_json()),
            }))))
        })
    }
}

enum Write {
    Site,
    Add,
    Remove,
}
impl Write {
    async fn preview_on(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        input: &Value,
    ) -> bilibi_agent_api::Result<Output> {
        let public = public()?;
        let value = match self {
            Self::Site => {
                let (before, after) = checked(
                    public
                        .preview_site(client, scope, &site_input(input))
                        .await
                        .map_err(found)?,
                )?;
                json!({
                    "summary": format!(
                        "Set the public site {} ({}){}",
                        after.title,
                        after.address,
                        if after.enabled { ", on" } else { ", off" }
                    ),
                    "before": before.map(|site| site.to_json()),
                    "after": after.to_json(),
                })
            }
            Self::Add => {
                let hostname = checked(
                    public
                        .preview_hostname(
                            client,
                            scope,
                            &hostname(input),
                            input["primary"].as_bool().unwrap_or(false),
                        )
                        .await
                        .map_err(found)?,
                )?;
                json!({
                    "summary": format!(
                        "Serve the public site at {}{}",
                        hostname.name,
                        if hostname.primary { " (primary: emailed links use it)" } else { "" }
                    ),
                    "after": {"hostname": hostname.name, "primary": hostname.primary},
                })
            }
            Self::Remove => {
                let site = public
                    .site_of(client, scope)
                    .await
                    .map_err(found)?
                    .ok_or(bilibi_agent_api::Error::NotFound)?;
                let name = hostname(input).trim().to_ascii_lowercase();
                let found = site
                    .hostnames
                    .iter()
                    .find(|h| h.name == name)
                    .ok_or(bilibi_agent_api::Error::NotFound)?;
                json!({
                    "summary": format!("Stop serving the public site at {}; links to it stop working", found.name),
                    "before": {"hostname": found.name, "primary": found.primary},
                })
            }
        };
        Ok(value.into())
    }
}
impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        "base/public"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            // The door's own transaction when it gives one: a preview never
            // asks the pool for a second connection.
            match client.transaction() {
                Some(tx) => self.preview_on(tx, scope, input).await,
                None => {
                    let pooled = public()?.db.get().await.map_err(Error::from)?;
                    self.preview_on(&**pooled, scope, input).await
                }
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
            let public = public()?;
            let value = match self {
                Self::Site => checked(
                    public
                        .set_site_in(tx, scope, &site_input(input))
                        .await
                        .map_err(found)?,
                )?
                .to_json(),
                Self::Add => {
                    let added = checked(
                        public
                            .add_hostname_in(
                                tx,
                                scope,
                                &hostname(input),
                                input["primary"].as_bool().unwrap_or(false),
                            )
                            .await
                            .map_err(found)?,
                    )?;
                    json!({"hostname": added.name, "primary": added.primary})
                }
                Self::Remove => {
                    let removed = checked(
                        public
                            .remove_hostname_in(tx, scope, &hostname(input))
                            .await
                            .map_err(found)?,
                    )?;
                    json!({"hostname": removed.name, "removed": true})
                }
            };
            Ok(Output::Data(Data::Json(value)))
        })
    }
}

fn empty_schema() -> Value {
    json!({"type":"object","properties":{}})
}
fn site_schema() -> Value {
    json!({"type":"object","required":["address","title"],"properties":{
        "address":{"type":"string","maxLength":63,"description":"The site's address, such as acme: lowercase letters, digits and inner hyphens, unique across the installation."},
        "title":{"type":"string","maxLength":120,"description":"The name visitors and customers see on the site's pages and in its emails."},
        "enabled":{"type":"boolean","description":"Turn the public site on or off. Leave it out to keep it as it is; a new site starts off. While off, every page of it answers 404."},
        "logo_asset_id":{"type":"integer","minimum":1,"description":"A PNG, JPEG, GIF or WebP image already stored in Media as this workspace's own. Leave it out to keep the logo."},
        "remove_logo":{"type":"boolean","description":"Remove the logo."},
        "version":{"type":"integer","minimum":1,"description":"The version you read; a site changed since is refused."},
    }})
}
fn hostname_schema() -> Value {
    json!({"type":"object","required":["hostname"],"properties":{
        "hostname":{"type":"string","maxLength":253,"description":"A name such as shop.example.com, without https:// or a port. Point its DNS at this installation's public address first."},
        "primary":{"type":"boolean","description":"Make it the primary hostname, which emailed links use. The first hostname is always primary."},
    }})
}
fn remove_schema() -> Value {
    json!({"type":"object","required":["hostname"],"properties":{
        "hostname":{"type":"string","maxLength":253,"description":"One of the site's hostnames. The primary one goes only when it is the last."},
    }})
}
fn site_output() -> Value {
    json!({"type":"object","description":"site: address, title, enabled, logo_asset_id, version and hostnames (hostname, primary), or null when none is set."})
}
fn record_output() -> Value {
    json!({"type":"object","description":"The saved record."})
}

const MODULE: &str = "base/public";
inventory::submit! { Contribution::new(DEFINITIONS, "base/public.agent", Provenance { module: MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: SITE_GET, title: "Read the public site", summary: "This tenant's public site: its address, title, whether it is on, and the hostnames it answers at.", keywords: &["public", "site", "hostname", "portal", "domain"], capability: VIEW, input: empty_schema, output: site_output, handler: Handler::Read(&Read), channels: Channels::Every, step_up: &[] },
        Operation { key: SITE_SET, title: "Set the public site", summary: "Set the public site's address and title, its logo, and whether it is on. A new site starts off; while off, every page of it answers 404.", keywords: &["public", "site", "title", "logo", "enable"], capability: MANAGE, input: site_schema, output: record_output, handler: Handler::SafeWrite(&Write::Site), channels: Channels::Every, step_up: &[] },
        Operation { key: HOSTNAME_ADD, title: "Add a public site hostname", summary: "Serve the public site at a hostname such as shop.example.com, or make one of its hostnames primary. The hostname names this workspace on the public listener, so it is unique across the installation.", keywords: &["public", "site", "hostname", "domain", "primary"], capability: MANAGE, input: hostname_schema, output: record_output, handler: Handler::SafeWrite(&Write::Add), channels: Channels::Every, step_up: &[] },
        Operation { key: HOSTNAME_REMOVE, title: "Remove a public site hostname", summary: "Stop serving the public site at one of its hostnames. Links already sent to it stop working.", keywords: &["public", "site", "hostname", "domain", "remove"], capability: MANAGE, input: remove_schema, output: record_output, handler: Handler::SafeWrite(&Write::Remove), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[Guide { key: "base.public", title: "The public site", summary: "How a workspace's public site is named and turned on: its address, title, hostnames and whether it is on.", keywords: &["public", "site", "hostname", "portal"], capability: VIEW,
        body: "A workspace's customers, visitors and the people its emails reach use its public site, never the staff site. Set it with base.public.site.set: an address such as acme, the title people see, and enabled true to turn it on; it is off until then, and while off every page of it answers 404. Give it at least one hostname with base.public.hostname.add, such as shop.example.com, after its DNS points at the installation's public address: the hostname alone names this workspace on the public listener. The first hostname is primary, and emailed links (customer invitations, password resets, calendar feeds) use the primary one. Remove one with base.public.hostname.remove. Read everything with base.public.site.get. For each write, run the dry-run check, show the complete preview, wait for explicit approval, then commit the unchanged operations." }],
}) }
