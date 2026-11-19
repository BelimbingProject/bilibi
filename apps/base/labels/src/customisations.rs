//! Registered sections of the tenant customisations page.
use serde_json::Value;

/// Registry key for customisation section contributions.
pub const SECTIONS: &str = "base/labels.customisation_sections";

/// A row returned by a section's adapter. `input` is held by the server until
/// confirmation and passed unchanged to the owner's reviewed write contract.
#[derive(Clone, Debug)]
pub struct CustomisationRow {
    pub description: String,
    pub audit_kind: &'static str,
    pub audit_id: i64,
    pub revision: i64,
    pub input: Value,
}

/// One owner's read and undo contracts, and its projection of the read result.
/// The read and write contracts retain their own authorisation checks.
pub struct CustomisationSection {
    pub title: &'static str,
    pub read: &'static str,
    pub read_input: fn() -> Value,
    pub undo: &'static str,
    pub event: &'static str,
    pub action: &'static str,
    pub detail: &'static str,
    pub rows: fn(&Value) -> Vec<CustomisationRow>,
}

fn empty_input() -> Value {
    serde_json::json!({})
}

fn label_rows(value: &Value) -> Vec<CustomisationRow> {
    value.as_array().into_iter().flatten().filter_map(|row| {
        let (id, revision) = crate::operations::version(row).ok()?;
        let label = row["label"].as_str()?;
        Some(CustomisationRow {
            description: format!("{} {}: {} → {}", row["kind"].as_str()?, row["subject"].as_str()?, row["default"].as_str()?, label),
            audit_kind: "label",
            audit_id: id,
            revision,
            input: serde_json::json!({"kind": row["kind"], "subject": row["subject"], "version": row["version"]}),
        })
    }).collect()
}

inventory::submit! { bilibi_module_registry::Contribution::new(SECTIONS, "base/labels.customisations", bilibi_module_registry::Provenance { module: "base/labels", source: file!(), line: line!() }, &CustomisationSection {
    title: "Labels", read: "base.labels.list", read_input: empty_input, undo: "base.labels.clear", event: "customisation.labels", action: "Restore default", detail: "The module's original word will be shown again.", rows: label_rows,
}) }
