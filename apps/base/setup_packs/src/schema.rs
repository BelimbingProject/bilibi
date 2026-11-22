use serde_json::{json, Value};

pub const VERSION: u8 = 3;

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

fn view_page() -> Value {
    json!({"type":"string","maxLength":90})
}

fn view_name() -> Value {
    json!({"type":"string","maxLength":80})
}

fn role_key() -> Value {
    json!({"type":"string","maxLength":200})
}

fn id() -> Value {
    json!({"type":"integer","minimum":1})
}

fn selection(views: Value, watchers: Value) -> Value {
    json!({
        "type": "object",
        "required": ["labels", "custom_fields"],
        "properties": {
            "labels": {"type": "array", "items": label_key()},
            "custom_fields": {"type": "array", "items": field_key()},
            "views": {"type":"array","items":views},
            "watchers": {"type":"array","items":watchers}
        }
    })
}

pub fn export_selection() -> Value {
    selection(
        json!({"type":"object","required":["page","id"],"properties":{"page":view_page(),"id":id()}}),
        id(),
    )
}

fn expected_selection() -> Value {
    selection(
        json!({"type":"object","required":["page","name","role_ids"],"properties":{
            "page":view_page(),"name":view_name(),"role_ids":{"type":"array","items":id()}
        }}),
        json!({"type":"string","maxLength":120}),
    )
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
        "required": ["version", "labels", "custom_fields", "views", "watchers"],
        "properties": {
            "version": {"type": "integer", "enum": [VERSION], "description": "Export a new version 3 pack before importing."},
            "labels": {"type": "array", "items": label},
            "custom_fields": {"type": "array", "items": field},
            "views": {"type":"array","items":{
                "type":"object","required":["page","name","view","params","roles"],
                "properties":{
                    "page":view_page(),"name":view_name(),
                    "view":{"type":"object","additionalProperties":true},
                    "params":{"type":"object","additionalProperties":true},
                    "roles":{"type":"array","items":role_key()}
                }
            }},
            "watchers": {"type":"array","items":{
                "type":"object","required":["name","watch","definition"],
                "properties":{
                    "name":{"type":"string","maxLength":120},
                    "watch":{"type":"string","maxLength":200},
                    "definition":{"type":"object","additionalProperties":true}
                }
            }},
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
            "watcher_examples": {
                "type":"object",
                "description":"Destination-local examples keyed by exact watcher name. Required for new watchers with conditions. Each value is an array of {facts, expect} examples. Facts is a field-to-value object and expect is match or no_match.",
                "additionalProperties":true
            },
            "role_map": {
                "type":"array",
                "description":"Map a role key from the pack to the ID of an existing destination role.",
                "items":{"type":"object","required":["from","to"],"properties":{"from":role_key(),"to":id()}}
            },
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
    schema["properties"]["expected_additions"] = expected_selection();
    schema["properties"]["expected_additions"]["description"] =
        json!("Copy expected_additions from the destination preview. Any newly required addition needs another review.");
    schema
}

pub fn preview() -> Value {
    json!({"type": "object", "description": "The destination's ready flag, additions (labels, custom_fields, views and watchers), collisions, missing dependencies, invalid entries and warnings about free-text watcher literals. Identical entries are omitted. Copy expected_additions into apply."})
}

pub fn applied() -> Value {
    json!({"type": "object", "description": "Numbers of labels, custom field definitions, views and watchers added. All are zero when the destination already matches."})
}
