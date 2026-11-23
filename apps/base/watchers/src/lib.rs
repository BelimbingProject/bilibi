//! Base Watchers: a tenant's "when this happens, do that" as data, with no
//! program release, with sandboxed JavaScript steps (ADR 28 and ADR 34).
//!
//! A watcher names a trigger (an event an owner offers, recorded in that
//! owner's transaction), conditions over the trigger record's fields (read
//! through Grid) and actions (owner write contracts; there is no "update any
//! record" action). It is drafted as a version, by a person or an agent,
//! and runs only once an administrator approves that one fixed version.
//! Runs happen after the trigger commits, through Base Events and Queue, as
//! the `watchers` principal under its declared bounds and the tenant's live
//! grants; each run leaves a receipt that says why it matched and what it
//! did, and each action's effect is keyed so a retry never repeats it.
//! See `README.md`.
mod agent;
mod catalog;
mod definition;
mod discard;
mod review;
mod run;
mod sandbox;
mod script;
pub use sandbox::enter_worker;
mod store;
mod transfer;
mod trial;
mod web;

pub use catalog::{Action, Lookup, Trigger, ACTIONS, LOOKUPS, TRIGGERS};
pub use review::Review;
pub use store::{
    EffectView, RunView, VersionView, WatcherState, WatcherSummary, WatcherView, Watchers,
};
pub use transfer::{WatcherSetup, WatcherTransfer};

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
    EndingBinding, EndingService, HandlerBinding, HandlerService, TenantFifo, Worker, ENDINGS,
    HANDLERS, TENANT_FIFO, WORKERS,
};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, ValueType, DEFINITIONS};
use serde_json::json;
use std::sync::Arc;

/// The watcher service owner modules and pages resolve.
pub const SERVICE: &str = "base/watchers.watchers";
/// The named automation principal watchers run as. Its bounds are its own
/// capabilities plus what trigger and action owners offer it
/// (`bilibi_authz::OFFERED_CAPABILITIES`); a tenant grants each by name.
pub const PRINCIPAL: &str = "watchers";
/// Read watchers, their versions and receipts.
pub const VIEW: &str = "admin.watchers.view";
/// Draft a watcher or a watcher's next version.
pub const MANAGE: &str = "admin.watchers.manage";
/// Approve a version (making it the one that runs) and switch watchers on and off.
pub const APPROVE: &str = "admin.watchers.approve";
/// The principal runs the tenant's approved watchers.
pub const RUN: &str = "watchers.run.execute";
/// Separate live consent to execute approved watcher script steps.
pub(crate) const RUN_SCRIPT: &str = "watchers.script.execute";
pub(crate) const SCRIPT_TIME: &str = "watchers.script_milliseconds";
pub(crate) const SCRIPT_HEAP: &str = "watchers.script_heap_mib";
/// The principal records why a run ended unfinished (a standing grant).
pub const SETTLE: &str = "watchers.run.settle";

/// Most actions one run may take.
pub const ACTIONS_PER_RUN: &str = "watchers.actions_per_run";
/// Most watchers in a row one first change may start (the causal-chain budget).
pub const CHAIN_DEPTH: &str = "watchers.chain_depth";
/// Tokens refilled per minute and maximum tokens held per tenant.
pub const RUNS_PER_MINUTE: &str = "watchers.runs_per_minute";
/// Most watchers switched on for one trigger in a tenant.
pub const ACTIVE_PER_TRIGGER: &str = "watchers.active_per_trigger";

pub(crate) const TRIAL_DAYS: &str = "watchers.trial_days";
pub(crate) const TRIAL_EVENTS: &str = "watchers.trial_events";
pub(crate) const CONDITIONS: &str = "watchers.conditions";
pub(crate) const WATCHER_ACTIONS: &str = "watchers.actions";
pub(crate) const EXAMPLES: &str = "watchers.examples";
pub(crate) const VALUES_PER_LIST: &str = "watchers.values_per_list";
pub(crate) const FACTS_READ: &str = "watchers.facts_read";
pub(crate) const MAX_ATTEMPTS: &str = "watchers.max_attempts";
const ATTEMPT_CEILING: i32 = 25;

pub(crate) const ADMISSION_WORKER: &str = "base/watchers-admit";
pub(crate) const WORKER: &str = "base/watchers-run";
pub(crate) const SUBSCRIBER: &str = "base/watchers.triggers";
pub(crate) const PAGES: &str = "base/watchers.pages";
const ADMISSION: &str = "base/watchers.admission";
const RUNNER: &str = "base/watchers.runner";
const SETTLED: &str = "base/watchers.settled";

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/watchers",
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! { Contribution::new(VOCABULARY, "base/watchers.vocabulary", provenance!(), &VocabularyContribution {
    domains: &["watchers"],
    verbs: &["approve", "execute", "settle"],
    capabilities: &[
        CapabilityDefinition::tenant(VIEW),
        CapabilityDefinition::tenant(MANAGE),
        CapabilityDefinition::tenant(APPROVE),
        CapabilityDefinition::tenant(RUN),
        CapabilityDefinition::tenant(RUN_SCRIPT),
        CapabilityDefinition::tenant(SETTLE),
    ],
    roles: &[],
    system_principals: &[SystemPrincipalDefinition { name: PRINCIPAL, capabilities: &[RUN, RUN_SCRIPT, SETTLE] }],
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
            group: "Watchers",
            label,
            help,
            capability: Some(APPROVE),
            reveal_duration_ms: 5_000,
        }),
    })
}
inventory::submit! { Contribution::new(DEFINITIONS, ACTIONS_PER_RUN, provenance!(), &limit(
"Actions per run", "The most actions one watcher may take each time it runs. A watcher with more cannot be approved.", || json!(3), 10.0)) }
inventory::submit! { Contribution::new(DEFINITIONS, SCRIPT_TIME, provenance!(), &limit(
"Script time per run", "Maximum milliseconds for a scripted run or a whole trial, including preparation and operation calls.", || json!(250), 1000.0)) }
inventory::submit! { Contribution::new(DEFINITIONS, SCRIPT_HEAP, provenance!(), &limit(
"Script memory", "Maximum QuickJS heap in MiB for a script invocation.", || json!(8), 32.0)) }
inventory::submit! { Contribution::new(DEFINITIONS, CHAIN_DEPTH, provenance!(), &limit(
"Watchers in a chain", "How many watchers in a row one change may start, when a watcher's action starts another watcher. The next one is refused, and its receipt says so.", || json!(3), 10.0)) }
inventory::submit! { Contribution::new(DEFINITIONS, RUNS_PER_MINUTE, provenance!(), &limit(
"Watcher runs per minute", "Tokens added per minute to this tenant's bucket, which holds up to one minute's worth. Each new run spends one token. When the bucket is empty, runs wait in order.", || json!(600), 10_000.0)) }
inventory::submit! { Contribution::new(DEFINITIONS, ACTIVE_PER_TRIGGER, provenance!(), &limit(
"Watchers per trigger", "The most watchers that may be switched on for one trigger.", || json!(20), 100.0)) }

inventory::submit! { Contribution::new(DEFINITIONS, CONDITIONS, provenance!(), &limit(
"Conditions per watcher", "The most conditions one watcher may contain.", || json!(definition::MAX_CONDITIONS), definition::MAX_CONDITIONS as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, WATCHER_ACTIONS, provenance!(), &limit(
"Actions per watcher", "The most actions one watcher may contain. Actions per run also applies.", || json!(definition::MAX_ACTIONS), definition::MAX_ACTIONS as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, EXAMPLES, provenance!(), &limit(
"Examples per watcher", "The most examples one watcher version may contain.", || json!(definition::MAX_EXAMPLES), definition::MAX_EXAMPLES as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, VALUES_PER_LIST, provenance!(), &limit(
"Values per condition list", "The most values in one condition's list of choices.", || json!(definition::MAX_CHOICES), definition::MAX_CHOICES as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, FACTS_READ, provenance!(), &limit(
"Facts read per watcher", "The most distinct field paths read for one watcher, including the record version.", || json!(definition::MAX_FACTS), definition::MAX_FACTS as f64)) }
inventory::submit! { Contribution::new(DEFINITIONS, MAX_ATTEMPTS, provenance!(), &limit(
"Attempts per watchers job", "The most attempts allowed for a watchers job, including its first attempt. Waiting runs are refused when this budget is exhausted.", || json!(ATTEMPT_CEILING), ATTEMPT_CEILING as f64)) }

inventory::submit! { Contribution::new(DEFINITIONS, TRIAL_DAYS, provenance!(), &limit(
"Trial window (days)", "The longest past window a watcher trial may read.", || json!(30), 365.0)) }
inventory::submit! { Contribution::new(DEFINITIONS, TRIAL_EVENTS, provenance!(), &limit(
"Events per trial", "The most readable past events a watcher trial may return.", || json!(100), 500.0)) }

inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: ATTEMPT_CEILING, validate: run::arguments }) }
inventory::submit! { Contribution::new(WORKERS, ADMISSION_WORKER, provenance!(), &Worker { max_attempts: ATTEMPT_CEILING, validate: run::admission_arguments }) }
inventory::submit! { Contribution::new(TENANT_FIFO, ADMISSION_WORKER, provenance!(), &TenantFifo(|_| Ok(0))) }
inventory::submit! { Contribution::new(TENANT_FIFO, WORKER, provenance!(), &TenantFifo(run::action_group)) }
inventory::submit! { Contribution::new(HANDLERS, ADMISSION_WORKER, provenance!(), &HandlerBinding { service: ADMISSION, capability: RUN }) }
inventory::submit! { Contribution::new(ENDINGS, ADMISSION_WORKER, provenance!(), &EndingBinding { service: SETTLED, principal: PRINCIPAL, capability: SETTLE }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: RUNNER, capability: RUN }) }
inventory::submit! { Contribution::new(ENDINGS, WORKER, provenance!(), &EndingBinding { service: SETTLED, principal: PRINCIPAL, capability: SETTLE }) }
inventory::submit! { Contribution::new(bilibi_events::SUBSCRIBERS, SUBSCRIBER, provenance!(), &bilibi_events::Subscriber {
    events: &[],
    handler: &run::TRIGGERS,
}) }

fn build(services: &bilibi_module_registry::Services) -> bilibi_module_registry::Result<Watchers> {
    let catalog = Catalog::discover(&bilibi_module_registry::discover()?)?;
    let registry: Arc<bilibi_agent_api::Registry> = services.get("base/agent_api.registry")?;
    let jobs: Arc<bilibi_agent_api::job::JobContracts> =
        services.get(bilibi_agent_api::job::SERVICE)?;
    let events: Arc<bilibi_events::Events> = services.get(bilibi_events::SERVICE)?;
    let offers = catalog::Offers::build(&catalog, &registry, &jobs, &events)?;
    Ok(Watchers {
        db: services
            .get::<bilibi_database::Db>("base/database.pool")?
            .as_ref()
            .clone(),
        jobs,
        registry,
        mutations: services.get("base/database.mutations")?,
        events,
        queue: services.get(bilibi_queue::SERVICE)?,
        settings: services.get("base/settings.store")?,
        authentication: services.get("base/tenancy.authentication")?,
        directory: services.get("base/principal_directory.directory")?,
        offers,
    })
}

inventory::submit! {
    Module {
        id: "base/watchers", layer: Layer::Base,
        dependencies: &["base/custom_fields", "base/labels", "base/module_registry", "base/database", "base/audit", "base/tenancy", "base/authz", "base/queue", "base/events", "base/grid", "base/settings", "base/agent_api", "base/principal_directory", "base/ui", "base/menu", "base/datetime", "base/locale"],
        migrations: &[("base-rules-001-rules", include_str!("../migrations/001-rules.sql")), ("base-rules-002-approval-bindings", include_str!("../migrations/002-approval-bindings.sql")), ("base-rules-003-logical-events", include_str!("../migrations/003-logical-events.sql")), ("base-watchers-004-watchers", include_str!("../migrations/004-watchers.sql")), ("base-watchers-005-token-bucket", include_str!("../migrations/005-token-bucket.sql")), ("base-watchers-006-activation", include_str!("../migrations/006-activation.sql")), ("base-watchers-007-trials", include_str!("../migrations/007-trials.sql")), ("base-watchers-008-name-lookup", include_str!("../migrations/008-name-lookup.sql")), ("base-watchers-009-script-plan", include_str!("../migrations/009-script-plan.sql")), ("base-watchers-010-script-permissions", include_str!("../migrations/010-script-permissions.sql"))],
        services: &[
            Service { id: SERVICE, factory: |services| {
                let watchers = Arc::new(build(services)?);
                if !run::bind(watchers.clone()) || !agent::bind(watchers.clone()) {
                    return Err(RegistryError::Service("duplicate watchers binding".into()));
                }
                Ok(watchers)
            } },
            Service { id: ADMISSION, factory: |services| {
                Ok(Arc::new(HandlerService(Arc::new(run::Worker { watchers: services.get(SERVICE)?, admission: true }))))
            } },
            Service { id: RUNNER, factory: |services| {
                Ok(Arc::new(HandlerService(Arc::new(run::Worker { watchers: services.get(SERVICE)?, admission: false }))))
            } },
            Service { id: SETTLED, factory: |_| Ok(Arc::new(EndingService(Arc::new(run::Settled)))) },
            Service { id: PAGES, factory: |services| {
                Ok(Arc::new(bilibi_ui::page::PageService::<bilibi_tenancy::Scope, bilibi_database::Error>(Arc::new(web::Pages {
                    db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                    watchers: services.get(SERVICE)?,
                    contracts: services.get(bilibi_agent_api::page::SERVICE)?,
                    display: services.get("base/datetime.display")?,
                    locales: services.get("base/locale.locales")?,
                }))))
            } },
        ],
    }
}

inventory::submit! { Contribution::new(ROUTES, "base/watchers.index", provenance!(), &Route { company_scope: CompanyScope::Tenant,
    method: Method::Get, path: web::INDEX, service: PAGES, operation: "index", access: Access::Capability(VIEW), framed: true,
}) }
inventory::submit! { Contribution::new(ROUTES, "base/watchers.show", provenance!(), &Route { company_scope: CompanyScope::Tenant,
    method: Method::Get, path: web::SHOW, service: PAGES, operation: "show", access: Access::Capability(VIEW), framed: true,
}) }
// The page's writes are the contracts agents call (ADR 01).
inventory::submit! { Contribution::new(PAGE_WRITES, PAGES, provenance!(), &PageWrites {
    service: PAGES,
    contracts: &[
        ("approve", "base.watchers.approve"),
        ("switch", "base.watchers.switch"),
        ("draft_review", "base.watchers.draft"),
        ("draft_save", "base.watchers.draft"),
    ],
    pending: Pending::None,
}) }
bilibi_menu::contribute!(
    "base/watchers",
    bilibi_menu::MenuItem::page("admin.watchers", "Watchers", web::INDEX)
        .icon("bolt")
        .under("admin")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(63)
);
