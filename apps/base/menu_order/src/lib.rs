//! Tenant ordering of staff menu siblings. See README.md for the contract.
mod operations;
mod store;

use bilibi_authz::{CapabilityDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{Contribution, Module, Provenance, Service};
use std::sync::Arc;
pub use store::{MenuOrder, Orders};

pub const SERVICE: &str = "base/menu_order.order";
pub const VIEW: &str = "base.menu-order.view";
pub const MANAGE: &str = "base.menu-order.manage";

inventory::submit! { Contribution::new(VOCABULARY, "base/menu_order", Provenance { module: "base/menu_order", source: file!(), line: line!() }, &VocabularyContribution {
    domains: &["base"], verbs: &[], capabilities: &[CapabilityDefinition::tenant(VIEW), CapabilityDefinition::tenant(MANAGE)], roles: &[], system_principals: &[],
}) }
inventory::submit! { Module {
    id: "base/menu_order", layer: bilibi_module_registry::Layer::Base,
    dependencies: &["base/module_registry", "base/database", "base/tenancy", "base/authz", "base/agent_api", "base/menu", "base/labels"],
    migrations: &[("base-menu-order-001-order", include_str!("../migrations/001-order.sql"))],
    services: &[Service { id: SERVICE, factory: |services| {
        let catalog = bilibi_module_registry::Catalog::discover(&bilibi_module_registry::discover()?)?;
        let order = Arc::new(MenuOrder::new(bilibi_menu::Menu::from_catalog(&catalog)?, services.get(bilibi_labels::SERVICE)?));
        operations::bind(order.clone())?;
        Ok(order)
    }}],
} }
