//! Transfer selected tenant setup through the owners' audited contracts.
mod operations;
mod pack;
mod schema;
mod transfer;

use bilibi_authz::{CapabilityDefinition, VocabularyContribution, VOCABULARY};
use bilibi_custom_fields::CustomFields;
use bilibi_labels::Labels;
use bilibi_module_registry::{Contribution, Layer, Module, Provenance, Service};
use std::sync::Arc;

const MODULE: &str = "base/setup_packs";
const SERVICE: &str = "base/setup_packs.packs";
const VIEW: &str = "base.setup-packs.view";
const MANAGE: &str = "base.setup-packs.manage";

struct SetupPacks {
    labels: Arc<Labels>,
    views: Arc<bilibi_grid::PageViews>,
    watchers: Arc<bilibi_watchers::Watchers>,
    fields: Arc<CustomFields>,
}

inventory::submit! { Contribution::new(VOCABULARY, MODULE, Provenance {
    module: MODULE, source: file!(), line: line!()
}, &VocabularyContribution {
    domains: &["base"],
    verbs: &[],
    capabilities: &[CapabilityDefinition::tenant(VIEW), CapabilityDefinition::tenant(MANAGE)],
    roles: &[],
    system_principals: &[],
}) }

inventory::submit! {
    Module {
        id: MODULE,
        layer: Layer::Base,
        dependencies: &[
            "base/agent_api", "base/authz", "base/custom_fields", "base/database",
            "base/grid", "base/labels", "base/module_registry", "base/tenancy", "base/watchers",
        ],
        migrations: &[],
        services: &[Service { id: SERVICE, factory: |services| {
            let packs = Arc::new(SetupPacks {
                views: services.get(bilibi_grid::PAGE_VIEWS)?,
                watchers: services.get(bilibi_watchers::SERVICE)?,
                labels: services.get(bilibi_labels::SERVICE)?,
                fields: services.get(bilibi_custom_fields::SERVICE)?,
            });
            operations::bind(packs.clone())?;
            Ok(packs)
        } }],
    }
}
