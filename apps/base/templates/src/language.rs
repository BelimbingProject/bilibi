//! Language preference for the words on outbound messages and portal notices.
use bilibi_module_registry::Contribution;
use bilibi_settings::{
    Definition, Editor, ScopeKind, Setting, TenantAdmin, Validator, ValueType, DEFINITIONS,
    TENANT_ADMIN,
};

pub const LANGUAGE: &str = "templates.language";

fn accepts(value: &serde_json::Value) -> bool {
    value.is_null()
        || value
            .as_str()
            .and_then(crate::catalogue::language)
            .is_some()
}

inventory::submit! { Contribution::new(DEFINITIONS, LANGUAGE, crate::provenance!(),
    &Setting::Definition(Definition {
        value_type: ValueType::String,
        scopes: &[ScopeKind::Company, ScopeKind::Tenant],
        default: || serde_json::Value::Null,
        nullable: true,
        encrypted: false,
        minimum: None,
        maximum: None,
        validator: Some(Validator {
            accepts,
            message: "Enter a language tag such as en or ms-MY.",
        }),
        editor: Some(Editor {
            group: "Templates",
            label: "Wording language",
            help: "Preferred language for business email and customer portal notices, such as en or ms-MY.",
            capability: None,
            reveal_duration_ms: 5_000,
        }),
    })
) }
inventory::submit! { Contribution::new(TENANT_ADMIN, LANGUAGE, crate::provenance!(), &TenantAdmin) }
