use crate::MODULE;
use bilibi_authz::{CapabilityDefinition as C, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_database::Error;
use bilibi_grid::{Field, FieldType, GridContribution, Source, TableDecl};
use bilibi_module_registry::contributions::{
    Access, CompanyScope, Embed, EmbedContext, Method, PageWrites, Pending, Route, EMBEDS,
    PAGE_WRITES, ROUTES,
};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_records::{Holder, RecordKind, RecordOwner};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, ValueType, DEFINITIONS};
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
use serde_json::json;

/// Read the activities the reader created, is assigned or is an account
/// guest of, and shared ones on records the reader may view.
pub const VIEW: &str = "core.activity.view";
/// Oversight: read every activity of the tenant, private ones included. No
/// contributed role holds it; an administrator grants it on purpose.
pub const VIEW_ALL: &str = "core.activity.all.view";
/// Create activities, and change, close and delete those the actor created
/// or is assigned.
pub const MANAGE: &str = "core.activity.manage";
/// With `MANAGE`: change, reassign, close and delete anyone's activities the
/// actor may see. Alone it grants nothing.
pub const MANAGE_ALL: &str = "core.activity.all.manage";
/// What the tenant owner gets: their own work, never everyone's.
const OWNER_CAPABILITIES: &[&str] = &[VIEW, MANAGE];

/// Minutes before a task is due or a call or meeting starts that its
/// assignee is reminded.
pub const REMINDER_MINUTES: &str = "core.activity.reminder_minutes";
/// Whether a reminder is also emailed to the assignee.
pub const REMINDER_EMAIL: &str = "core.activity.reminder_email";

macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

// `view` and `manage` are the Platform's verbs.
inventory::submit! { Contribution::new(VOCABULARY, MODULE, provenance!(), &VocabularyContribution {
    domains: &["core"], verbs: &[],
    capabilities: &[C::tenant(VIEW), C::tenant(VIEW_ALL), C::tenant(MANAGE), C::tenant(MANAGE_ALL), C::tenant(crate::SYSTEM_CREATE)],
    roles: &[RoleDefinition { code: "tenant_owner", name: "Tenant Owner", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: OWNER_CAPABILITIES }],
    system_principals: &[],
}) }

inventory::submit! { Contribution::new(DEFINITIONS, REMINDER_MINUTES, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global, ScopeKind::Tenant], default: || json!(15),
    nullable: false, encrypted: false, minimum: Some(0.0), maximum: Some(10080.0), validator: None,
    editor: Some(Editor {
        group: "Activities",
        label: "Remind before (minutes)",
        help: "How long before a task is due, or a call or meeting starts, its assignee gets a reminder in the bell's To do. 0 reminds when it is due.",
        capability: None,
        reveal_duration_ms: 5_000,
    }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, REMINDER_EMAIL, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Boolean, scopes: &[ScopeKind::Global, ScopeKind::Tenant], default: || json!(false),
    nullable: false, encrypted: false, minimum: None, maximum: None, validator: None,
    editor: Some(Editor {
        group: "Activities",
        label: "Email reminders",
        help: "Also email each reminder to the assignee. The email names the activity and links to it; its notes stay in Bilibi.",
        capability: None,
        reveal_duration_ms: 5_000,
    }),
})) }

// The activity event subscribers (CX's customer timeline) read; README,
// Events, documents the payload.
inventory::submit! { Contribution::new(bilibi_events::EVENTS, crate::HAPPENED, provenance!(), &bilibi_events::EventType { version: 1 }) }

// Grid reads with the reader's account as `$2` and no other per-row rule, so
// Reports see only the activities the reader takes part in (`store::MINE`).
// Shared ones on records they may view need the owner's record check, which
// SQL cannot ask; they stay on the record's panel and the filtered list.
static GRID: GridContribution = GridContribution {
    record_fields: &[],
    tables: &[TableDecl::new("activities", "Activities", VIEW,
        Source::tenant("SELECT a.id,a.kind,a.subject,a.status,a.priority,a.direction,a.starts_at,a.ends_at,a.due_at,a.start_at,a.call_outcome,a.related_kind,a.related_id,a.shared,a.assignee_user_id,a.created_by,a.company_id,a.created_at,a.updated_at,a.closed_at FROM core_activities a WHERE a.tenant_id=$1 AND a.deleted_at IS NULL AND (a.created_by=$2 OR a.assignee_user_id=$2 OR EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_user_id=$2))"), "id", &[
            Field::new("id", "ID", FieldType::Integer).protected(),
            Field::choice("kind", "Kind", &["call", "meeting", "task", "note"]).protected(),
            Field::new("subject", "Subject", FieldType::String).protected(),
            Field::choice("status", "Status", &["scheduled", "held", "not_held", "canceled", "not_started", "in_progress", "pending_input", "completed", "deferred"]).protected(),
            Field::choice("priority", "Priority", &["high", "medium", "low"]).protected(),
            Field::choice("direction", "Direction", &["inbound", "outbound"]).protected(),
            Field::new("starts_at", "Starts", FieldType::DateTime).protected(),
            Field::new("ends_at", "Ends", FieldType::DateTime).protected(),
            Field::new("due_at", "Due", FieldType::DateTime).protected(),
            Field::new("start_at", "Start", FieldType::DateTime).protected(),
            Field::new("call_outcome", "Call outcome", FieldType::String).protected(),
            Field::new("related_kind", "Related record kind", FieldType::String).protected(),
            Field::new("related_id", "Related record", FieldType::String).protected(),
            Field::new("shared", "Shared", FieldType::Boolean).protected(),
            Field::new("assignee_user_id", "Assignee user ID", FieldType::Integer).protected(),
            Field::new("created_by", "Created by user ID", FieldType::Integer).protected(),
            Field::new("company_id", "Filing company ID", FieldType::Integer).protected(),
            Field::new("created_at", "Created", FieldType::DateTime).protected(),
            Field::new("updated_at", "Updated", FieldType::DateTime).protected(),
            Field::new("closed_at", "Closed", FieldType::DateTime).protected(),
        ]).label_field("subject").time_field("starts_at").record_types(&[crate::RECORD])],
    links: &[],
};
bilibi_grid::contribute!("core/activity", "core/activity.grid", &GRID);

// Other modules may point at an activity (a timeline line's source): the
// record kind's access check is the facade's visibility rule.
pub struct ActivityRecords;
pub static RECORDS: ActivityRecords = ActivityRecords;
impl RecordOwner for ActivityRecords {
    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        id: &'a str,
    ) -> ProofFuture<'a, bilibi_records::Access> {
        Box::pin(async move {
            use bilibi_records::Access as A;
            let Some(id) = id.parse::<i64>().ok().filter(|id| *id > 0) else {
                return Ok(A::NotFound);
            };
            match crate::store::readable_proof(client, scope, id).await {
                Ok(()) => Ok(A::Readable),
                Err(Error::NotFound) => Ok(A::NotFound),
                Err(Error::Forbidden) => Ok(A::Forbidden),
                Err(error) => Err(error),
            }
        })
    }
}
bilibi_records::declare!(
    "core/activity",
    RecordKind::new("core.activity.activity", "activities", &RECORDS).page("/activities/{id}")
);
// The related record each activity holds, so `referrers` counts activities
// per record.
bilibi_records::holder!(
    "core/activity",
    Holder {
        table: "activities",
        kind_field: "related_kind",
        id_field: "related_id"
    }
);

// Invitations: queued by Send invites, run as the person who sent them
// (`invitations.rs`).

// Pages: the list and the activity's record page.
inventory::submit! { Contribution::new(ROUTES, "core/activity.list", provenance!(), &Route {
    method: Method::Get, path: "/activities", service: crate::web::SERVICE, operation: "list",
    access: Access::Capability(VIEW), framed: true,
    company_scope: CompanyScope::Tenant,
}) }
inventory::submit! { Contribution::new(ROUTES, "core/activity.detail", provenance!(), &Route {
    method: Method::Get, path: "/activities/{id}", service: crate::web::SERVICE, operation: "detail",
    access: Access::Capability(VIEW), framed: true,
    company_scope: CompanyScope::Tenant,
}) }

bilibi_menu::contribute!(
    "core/activity",
    bilibi_menu::MenuItem::page("core.activity", "Activities", "/activities")
        .under("core")
        .icon("calendar")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(40)
);

// Activities panels on the Core company and person pages, and the generic
// panel any record page renders for its own kind. The hosts discover them by
// kind and pass the record's ID; they never import this module.
inventory::submit! { Contribution::new(EMBEDS, "core/activity.company", provenance!(), &Embed {
    slot: "core/activity.company", context: EmbedContext::Record { kind: "core/company" },
    service: crate::web::SERVICE, operation: "company",
    access: Access::Capability(VIEW),
}) }
inventory::submit! { Contribution::new(EMBEDS, "core/activity.person", provenance!(), &Embed {
    slot: "core/activity.person", context: EmbedContext::Record { kind: "core/person" },
    service: crate::web::SERVICE, operation: "person",
    access: Access::Capability(VIEW),
}) }
inventory::submit! { Contribution::new(EMBEDS, "core/activity.record", provenance!(), &Embed {
    slot: "record.activities", context: EmbedContext::Record { kind: "*" },
    service: crate::web::SERVICE, operation: "record",
    access: Access::Capability(VIEW),
}) }

// Every write on the pages and panels calls the contract agents use (ADR 01).
inventory::submit! { Contribution::new(PAGE_WRITES, "core/activity.pages", provenance!(), &PageWrites {
    service: crate::web::SERVICE,
    contracts: &[
        ("create_task", "core.activity.create_task"),
        ("log_call", "core.activity.log_call"),
        ("create_meeting", "core.activity.create_meeting"),
        ("create_note", "core.activity.create_note"),
        ("respond", "core.activity.respond"),
        ("pick_guest", "core.activity.add_guest"),
        ("add_guest_email", "core.activity.add_guest"),
        ("remove_guest", "core.activity.remove_guest"),
        ("send_invites", "core.activity.send_invites"),
        ("save_field", "core.activity.update"),
        ("pick_related", "core.activity.update"),
        ("clear_related", "core.activity.update"),
        ("close", "core.activity.close"),
        ("close_and_new", "core.activity.close"),
        ("delete", "core.activity.delete"),
    ],
    pending: Pending::None,
}) }
