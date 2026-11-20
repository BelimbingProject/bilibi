//! Tenant-defined custom fields: stable typed values
//! an administrator adds to the records of a module that opts in, without a
//! release. This module owns the tenant's definitions and the page and agent
//! operations that manage them; the record's owner keeps the values in its own
//! row and checks them against these definitions in its own write.
//! Contract: `README.md`; decision: ADR 21.
mod declaration;
mod definitions;
mod grid;
mod operations;
mod present;
mod values;
mod web;

pub use declaration::{RecordType, LABEL_MAX, RECORD_TYPES, RECORD_TYPE_MAX};
pub use definitions::{
    Checked, CustomFields, Definition, FieldError, FieldUpdate, Kind, NewField, HELP_MAX, KEY_MAX,
};
pub use grid::{GridSource, GRID_SOURCES};
pub use present::{codes_value, picked};
pub use values::{codes_of, valid_date, valid_number};
#[doc(hidden)]
pub use {
    bilibi_module_registry::{Contribution, Provenance},
    inventory,
};

use bilibi_authz::{CapabilityDefinition, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{
    contributions::{Access, Method, PageWrites, Pending, Route, PAGE_WRITES, ROUTES},
    Catalog, Module, Service,
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

pub const SERVICE: &str = "base/custom_fields.fields";
/// The administration page.
pub(crate) const PAGES: &str = "base/custom_fields.pages";
/// Read the definitions on the page and through agents.
pub const VIEW: &str = "base.custom-fields.view";
/// Define, rename, archive and restore fields.
pub const MANAGE: &str = "base.custom-fields.manage";
/// The most active fields one record type may have, per organisation.
pub const MAX_FIELDS: &str = "base.custom_fields.max_fields";
pub const MAX_FIELDS_DEFAULT: i64 = 50;
pub const MAX_FIELDS_MIN: i64 = 1;
pub const MAX_FIELDS_MAX: i64 = 200;
/// The longest text value a text field may hold, per organisation.
pub const MAX_TEXT_LENGTH: &str = "base.custom_fields.max_text_length";
pub const MAX_TEXT_LENGTH_DEFAULT: i64 = 1_000;
pub const MAX_TEXT_LENGTH_MIN: i64 = 1;
/// Keeps every valid value inside a live page event.
pub const MAX_TEXT_LENGTH_MAX: i64 = 10_000;

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/custom_fields",
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! { Contribution::new(VOCABULARY, "base/custom_fields", provenance!(), &VocabularyContribution {
    domains: &["base"],
    verbs: &[],
    capabilities: &[CapabilityDefinition::tenant(VIEW), CapabilityDefinition::tenant(MANAGE)],
    roles: &[RoleDefinition { code: "tenant_owner", name: "Tenant Owner", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: &[VIEW, MANAGE] }],
    system_principals: &[],
}) }

inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, MAX_FIELDS, provenance!(),
&bilibi_settings::Setting::Definition(bilibi_settings::Definition {
    value_type: bilibi_settings::ValueType::Integer,
    scopes: &[bilibi_settings::ScopeKind::Tenant],
    default: || json!(MAX_FIELDS_DEFAULT),
    nullable: false,
    encrypted: false,
    minimum: Some(MAX_FIELDS_MIN as f64),
    maximum: Some(MAX_FIELDS_MAX as f64),
    validator: None,
    editor: Some(bilibi_settings::Editor {
        group: "Custom fields",
        label: "Custom fields per record type",
        help: "The most active custom fields one kind of record, such as a company, may have. Archived fields do not count.",
        capability: Some(MANAGE),
        reveal_duration_ms: 5_000,
    }),
})) }

inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, MAX_TEXT_LENGTH, provenance!(),
&bilibi_settings::Setting::Definition(bilibi_settings::Definition {
    value_type: bilibi_settings::ValueType::Integer,
    scopes: &[bilibi_settings::ScopeKind::Tenant],
    default: || json!(MAX_TEXT_LENGTH_DEFAULT),
    nullable: false,
    encrypted: false,
    minimum: Some(MAX_TEXT_LENGTH_MIN as f64),
    maximum: Some(MAX_TEXT_LENGTH_MAX as f64),
    validator: None,
    editor: Some(bilibi_settings::Editor {
        group: "Custom fields",
        label: "Custom text field length",
        help: "The most characters a custom text field may hold. A lower limit never changes values already saved.",
        capability: Some(MANAGE),
        reveal_duration_ms: 5_000,
    }),
})) }

fn build(
    services: &bilibi_module_registry::Services,
) -> bilibi_module_registry::Result<CustomFields> {
    // The linked modules' declarations; an unmounted module declares none.
    let catalog = &Catalog::discover(&bilibi_module_registry::discover()?)?;
    let mut record_types = BTreeMap::new();
    for entry in catalog.entries::<RecordType>(RECORD_TYPES)? {
        let record = entry.get::<RecordType>()?;
        record_types.insert(record.id, record);
    }
    Ok(CustomFields {
        record_types,
        sources: grid::sources(catalog)?,
        picklists: services.get(bilibi_picklists::SERVICE)?,
        records: services.get(bilibi_records::SERVICE)?,
        settings: services.get("base/settings.store")?,
    })
}

inventory::submit! {
    Module {
        id: "base/custom_fields", layer: bilibi_module_registry::Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/grid", "base/labels", "base/tenancy", "base/authz", "base/settings", "base/picklists", "base/records", "base/ui", "base/audit", "base/agent_api", "base/menu"],
        migrations: &[("base-custom_fields-001-definitions", include_str!("../migrations/001-definitions.sql")), ("base-custom_fields-002-more-types", include_str!("../migrations/002-more-types.sql")), ("base-custom_fields-003-structured-values", include_str!("../migrations/003-structured-values.sql")), ("base-custom_fields-004-rule-conditions", include_str!("../migrations/004-rule-conditions.sql")), ("base-custom_fields-005-watcher-conditions", include_str!("../migrations/005-watcher-conditions.sql"))],
        services: &[
            Service { id: SERVICE, factory: |services| {
                let fields = Arc::new(build(services)?);
                // Grid reads the tenant's active fields into each fresh catalog,
                // and Authz offers them to Field Restrictions.
                bilibi_grid::bind_custom_columns(Box::leak(Box::new(grid::Columns { fields: fields.clone() })))?;
                bilibi_authz::field_catalog::bind_tenant_fields(Box::leak(Box::new(grid::Restrictable { fields: fields.clone() })))?;
                operations::bind(services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(), fields.clone())?;
                Ok(fields)
            } },
            Service { id: PAGES, factory: |services| Ok(Arc::new(bilibi_ui::page::PageService::<bilibi_tenancy::Scope, bilibi_database::Error>(Arc::new(web::Pages {
                db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                fields: services.get(SERVICE)?,
                contracts: services.get(bilibi_agent_api::page::SERVICE)?,
            })))) },
        ],
    }
}

inventory::submit! { Contribution::new(ROUTES, "base/custom_fields.index", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: web::INDEX, service: PAGES, operation: "index", access: Access::Capability(VIEW), framed: true,
}) }
// Every write goes through the contracts agents call.
inventory::submit! { Contribution::new(PAGE_WRITES, PAGES, provenance!(), &PageWrites {
    service: PAGES,
    contracts: &[
        ("create", "base.custom_fields.define"),
        ("save_field", "base.custom_fields.update"),
        ("switch_watcher_condition", "base.custom_fields.update"),
        ("archive", "base.custom_fields.archive"),
        ("restore", "base.custom_fields.restore"),
    ],
    pending: Pending::None,
}) }
bilibi_menu::contribute!(
    "base/custom_fields",
    bilibi_menu::MenuItem::page("admin.custom_fields", "Custom fields", web::INDEX)
        .icon("puzzle-piece")
        .under("admin")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(62)
);

fn customisation_input() -> serde_json::Value {
    serde_json::json!({"include_archived": true})
}
fn customisation_rows_for(
    value: &serde_json::Value,
    archived: bool,
) -> Vec<bilibi_labels::CustomisationRow> {
    value.as_array().into_iter().flatten().filter(|row| row["archived_on"].is_null() != archived).filter_map(|row| {
        Some(bilibi_labels::CustomisationRow {
            description: format!("{}: {} ({})", row["record_type"].as_str()?, row["label"].as_str()?, row["key"].as_str()?),
            audit_kind: "custom_field",
            audit_id: row["id"].as_i64()?,
            revision: row["version"].as_i64()?,
            input: serde_json::json!({"record_type": row["record_type"], "key": row["key"], "version": row["version"]}),
        })
    }).collect()
}
fn customisation_rows(value: &serde_json::Value) -> Vec<bilibi_labels::CustomisationRow> {
    customisation_rows_for(value, false)
}
fn archived_customisation_rows(value: &serde_json::Value) -> Vec<bilibi_labels::CustomisationRow> {
    customisation_rows_for(value, true)
}
fn no_input() -> serde_json::Value {
    serde_json::json!({})
}
/// What may be changed: every kind of record that takes custom fields, with
/// or without one, then each field the tenant already has.
fn customisation_choices(reads: &[serde_json::Value]) -> Vec<bilibi_labels::Choice> {
    use serde_json::json;
    let fields = || reads[1].as_array().into_iter().flatten();
    let types = reads[0]["record_types"].as_array().into_iter().flatten();
    let mut choices = Vec::new();
    for record in types {
        let (Some(id), Some(label)) = (record["id"].as_str(), record["label"].as_str()) else {
            continue;
        };
        let own: Vec<_> = fields()
            .filter(|field| field["record_type"] == record["id"])
            .collect();
        let active: Vec<_> = own
            .iter()
            .filter(|field| field["archived_on"].is_null())
            .map(|field| field["key"].clone())
            .collect();
        let page = record["path"].as_str().map(str::to_owned);
        let record_kinds: Vec<_> = record["record_kinds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|kind| kind.as_str().map(str::to_owned))
            .collect();
        let limit = reads[0]["max_fields"].as_u64().unwrap_or_default();
        let active_count = active.len() as u64;
        let has_room = active_count < limit;
        let capacity = json!({"max_fields": limit, "active_fields": active_count});
        let verify_input = json!({"record_type": id, "include_archived": true});
        choices.push(bilibi_labels::Choice {
            subject: id.to_owned(),
            keywords: "add create define custom field attribute extra column",
            title: format!("{label}: add a field"),
            page: page.clone(),
            record_kinds: record_kinds.clone(),
            value: Some(json!(active)),
            default: Some(json!([])),
            source: Some(
                if active.is_empty() {
                    "default"
                } else {
                    "tenant"
                }
                .into(),
            ),
            customised: !active.is_empty(),
            input: json!({"record_type": id}),
            verify_input: verify_input.clone(),
            limits: json!({
                "max_fields": limit, "active_fields": active_count,
                "pick_lists": reads[0]["pick_lists"], "reference_kinds": reads[0]["record_kinds"],
            }),
            writes: Some(if has_room {
                vec!["base.custom_fields.define"]
            } else {
                Vec::new()
            }),
            ..Default::default()
        });
        choices.extend(own.into_iter().filter_map(|field| {
            let key = field["key"].as_str()?;
            let archived = !field["archived_on"].is_null();
            let mut writes = vec!["base.custom_fields.update"];
            if !archived {
                writes.push("base.custom_fields.archive");
            } else if has_room {
                writes.push("base.custom_fields.restore");
            }
            Some(bilibi_labels::Choice {
                subject: format!("{id}.{key}"),
                keywords: if archived {
                    "custom field attribute column rename edit update restore"
                } else {
                    "custom field attribute column rename edit update archive"
                },
                title: format!(
                    "{label}: {} ({key}){}",
                    field["label"].as_str()?,
                    if archived { ", archived" } else { "" }
                ),
                page: page.clone(),
                record_kinds: record_kinds.clone(),
                value: Some(field.clone()),
                source: Some("tenant".into()),
                customised: true,
                version: Some(field["version"].clone()),
                input: json!({"record_type": id, "key": key, "version": field["version"]}),
                verify_input: verify_input.clone(),
                limits: capacity.clone(),
                writes: Some(writes),
                ..Default::default()
            })
        }));
    }
    choices
}
inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/custom_fields.customisations", provenance!(), &bilibi_labels::CustomisationSection {
    title: "Custom fields",
    undo: Some(bilibi_labels::Undo { read: "base.custom_fields.list", read_input: customisation_input, operation: "base.custom_fields.archive", event: "customisation.fields", action: "Archive", detail: "The field is hidden and records keep their saved values. Approved rules that use this field may stop running and require review and reapproval.", rows: customisation_rows }),
    changes: Some(bilibi_labels::Changes {
        purpose: "Add your own typed fields to a kind of record, rename them, and archive or bring back the ones you no longer use.",
        limits: "A field's key and type never change and a key is never reused. Each kind of record takes a limited number of active fields (the base.custom_fields.max_fields setting). Archive a field to remove it; records keep their saved values.",
        guide: "base.custom_fields",
        reads: &[bilibi_labels::Read { operation: "base.custom_fields.record_types", input: no_input }, bilibi_labels::Read { operation: "base.custom_fields.list", input: customisation_input }],
        choices: customisation_choices,
        change: &["base.custom_fields.define", "base.custom_fields.update", "base.custom_fields.restore"],
        preview: None, verify: "base.custom_fields.list", restore: Some("base.custom_fields.archive"),
    }),
}) }

inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.custom_fields.archive", provenance!(), &bilibi_agent_api::page::PageBinding { service: "base/labels.customisations", event: "customisation.fields", contract: "base.custom_fields.archive" }) }

inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/custom_fields.archived_customisations", provenance!(), &bilibi_labels::CustomisationSection {
    title: "Archived custom fields",
    undo: Some(bilibi_labels::Undo { read: "base.custom_fields.list", read_input: customisation_input, operation: "base.custom_fields.restore", event: "customisation.fields_restore", action: "Restore", detail: "The field and its saved values become available again. Watchers already marked for review still need reapproval.", rows: archived_customisation_rows }),
    changes: None,
}) }
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.custom_fields.restore", provenance!(), &bilibi_agent_api::page::PageBinding { service: "base/labels.customisations", event: "customisation.fields_restore", contract: "base.custom_fields.restore" }) }

inventory::submit! { Contribution::new(bilibi_labels::RECEIPT_SOURCES, "base/custom_fields.receipts", provenance!(), &bilibi_labels::ReceiptSource { kind: "custom_field", capability: VIEW }) }
