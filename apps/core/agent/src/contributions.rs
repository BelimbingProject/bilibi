//! Operator settings (Administration → Settings → Operator, P3), the platform
//! capability that edits them, and audit redaction for the stored digests.
use bilibi_audit::{Redaction, REDACTIONS};
use bilibi_authz::{CapabilityDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{
    contributions::{Access, Embed, EmbedContext, Method, Moved, Route, EMBEDS, MOVED, ROUTES},
    Contribution, Provenance,
};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, ValueType, DEFINITIONS};
use serde_json::json;

/// Operator settings are platform-only: a grant alone is not enough, the
/// persistent operator identity is required too.
pub const MANAGE: &str = "admin.system.agent-access.manage";
macro_rules! provenance {
    () => {
        Provenance {
            module: "core/agent",
            source: file!(),
            line: line!(),
        }
    };
}
const GLOBAL: &[ScopeKind] = &[ScopeKind::Global];

inventory::submit! { Contribution::new(VOCABULARY, "core/agent", provenance!(), &VocabularyContribution {
    capabilities: &[CapabilityDefinition::platform(MANAGE)], domains: &[], verbs: &[], roles: &[], system_principals: &[],
}) }

inventory::submit! { Contribution::new(DEFINITIONS, crate::AUTHORIZATION_CODE_SECONDS, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: GLOBAL, default: || json!(120), nullable: false, encrypted: false, minimum: Some(30.0), maximum: Some(600.0), validator: None,
    editor: Some(Editor { group: "Agent access", label: "OAuth authorization code lifetime (seconds)", help: "Seconds an app has to use its sign-in code after you approve it.", capability: Some(MANAGE), reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, crate::OAUTH_REQUESTS_PER_MINUTE, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: GLOBAL, default: || json!(60), nullable: false, encrypted: false, minimum: Some(1.0), maximum: Some(10_000.0), validator: None,
    editor: Some(Editor { group: "Agent access", label: "OAuth requests per minute", help: "Most connection requests one address can make per minute, on each server.", capability: Some(MANAGE), reveal_duration_ms: 5_000 }),
})) }

// Audit never receives a digest from this module, and the policy also masks the
// field names should a later change capture a whole row.
inventory::submit! { Contribution::new(REDACTIONS, "core/agent.devices", provenance!(), &Redaction { kind: "agent_device", fields: &["device_hash", "user_hash"] }) }
inventory::submit! { Contribution::new(REDACTIONS, "core/agent.tokens", provenance!(), &Redaction { kind: "agent_token", fields: &["token_hash"] }) }
inventory::submit! { Contribution::new(REDACTIONS, "core/agent.provider", provenance!(), &Redaction { kind: "agent_connection", fields: &["provider_token_hash", "enabled_session_id"] }) }
inventory::submit! { Contribution::new(REDACTIONS, "core/agent.codes", provenance!(), &Redaction { kind: "agent_authorization_code", fields: &["code_hash", "code_challenge", "enabled_session_id"] }) }

// The three pages (E4). Every signed-in person may open them; `AgentAccess`
// keeps the self-service gate. The consent pages are never framed.
inventory::submit! { Contribution::new(ROUTES, "core/agent.device", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: "/oauth/device", service: "core/agent.pages", operation: "device", access: Access::SignedIn, framed: false,
}) }
inventory::submit! { Contribution::new(ROUTES, "core/agent.authorize", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: "/oauth/authorize", service: "core/agent.pages", operation: "authorize", access: Access::SignedIn, framed: false,
}) }
inventory::submit! { Contribution::new(ROUTES, "core/agent.connections", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: "/settings/ai-connections", service: "core/agent.pages", operation: "connections", access: Access::SignedIn, framed: true,
}) }
inventory::submit! { Contribution::new(ROUTES, "core/agent.review", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: "/ai/reviews/{review}", service: "core/agent.pages", operation: "review", access: Access::SignedIn, framed: false,
}) }
// The status bar's AI tag and its panel (`src/ai_status.rs`). Web renders
// every `EmbedContext::Shell` embed into the shell and mounts it as the panel
// `#app-ai`; the panel shows only the signed-in person's own connections and
// activity, so every signed-in account may see it.
inventory::submit! { Contribution::new(EMBEDS, "core/agent.ai_status", provenance!(), &Embed {
    slot: "shell.ai", context: EmbedContext::Shell, service: "core/agent.pages", operation: "ai_status", access: Access::SignedIn,
}) }
// The MCP page and the connections list are one tab. Old addresses keep working.
inventory::submit! { Contribution::new(MOVED, "core/agent.connections.moved", provenance!(), &Moved {
    from: "/settings/agent-connections", to: "/settings/ai-connections",
}) }
inventory::submit! { Contribution::new(MOVED, "core/agent.mcp.moved", provenance!(), &Moved {
    from: "/settings/mcp", to: "/settings/ai-connections",
}) }
inventory::submit! { Contribution::new(MOVED, "core/agent.chatgpt.moved", provenance!(), &Moved {
    from: "/settings/chatgpt-plugin", to: "/settings/ai-connections",
}) }
