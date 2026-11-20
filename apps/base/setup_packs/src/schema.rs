use serde_json::{json, Value};

pub const VERSION: u8 = 2;

fn label_key() -> Value {
    json!({
        "type": "object",
        "required": ["kind", "subject"],
        "properties": {
            "kind": {"type": "string", "enum": ["menu", "record", "field"]},
            "subject": {"type": "string", "maxLength": 200},
        }
    })
}

fn field_key() -> Value {
    json!({
        "type": "object",
        "required": ["record_type", "key"],
        "properties": {
            "record_type": {"type": "string", "maxLength": bilibi_custom_fields::RECORD_TYPE_MAX},
            "key": {"type": "string", "maxLength": bilibi_custom_fields::KEY_MAX},
        }
    })
}

pub fn selection() -> Value {
    json!({
        "type": "object",
        "required": ["labels", "custom_fields"],
        "properties": {
            "labels": {"type": "array", "items": label_key()},
            "custom_fields": {"type": "array", "items": field_key()},
        }
    })
}

pub fn pack() -> Value {
    let mut label = label_key();
    label["required"] = json!(["kind", "subject", "label"]);
    label["properties"]["label"] = json!({"type": "string", "maxLength": bilibi_labels::MAX_LABEL});
    let mut field = field_key();
    field["required"] = json!(["record_type", "key", "label", "kind", "watcher_condition"]);
    field["properties"]
        .as_object_mut()
        .expect("field properties")
        .extend([
            ("label".into(), json!({"type": "string", "maxLength": bilibi_custom_fields::LABEL_MAX})),
            ("help".into(), json!({"type": "string", "maxLength": bilibi_custom_fields::HELP_MAX})),
            ("kind".into(), json!({"type": "string", "enum": bilibi_custom_fields::Kind::ALL.map(bilibi_custom_fields::Kind::as_str)})),
            ("picklist".into(), list_key()),
            ("target".into(), list_key()),
            ("watcher_condition".into(), json!({"type": "boolean"})),
        ]);
    json!({
        "type": "object",
        "required": ["version", "labels", "custom_fields"],
        "properties": {
            "version": {"type": "integer", "enum": [VERSION], "description": "Only setup pack version 2 is supported; export a new pack instead of using version 1."},
            "labels": {"type": "array", "items": label},
            "custom_fields": {"type": "array", "items": field},
        }
    })
}

fn list_key() -> Value {
    json!({"type": "string", "maxLength": 128})
}

pub fn import() -> Value {
    json!({
        "type": "object",
        "required": ["pack"],
        "properties": {
            "pack": pack(),
            "picklist_map": {
                "type": "array",
                "description": "Map a source pick-list key in this pack to an eligible destination key. Omitted keys match by the same stable key.",
                "items": {
                    "type": "object",
                    "required": ["from", "to"],
                    "properties": {"from": list_key(), "to": list_key()}
                }
            }
        }
    })
}

pub fn apply() -> Value {
    let mut schema = import();
    schema["required"] = json!(["pack", "expected_additions"]);
    schema["properties"]["expected_additions"] = selection();
    schema["properties"]["expected_additions"]["description"] =
        json!("Copy expected_additions from the destination preview. Any newly required addition needs another review.");
    schema
}

pub fn preview() -> Value {
    json!({"type": "object", "description": "The destination's ready flag, additions (labels and custom_fields), collisions, missing dependencies and invalid entries. Identical entries are omitted. Copy expected_additions into apply."})
}

pub fn applied() -> Value {
    json!({"type": "object", "description": "Numbers of labels and custom field definitions added. Both are zero when the destination already matches."})
}
