use crate::MODULE;
use bilibi_authz::{CapabilityDefinition as C, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::contributions::{
    Access, CompanyScope, Embed, EmbedContext, Method, PageWrites, Pending, Route, Seed,
    SeedAction, SeedMode, EMBEDS, PAGE_WRITES, ROUTES, SEEDS,
};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_picklists::{PickList, Start};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, Validator, ValueType};
use serde_json::{json, Value};

/// Read consent, suppressions and the check log.
pub const VIEW: &str = "core.consent.view";
/// Record consent decisions: staff capturing consent.
pub const RECORD: &str = "core.consent.record";
/// Check before sending, and suppress addresses: senders and their system
/// principals.
pub const CHECK: &str = "core.consent.check";
/// Lift suppressions.
pub const MANAGE: &str = "core.consent.manage";
const CAPABILITIES: &[&str] = &[VIEW, RECORD, CHECK, MANAGE];

/// The tenant's elective communication purposes.
pub const PURPOSES: &str = "core.consent.purposes";
/// `opted_out` or `opted_in`: what applies with no decision.
pub const DEFAULT_NEW_ADDRESS: &str = "core.consent.default_new_address";

macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

// `view` and `manage` are the Platform's verbs; `record` and `check` are
// this module's.
inventory::submit! { Contribution::new(VOCABULARY, MODULE, provenance!(), &VocabularyContribution {
    domains: &["core"], verbs: &["record", "check"],
    capabilities: &[C::tenant(VIEW), C::tenant(RECORD), C::tenant(CHECK), C::tenant(MANAGE)],
    roles: &[RoleDefinition { code: "tenant_owner", name: "Tenant Owner", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: CAPABILITIES }],
    system_principals: &[],
}) }

bilibi_picklists::declare!(
    "core/consent",
    PickList::new(
        PURPOSES,
        "Consent purpose",
        &[
            Start::new("service", "Service"),
            Start::new("marketing", "Marketing"),
        ]
    )
);
inventory::submit! { Contribution::new(SEEDS, "core/consent.picklists", provenance!(), &Seed {
    version: "core-consent-picklists-v1", mode: SeedMode::Production,
    action: SeedAction::Service { service: bilibi_picklists::SEEDS_SERVICE, operation: "reconcile" },
}) }

fn new_address(value: &Value) -> bool {
    matches!(value.as_str(), Some("opted_out" | "opted_in"))
}
inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, DEFAULT_NEW_ADDRESS, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: &[ScopeKind::Global, ScopeKind::Tenant], default: || json!("opted_out"),
    nullable: false, encrypted: false, minimum: None, maximum: None,
    validator: Some(Validator { accepts: new_address, message: "must be opted_out or opted_in" }),
    editor: Some(Editor {
        group: "Consent",
        label: "Contacts with no decision",
        help: "opted_out: a contact with no decision for a purpose is not contacted for it. opted_in: they are, until they opt out. Required business messages do not use Consent.",
        capability: Some(MANAGE),
        reveal_duration_ms: 5_000,
    }),
})) }

// The consent panel on the Core person page. Person discovers record embeds
// by kind and passes its id as `person_id`; it never imports us.
inventory::submit! { Contribution::new(EMBEDS, "core/consent.person", provenance!(), &Embed {
    slot: "core/consent.person", context: EmbedContext::Record { kind: "core/person" },
    service: crate::web::SERVICE, operation: "person",
    access: Access::Capability(VIEW),
}) }

inventory::submit! { Contribution::new(ROUTES, "core/consent.suppressions", provenance!(), &Route {
    method: Method::Get, path: "/consent/suppressions", service: crate::web::SERVICE, operation: "suppressions",
    access: Access::Capability(VIEW), framed: true,
    company_scope: CompanyScope::Company,
}) }
bilibi_menu::contribute!(
    "core/consent",
    bilibi_menu::MenuItem::container("admin.consent", "Consent")
        .icon("shield-check")
        .under("admin")
        .order(60)
);
bilibi_menu::contribute!(
    "core/consent",
    bilibi_menu::MenuItem::page(
        "admin.consent.suppressions",
        "Suppressions",
        "/consent/suppressions"
    )
    .under("admin.consent")
    .company_scoped()
    .requires(bilibi_menu::Requirement::Capability(VIEW))
    .order(10)
);

// Every write on the panel and the page calls the contract agents use
// (ADR 01).
inventory::submit! { Contribution::new(PAGE_WRITES, "core/consent.pages", provenance!(), &PageWrites {
    service: crate::web::SERVICE,
    contracts: &[("record", "core.consent.record"), ("confirm_lift", "core.consent.lift")],
    pending: Pending::None,
}) }
