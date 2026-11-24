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
            "views_added": additions.views.len(),
            "watchers_added": additions.watchers.len(),
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
        for saved in additions.views {
            self.views
                .import_view(tx, scope, &saved.setup, &saved.audience)
                .await?;
        }
        for watcher in additions.watchers {
            self.watchers
                .import_draft(tx, scope, &watcher.setup, &watcher.examples)
                .await?;
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
            summary: "Export selected label overrides, active custom field definitions, shared list views and watcher drafts as one versioned pack.",
            keywords: &["setup", "customisation", "pack", "export"],
            capability: VIEW,
            input: schema::export_selection,
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
        summary: "Copy selected labels, field definitions, shared views and watcher drafts between tenants.",
        capability: VIEW,
        keywords: &["setup", "customisation", "pack", "transfer"],
        body: "Choose source entries with base.labels.list, base.custom_fields.list, base.grid.view.pages and base.watchers.list. Export up to 100 entries with base.setup_packs.export. In the destination, preview with base.setup_packs.preview. Use picklist_map (from and to keys) to map pick-lists. Use role_map (from exported role key, to destination role ID) to map audiences. System roles and pick-lists can use unchanged stable keys. Supply destination-local watcher_examples for new watchers with conditions. Review free-text warnings and remove source record values. Resolve all missing dependencies, collisions and invalid entries. Copy expected_additions into base.setup_packs.apply with the same pack, mappings and destination examples, dry-run and obtain explicit approval before committing. All additions commit together through their owners. Watchers arrive as drafts and need separate destination approval. A repeat preview shows an empty difference.",
    }],
}) }
