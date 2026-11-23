//! What owners offer watchers: triggers (an event of theirs, with the Grid table
//! its subject lives in) and actions (a write contract of theirs). Base names
//! no owner: each contributes here, offers the `watchers` principal the
//! capabilities it needs (`bilibi_authz::OFFERED_CAPABILITIES`) and admits
//! it to the contract (`bilibi_agent_api::job::PRINCIPAL_WRITES`). Boot
//! refuses an offer that would not work or that could grant access.
use bilibi_agent_api::{job::JobContracts, Registry};
use bilibi_events::On;
use bilibi_module_registry::{Catalog, Consumer, Entry, RegistryError, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// The catalog an owner offers a watcher trigger in:
/// `Contribution::new(TRIGGERS, "<its event name>", provenance, &Trigger { .. })`.
pub const TRIGGERS: &str = "base/watchers.triggers";
/// The catalog an owner offers a watcher action in:
/// `Contribution::new(ACTIONS, "<its write contract key>", provenance, &Action { .. })`.
pub const ACTIONS: &str = "base/watchers.actions";
/// The catalog an owner uses to offer one of its read contracts to approved
/// tenant scripts.
pub const LOOKUPS: &str = "base/watchers.lookups";

/// An event of the contributor's own, offered as a watcher trigger. The watcher
/// engine subscribes to it through Base Events, so it is recorded in the
/// owner's transaction and handed over after commit.
pub struct Trigger {
    /// The event and the payload versions the trigger reads.
    pub on: On,
    /// What happened, as an administrator reads it: "A case is opened".
    pub title: &'static str,
    /// The Grid table of the record the event is about; conditions read its
    /// fields through Grid as the `watchers` principal.
    pub table: &'static str,
    /// JSON pointer to that record's key in the event payload (`/case`).
    pub subject: &'static str,
    pub logical_event: Option<&'static str>,
    /// The integer Grid field used to detect changes to the evaluated record.
    pub version: &'static str,
}

/// A write contract of the contributor's own that a watcher may run on the
/// trigger's record. There is no "update any record" action: each one is an
/// owner's operation, with its own schema, preview, checks and audit.
pub struct Action {
    /// What it does, as an administrator reads it: "Send the case to a queue".
    pub title: &'static str,
    /// The Grid table of the record it acts on; a watcher may use it only with a
    /// trigger on the same table.
    pub table: &'static str,
    /// The input property that receives the trigger's record key. A watcher
    /// never supplies it: the runner binds it.
    pub subject: &'static str,
    /// The integer input the runner binds to the evaluated record version;
    /// the owner must check it under the record lock before mutating.
    pub version: &'static str,
    /// The events its commit may record, so an approval can refuse a watcher
    /// that would close a cycle through active watchers.
    pub records: &'static [&'static str],
    pub approval_capabilities: &'static [&'static str],
}

/// A read contract of the contributor's own that an approved script may call.
/// The operation's schema, capability and channel checks remain authoritative.
pub struct Lookup {
    /// What the read supplies, as an administrator reads it.
    pub title: &'static str,
}

/// Contract owners never offered: an action that grants access, changes
/// identity or decides an approval runs only by a person's hand.
const NEVER: [&str; 10] = [
    "base.authz.",
    "base.authz_agent.",
    "base.workflow.",
    "base.step_up.",
    "base.watchers.",
    "base.tenancy.",
    "base.session.",
    "core.user.",
    "core.tenant.",
    "core.agent.",
];

pub(crate) struct OfferedTrigger {
    pub event: &'static str,
    pub definition: &'static Trigger,
}
pub(crate) struct OfferedAction {
    pub key: &'static str,
    pub definition: &'static Action,
    pub title: &'static str,
    pub summary: &'static str,
    pub capability: &'static str,
    pub input: Value,
    pub local_script: bool,
}

pub(crate) struct OfferedLookup {
    pub key: &'static str,
    pub title: &'static str,
    pub capability: &'static str,
    pub input: Value,
    pub output: Value,
}

/// The mounted triggers and actions, checked against the Events, Grid,
/// Authz and Agent API catalogs once at boot.
pub(crate) struct Offers {
    pub triggers: BTreeMap<&'static str, OfferedTrigger>,
    pub actions: BTreeMap<&'static str, OfferedAction>,
    pub lookups: BTreeMap<&'static str, OfferedLookup>,
}

fn refuse(entry: &Entry, why: impl std::fmt::Display) -> RegistryError {
    RegistryError::Contribution(format!(
        "invalid watcher offer {} at {}:{}: {why}",
        entry.id, entry.provenance.source, entry.provenance.line
    ))
}

impl Offers {
    pub(crate) fn build(
        catalog: &Catalog,
        registry: &Registry,
        jobs: &JobContracts,
        events: &bilibi_events::Events,
    ) -> Result<Self> {
        let vocabulary = bilibi_authz::vocabulary()
            .map_err(|_| RegistryError::Contribution("authz vocabulary unavailable".into()))?;
        let bounds = vocabulary
            .system_principals
            .get(crate::PRINCIPAL)
            .map(|principal| principal.capabilities)
            .unwrap_or_default();
        let grid = bilibi_grid::installed()?;
        let declared: Vec<&str> = events.event_types().collect();
        let mut triggers = BTreeMap::new();
        for entry in catalog.entries::<Trigger>(TRIGGERS)? {
            let trigger = entry.get::<Trigger>()?;
            let module = trigger.on.event.split_once('.').map(|(module, _)| module);
            if entry.id != trigger.on.event || module != Some(entry.provenance.module) {
                return Err(refuse(
                    entry,
                    "a trigger is one of the contributor's own events, named by its contribution",
                ));
            }
            if !declared.contains(&trigger.on.event) {
                return Err(refuse(entry, "the event is not a declared event type"));
            }
            let Some(table) = grid.table(trigger.table) else {
                return Err(refuse(
                    entry,
                    format!("{} is not a Grid table", trigger.table),
                ));
            };
            if table
                .field(trigger.version)
                .is_none_or(|f| f.ty != bilibi_grid::FieldType::Integer)
            {
                return Err(refuse(entry, "needs an integer record version field"));
            }
            if !bounds.contains(&table.capability()) {
                return Err(refuse(
                    entry,
                    format!(
                        "the watchers principal cannot read {}: offer it {} (OFFERED_CAPABILITIES)",
                        trigger.table,
                        table.capability()
                    ),
                ));
            }
            if trigger
                .logical_event
                .is_some_and(|pointer| !pointer.starts_with('/'))
            {
                return Err(refuse(entry, "logical event identity needs a JSON pointer"));
            }
            if !trigger.subject.starts_with('/') || trigger.title.trim().is_empty() {
                return Err(refuse(
                    entry,
                    "needs a title and a JSON pointer to the subject",
                ));
            }
            triggers.insert(
                trigger.on.event,
                OfferedTrigger {
                    event: trigger.on.event,
                    definition: trigger,
                },
            );
        }
        let mut actions = BTreeMap::new();
        for entry in catalog.entries::<Action>(ACTIONS)? {
            let action = entry.get::<Action>()?;
            if NEVER.iter().any(|prefix| entry.id.starts_with(prefix)) {
                return Err(refuse(
                    entry,
                    "an action that grants access or decides an approval is never automated",
                ));
            }
            let Some(registered) = registry.operations.get(entry.id) else {
                return Err(refuse(entry, "not a registered operation"));
            };
            if registered.provenance.module != entry.provenance.module {
                return Err(refuse(entry, "the contract belongs to another module"));
            }
            jobs.principal_writable(entry.id, crate::PRINCIPAL)
                .map_err(|_| {
                    refuse(
                        entry,
                        "the contract does not admit the watchers principal's jobs (PRINCIPAL_WRITES)",
                    )
                })?;
            for key in action.approval_capabilities {
                if !vocabulary.capabilities.contains_key(key) {
                    return Err(refuse(
                        entry,
                        format!("undeclared approval capability {key}"),
                    ));
                }
            }
            let op = registered.definition;
            if !bounds.contains(&op.capability) {
                return Err(refuse(
                    entry,
                    format!(
                        "the watchers principal cannot hold {}: offer it (OFFERED_CAPABILITIES)",
                        op.capability
                    ),
                ));
            }
            if grid.table(action.table).is_none() {
                return Err(refuse(
                    entry,
                    format!("{} is not a Grid table", action.table),
                ));
            }
            if action.version == action.subject
                || registered.input["properties"][action.version]["type"] != "integer"
            {
                return Err(refuse(
                    entry,
                    "needs a separate integer expected-version input",
                ));
            }
            if registered.input["properties"].get(action.subject).is_none() {
                return Err(refuse(
                    entry,
                    format!("the contract has no input {}", action.subject),
                ));
            }
            if let Some(event) = action
                .records
                .iter()
                .find(|event| !declared.contains(event))
            {
                return Err(refuse(
                    entry,
                    format!("{event} is not a declared event type"),
                ));
            }
            actions.insert(
                entry.id,
                OfferedAction {
                    key: entry.id,
                    definition: action,
                    title: action.title,
                    summary: op.summary,
                    capability: op.capability,
                    input: registered.input.clone(),
                    local_script: matches!(op.handler, bilibi_agent_api::Handler::SafeWrite(handler) if handler.local_script_action()),
                },
            );
        }
        let mut lookups = BTreeMap::new();
        for entry in catalog.entries::<Lookup>(LOOKUPS)? {
            let lookup = entry.get::<Lookup>()?;
            let Some(registered) = registry.operations.get(entry.id) else {
                return Err(refuse(entry, "not a registered operation"));
            };
            if registered.provenance.module != entry.provenance.module {
                return Err(refuse(entry, "the contract belongs to another module"));
            }
            if !matches!(
                registered.definition.handler,
                bilibi_agent_api::Handler::Read(_)
            ) {
                return Err(refuse(entry, "a script lookup must be a read contract"));
            }
            if !bounds.contains(&registered.definition.capability) {
                return Err(refuse(
                    entry,
                    format!(
                        "the watchers principal cannot hold {}: offer it (OFFERED_CAPABILITIES)",
                        registered.definition.capability
                    ),
                ));
            }
            if lookup.title.trim().is_empty() {
                return Err(refuse(entry, "needs a title"));
            }
            lookups.insert(
                entry.id,
                OfferedLookup {
                    key: entry.id,
                    title: lookup.title,
                    capability: registered.definition.capability,
                    input: registered.input.clone(),
                    output: registered.output.clone(),
                },
            );
        }
        Ok(Self {
            triggers,
            actions,
            lookups,
        })
    }
}

/// The events every mounted trigger names, for the watchers subscriber.
pub(crate) fn trigger_events(catalog: &Catalog) -> Result<Vec<&'static On>> {
    catalog
        .entries::<Trigger>(TRIGGERS)?
        .iter()
        .map(|entry| entry.get::<Trigger>().map(|trigger| &trigger.on))
        .collect()
}

inventory::submit! { Consumer::new::<Trigger>(TRIGGERS, "base/watchers", |entries| entries.iter().try_for_each(|entry| entry.get::<Trigger>().map(|_| ()))) }
inventory::submit! { Consumer::new::<Action>(ACTIONS, "base/watchers", |entries| entries.iter().try_for_each(|entry| entry.get::<Action>().map(|_| ()))) }
inventory::submit! { Consumer::new::<Lookup>(LOOKUPS, "base/watchers", |entries| entries.iter().try_for_each(|entry| entry.get::<Lookup>().map(|_| ()))) }
