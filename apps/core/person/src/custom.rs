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
/// `[{key, label, type, value}]` in the fields' order, with `target` (the
/// Base Records kind) on a record field. `value` is the stored JSON: text, an
/// array of codes for several choices, a record's id for a record field; the
/// record is never named here (agents name it with `base.records.resolve`).
/// An archived field, and its value, is left out.
pub(crate) fn listed(active: &[bilibi_custom_fields::Definition], values: &Custom) -> Value {
    Value::Array(
        active
            .iter()
            .map(|definition| {
                let mut item = json!({
                    "key": definition.key,
                    "label": definition.label,
                    "type": definition.kind.as_str(),
                    "value": values.get(&definition.key).cloned().unwrap_or(Value::Null),
                });
                if definition.kind == bilibi_custom_fields::Kind::Reference {
                    item["target"] = json!(definition.target);
                }
                item
            })
            .collect(),
    )
}
/// The tenant's active person fields, in their order: read once per answer
/// and passed to `listed` for each person (#603).
pub(crate) async fn active(
    client: &dyn ReadClient,
    scope: &Scope,
) -> Result<Vec<bilibi_custom_fields::Definition>> {
    fields()?.active(client, scope, RECORD_TYPE).await
}

/// Whether the list and `core.person.list` sort by the field. Several
/// choices and a record have no order of their own, so neither sorts.
pub(crate) fn sortable(kind: bilibi_custom_fields::Kind) -> bool {
    use bilibi_custom_fields::Kind;
    match kind {
        Kind::Text
        | Kind::Number
        | Kind::Date
        | Kind::Choice
        | Kind::Boolean
        | Kind::LongText
        | Kind::Link
        | Kind::Phone
        | Kind::Email => true,
        Kind::Choices | Kind::Reference => false,
    }
}

/// A custom field's stored value as the SQL type its sort and comparisons
/// need, read from `persons.custom_fields` with the key as bound parameter
/// `$index` (never part of the statement text). Only for a `sortable` kind.
pub(crate) fn expression(index: usize, kind: bilibi_custom_fields::Kind) -> String {
    use bilibi_custom_fields::Kind;
    let cast = match kind {
        Kind::Number => "::numeric",
        Kind::Date => "::date",
        Kind::Text
        | Kind::Choice
        | Kind::Boolean
        | Kind::LongText
        | Kind::Link
        | Kind::Phone
        | Kind::Email
        | Kind::Choices
        | Kind::Reference => "",
    };
    format!("(custom_fields ->> ${index}::text){cast}")
}

/// The SQL condition a filter makes on `persons.custom_fields`, with the
/// field's key as bound parameter `$key` and the filter's value as `$value`;
/// `None` when the operator does not suit the field or the value is not one
/// the field can hold. Several choices hold a code (`has`) by containment,
/// which the `gin (custom_fields jsonb_path_ops)` index serves; a record
/// field takes no condition.
pub(crate) fn condition(
    definition: &bilibi_custom_fields::Definition,
    filter: &crate::model::CustomFilter,
    key: usize,
    value: usize,
) -> Option<String> {
    use bilibi_custom_fields::Kind;
    let text = filter.value.trim();
    let bounded = !text.is_empty() && text.chars().count() <= 256;
    let expression = expression(key, definition.kind);
    match (definition.kind, filter.op.as_str()) {
        (Kind::Text | Kind::LongText | Kind::Link | Kind::Phone | Kind::Email, "contains")
            if bounded =>
        {
            Some(format!(
                "position(lower(${value}::text) in lower({expression}))>0"
            ))
        }
        (Kind::Boolean, "eq") if matches!(text, "true" | "false") => {
            Some(format!("{expression}=${value}::text"))
        }
        (
            Kind::Text | Kind::Choice | Kind::LongText | Kind::Link | Kind::Phone | Kind::Email,
            "eq",
        ) if bounded => Some(format!("{expression}=${value}::text")),
        (Kind::Choices, "has") if bounded => Some(format!(
            "custom_fields @> jsonb_build_object(${key}::text, jsonb_build_array(${value}::text))"
        )),
        (Kind::Number, "gte" | "lte") if bilibi_custom_fields::valid_number(text) => Some(format!(
            "{expression}{}${value}::text::numeric",
            if filter.op == "gte" { ">=" } else { "<=" }
        )),
        (Kind::Date, "gte" | "lte") if bilibi_custom_fields::valid_date(text) => Some(format!(
            "{expression}{}${value}::text::date",
            if filter.op == "gte" { ">=" } else { "<=" }
        )),
        _ => None,
    }
}

/// A `custom` input of `[{key, value}]` or `[{key, values}]` pairs (the
/// agent dialect has no free-form object and one type per property) as a
/// patch. `value` is text, or null to clear; `values`, for several choices,
/// is a list of codes, an empty one clearing. A pair carries exactly one of
/// them; Custom Fields refuses the shape that does not suit the field.
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
        let object = item.as_object().ok_or(Error::Invalid)?;
        let key = object
            .get("key")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid)?;
        let value = match (object.get("value"), object.get("values")) {
            (Some(value), None) if value.is_string() || value.is_null() => value.clone(),
            (None, Some(Value::Array(codes))) if codes.iter().all(Value::is_string) => {
                Value::Array(codes.clone())
            }
            _ => return Err(Error::Invalid),
        };
        if object.len() > 2 || patch.insert(key.to_owned(), value).is_some() {
            return Err(Error::Invalid);
        }
    }
    Ok(patch)
}

/// The agent input schema for `custom`.
pub(crate) fn schema() -> Value {
    json!({"type":"array","description":"Custom field values as {key, value} or {key, values} pairs, exactly one of value and values. value is text (a number as a decimal string, a date as YYYY-MM-DD, a choice as its code, yes/no as \"true\" or \"false\", a record field as the record's id from base.records.search) or null to clear; values is for a several-choices field only: its pick-list codes, an empty list clearing it. Find keys, types and a record field's target kind with base.custom_fields.list (record type person).","items":{"type":"object","required":["key"],"properties":{"key":{"type":"string","maxLength":40},"value":{"type":"string","maxLength":10000,"nullable":true},"values":{"type":"array","items":{"type":"string","maxLength":64},"description":"For a several-choices field, the pick-list codes; an empty list clears it."}}}})
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
