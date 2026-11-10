//! A person's tenant-defined custom fields (`base/custom_fields`, ADR 21).
//! Values live in `persons.custom_fields`, keyed by the field's stable key,
//! and are checked here inside Core Person's own writes; the definitions,
//! their page and their agent operations are Base's.
use crate::model::{Custom, CustomFieldError};
use bilibi_database::{Error, Result};
use bilibi_module_registry::RegistryError;
use bilibi_tenancy::{ReadClient, Scope};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use tokio_postgres::Transaction;

/// The record type Core Person offers to `base/custom_fields`.
pub(crate) const RECORD_TYPE: &str = "person";

static FIELDS: OnceLock<Arc<bilibi_custom_fields::CustomFields>> = OnceLock::new();
pub(crate) fn bind(
    fields: Arc<bilibi_custom_fields::CustomFields>,
) -> bilibi_module_registry::Result<()> {
    FIELDS
        .set(fields)
        .map_err(|_| RegistryError::Service("duplicate Person custom-field binding".into()))
}
pub(crate) fn fields() -> Result<&'static bilibi_custom_fields::CustomFields> {
    FIELDS.get().map(AsRef::as_ref).ok_or(Error::Internal)
}

/// How a custom field is named in a form control and an error: `cf_<key>`,
/// so it never collides with a built-in field.
pub(crate) fn name(key: &str) -> String {
    format!("cf_{key}")
}

/// `current` with the checked `stored` patch folded in (a `null` clears).
pub(crate) fn merge(current: &Custom, stored: &Custom) -> Custom {
    let mut next = current.clone();
    for (key, value) in stored {
        if value.is_null() {
            next.remove(key);
        } else {
            next.insert(key.clone(), value.clone());
        }
    }
    next
}

/// Checks a patch against the tenant's active person fields and returns the
/// values to store. On the owner's write transaction (`lock`) a changed
/// pick-list code is held `FOR SHARE`; a preview only reads. A refusal is
/// named `cf_<key>`.
pub(crate) async fn check(
    client: &dyn ReadClient,
    lock: Option<&Transaction<'_>>,
    scope: &Scope,
    patch: &Custom,
    current: &Custom,
) -> Result<std::result::Result<Custom, Vec<CustomFieldError>>> {
    if patch.is_empty() {
        return Ok(Ok(Custom::new()));
    }
    let checked = match lock {
        Some(tx) => {
            fields()?
                .validate_locked(tx, scope, RECORD_TYPE, patch, current)
                .await?
        }
        None => {
            fields()?
                .validate(client, scope, RECORD_TYPE, patch, current)
                .await?
        }
    };
    Ok(checked.map_err(|errors| {
        errors
            .into_iter()
            .map(|e| CustomFieldError {
                field: name(&e.field),
                message: e.message,
            })
            .collect()
    }))
}

/// The active fields and this person's values, for agents and pages:
/// `[{key, label, type, value}]` in the fields' order. An archived field, and
/// its value, is left out.
pub(crate) async fn listed(
    client: &dyn ReadClient,
    scope: &Scope,
    values: &Custom,
) -> Result<Value> {
    let active = fields()?.active(client, scope, RECORD_TYPE).await?;
    Ok(Value::Array(
        active
            .iter()
            .map(|definition| {
                json!({
                    "key": definition.key,
                    "label": definition.label,
                    "type": definition.kind.as_str(),
                    "value": values.get(&definition.key).cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
    ))
}

/// A custom field's stored value as the SQL type its sort and comparisons
/// need, read from `persons.custom_fields` with the key as bound parameter
/// `$index` (never part of the statement text).
pub(crate) fn expression(index: usize, kind: bilibi_custom_fields::Kind) -> String {
    let cast = match kind {
        bilibi_custom_fields::Kind::Number => "::numeric",
        bilibi_custom_fields::Kind::Date => "::date",
        _ => "",
    };
    format!("(custom_fields ->> ${index}::text){cast}")
}

/// The SQL comparison a condition makes on `expression`, with its value as
/// bound parameter `$index`; `None` when the operator does not suit the
/// field or the value is not one the field can hold.
pub(crate) fn condition(
    definition: &bilibi_custom_fields::Definition,
    filter: &crate::model::CustomFilter,
    expression: &str,
    index: usize,
) -> Option<String> {
    use bilibi_custom_fields::Kind;
    let value = filter.value.trim();
    match (definition.kind, filter.op.as_str()) {
        (Kind::Text, "contains") if !value.is_empty() && value.chars().count() <= 256 => Some(
            format!("position(lower(${index}::text) in lower({expression}))>0"),
        ),
        (Kind::Text | Kind::Choice, "eq") if !value.is_empty() && value.chars().count() <= 256 => {
            Some(format!("{expression}=${index}::text"))
        }
        (Kind::Number, "gte" | "lte") if bilibi_custom_fields::valid_number(value) => {
            Some(format!(
                "{expression}{}${index}::text::numeric",
                if filter.op == "gte" { ">=" } else { "<=" }
            ))
        }
        (Kind::Date, "gte" | "lte") if bilibi_custom_fields::valid_date(value) => Some(format!(
            "{expression}{}${index}::text::date",
            if filter.op == "gte" { ">=" } else { "<=" }
        )),
        _ => None,
    }
}

/// A `custom` input of `[{key, value}]` pairs (the agent dialect has no
/// free-form object) as a patch. A value is text, or null to clear.
pub(crate) fn patch(input: &Value) -> Result<Custom> {
    let Some(items) = input.as_array() else {
        return if input.is_null() {
            Ok(Custom::new())
        } else {
            Err(Error::Invalid)
        };
    };
    let mut patch = Custom::new();
    for item in items {
        let key = item["key"].as_str().ok_or(Error::Invalid)?;
        let value = &item["value"];
        if !(value.is_string() || value.is_null()) || item.as_object().is_some_and(|o| o.len() > 2)
        {
            return Err(Error::Invalid);
        }
        if patch.insert(key.to_owned(), value.clone()).is_some() {
            return Err(Error::Invalid);
        }
    }
    Ok(patch)
}

/// The agent input schema for `custom`.
pub(crate) fn schema() -> Value {
    json!({"type":"array","description":"Custom field values as {key, value} pairs; value is text (a number as a decimal string, a date as YYYY-MM-DD, a choice as its code) or null to clear. Find keys with base.custom_fields.list (record type person).","items":{"type":"object","required":["key"],"properties":{"key":{"type":"string","maxLength":40},"value":{"type":"string","maxLength":10000,"nullable":true}}}})
}

// People take tenant-defined custom fields: the values live in
// `persons.custom_fields` (migration `core-person-006-custom-fields`).
inventory::submit! {
    bilibi_module_registry::Contribution::new(
        bilibi_custom_fields::RECORD_TYPES,
        "person",
        bilibi_module_registry::Provenance { module: "core/person", source: file!(), line: line!() },
        &bilibi_custom_fields::RecordType { id: RECORD_TYPE, label: "Contacts" },
    )
}
