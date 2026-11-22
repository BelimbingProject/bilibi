//! Installation limits as Settings definitions.
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{
    Definition, Editor, ScopeKind, Setting, TenantAdmin, Validator, ValueType, DEFINITIONS,
    TENANT_ADMIN,
};
use serde_json::json;

pub const MAX_MESSAGE_BYTES: &str = "mail.max_message_bytes";
pub const MAX_RECIPIENTS: &str = "mail.max_recipients";
/// The name a workspace's mail is sent under; blank keeps the installation's.
pub const SENDER_NAME: &str = "mail.sender_name";

/// A resolved snapshot for the IO-free renderer and standalone mailers.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_message_bytes: usize,
    pub max_recipients: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message_bytes: 25 * 1024 * 1024,
            max_recipients: 50,
        }
    }
}
macro_rules! setting {
    ($key:expr, $default:expr, $label:literal, $help:literal) => {
        inventory::submit! { Contribution::new(DEFINITIONS, $key, Provenance {
            module: "base/mail", source: file!(), line: line!(),
        }, &Setting::Definition(Definition {
            value_type: ValueType::Integer, scopes: &[ScopeKind::Global],
            default: || json!($default), nullable: false, encrypted: false,
            minimum: Some(1.0), maximum: Some(u32::MAX as f64), validator: None,
            editor: Some(Editor { group: "Mail", label: $label, help: $help,
                capability: None, reveal_duration_ms: 5_000 }),
        })) }
    };
}
setting!(
    MAX_MESSAGE_BYTES,
    Limits::default().max_message_bytes,
    "Maximum email size (bytes)",
    "Includes encoded attachments and headers; server owner only."
);
setting!(
    MAX_RECIPIENTS,
    Limits::default().max_recipients,
    "Maximum email recipients",
    "Counts To, Cc and Bcc together; server owner only."
);

/// A display name only: an address or angle brackets in it would let one
/// workspace's mail pose as another sender's.
fn sender_name(value: &serde_json::Value) -> bool {
    value.as_str().is_some_and(|name| {
        name.trim() == name
            && name.chars().count() <= 100
            && !name
                .chars()
                .any(|c| c.is_control() || matches!(c, '@' | '<' | '>' | '"'))
    })
}

inventory::submit! { Contribution::new(DEFINITIONS, SENDER_NAME, Provenance {
    module: "base/mail", source: file!(), line: line!(),
}, &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: &[ScopeKind::Tenant],
    default: || json!(""), nullable: false, encrypted: false,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: sender_name, message: "Enter a name of up to 100 characters without @, quotes or angle brackets." }),
    editor: Some(Editor { group: "Mail", label: "Sender name",
        help: "The sender name on this tenant's email. Blank uses the server's name; the address stays the server's.",
        capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(TENANT_ADMIN, SENDER_NAME, Provenance {
    module: "base/mail", source: file!(), line: line!(),
}, &TenantAdmin) }
