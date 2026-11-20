//! Registered sections of tenant customisation: the rows the Customisations
//! page lists with their undo action, and what an agent may change
//! (`base.labels.customisations.discover`). One contribution feeds both.
use crate::Kind;
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

/// One owner's kind of customisation. `undo` puts its rows on the
/// Customisations page; `changes` tells agents what may be changed. A section
/// declares at least one. Every operation keeps its own authorisation checks.
pub struct CustomisationSection {
    pub title: &'static str,
    pub undo: Option<Undo>,
    pub changes: Option<Changes>,
}

/// The owner's read and undo contracts for the page, and its projection of
/// the read result into rows.
pub struct Undo {
    pub read: &'static str,
    pub read_input: fn() -> Value,
    /// The reviewed write a confirmed row calls with its `input`.
    pub operation: &'static str,
    pub event: &'static str,
    pub action: &'static str,
    pub detail: &'static str,
    pub rows: fn(&Value) -> Vec<CustomisationRow>,
}

/// What an agent may change in a section: the owner's read that lists every
/// choice, with or without an override, and the operations that change,
/// preview, verify and restore one.
pub struct Changes {
    /// One plain sentence saying what this lets a tenant change.
    pub purpose: &'static str,
    /// The bounds a change must stay within, in plain words.
    pub limits: &'static str,
    /// The owner's guide key.
    pub guide: &'static str,
    /// The reads that together list what may be changed, including
    /// untouched choices. None may need a transaction.
    pub reads: &'static [Read],
    /// Projects the results of `reads`, in their order, into choices.
    pub choices: fn(&[Value]) -> Vec<Choice>,
    /// The reviewed writes that make or edit a customisation. Their dry run
    /// is the preview.
    pub change: &'static [&'static str],
    /// A read that shows the result before saving, when the owner has one.
    pub preview: Option<&'static str>,
    /// A read that shows the saved result.
    pub verify: &'static str,
    /// The reviewed write that returns a choice to its default.
    pub restore: Option<&'static str>,
}

/// A read operation and the input to call it with.
pub struct Read {
    pub operation: &'static str,
    pub input: fn() -> Value,
}

/// One thing that may be changed.
#[derive(Clone, Debug, Default)]
pub struct Choice {
    /// Stable within its section.
    pub subject: String,
    pub title: String,
    pub keywords: &'static str,
    /// The page address this choice shows on, when it belongs to one page.
    pub page: Option<String>,
    /// Every stable ID the owner knows this choice's kind of record by.
    pub record_kinds: Vec<String>,
    /// The value in effect; `None` when it may not be read (a secret).
    pub value: Option<Value>,
    /// What a restore returns to, when the owner knows it.
    pub default: Option<Value>,
    /// Where the value in effect comes from: `default`, `tenant`, …
    pub source: Option<String>,
    /// Whether the tenant has changed it.
    pub customised: bool,
    /// The token the owner's writes ask for.
    pub version: Option<Value>,
    /// The fields identifying the choice in its owner's write input.
    pub input: Value,
    /// Input to the section's verify operation.
    pub verify_input: Value,
    /// Choice-specific constraints read from the owner.
    pub limits: Value,
    /// Writes supported for this choice. `None` uses all section writes.
    pub writes: Option<Vec<&'static str>>,
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

fn label_choices(reads: &[Value]) -> Vec<Choice> {
    reads[0].as_array().into_iter().flatten().filter_map(|row| {
        let (kind, subject, default) = (row["kind"].as_str()?, row["subject"].as_str()?, row["default"].as_str()?);
        let keywords = match Kind::from_key(kind)? {
            Kind::Menu => "rename label word wording terminology menu item restore",
            Kind::Record => "rename label word wording terminology record type restore",
            Kind::Field => "rename label word wording terminology field column heading restore",
        };
        let trail = row["trail"].as_str().unwrap_or_default();
        let label = row["label"].as_str();
        Some(Choice {
            subject: format!("{kind}:{subject}"),
            keywords,
            title: if trail.is_empty() { format!("{kind}: {default}") } else { format!("{kind}: {trail} › {default}") },
            page: row["page"].as_str().map(str::to_owned),
            record_kinds: row["record_kinds"].as_array().into_iter().flatten().filter_map(|kind| kind.as_str().map(str::to_owned)).collect(),
            value: Some(serde_json::json!(label.unwrap_or(default))),
            default: Some(serde_json::json!(default)),
            source: Some(if label.is_some() { "tenant" } else { "default" }.into()),
            customised: label.is_some(),
            version: Some(row["version"].clone()),
            input: serde_json::json!({"kind": kind, "subject": subject, "version": row["version"]}),
            verify_input: empty_input(),
            ..Default::default()
        })
    }).collect()
}

inventory::submit! { bilibi_module_registry::Contribution::new(SECTIONS, "base/labels.customisations", bilibi_module_registry::Provenance { module: "base/labels", source: file!(), line: line!() }, &CustomisationSection {
    title: "Labels",
    undo: Some(Undo { read: "base.labels.list", read_input: empty_input, operation: "base.labels.clear", event: "customisation.labels", action: "Restore default", detail: "The module's original word will be shown again.", rows: label_rows }),
    changes: Some(Changes {
        purpose: "Show your own word for a menu item, a record type or a field.",
        limits: "1 to 60 characters of plain text. Operation keys, routes and permission keys never change.",
        guide: "base.labels",
        reads: &[Read { operation: "base.labels.list", input: empty_input }], choices: label_choices,
        change: &["base.labels.set"], preview: None, verify: "base.labels.list", restore: Some("base.labels.clear"),
    }),
}) }

/// An owner's audit kind and read capability for customisation receipts.
pub struct ReceiptSource {
    pub kind: &'static str,
    pub capability: &'static str,
}
/// Registry key for owner-authorized receipt sources.
pub const RECEIPT_SOURCES: &str = "base/labels.receipt_sources";
inventory::submit! { bilibi_module_registry::Contribution::new(RECEIPT_SOURCES, "base/labels.receipts", bilibi_module_registry::Provenance { module: "base/labels", source: file!(), line: line!() }, &ReceiptSource { kind: "label", capability: crate::VIEW }) }
