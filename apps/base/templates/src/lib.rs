//! Named, versioned prose slots for business owners. See README.md.
mod catalogue;
mod operations;
mod store;
mod web;

pub use catalogue::{
    Audience, Context, DefaultText, Origin, Output, Problem, Resolved, Slot, Template, Text, Value,
    Variable, CATALOGUE,
};
pub use store::Templates;

use bilibi_authz::{CapabilityDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{
    contributions::{
        Access, CompanyScope, Method, PageWrites, Pending, Route, PAGE_WRITES, ROUTES,
    },
    Catalog, Contribution, Module, Service,
};
use std::sync::Arc;

const MODULE: &str = "base/templates";
pub const SERVICE: &str = "base/templates.templates";
const PAGES: &str = "base/templates.pages";
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
        dependencies: &["base/agent_api","base/authz","base/database","base/datetime","base/labels","base/locale","base/menu","base/module_registry","base/party_ids","base/tenancy","base/ui"],
        migrations: &[("base-templates-001-revisions", include_str!("../migrations/001-revisions.sql"))],
        services: &[
            Service { id: SERVICE, factory: |services| {
                let catalogue = Catalog::discover(&bilibi_module_registry::discover()?)?;
                let templates = Arc::new(Templates::new(&catalogue, services.get("base/tenancy.parties")?)?);
                operations::bind(templates.clone())?;
                Ok(templates)
            }},
            Service { id: PAGES, factory: |services| Ok(Arc::new(
                bilibi_ui::page::PageService::<bilibi_tenancy::Scope, bilibi_database::Error>(Arc::new(web::Pages {
                    db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                    templates: services.get(SERVICE)?,
                    contracts: services.get(bilibi_agent_api::page::SERVICE)?,
                }))
            ))},
        ],
    }
}

inventory::submit! { Contribution::new(VOCABULARY, MODULE, provenance!(), &VocabularyContribution {
    domains: &["base"], verbs: &[], roles: &[], system_principals: &[],
    capabilities: &[CapabilityDefinition::tenant(VIEW), CapabilityDefinition::tenant(MANAGE)],
}) }

inventory::submit! { Contribution::new(ROUTES, "base/templates.index", provenance!(), &Route {
    method: Method::Get, path: web::INDEX, service: PAGES, operation: "index",
    access: Access::Capability(VIEW), company_scope: CompanyScope::Tenant, framed: true,
}) }
inventory::submit! { Contribution::new(ROUTES, "base/templates.detail", provenance!(), &Route {
    method: Method::Get, path: "/templates/{key}", service: PAGES, operation: "detail",
    access: Access::Capability(VIEW), company_scope: CompanyScope::Tenant, framed: true,
}) }
inventory::submit! { Contribution::new(PAGE_WRITES, PAGES, provenance!(), &PageWrites {
    service: PAGES,
    contracts: &[("publish_template", operations::PUBLISH), ("restore_template", operations::RESTORE)],
    pending: Pending::None,
}) }

bilibi_menu::contribute!(
    "base/templates",
    bilibi_menu::MenuItem::page("admin.templates", "Templates", web::INDEX)
        .under("admin")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(65)
);
