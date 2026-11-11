//! What Visitor contributes: its one platform capability and the operator
//! settings that bound a public session. Every limit here is an operator's to
//! tune; none is a constant of the transport.
use bilibi_authz::{CapabilityDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, ValueType, DEFINITIONS};
use serde_json::json;

pub const MANAGE: &str = "admin.system.visitor.manage";
pub const SESSION_MINUTES_KEY: &str = "visitor.session_minutes";
pub const SESSION_MAX_HOURS_KEY: &str = "visitor.session_max_hours";
pub const SESSIONS_PER_SENDER_KEY: &str = "visitor.sessions_per_sender_per_minute";
pub const SESSIONS_PER_SITE_KEY: &str = "visitor.sessions_per_site_per_minute";
pub const MESSAGES_PER_MINUTE_KEY: &str = "visitor.messages_per_minute";
pub const SOCKETS_PER_SESSION_KEY: &str = "visitor.sockets_per_session";
pub const SOCKETS_TOTAL_KEY: &str = "visitor.sockets_total";
pub const UPLOAD_MAX_BYTES_KEY: &str = "visitor.upload_max_bytes";
pub const UPLOADS_PER_MINUTE_KEY: &str = "visitor.uploads_per_minute";

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/visitor",
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! { Contribution::new(VOCABULARY, "base/visitor", provenance!(), &VocabularyContribution {
    capabilities: &[CapabilityDefinition::platform(MANAGE)],
    roles: &[], domains: &[], verbs: &[], system_principals: &[],
}) }

macro_rules! setting {
    ($key:expr, $default:expr, $min:expr, $max:expr, $label:literal, $help:literal) => {
        inventory::submit! { Contribution::new(DEFINITIONS, $key, provenance!(), &Setting::Definition(Definition {
            value_type: ValueType::Integer, scopes: &[ScopeKind::Global], default: || json!($default), nullable: false, encrypted: false,
            minimum: Some($min as f64), maximum: Some($max as f64), validator: None,
            editor: Some(Editor { group: "Visitor", label: $label, help: $help, capability: Some(MANAGE), reveal_duration_ms: 5_000 }),
        })) }
    };
}
setting!(
    SESSION_MINUTES_KEY,
    15,
    1,
    1_440,
    "Visitor session lifetime (minutes)",
    "How long a visitor's session lasts before it must be renewed. Using the session renews it."
);
setting!(
    SESSION_MAX_HOURS_KEY,
    12,
    1,
    168,
    "Visitor session limit (hours)",
    "The longest a visitor's session can last, however often it is renewed."
);
setting!(
    SESSIONS_PER_SENDER_KEY,
    10,
    1,
    10_000,
    "New visitor sessions per sender per minute",
    "Most visitor sessions one network address may start per minute on each server."
);
setting!(
    SESSIONS_PER_SITE_KEY,
    120,
    1,
    100_000,
    "New visitor sessions per site per minute",
    "Most visitor sessions one site may start per minute on each server, whoever the sender."
);
setting!(
    MESSAGES_PER_MINUTE_KEY,
    60,
    5,
    10_000,
    "Visitor messages per minute",
    "Most messages and typing signals one visitor session may send per minute. Excess ones are refused without acting."
);
setting!(
    SOCKETS_PER_SESSION_KEY,
    3,
    1,
    100,
    "Open connections per visitor session",
    "Most live connections (browser tabs) one visitor session may keep open on each server."
);
setting!(
    SOCKETS_TOTAL_KEY,
    2_000,
    1,
    1_000_000,
    "Open visitor connections per server",
    "Most visitor connections open at once on each server. Staff and customer portal pages have their own places."
);
setting!(
    UPLOAD_MAX_BYTES_KEY,
    5_242_880,
    1_024,
    67_108_864,
    "Visitor file size limit (bytes)",
    "The largest file a visitor may send. The Domain's Media settings can only make this smaller."
);
setting!(
    UPLOADS_PER_MINUTE_KEY,
    6,
    1,
    1_000,
    "Visitor files per minute",
    "Most files one visitor session may send per minute."
);
