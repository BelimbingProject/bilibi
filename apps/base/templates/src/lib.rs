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
        dependencies: &["base/agent_api","base/artifacts","base/authz","base/branding","base/database","base/datetime","base/labels","base/module_registry","base/party_ids","base/queue","base/schedule","base/settings","base/tenancy","base/ui"],
        migrations: &[
            ("base-templates-001-revisions", include_str!("../migrations/001-revisions.sql")),
            ("base-templates-002-print-revisions", include_str!("../migrations/002-print-revisions.sql")),
        ],
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
            Service { id: print::preview::DOWNLOADS, factory: |services| {
                let preview = Arc::new(print::preview::Previews {
                    templates: services.get(SERVICE)?,
                    artifacts: services.get("base/artifacts.store")?,
                    branding: services.get(bilibi_branding::SERVICE)?,
                    db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                });
                print::preview::bind(preview.clone())?;
                Ok(Arc::new(bilibi_ui::download::DownloadService(preview)))
            }},
            Service { id: print::sweep::SERVICE, factory: |services| {
                let handler = print::sweep::Sweep {
                    schedule: services.get("base/schedule.store")?,
                    artifacts: services.get("base/artifacts.store")?,
                    db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                };
                Ok(Arc::new(bilibi_queue::HandlerService(Arc::new(handler))))
            }},


        ],
    }
}

inventory::submit! { Contribution::new(VOCABULARY, MODULE, provenance!(), &VocabularyContribution {
    domains: &["base"], verbs: &[], roles: &[], system_principals: &[],
    capabilities: &[CapabilityDefinition::tenant(VIEW), CapabilityDefinition::tenant(MANAGE)],
}) }
