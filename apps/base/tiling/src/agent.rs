//! The shared workspace contracts used by the connector and both pages.
use crate::layouts::{self, Entry, Layouts, SHARED};
use bilibi_agent_api::{
    invalid, Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS, SIGNED_IN,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use tokio_postgres::GenericClient;

pub(crate) const PUBLISH: &str = "base.tiling.publish";
pub(crate) const WITHDRAW: &str = "base.tiling.withdraw";
const READ: &str = "base.tiling.read";
const MANAGE: &str = "base.tiling.manage";
const GUIDE: &str = "base.tiling";
const UNDO: &str = "customisation.workspace_withdraw";
static LAYOUTS: OnceLock<Arc<Layouts>> = OnceLock::new();

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/tiling",
            source: file!(),
            line: line!(),
        }
    };
}

pub(crate) fn bind(layouts: Arc<Layouts>) -> Result<(), RegistryError> {
    LAYOUTS
        .set(layouts)
        .map_err(|_| RegistryError::Contribution("shared workspace contracts already bound".into()))
}

fn layouts() -> bilibi_agent_api::Result<&'static Layouts> {
    LAYOUTS.get().map(Arc::as_ref).ok_or(Error::Internal.into())
}

pub(crate) fn version(entry: Option<&Entry>) -> String {
    entry.map_or_else(String::new, |entry| {
        Sha256::digest(json!(entry).to_string().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    })
}

fn presented(entry: &Entry) -> Value {
    let mut value = json!(entry);
    value["version"] = json!(version(Some(entry)));
    value["url"] = json!(format!("/workspace/shared/{}", entry.slug));
    value
}

enum Read {
    Visible,
    Manage,
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "base/tiling"
    }

    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let client = client.sql_client().await?;
            let company = scope.current_company().ok_or(Error::Forbidden)?;
            let layouts = layouts()?;
            let mut reply = json!({"company_id": company.get()});
            let entries = match self {
                Self::Visible => layouts.shared_visible(client, scope).await?,
                Self::Manage => {
                    bilibi_authz::authorize_key(client, scope, layouts::PUBLISH).await?;
                    reply["roles"] = json!(layouts::role_options(client, scope)
                        .await?
                        .into_iter()
                        .map(|role| json!({"name": role.name, "code": role.code}))
                        .collect::<Vec<_>>());
                    layouts.shared(client, scope).await?
                }
            };
            if let (Self::Manage, Some(label)) = (self, input["label"].as_str()) {
                let label = layouts::clean_label(label)
                    .ok_or_else(|| invalid("label", "enter a layout name of up to 60 bytes"))?;
                let current = entries.iter().find(|entry| entry.label == label);
                reply["proposal"] = json!({
                    "slug":layouts::publish_slug(&label, current, &entries),
                    "version":version(current),
                });
            }
            reply["layouts"] = json!(entries.iter().map(presented).collect::<Vec<_>>());
            Ok(reply.into())
        })
    }
}

enum Write {
    Publish,
    Withdraw,
}

impl Write {
    async fn plan(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        input: &Value,
    ) -> bilibi_agent_api::Result<(Value, Value)> {
        bilibi_authz::authorize_key(client, scope, layouts::PUBLISH).await?;
        let company = scope.current_company().ok_or(Error::Forbidden)?;
        if input["company_id"].as_i64() != Some(company.get()) {
            return Err(invalid(
                "company_id",
                "does not match the current workplace",
            ));
        }
        let layouts = layouts()?;
        let mut entries = layouts.shared(client, scope).await?;
        let (before, after) = match self {
            Self::Publish => {
                let label = layouts::clean_label(input["label"].as_str().unwrap_or_default())
                    .ok_or_else(|| invalid("label", "enter a layout name of up to 60 bytes"))?;
                let tree = layouts::clean_tree(input["tree"].as_str().unwrap_or_default())
                    .ok_or_else(|| {
                        invalid(
                            "tree",
                            "provide at least one page in a valid workspace tree",
                        )
                    })?;
                let layout = crate::layout::Layout::decode(&tree).ok_or(Error::Internal)?;
                if layout
                    .leaves()
                    .iter()
                    .any(|leaf| !layouts.route(&leaf.path).is_some_and(|route| route.framed))
                {
                    return Err(invalid(
                        "tree",
                        "each tile must open an installed page that supports tiles",
                    ));
                }
                let mut roles: Vec<String> =
                    serde_json::from_value(input["roles"].clone()).map_err(|_| Error::Invalid)?;
                let known = layouts::role_options(client, scope).await?;
                if roles.len() > known.len()
                    || !roles
                        .iter()
                        .all(|role| known.iter().any(|r| &r.code == role))
                {
                    return Err(invalid("roles", "choose role codes from this tenant"));
                }
                roles.sort_unstable();
                roles.dedup();
                let slot = entries.iter().position(|entry| entry.label == label);
                let current = slot.map(|index| &entries[index]);
                check_version(input, current)?;
                let slug = layouts::publish_slug(&label, current, &entries);
                if input["slug"].as_str() != Some(slug.as_str()) {
                    return Err(Error::Conflict.into());
                }
                let next = Entry {
                    slug,
                    label,
                    tree,
                    roles,
                };
                let after = presented(&next);
                let before = if let Some(index) = slot {
                    presented(&std::mem::replace(&mut entries[index], next))
                } else {
                    entries.push(next);
                    Value::Null
                };
                (before, after)
            }
            Self::Withdraw => {
                let slug = input["slug"].as_str().ok_or(Error::Invalid)?;
                let Some(index) = entries.iter().position(|entry| entry.slug == slug) else {
                    return Err(bilibi_agent_api::Error::NotFound);
                };
                check_version(input, Some(&entries[index]))?;
                (presented(&entries.remove(index)), Value::Null)
            }
        };
        let stored = json!(entries);
        if !layouts::valid_entries(&stored) {
            return Err(invalid(
                "label",
                "the company has reached its shared workspace limit",
            ));
        }
        Ok((
            stored,
            json!({"company_id": company.get(), "before": before, "after": after}),
        ))
    }
}

fn check_version(input: &Value, current: Option<&Entry>) -> bilibi_agent_api::Result<()> {
    if input["version"] != version(current) {
        return Err(Error::Conflict.into());
    }
    Ok(())
}

impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        "base/tiling"
    }

    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (_, review) = self.plan(client.sql_client().await?, scope, input).await?;
            Ok(review.into())
        })
    }

    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let company = scope.current_company().ok_or(Error::Forbidden)?;
            // Serialize the read/modify/write of the company's array, including
            // first publication when there is no settings row to lock.
            let lock = format!("base/tiling:{}:{}", scope.tenant(), company.get());
            tx.sql()
                .execute(
                    "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                    &[&lock],
                )
                .await
                .map_err(Error::from)?;
            let (stored, review) = self.plan(tx.sql(), scope, input).await?;
            let settings = &layouts()?.settings;
            let target = layouts::shared_target(scope)?;
            if stored.as_array().is_some_and(Vec::is_empty) {
                settings.delete(tx, scope, target, SHARED).await?;
            } else if review["before"] != review["after"] {
                settings.put(tx, scope, target, SHARED, stored).await?;
            }
            Ok(review.into())
        })
    }
}

fn empty() -> Value {
    json!({})
}
fn empty_schema() -> Value {
    json!({"type":"object","properties":{}})
}
fn manage_schema() -> Value {
    json!({"type":"object","properties":{
        "label":{"type":"string","maxLength":60,"description":"Optional label to get its proposed slug and version."}
    }})
}
fn publish_schema() -> Value {
    json!({"type":"object","required":["company_id","label","slug","tree","roles","version"],"properties":{
        "company_id":{"type":"integer","minimum":1,"description":"Current workplace ID from manage."},
        "label":{"type":"string","maxLength":60},
        "slug":{"type":"string","maxLength":90,"description":"Proposed slug from manage for this label."},
        "tree":{"type":"string","maxLength":2048,"description":"Workspace tree, for example h.5(/companies,/users) for side-by-side tiles."},
        "roles":{"type":"array","items":{"type":"string","maxLength":100},"description":"Audience role codes from manage. Empty means the company."},
        "version":{"type":"string","maxLength":64,"description":"Matching layout version from manage, or an empty string to publish a new label."}
    }})
}
fn withdraw_schema() -> Value {
    json!({"type":"object","required":["company_id","slug","version"],"properties":{
        "company_id":{"type":"integer","minimum":1},
        "slug":{"type":"string","maxLength":90},
        "version":{"type":"string","maxLength":64}
    }})
}
fn output() -> Value {
    json!({"type":"object"})
}

inventory::submit! { Contribution::new(DEFINITIONS, "base/tiling.agent", provenance!(), &Definitions {
    operations: &[
        Operation { key:READ, title:"Read shared workspaces", summary:"Read the workspaces published for your workplace and roles.", keywords:&["tiles","workspace","layout"], capability:SIGNED_IN, input:empty_schema, output, handler:Handler::Read(&Read::Visible), channels:Channels::Every, step_up:&[] },
        Operation { key:MANAGE, title:"List shared layouts and role audiences", summary:"Publishers can read all shared workspaces and role codes for the current workplace.", keywords:&["tiles","workspace","layout","role"], capability:SIGNED_IN, input:manage_schema, output, handler:Handler::Read(&Read::Manage), channels:Channels::Every, step_up:&[] },
        Operation { key:PUBLISH, title:"Publish a shared workspace", summary:"Publish a tile layout for the current company or selected roles.", keywords:&["tiles","workspace","layout","publish"], capability:layouts::PUBLISH, input:publish_schema, output, handler:Handler::SafeWrite(&Write::Publish), channels:Channels::Every, step_up:&[] },
        Operation { key:WITHDRAW, title:"Withdraw a shared workspace", summary:"Remove one published layout from the current company.", keywords:&["tiles","workspace","layout","withdraw"], capability:layouts::PUBLISH, input:withdraw_schema, output, handler:Handler::SafeWrite(&Write::Withdraw), channels:Channels::Every, step_up:&[] },
    ],
    guides: &[Guide { key:GUIDE, title:"Shared tile layouts", summary:"Publish a workspace of existing pages side by side for a company or role.", keywords:&["workspace","tile","side-by-side","role","service desk"], capability:layouts::PUBLISH, body:"Use base.tiling.manage to find the current company, layouts and audience role codes. For side-by-side tiles use h.5(/companies,/users); v.5 places tiles top and bottom. Splits can nest, for example h.5(/companies,v.6(/employees,/users)). Use paths from the site's pages. Percent-encode reserved tree markers ( ) , > and % inside paths. Pass the label to base.tiling.manage for its proposed slug and version. Publish with the company ID, label, proposed slug, tree, audience role codes and version. An empty role list means the whole company. Pass the saved version to replace the same label, or an empty version for a new label. Use base.tiling.read for the viewer's layout links. Each tile checks page access when opened. Withdraw by company ID, slug and version. Review the exact proposal before saving." }],
}) }

fn rows(value: &Value) -> Vec<bilibi_labels::CustomisationRow> {
    value["layouts"].as_array().into_iter().flatten().filter_map(|row| {
        let slug = row["slug"].as_str()?;
        let label = row["label"].as_str()?;
        let roles = row["roles"].as_array()?.iter().filter_map(Value::as_str).collect::<Vec<_>>();
        let audience = if roles.is_empty() { "Whole company".into() } else { roles.join(", ") };
        let id = i64::from_be_bytes(Sha256::digest(slug.as_bytes())[..8].try_into().ok()?);
        Some(bilibi_labels::CustomisationRow {
            description: format!("{label} · {audience} · {}", row["tree"].as_str()?),
            audit_kind: "shared_workspace",
            audit_id: id,
            revision: 0,
            input: json!({"company_id":value["company_id"],"slug":slug,"version":row["version"]}),
        })
    }).collect()
}

fn choices(reads: &[Value]) -> Vec<bilibi_labels::Choice> {
    let managed = &reads[0];
    let company_id = &managed["company_id"];
    let mut choices = vec![bilibi_labels::Choice {
        subject: "new".into(),
        title: "Publish a new shared workspace".into(),
        keywords: "workspace tile layout side-by-side service desk role publish",
        value: Some(Value::Null),
        version: Some(json!("")),
        input: json!({"company_id":company_id}),
        verify_input: empty(),
        limits: json!({"roles":managed["roles"]}),
        writes: Some(vec![PUBLISH]),
        ..Default::default()
    }];
    choices.extend(
        managed["layouts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|layout| {
                let slug = layout["slug"].as_str()?;
                let label = layout["label"].as_str()?;
                Some(bilibi_labels::Choice {
                    subject: format!("layout:{slug}"),
                    title: label.into(),
                    keywords:
                        "workspace tile layout side-by-side service desk role publish withdraw",
                    page: layout["url"].as_str().map(str::to_owned),
                    value: Some(layout.clone()),
                    default: Some(Value::Null),
                    customised: true,
                    version: Some(layout["version"].clone()),
                    input: json!({"company_id":company_id,"label":label,"slug":slug}),
                    verify_input: empty(),
                    limits: json!({"roles":managed["roles"]}),
                    ..Default::default()
                })
            }),
    );
    choices
}

inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/tiling.customisations", provenance!(), &bilibi_labels::CustomisationSection {
    title:"Shared workspaces",
    undo:Some(bilibi_labels::Undo { read:MANAGE, read_input:empty, operation:WITHDRAW, event:UNDO, action:"Withdraw workspace", detail:"This removes the workspace from the people it was shared with.", rows }),
    changes:Some(bilibi_labels::Changes {
        purpose:"Publish and withdraw shared tile layouts for the current company.",
        limits:"Use existing page paths and role codes from the current company. Each tile keeps its own access checks.",
        guide:GUIDE,
        reads:&[bilibi_labels::Read { operation:MANAGE, input:empty }],
        choices,
        change:&[PUBLISH],
        preview:None,
        verify:MANAGE,
        restore:Some(WITHDRAW),
    }),
}) }
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.tiling.customisations_withdraw", provenance!(), &bilibi_agent_api::page::PageBinding {
    service:"base/labels.customisations", event:UNDO, contract:WITHDRAW,
}) }
