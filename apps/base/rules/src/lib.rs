//! Base Rules: a tenant's "when this happens, do that" as data, with no
//! program release and no tenant code (ADR 28, #682).
//!
//! A rule names a trigger (an event an owner offers, recorded in that
//! owner's transaction), conditions over the trigger record's fields (read
//! through Grid) and actions (owner write contracts; there is no "update any
//! record" action). It is drafted as a version, by a person or an agent,
//! and runs only once an administrator approves that one fixed version.
//! Runs happen after the trigger commits, through Base Events and Queue, as
//! the `rules` principal under its declared bounds and the tenant's live
//! grants; each run leaves a receipt that says why it matched and what it
//! did, and each action's effect is keyed so a retry never repeats it.
//! See `README.md`.
mod agent;
mod catalog;
mod definition;
mod review;
mod run;
mod store;
mod web;

pub use catalog::{Action, Trigger, ACTIONS, TRIGGERS};
pub use review::Review;
pub use store::{EffectView, RuleState, RuleSummary, RuleView, Rules, RunView, VersionView};

use bilibi_authz::{
    CapabilityDefinition, StandingGrant, SystemPrincipalDefinition, VocabularyContribution,
    STANDING_GRANTS, VOCABULARY,
};
use bilibi_module_registry::{
    contributions::{
        Access, CompanyScope, Method, PageWrites, Pending, Route, PAGE_WRITES, ROUTES,
    },
    Catalog, Contribution, Layer, Module, Provenance, RegistryError, Service,
};
use bilibi_queue::{
    EndingBinding, EndingService, HandlerBinding, HandlerService, Worker, ENDINGS, HANDLERS,
    WORKERS,
};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, ValueType, DEFINITIONS};
use serde_json::json;
use std::sync::Arc;

/// The rule service owner modules and pages resolve.
pub const SERVICE: &str = "base/rules.rules";
/// The named automation principal rules run as. Its bounds are its own
/// capabilities plus what trigger and action owners offer it
/// (`bilibi_authz::OFFERED_CAPABILITIES`); a tenant grants each by name.
pub const PRINCIPAL: &str = "rules";
/// Read rules, their versions and receipts.
pub const VIEW: &str = "admin.rules.view";
/// Draft a rule or a rule's next version.
pub const MANAGE: &str = "admin.rules.manage";
/// Approve a version (making it the one that runs) and switch rules on and off.
pub const APPROVE: &str = "admin.rules.approve";
/// The principal runs the tenant's approved rules.
pub const RUN: &str = "rules.run.execute";
/// The principal records why a run ended unfinished (a standing grant).
pub const SETTLE: &str = "rules.run.settle";

/// Most actions one run may take.
pub const ACTIONS_PER_RUN: &str = "rules.actions_per_run";
/// Most rules in a row one first change may start (the causal-chain budget).
pub const CHAIN_DEPTH: &str = "rules.chain_depth";
/// Most rule runs one tenant starts in a minute; more wait for the next one.
pub const RUNS_PER_MINUTE: &str = "rules.runs_per_minute";
/// Most rules switched on for one trigger in a tenant.
pub const ACTIVE_PER_TRIGGER: &str = "rules.active_per_trigger";

pub(crate) const CONDITIONS: &str = "rules.conditions";
pub(crate) const RULE_ACTIONS: &str = "rules.actions";
pub(crate) const EXAMPLES: &str = "rules.examples";
pub(crate) const VALUES_PER_LIST: &str = "rules.values_per_list";
pub(crate) const FACTS_READ: &str = "rules.facts_read";
pub(crate) const MAX_ATTEMPTS: &str = "rules.max_attempts";
const ATTEMPT_CEILING: i32 = 25;

pub(crate) const WORKER: &str = "base/rules-run";
pub(crate) const SUBSCRIBER: &str = "base/rules.triggers";
pub(crate) const PAGES: &str = "base/rules.pages";
const RUNNER: &str = "base/rules.runner";
const SETTLED: &str = "base/rules.settled";

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/rules",
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! { Contribution::new(VOCABULARY, "base/rules.vocabulary", provenance!(), &VocabularyContribution {
    domains: &["rules"],
    verbs: &["approve", "execute", "settle"],
    capabilities: &[
        CapabilityDefinition::tenant(VIEW),
        CapabilityDefinition::tenant(MANAGE),
        CapabilityDefinition::tenant(APPROVE),
        CapabilityDefinition::tenant(RUN),
        CapabilityDefinition::tenant(SETTLE),
    ],
    roles: &[],
    system_principals: &[SystemPrincipalDefinition { name: PRINCIPAL, capabilities: &[RUN, SETTLE] }],
}) }
// Recording why a run ended is the way out of a refused job, never an effect.
inventory::submit! { Contribution::new(STANDING_GRANTS, PRINCIPAL, provenance!(), &StandingGrant { principal: PRINCIPAL, capabilities: &[SETTLE] }) }

const fn limit(
    label: &'static str,
    help: &'static str,
    default: fn() -> serde_json::Value,
    maximum: f64,
) -> Setting {
    Setting::Definition(Definition {
        value_type: ValueType::Integer,
        scopes: &[ScopeKind::Global, ScopeKind::Tenant],
        default,
        nullable: false,
        encrypted: false,
        minimum: Some(1.0),
        maximum: Some(maximum),
        validator: None,
        editor: Some(Editor {
            group: "Rules",
            label,
            help,
            capability: Some(APPROVE),
            reveal_duration_ms: 5_000,
        }),
    })
}
inventory::submit! { Contribution::new(DEFINITIONS, ACTIONS_PER_RUN, provenance!(), &limit(
"Actions per run", "The most actions one rule may take each time it runs. A rule with more cannot be approved.", || json!(3), 10.0)) }
inventory::submit! { Contribution::new(DEFINITIONS, CHAIN_DEPTH, provenance!(), &limit(
"Rules in a chain", "How many rules in a row one change may start, when a rule's action starts another rule. The next one is refused, and its receipt says so.", || json!(3), 10.0)) }
inventory::submit! { Contribution::new(DEFINITIONS, RUNS_PER_MINUTE, provenance!(), &limit(
"Rule runs per minute", "The most rule runs this tenant starts in a minute. More wait for the next minute, so a burst never holds up other work.", || json!(60), 10_000.0)) }
inventory::submit! { Contribution::new(DEFINITIONS, ACTIVE_PER_TRIGGER, provenance!(), &limit(
"Rules per trigger", "The most rules that may be switched on for one trigger.", || json!(20), 100.0)) }

inventory::submit! { Contribution::new(DEFINITIONS, CONDITIONS, provenance!(), &limit(
"Conditions per rule", "The most conditions one rule may contain.", || json!(definition::MAX_CONDITIONS), definition::MAX_CONDITIONS as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, RULE_ACTIONS, provenance!(), &limit(
"Actions per rule", "The most actions one rule may contain. Actions per run also applies.", || json!(definition::MAX_ACTIONS), definition::MAX_ACTIONS as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, EXAMPLES, provenance!(), &limit(
"Examples per rule", "The most examples one rule version may contain.", || json!(definition::MAX_EXAMPLES), definition::MAX_EXAMPLES as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, VALUES_PER_LIST, provenance!(), &limit(
"Values per condition list", "The most values in one condition's list of choices.", || json!(definition::MAX_CHOICES), definition::MAX_CHOICES as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, FACTS_READ, provenance!(), &limit(
"Facts read per rule", "The most distinct field paths read for one rule, including the record version.", || json!(definition::MAX_FACTS), definition::MAX_FACTS as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, MAX_ATTEMPTS, provenance!(), &limit(
"Attempts per rules job", "The most attempts allowed for a rules job, including its first attempt. Waiting runs are refused when this budget is exhausted.", || json!(ATTEMPT_CEILING), ATTEMPT_CEILING as f64)) }

inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: ATTEMPT_CEILING, validate: run::arguments }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: RUNNER, capability: RUN }) }
inventory::submit! { Contribution::new(ENDINGS, WORKER, provenance!(), &EndingBinding { service: SETTLED, principal: PRINCIPAL, capability: SETTLE }) }
inventory::submit! { Contribution::new(bilibi_events::SUBSCRIBERS, SUBSCRIBER, provenance!(), &bilibi_events::Subscriber {
    events: &[],
    handler: &run::TRIGGERS,
}) }

fn build(services: &bilibi_module_registry::Services) -> bilibi_module_registry::Result<Rules> {
    let catalog = Catalog::discover(&bilibi_module_registry::discover()?)?;
    let registry: Arc<bilibi_agent_api::Registry> = services.get("base/agent_api.registry")?;
    let jobs: Arc<bilibi_agent_api::job::JobContracts> =
        services.get(bilibi_agent_api::job::SERVICE)?;
    let events: Arc<bilibi_events::Events> = services.get(bilibi_events::SERVICE)?;
    let offers = catalog::Offers::build(&catalog, &registry, &jobs, &events)?;
    Ok(Rules {
        db: services
            .get::<bilibi_database::Db>("base/database.pool")?
            .as_ref()
            .clone(),
        jobs,
        events,
        queue: services.get("base/queue.store")?,
        settings: services.get("base/settings.store")?,
        authentication: services.get("base/tenancy.authentication")?,
        directory: services.get("base/principal_directory.directory")?,
        offers,
    })
}

inventory::submit! {
    Module {
        id: "base/rules", layer: Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/audit", "base/tenancy", "base/authz", "base/queue", "base/events", "base/grid", "base/settings", "base/agent_api", "base/principal_directory", "base/ui", "base/menu", "base/datetime", "base/locale"],
        migrations: &[("base-rules-001-rules", include_str!("../migrations/001-rules.sql")), ("base-rules-002-approval-bindings", include_str!("../migrations/002-approval-bindings.sql"))],
        services: &[
            Service { id: SERVICE, factory: |services| {
                let rules = Arc::new(build(services)?);
                if !run::bind(rules.clone()) || !agent::bind(rules.clone()) {
                    return Err(RegistryError::Service("duplicate rules binding".into()));
                }
                Ok(rules)
            } },
            Service { id: RUNNER, factory: |services| {
                Ok(Arc::new(HandlerService(Arc::new(run::Worker { rules: services.get(SERVICE)? }))))
            } },
            Service { id: SETTLED, factory: |_| Ok(Arc::new(EndingService(Arc::new(run::Settled)))) },
            Service { id: PAGES, factory: |services| {
                Ok(Arc::new(bilibi_ui::page::PageService::<bilibi_tenancy::Scope, bilibi_database::Error>(Arc::new(web::Pages {
                    db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                    rules: services.get(SERVICE)?,
                    contracts: services.get(bilibi_agent_api::page::SERVICE)?,
                    display: services.get("base/datetime.display")?,
                    locales: services.get("base/locale.locales")?,
                }))))
            } },
        ],
    }
}

inventory::submit! { Contribution::new(ROUTES, "base/rules.index", provenance!(), &Route { company_scope: CompanyScope::Tenant,
    method: Method::Get, path: web::INDEX, service: PAGES, operation: "index", access: Access::Capability(VIEW), framed: true,
}) }
inventory::submit! { Contribution::new(ROUTES, "base/rules.show", provenance!(), &Route { company_scope: CompanyScope::Tenant,
    method: Method::Get, path: web::SHOW, service: PAGES, operation: "show", access: Access::Capability(VIEW), framed: true,
}) }
// The page's writes are the contracts agents call (ADR 01).
inventory::submit! { Contribution::new(PAGE_WRITES, PAGES, provenance!(), &PageWrites {
    service: PAGES,
    contracts: &[
        ("approve", "base.rules.approve"),
        ("switch", "base.rules.switch"),
        ("draft_review", "base.rules.draft"),
        ("draft_save", "base.rules.draft"),
    ],
    pending: Pending::None,
}) }
bilibi_menu::contribute!(
    "base/rules",
    bilibi_menu::MenuItem::page("admin.rules", "Rules", web::INDEX)
        .icon("bolt")
        .under("admin")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(63)
);
