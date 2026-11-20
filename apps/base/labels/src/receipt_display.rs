//! Presentation of owner-authorized receipt values.
use bilibi_database::{Error, Result};
use bilibi_ui::{BuiltinColumn, ListColumns};
use serde_json::Value;

pub(crate) fn receipt_columns() -> Result<ListColumns> {
    ListColumns::mount(
        "customisation-receipts",
        vec![
            BuiltinColumn::new("trace", "Trace"),
            BuiltinColumn::new("who", "Changed by"),
            BuiltinColumn::new("where", "Where"),
            BuiltinColumn::new("before", "Before"),
            BuiltinColumn::new("after", "After"),
            BuiltinColumn::new("effective", "Effective now"),
            BuiltinColumn::new("state", "Status"),
        ],
    )
    .map_err(|_| Error::Internal)
}
pub(crate) fn preview_text(value: &Value) -> String {
    fn walk(value: &Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, item) in map {
                    if key == "summary" {
                        continue;
                    }
                    let name = if key == "plan" {
                        prefix.to_owned()
                    } else if prefix.is_empty() {
                        key.replace('_', " ")
                    } else {
                        format!("{prefix} › {}", key.replace('_', " "))
                    };
                    walk(item, &name, out);
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(item, &format!("{prefix} {}", i + 1), out);
                }
            }
            _ => {
                let text = match value {
                    Value::Null => "None".to_owned(),
                    Value::String(text) => text.clone(),
                    _ => value.to_string(),
                };
                out.push(format!("{prefix}: {text}"));
            }
        }
    }
    let mut lines = Vec::new();
    if let Some(summary) = value["summary"].as_str() {
        lines.push(summary.to_owned());
    }
    walk(value, "", &mut lines);
    lines.join(" · ")
}
pub(crate) fn receipt_where(change: &Value) -> String {
    let value = if change["effective"].is_object() {
        &change["effective"]
    } else if change["after"].is_object() {
        &change["after"]
    } else {
        &change["before"]
    };
    let kind = change["kind"].as_str().unwrap_or_default();
    match kind {
        "label" => format!(
            "{}: {}",
            value["kind"].as_str().unwrap_or_default(),
            value["subject"].as_str().unwrap_or_default()
        ),
        "custom_field" => format!(
            "{}: {}",
            value["record_type"].as_str().unwrap_or_default(),
            value["key"].as_str().unwrap_or_default()
        ),
        _ => format!("{} #{}", kind, change["id"].as_str().unwrap_or_default()),
    }
}
pub(crate) fn receipt_value(value: &Value) -> String {
    if value.is_null() || value.as_object().is_some_and(|object| object.is_empty()) {
        return "None".into();
    }
    let mut parts = Vec::new();
    for (key, label) in [
        ("label", "Label"),
        ("help", "Help"),
        ("archived_on", "Archived"),
        ("rule_condition", "Available to watchers"),
        ("default", "Default"),
    ] {
        if let Some(value) = value.get(key) {
            let text = match value {
                Value::Null => "None".to_owned(),
                Value::String(text) => text.clone(),
                _ => value.to_string(),
            };
            parts.push(format!("{label}: {text}"));
        }
    }
    if parts.is_empty() {
        "Unavailable".into()
    } else {
        parts.join("; ")
    }
}
