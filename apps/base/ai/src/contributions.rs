//! The log's capability and the service's settings: the two switches, the
//! providers, their keys, the time limit and the log's retention.
use crate::providers;
use bilibi_authz::{CapabilityDefinition, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{
    Definition, Editor, ScopeKind, Setting, Validator, ValueType, DEFINITIONS, NEVER_REVEALED,
};
use serde_json::{json, Value};

/// The interaction list and record pages and reads. Tenant-usable: a tenant
/// sees its own rows; only the platform operator scope sees every tenant.
pub const LIST: &str = "admin.system.ai-interaction.list";
/// The installation switch, off by default.
pub const ENABLED: &str = "ai.enabled";
/// The company switch, off by default: a company uses AI only when both are on.
pub const COMPANY_ENABLED: &str = "ai.company_enabled";
/// The configured providers, a list of `{name, base_url, model}` with an
/// optional `local: true`; the first is the default.
pub const PROVIDERS: &str = "ai.providers";
/// The providers' API keys by provider name, encrypted and never shown back.
pub const PROVIDER_KEYS: &str = "ai.provider_keys";
/// The upper bound, in seconds, on any call's deadline.
pub const TIMEOUT_SECONDS: &str = "ai.timeout_seconds";
pub(crate) const TIMEOUT_MIN: i64 = 5;
pub(crate) const TIMEOUT_MAX: i64 = 60;
/// Days to keep an interaction row; the daily retention job deletes older ones.
pub const RETENTION_DAYS: &str = "ai.interaction_retention_days";
pub(crate) const RETENTION_MAX: i64 = 3650;
/// Core Company's capability for a company's own settings, as Datetime's
/// company timezone uses; Authz checks the key against the vocabulary at boot.
const COMPANY_UPDATE: &str = "admin.company.update";

macro_rules! provenance {
    () => {
        Provenance {
            module: crate::MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! { Contribution::new(VOCABULARY, crate::MODULE, provenance!(), &VocabularyContribution {
    domains: &[],
    verbs: &[],
    capabilities: &[CapabilityDefinition::tenant(LIST)],
    // As for API exchanges: auditors and system viewers may read the log.
    roles: &[
        RoleDefinition { code: "auditor", name: "Auditor", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: &[LIST] },
        RoleDefinition { code: "system_viewer", name: "System Viewer", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: &[LIST] },
    ],
    system_principals: &[],
}) }

fn accepts_providers(value: &Value) -> bool {
    providers::parse(value).is_some()
}
fn accepts_keys(value: &Value) -> bool {
    providers::keys(value).is_some()
}

const GLOBAL: &[ScopeKind] = &[ScopeKind::Global];

inventory::submit! { Contribution::new(DEFINITIONS, ENABLED, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Boolean, scopes: GLOBAL, default: || json!(false), nullable: false, encrypted: false,
    minimum: None, maximum: None, validator: None,
    editor: Some(Editor { group: "AI", label: "AI on this installation",
        help: "Lets modules send text to the configured AI providers. Each company must also turn AI on for itself.",
        capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, COMPANY_ENABLED, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Boolean, scopes: &[ScopeKind::Company], default: || json!(false), nullable: false, encrypted: false,
    minimum: None, maximum: None, validator: None,
    editor: Some(Editor { group: "AI", label: "AI for this company",
        help: "Lets modules send this company's text to the configured AI providers. It works only while AI is on for the installation.",
        capability: Some(COMPANY_UPDATE), reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, PROVIDERS, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Array, scopes: GLOBAL, default: || json!([]), nullable: false, encrypted: false,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: accepts_providers, message: "must be a list of up to 20 providers, each {\"name\", \"base_url\", \"model\"} with a unique lower-case name and an https base URL, or \"local\": true for an http or https server on this machine or a private network" }),
    editor: Some(Editor { group: "AI", label: "AI providers",
        help: "OpenAI-compatible servers. A hosted service needs a public https URL. Mark a server on this machine or your private network, such as Ollama, vLLM or LM Studio, with \"local\": true. Example: [{\"name\": \"local\", \"base_url\": \"http://127.0.0.1:11434/v1\", \"model\": \"llama3.1\", \"local\": true}]. The first is the default.",
        capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, PROVIDER_KEYS, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Array, scopes: GLOBAL, default: || Value::Null, nullable: true, encrypted: true,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: accepts_keys, message: "must map provider names to API keys, such as {\"hosted\": \"sk-...\"}, with no spaces in a key" }),
    editor: Some(Editor { group: "AI", label: "AI provider API keys",
        help: "Only for providers that need one, by provider name: {\"hosted\": \"sk-...\"}. Stored encrypted and never shown back, not even to a holder of the stored-secret reveal; saving replaces every key.",
        capability: None, reveal_duration_ms: NEVER_REVEALED }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, TIMEOUT_SECONDS, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: GLOBAL, default: || json!(20), nullable: false, encrypted: false,
    minimum: Some(TIMEOUT_MIN as f64), maximum: Some(TIMEOUT_MAX as f64), validator: None,
    editor: Some(Editor { group: "AI", label: "AI answer time limit (seconds)",
        help: "The longest a person waits for one AI answer. A slower answer is reported as unavailable.",
        capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, RETENTION_DAYS, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: GLOBAL, default: || json!(180), nullable: false, encrypted: false,
    minimum: Some(1.0), maximum: Some(RETENTION_MAX as f64), validator: None,
    editor: Some(Editor { group: "AI", label: "AI interaction retention (days)",
        help: "Days to keep the record of each AI call. Older records are deleted by the daily retention schedule.",
        capability: None, reveal_duration_ms: 5_000 }),
})) }
