//! Tenant-local CSV fields, adapted to the owner's existing custom patch.
use crate::record::{Field, Kind, Registered};
use bilibi_module_registry::{Catalog, Consumer, Entry, RegistryError};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub const CUSTOM_IMPORTS: &str = "base/bulk_data.custom_imports";

/// Opt an import type into the tenant's custom fields. Contribute under the
/// same id as its `RecordType`, from the same owner. Both operations take a
/// top-level array of `{key, value, values}` pairs (`values` for several
/// choices); updates receive only mapped keys.
#[derive(Debug)]
pub struct CustomImport {
    pub record_type: &'static str,
    pub create_field: &'static str,
    pub update_field: &'static str,
}

fn invalid(entry: &Entry) -> RegistryError {
    RegistryError::Contribution(format!(
        "custom import {} at {}:{} must name its owner's import and custom record type, and array inputs of key/value pairs on its create and update contracts",
        entry.id, entry.provenance.source, entry.provenance.line
    ))
}

inventory::submit! { Consumer::new::<CustomImport>(CUSTOM_IMPORTS, "base/bulk_data", |_| Ok(())) }

pub(crate) fn register(
    catalog: &Catalog,
    jobs: &bilibi_agent_api::job::JobContracts,
    types: &mut BTreeMap<&'static str, Registered>,
) -> bilibi_module_registry::Result<()> {
    let owners = catalog.entries::<crate::RecordType>(crate::RECORD_TYPES)?;
    let custom_types =
        catalog.entries::<bilibi_custom_fields::RecordType>(bilibi_custom_fields::RECORD_TYPES)?;
    for entry in catalog.entries::<CustomImport>(CUSTOM_IMPORTS)? {
        let binding = entry.get::<CustomImport>()?;
        let Some(registered) = types.get_mut(entry.id) else {
            return Err(invalid(entry));
        };
        if !owners
            .iter()
            .any(|owner| owner.id == entry.id && owner.provenance.module == entry.provenance.module)
            || !custom_types.iter().any(|owner| {
                owner.id == binding.record_type
                    && owner.provenance.module == entry.provenance.module
            })
            || registered
                .fields
                .iter()
                .any(|field| field.name.starts_with("cf_"))
        {
            return Err(invalid(entry));
        }
        let accepts = |operation: &str, field: &str| {
            jobs.writable(operation).ok().is_some_and(|operation| {
                let schema = &operation.input["properties"][field];
                schema["type"] == "array"
                    && schema["items"]["type"] == "object"
                    && schema["items"]["properties"]["key"]["type"] == "string"
                    && schema["items"]["properties"]["value"]["type"] == "string"
                    && schema["items"]["properties"]["values"]["type"] == "array"
                    && schema["items"]["properties"]["values"]["items"]["type"] == "string"
            })
        };
        let Some(import) = &registered.definition.import else {
            return Err(invalid(entry));
        };
        if !accepts(import.create, binding.create_field)
            || import
                .duplicates
                .as_ref()
                .is_some_and(|duplicates| !accepts(duplicates.update, binding.update_field))
        {
            return Err(invalid(entry));
        }
        registered.custom = Some(binding);
    }
    Ok(())
}

pub(crate) fn field(definition: bilibi_custom_fields::Definition) -> Field {
    Field {
        name: format!("cf_{}", definition.key),
        label: definition.label,
        kind: Kind::Custom(definition.kind),
        required: false,
        choices: Vec::new(),
        reference: None,
    }
}

/// Moves CSV's flat custom keys into the owner's canonical patch input.
pub(crate) fn take(input: &mut Map<String, Value>) -> Vec<Value> {
    let names: Vec<_> = input
        .keys()
        .filter(|name| name.starts_with("cf_"))
        .cloned()
        .collect();
    // Several choices travel as `values`, every other kind as `value`: the
    // agent dialect gives each property one type.
    names
        .into_iter()
        .map(|name| match input.remove(&name).expect("collected key") {
            Value::Array(values) => json!({"key": &name[3..], "values": values}),
            value => json!({"key": &name[3..], "value": value}),
        })
        .collect()
}
