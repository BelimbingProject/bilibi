//! Named, versioned prose slots for business owners. See README.md.
mod catalogue;
mod language;
mod operations;
mod print;
mod store;

pub use catalogue::{
    bounded, Audience, Context, DefaultText, Origin, Output, Problem, Resolved, Slot, Template,
    Text, Value, Variable, CATALOGUE, PLATFORM_CATALOGUE,
};
pub use language::LANGUAGE;
pub use print::{
    CompiledPrint, PrintContract, PrintDefault, PrintFacts, PrintField, PrintKind, PrintValue,
    ResolvedPrint, PRINT_CATALOGUE,
};
pub use store::Templates;

use bilibi_authz::{CapabilityDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{Catalog, Contribution, Module, Service};
use std::sync::Arc;

const MODULE: &str = "base/templates";
pub const SERVICE: &str = "base/templates.templates";
const VIEW: &str = "base.templates.view";
const MANAGE: &str = "base.templates.manage";

macro_rules! provenance {
    () => {
        bilibi_module_registry::Provenance {
            module: crate::MODULE,
            source: file!(),
            line: line!(),
        }
    };
}
use provenance;

inventory::submit! {
    Module {
        id: MODULE, layer: bilibi_module_registry::Layer::Base,
        dependencies: &["base/agent_api","base/authz","base/database","base/datetime","base/labels","base/module_registry","base/party_ids","base/settings","base/tenancy"],
        migrations: &[("base-templates-001-revisions", include_str!("../migrations/001-revisions.sql"))],
        services: &[
            Service { id: SERVICE, factory: |services| {
                let catalogue = Catalog::discover(&bilibi_module_registry::discover()?)?;
                let templates = Arc::new(Templates::new(
                    &catalogue, services.get("base/tenancy.parties")?,
                    services.get("base/settings.store")?,
                )?);
                operations::bind(templates.clone())?;
                Ok(templates)
            }},
        ],
    }
}

inventory::submit! { Contribution::new(VOCABULARY, MODULE, provenance!(), &VocabularyContribution {
    domains: &["base"], verbs: &[], roles: &[], system_principals: &[],
    capabilities: &[CapabilityDefinition::tenant(VIEW), CapabilityDefinition::tenant(MANAGE)],
}) }
