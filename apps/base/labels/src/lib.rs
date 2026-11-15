//! Tenant label overrides: plain text a tenant shows instead of a module's
//! own word for a menu item or a record type, kept against the stable ID.
//! Data only (ADR 28): it never renames an operation key, route or
//! permission key. Contract: `README.md`.
mod operations;
mod store;
mod web;

pub use store::{Edit, Kind, LabelSet, Labels, Plan, Stored, Subject, MAX_LABEL};

use bilibi_authz::{CapabilityDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{
    contributions::{Access, Method, PageWrites, Pending, Route, PAGE_WRITES, ROUTES},
    Contribution, Module, Provenance, Service,
};
use std::sync::Arc;

pub const SERVICE: &str = "base/labels.labels";
/// The operator page.
pub(crate) const PAGES: &str = "base/labels.pages";
/// See the labels page and read overrides through agents.
pub const VIEW: &str = "base.labels.view";
/// Rename or restore a tenant's labels.
pub const MANAGE: &str = "base.labels.manage";

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/labels",
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! { Contribution::new(VOCABULARY, "base/labels", provenance!(), &VocabularyContribution {
    domains: &["base"],
    verbs: &[],
    capabilities: &[CapabilityDefinition::tenant(VIEW), CapabilityDefinition::tenant(MANAGE)],
    roles: &[],
    system_principals: &[],
}) }

inventory::submit! {
    Module {
        id: "base/labels", layer: bilibi_module_registry::Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/tenancy", "base/authz", "base/ui", "base/audit", "base/agent_api", "base/menu", "base/grid"],
        migrations: &[("base-labels-001-labels", include_str!("../migrations/001-labels.sql"))],
        services: &[
            Service { id: SERVICE, factory: |services| {
                let labels = Arc::new(Labels::new(
                    services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                    bilibi_menu::Menu::from_catalog(&bilibi_module_registry::Catalog::discover(&bilibi_module_registry::discover()?)?)?,
                ));
                operations::bind(labels.clone())?;
                Ok(labels)
            } },
            Service { id: PAGES, factory: |services| Ok(Arc::new(bilibi_ui::page::PageService::<bilibi_tenancy::Scope, bilibi_database::Error>(Arc::new(web::Pages {
                labels: services.get(SERVICE)?,
                contracts: services.get(bilibi_agent_api::page::SERVICE)?,
            })))) },
        ],
    }
}

inventory::submit! { Contribution::new(ROUTES, "base/labels.index", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: web::INDEX, service: PAGES, operation: "index", access: Access::Capability(VIEW), framed: true,
}) }
// Every write goes through the contracts agents call.
inventory::submit! { Contribution::new(PAGE_WRITES, PAGES, provenance!(), &PageWrites {
    service: PAGES,
    contracts: &[("save_field", "base.labels.set"), ("save_field", "base.labels.clear"), ("restore", "base.labels.clear")],
    pending: Pending::None,
}) }
bilibi_menu::contribute!(
    "base/labels",
    bilibi_menu::MenuItem::page("admin.labels", "Labels", web::INDEX)
        .icon("tag")
        .under("admin")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(65)
);
