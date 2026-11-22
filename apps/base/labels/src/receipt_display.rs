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
            BuiltinColumn::new("when", "Changed"),
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
        "base/settings" => value["key"].as_str().unwrap_or_default().to_owned(),
        "grid.shared_view" => value["page"].as_str().unwrap_or_default().to_owned(),
        "base/watchers.watcher" => "Watchers".into(),
        "menu_order" => match value["parent"].as_str() {
            Some(parent) if !parent.is_empty() => format!("Menu › {parent}"),
            _ => "Main menu".into(),
        },
        _ => format!("{} #{}", kind, change["id"].as_str().unwrap_or_default()),
    }
}
pub(crate) fn receipt_value(kind: &str, value: &Value) -> String {
    if value.is_null() || value.as_object().is_some_and(|object| object.is_empty()) {
        return "None".into();
    }
    if kind == "grid.shared_view" {
        let name = value["name"].as_str().unwrap_or("Saved view");
        let audience =
            if value["shared_with_everyone"] == true || value["audience"]["everyone"] == true {
                "Shared with everyone".into()
            } else {
                match value["roles"]
                    .as_array()
                    .or_else(|| value["audience"]["roles"].as_array())
                    .map(Vec::len)
                {
                    Some(0) | None => "No shared audience".into(),
                    Some(1) => "Shared with one role".into(),
                    Some(count) => format!("Shared with {count} roles"),
                }
            };
        let mut details = serde_json::json!({
            "summary": format!("{name} · {audience}"),
            "columns": value["view"]["columns"],
            "lenses": value["view"]["lenses"],
            "row height": value["view"]["zoom"],
            "changed since": value["view"]["since"],
            "filters and sort": value.get("params").unwrap_or(&value["view"]["params"]),
        });
        if let Some(roles) = value["roles"]
            .as_array()
            .or_else(|| value["audience"]["roles"].as_array())
        {
            details["role IDs"] = serde_json::json!(roles);
        }
        return preview_text(&details);
    }
    if kind == "base/watchers.watcher" {
        let name = value["name"].as_str().unwrap_or("Watcher");
        let summary = match value["state"].as_str() {
            Some("draft") => format!("{name} · Awaiting approval"),
            Some("on") => format!("{name} · On"),
            Some("needs_review") => format!("{name} · Needs review"),
            Some("off") => format!("{name} · Off"),
            _ => name.to_owned(),
        };
        let mut details = serde_json::json!({"summary":summary});
        if let Some(trigger) = value.get("watch").or_else(|| value.get("trigger")) {
            details["watch"] = trigger.clone();
        }
        if let Some(rule) = value.get("rule") {
            details["rule"] = rule.clone();
        }
        return preview_text(&details);
    }
    let mut parts = Vec::new();
    for (key, label) in [
        ("label", "Label"),
        ("kind", "Type"),
        ("picklist", "Choice list"),
        ("target", "Reference"),
        ("value", "Value"),
        ("source", "Source"),
        ("item_ids", "Order"),
        ("help", "Help"),
        ("archived_on", "Archived"),
        ("watcher_condition", "Available to watchers"),
        ("default", "Default"),
    ] {
        if let Some(value) = value.get(key) {
            let text = match value {
                Value::Null => "None".to_owned(),
                Value::String(text) => text.clone(),
                _ => value.to_string(),
            };
            let label = if key == "kind"
                && value.is_string()
                && matches!(text.as_str(), "menu" | "record" | "field")
            {
                "Subject type"
            } else {
                label
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
