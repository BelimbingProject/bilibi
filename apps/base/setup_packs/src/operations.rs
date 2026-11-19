use crate::{pack, schema, SetupPacks, MANAGE, MODULE, VIEW};
use bilibi_agent_api::{
    invalid, Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, Result, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_tenancy::Scope;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

static BOUND: OnceLock<Arc<SetupPacks>> = OnceLock::new();

pub(crate) fn bind(packs: Arc<SetupPacks>) -> bilibi_module_registry::Result<()> {
    BOUND
        .set(packs)
        .map_err(|_| RegistryError::Service("duplicate setup packs binding".into()))
}

fn bound() -> Result<&'static SetupPacks> {
    BOUND.get().map(Arc::as_ref).ok_or(Error::Internal.into())
}

fn input<T: DeserializeOwned>(value: &Value) -> Result<T> {
    serde_json::from_value(value.clone())
        .map_err(|_| invalid("input", "Supply the described setup pack format."))
}

fn output(value: impl Serialize) -> bilibi_agent_api::json::Output {
    serde_json::to_value(value)
        .expect("setup pack output serializes")
        .into()
}

enum Read {
    Export,
    Preview,
}

impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        MODULE
    }

    fn requires_transaction(&self) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        value: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let packs = bound()?;
            let tx = client.transaction().ok_or(Error::Internal)?;
            match self {
                Self::Export => Ok(output(packs.export(tx, scope, input(value)?).await?)),
                Self::Preview => Ok(output(
                    packs.difference(tx, scope, input(value)?, false).await?,
                )),
            }
        })
    }
}

struct Apply;

impl WriteHandler for Apply {
    fn owner(&self) -> &'static str {
        MODULE
    }

    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        value: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let tx = client.transaction().ok_or(Error::Internal)?;
            let request: pack::ApplyRequest = input(value)?;
            let diff = bound()?
                .difference(tx, scope, request.import, false)
                .await?;
            diff.check_review(&request.expected_additions)?;
            Ok(output(diff))
        })
    }

    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        value: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_key(tx.sql(), scope, MANAGE).await?;
            let packs = bound()?;
            let request: pack::ApplyRequest = input(value)?;
            let diff = packs
                .difference(tx.sql(), scope, request.import, true)
                .await?;
            diff.check_review(&request.expected_additions)?;
            packs.apply(tx, scope, diff.additions).await
        })
    }
}

impl SetupPacks {
    async fn apply(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        additions: pack::Additions,
    ) -> Result<bilibi_agent_api::json::Output> {
        let counts = json!({
            "labels_added": additions.labels.len(),
            "custom_fields_added": additions.custom_fields.len(),
        });
        for label in additions.labels {
            self.labels.apply(tx, scope, label.edit()?).await?;
        }
        for field in additions.custom_fields {
            if let Err(errors) = self.fields.define(tx, scope, &(&field).into()).await? {
                let mut fields = std::collections::BTreeMap::<_, Vec<_>>::new();
                for error in errors {
                    fields.entry(error.field).or_default().push(error.message);
                }
                return Err(bilibi_agent_api::Error::InvalidInput(fields));
            }
        }
        Ok(counts.into())
    }
}

inventory::submit! { Contribution::new(DEFINITIONS, "base/setup_packs.agent", Provenance {
    module: MODULE, source: file!(), line: line!()
}, &Definitions {
    operations: &[
        Operation {
            key: "base.setup_packs.export",
            title: "Export selected setup",
            summary: "Export selected label overrides and active custom field definitions as one versioned pack.",
            keywords: &["setup", "customisation", "pack", "export"],
            capability: VIEW,
            input: schema::selection,
            output: schema::pack,
            handler: Handler::Read(&Read::Export),
            channels: Channels::Every,
            step_up: &[],
        },
        Operation {
            key: "base.setup_packs.preview",
            title: "Preview a setup pack",
            summary: "Show additions, collisions, missing dependencies and invalid entries in the current tenant. Identical entries leave an empty difference.",
            keywords: &["setup", "customisation", "pack", "import"],
            capability: VIEW,
            input: schema::import,
            output: schema::preview,
            handler: Handler::Read(&Read::Preview),
            channels: Channels::Every,
            step_up: &[],
        },
        Operation {
            key: "base.setup_packs.apply",
            title: "Apply a setup pack",
            summary: "Add a setup pack to the current tenant in one reviewed write. Collisions, missing dependencies or invalid entries refuse the entire pack.",
            keywords: &["setup", "customisation", "pack", "import"],
            capability: MANAGE,
            input: schema::apply,
            output: schema::applied,
            handler: Handler::SafeWrite(&Apply),
            channels: Channels::Every,
            step_up: &[],
        },
    ],
    guides: &[Guide {
        key: "base.setup_packs",
        title: "Setup packs",
        summary: "Copy selected label overrides and custom field definitions between tenants.",
        capability: VIEW,
        keywords: &["setup", "customisation", "pack", "transfer"],
        body: "Use base.labels.list and base.custom_fields.list in the source tenant to choose entries, then call base.setup_packs.export. The pack contains only label overrides and active custom field definitions. In the destination tenant, call base.setup_packs.preview with the pack. Map any pick-list to an eligible destination key using picklist_map (from and to); unmapped lists use their stable keys. Each pack carries at most 100 entries. Resolve every reported issue before applying. Existing identical entries are omitted. A different label or a different or archived field with the same key is a collision. Copy expected_additions into base.setup_packs.apply along with the same pack and mappings. Dry-run the operation, show the additions and wait for explicit approval before committing the unchanged operation. All additions commit together and are audited by their owners. An empty difference applies zero changes.",
    }],
}) }
