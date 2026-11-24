//! Tenant label overrides: plain text a tenant shows instead of a module's
//! own word for a menu item, a record type or a field, kept against the stable ID.
//! Data only (ADR 28): it never renames an operation key, route or
//! permission key. Contract: `README.md`.
mod customisations;
mod customisations_web;
mod discovery;
mod operations;
mod receipt_display;
mod store;
mod web;

pub use customisations::{
    Changes, Choice, CustomisationRow, CustomisationSection, PreviewLink, Read, ReceiptCorrection,
    ReceiptFuture, ReceiptSource, ReceiptUndo, Undo, UndoAction, PREVIEW_LINKS, RECEIPT_SOURCES,
    SECTIONS,
};

pub use store::{
    field_labels, subject_id, Edit, FieldLabels, Kind, LabelSet, Labels, Plan, Stored, Subject,
    SubjectSource, MAX_LABEL, SUBJECTS,
};

use bilibi_authz::{CapabilityDefinition, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{
    contributions::{Access, Method, PageWrites, Pending, Route, PAGE_WRITES, ROUTES},
    Contribution, Module, Provenance, Service,
};
use std::sync::Arc;

pub(crate) const CUSTOMISATIONS: &str = "base/labels.customisations";
pub const CUSTOMISATIONS_VIEW: &str = "base.labels.customisations.view";
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
    capabilities: &[CapabilityDefinition::tenant(VIEW), CapabilityDefinition::tenant(MANAGE), CapabilityDefinition::tenant(CUSTOMISATIONS_VIEW)],
    roles: &[RoleDefinition { code: "tenant_owner", name: "Tenant Owner", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: &[VIEW, MANAGE, CUSTOMISATIONS_VIEW] }],
    system_principals: &[],
}) }

inventory::submit! {
    Module {
        id: "base/labels", layer: bilibi_module_registry::Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/datetime", "base/locale", "base/principal_directory", "base/tenancy", "base/authz", "base/ui", "base/audit", "base/agent_api", "base/menu", "base/settings", "base/system"],
        migrations: &[("base-labels-001-labels", include_str!("../migrations/001-labels.sql")), ("base-labels-002-field-labels", include_str!("../migrations/002-field-labels.sql"))],
        services: &[
            Service { id: SERVICE, factory: |services| {
                let catalog = bilibi_module_registry::Catalog::discover(&bilibi_module_registry::discover()?)?;
                let sources = catalog.entries::<SubjectSource>(SUBJECTS)?.iter().map(|entry| entry.get::<SubjectSource>()).collect::<Result<Vec<_>, _>>()?;
                let labels = Arc::new(Labels::new(
                    services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                    bilibi_menu::Menu::from_catalog(&catalog)?,
                    sources,
                ));
                operations::bind(labels.clone(), services.get("base/principal_directory.directory")?)?;
                Ok(labels)
            } },
            Service { id: CUSTOMISATIONS, factory: |services| {
                let catalog = bilibi_module_registry::Catalog::discover(&bilibi_module_registry::discover()?)?;
                let entries = catalog.entries::<CustomisationSection>(SECTIONS)?;
                let sections = entries.iter().map(|entry| entry.get::<CustomisationSection>()).collect::<Result<Vec<_>, _>>()?;
                let previews = catalog.entries::<PreviewLink>(PREVIEW_LINKS)?.iter().map(|entry| {
                    let link = entry.get::<PreviewLink>()?;
                    if !sections.iter().any(|section| section.undo.as_ref().is_some_and(|undo| undo.read == link.read)) {
                        return Err(bilibi_module_registry::RegistryError::Contribution(
                            format!("Customisation preview {} needs an existing override read", entry.id),
                        ));
                    }
                    Ok((link.read, link))
                }).collect::<Result<std::collections::HashMap<_, _>, _>>()?;
                discovery::bind(services.get("base/agent_api.registry")?, entries.iter().map(|entry| entry.id).zip(sections.iter().copied()).collect())?;
                Ok(Arc::new(bilibi_ui::page::PageService::<bilibi_tenancy::Scope, bilibi_database::Error>(Arc::new(customisations_web::Pages {
                    db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                    settings: services.get("base/settings.store")?,
                    display: services.get("base/datetime.display")?, locales: services.get("base/locale.locales")?, directory: services.get("base/principal_directory.directory")?,
                    contracts: services.get(bilibi_agent_api::page::SERVICE)?, sections, previews,
                    receipt_sources: catalog.entries::<ReceiptSource>(RECEIPT_SOURCES)?.iter().map(|entry| entry.get::<ReceiptSource>()).collect::<Result<Vec<_>, _>>()?,
                }))))
            } },
            Service { id: PAGES, factory: |services| Ok(Arc::new(bilibi_ui::page::PageService::<bilibi_tenancy::Scope, bilibi_database::Error>(Arc::new(web::Pages {
                labels: services.get(SERVICE)?,
                contracts: services.get(bilibi_agent_api::page::SERVICE)?,
            })))) },
        ],
    }
}

inventory::submit! { bilibi_module_registry::Consumer::new::<PreviewLink>(PREVIEW_LINKS, "base/labels", |entries| {
    let mut reads = std::collections::BTreeSet::new();
    for entry in entries {
        let preview = entry.get::<PreviewLink>()?;
        if preview.label.trim().is_empty() || !reads.insert(preview.read) {
            return Err(bilibi_module_registry::RegistryError::Contribution(
                format!("Invalid or duplicate customisation preview {}", entry.id),
            ));
        }
    }
    Ok(())
}) }

inventory::submit! { bilibi_module_registry::Consumer::new::<SubjectSource>(SUBJECTS, "base/labels", |_| Ok(())) }

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

inventory::submit! { bilibi_module_registry::Consumer::new::<CustomisationSection>(SECTIONS, "base/labels", |entries| {
    for entry in entries {
        let section = entry.get::<CustomisationSection>()?;
        let undo_valid = section.undo.as_ref().is_none_or(|undo| !undo.read.is_empty() && undo.action.as_ref().is_none_or(|action| !action.operation.is_empty() && action.event.starts_with("customisation.")));
        if section.title.is_empty() || !undo_valid || (section.undo.is_none() && section.changes.is_none()) {
            return Err(bilibi_module_registry::RegistryError::Contribution(format!("invalid customisation section {}", entry.id)));
        }
    }
    Ok(())
}) }

inventory::submit! { Contribution::new(ROUTES, "base/labels.customisations", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: "/admin/customisations", service: CUSTOMISATIONS, operation: "index", access: Access::Capability(CUSTOMISATIONS_VIEW), framed: true,
}) }
inventory::submit! { Contribution::new(PAGE_WRITES, CUSTOMISATIONS, provenance!(), &PageWrites { service: CUSTOMISATIONS, contracts: &[], pending: Pending::None }) }
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_FAMILIES, CUSTOMISATIONS, provenance!(), &bilibi_agent_api::page::PageFamily { service: CUSTOMISATIONS, event_prefix: "customisation." }) }
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.labels.clear", provenance!(), &bilibi_agent_api::page::PageBinding { service: CUSTOMISATIONS, event: "customisation.labels", contract: "base.labels.clear" }) }
bilibi_menu::contribute!(
    "base/labels",
    bilibi_menu::MenuItem::page(
        "admin.customisations",
        "Customisations",
        "/admin/customisations"
    )
    .icon("settings")
    .under("admin")
    .requires(bilibi_menu::Requirement::Capability(CUSTOMISATIONS_VIEW))
    .order(64)
);

inventory::submit! { bilibi_module_registry::Consumer::new::<ReceiptSource>(RECEIPT_SOURCES, "base/labels", |_| Ok(())) }

inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.labels.receipt_set", provenance!(), &bilibi_agent_api::page::PageBinding { service: CUSTOMISATIONS, event: "customisation.receipt_label_set", contract: "base.labels.set" }) }
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.labels.receipt_clear", provenance!(), &bilibi_agent_api::page::PageBinding { service: CUSTOMISATIONS, event: "customisation.receipt_label_clear", contract: "base.labels.clear" }) }
