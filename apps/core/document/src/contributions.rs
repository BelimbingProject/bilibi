use crate::{MODULE, RECORD};
use bilibi_authz::{CapabilityDefinition as C, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_database::Error;
use bilibi_grid::{Field, FieldType, GridContribution, LinkDecl, LinkKind, Source, TableDecl};
use bilibi_media::{Subject as MediaSubject, SUBJECTS as MEDIA_SUBJECTS};
use bilibi_module_registry::contributions::{
    Access, CompanyScope, Download, Method, PageWrites, Pending, Route, Seed, SeedAction, SeedMode,
    DOWNLOADS, PAGE_WRITES, ROUTES, SEEDS,
};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_picklists::{PickList, Start};
use bilibi_records::{Holder, RecordKind, RecordOwner};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, Validator, ValueType, DEFINITIONS};
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
use serde_json::{json, Value};

/// See metadata for documents with an explicit user or current-workplace grant.
pub const VIEW: &str = "core.document.view";
/// Download files. Separate from seeing a document.
pub const DOWNLOAD: &str = "core.document.download";
/// Create, edit, revise, link and delete the documents the actor may see.
pub const MANAGE: &str = "core.document.manage";
/// Merge duplicate documents (slice 3).
pub const MERGE: &str = "core.document.merge";
const CAPABILITIES: &[&str] = &[VIEW, DOWNLOAD, MANAGE, MERGE];

pub const LIST_CATEGORY: &str = "core.document.category";
pub const LIST_SUB_CATEGORY: &str = "core.document.sub_category";
pub const LIST_TYPE: &str = "core.document.type";
pub const LIST_STATUS: &str = "core.document.status";
/// The status a new document takes when the tenant's list has no default.
pub(crate) const ACTIVE: &str = "active";

/// The tenant's upload limit in bytes, never above `artifacts.max_bytes`.
pub const MAX_UPLOAD_BYTES: &str = "core.document.max_upload_bytes";
/// The file types the tenant accepts, from `FILE_TYPES`.
pub const ALLOWED_TYPES: &str = "core.document.allowed_types";

macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! { Contribution::new(MEDIA_SUBJECTS, crate::files::MEDIA_SUBJECT, provenance!(), &MediaSubject { owner: &crate::files::FILES }) }

// `view` and `manage` are the Platform's verbs; the other three are ours.
inventory::submit! { Contribution::new(VOCABULARY, MODULE, provenance!(), &VocabularyContribution {
    domains: &["core"], verbs: &["download", "merge"],
    capabilities: &[C::tenant(VIEW), C::tenant(DOWNLOAD), C::tenant(MANAGE), C::tenant(MERGE)],
    roles: &[RoleDefinition { code: "tenant_owner", name: "Tenant Owner", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: CAPABILITIES }],
    system_principals: &[],
}) }

// Operators curate these on the pick-lists page.
bilibi_picklists::declare!(
    "core/document",
    PickList::new(
        LIST_STATUS,
        "Document status",
        &[
            Start::new("draft", "Draft"),
            Start::new(ACTIVE, "Active"),
            Start::new("archived", "Archived"),
        ]
    )
    .default(ACTIVE)
);
bilibi_picklists::declare!(
    "core/document",
    PickList::new(
        LIST_CATEGORY,
        "Document category",
        &[
            Start::new("legal", "Legal"),
            Start::new("sales", "Sales"),
            Start::new("marketing", "Marketing"),
            Start::new("support", "Support"),
            Start::new("other", "Other"),
        ]
    )
);
bilibi_picklists::declare!(
    "core/document",
    PickList::new(
        LIST_SUB_CATEGORY,
        "Document sub-category",
        &[
            Start::new("contract", "Contract").under(&["legal"]),
            Start::new("policy", "Policy").under(&["legal"]),
            Start::new("proposal", "Proposal").under(&["sales"]),
            Start::new("quote", "Quote").under(&["sales"]),
            Start::new("brochure", "Brochure").under(&["marketing"]),
            Start::new("guide", "Guide").under(&["support"]),
        ]
    )
    .under(LIST_CATEGORY)
);
bilibi_picklists::declare!(
    "core/document",
    PickList::new(
        LIST_TYPE,
        "Document type",
        &[
            Start::new("agreement", "Agreement"),
            Start::new("non_disclosure", "Non-disclosure agreement"),
            Start::new("brochure", "Brochure"),
            Start::new("form", "Form"),
            Start::new("report", "Report"),
            Start::new("other", "Other"),
        ]
    )
);
inventory::submit! { Contribution::new(SEEDS, "core/document.picklists", provenance!(), &Seed {
    version: "core-document-picklists-v1", mode: SeedMode::Production,
    action: SeedAction::Service { service: bilibi_picklists::SEEDS_SERVICE, operation: "reconcile" },
}) }

fn allowed_types() -> Value {
    let mut types: Vec<&str> = crate::FILE_TYPES.iter().map(|(_, t)| *t).collect();
    types.dedup();
    json!(types)
}
fn known_types(value: &Value) -> bool {
    value.as_array().is_some_and(|types| {
        types
            .iter()
            .all(|t| t.as_str().is_some_and(crate::model::known_type))
    })
}
inventory::submit! { Contribution::new(DEFINITIONS, MAX_UPLOAD_BYTES, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global, ScopeKind::Tenant], default: || json!(25 * 1024 * 1024),
    nullable: false, encrypted: false, minimum: Some(1.0), maximum: Some(512.0 * 1024.0 * 1024.0), validator: None,
    editor: Some(Editor {
        group: "Documents",
        label: "Largest document file (bytes)",
        help: "The largest file a document revision may have. The server's private document limit still applies.",
        capability: None,
        reveal_duration_ms: 5_000,
    }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, ALLOWED_TYPES, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Array, scopes: &[ScopeKind::Global, ScopeKind::Tenant], default: allowed_types,
    nullable: false, encrypted: false, minimum: None, maximum: None,
    validator: Some(Validator { accepts: known_types, message: "Name only file types documents support, such as application/pdf" }),
    editor: Some(Editor {
        group: "Documents",
        label: "Document file types",
        help: "The file types documents accept, as a list of content types.",
        capability: None,
        reveal_duration_ms: 5_000,
    }),
})) }

// Grid and Base Records see only explicit user grants. Workplace grants need
// the live selected-company proof carried by the document facade, which Grid
// does not receive; omitting those rows is safe, while treating membership as
// access would expose restricted documents.
static GRID: GridContribution = GridContribution {
    record_fields: &[],
    tables: &[
        TableDecl::new(
            "documents",
            "Documents",
            VIEW,
            Source::tenant("SELECT d.id,d.company_id,d.name,d.publish_date,d.source,d.external_url,d.category,d.sub_category,d.document_type,d.status,d.revision,d.last_revision_at,d.created_by,d.created_at,d.updated_at FROM core_documents d WHERE d.tenant_id=$1 AND d.deleting_at IS NULL AND EXISTS (SELECT 1 FROM core_document_access a WHERE a.tenant_id=d.tenant_id AND a.document_id=d.id AND a.principal_type='user' AND a.principal_id=$2 AND a.can_view)"),
            "id",
            &[
                Field::new("id", "ID", FieldType::Integer).protected(),
                Field::new("company_id", "Filing company ID", FieldType::Integer).protected(),
                Field::new("name", "Name", FieldType::String).protected(),
                Field::new("publish_date", "Publication date", FieldType::Date).protected(),
                Field::choice("source", "Source", &["internal", "external"]).protected(),
                Field::new("external_url", "Link", FieldType::String).protected(),
                Field::new("category", "Category", FieldType::String).protected(),
                Field::new("sub_category", "Sub-category", FieldType::String).protected(),
                Field::new("document_type", "Type", FieldType::String).protected(),
                Field::new("status", "Status", FieldType::String).protected(),
                Field::new("revision", "Revision", FieldType::Integer).protected(),
                Field::new("last_revision_at", "Last revision", FieldType::DateTime).protected(),
                Field::new("created_by", "Created by account ID", FieldType::Integer).protected(),
                Field::new("created_at", "Created", FieldType::DateTime).protected(),
                Field::new("updated_at", "Updated", FieldType::DateTime).protected(),
            ],
        ).label_field("name").time_field("publish_date").record_types(&[RECORD]),
        TableDecl::new(
            "document_revisions",
            "Document revisions",
            VIEW,
            Source::tenant("SELECT r.id,r.document_id,r.revision,r.name,r.file_name,r.content_type,r.byte_size,r.external_url,r.change_log,r.scan_status,r.created_by,r.created_at FROM core_document_revisions r JOIN core_documents d ON d.tenant_id=r.tenant_id AND d.id=r.document_id AND d.deleting_at IS NULL JOIN core_document_access a ON a.tenant_id=r.tenant_id AND a.document_id=r.document_id AND a.principal_type='user' AND a.principal_id=$2 AND a.can_view WHERE r.tenant_id=$1"),
            "id",
            &[
                Field::new("id", "ID", FieldType::Integer).protected(),
                Field::new("document_id", "Document ID", FieldType::Integer).protected(),
                Field::new("revision", "Revision", FieldType::Integer).protected(),
                Field::new("name", "Name", FieldType::String).protected(),
                Field::new("file_name", "File name", FieldType::String).protected(),
                Field::new("content_type", "Content type", FieldType::String).protected(),
                Field::new("byte_size", "Size (bytes)", FieldType::Integer).protected(),
                Field::new("external_url", "Link", FieldType::String).protected(),
                Field::new("change_log", "Change log", FieldType::String).protected(),
                Field::choice("scan_status", "Scan status", &["not_scanned", "clean", "quarantined"]).protected(),
                Field::new("created_by", "Created by account ID", FieldType::Integer).protected(),
                Field::new("created_at", "Created", FieldType::DateTime).protected(),
            ],
        ).label_field("name").time_field("created_at").record_types(&[crate::REVISION_RECORD]),
        TableDecl::new(
            "document_links",
            "Document links",
            VIEW,
            Source::tenant("SELECT l.id,l.document_id,l.record_kind,l.record_id,l.linked_by,l.linked_at FROM core_document_links l JOIN core_documents d ON d.tenant_id=l.tenant_id AND d.id=l.document_id AND d.deleting_at IS NULL JOIN core_document_access a ON a.tenant_id=l.tenant_id AND a.document_id=l.document_id AND a.principal_type='user' AND a.principal_id=$2 AND a.can_view WHERE l.tenant_id=$1"),
            "id",
            &[
                Field::new("id", "ID", FieldType::Integer).protected(),
                Field::new("document_id", "Document ID", FieldType::Integer).protected(),
                Field::new("record_kind", "Related record kind", FieldType::String).protected(),
                Field::new("record_id", "Related record", FieldType::String).protected(),
                Field::new("linked_by", "Linked by account ID", FieldType::Integer).protected(),
                Field::new("linked_at", "Linked", FieldType::DateTime).protected(),
            ],
        ).time_field("linked_at"),
    ],
    links: &[
        LinkDecl::on("revisions", "Revisions", LinkKind::Many, "documents", "id", "document_revisions", "document_id"),
        LinkDecl::on("links", "Related records", LinkKind::Many, "documents", "id", "document_links", "document_id"),
    ],
};
bilibi_grid::contribute!("core/document", "core/document.grid", &GRID);

// Other modules may point at a document. The owner checks explicit access.
pub struct DocumentRecords;
pub static RECORDS: DocumentRecords = DocumentRecords;
impl RecordOwner for DocumentRecords {
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
            match crate::store::filed_proof(client, scope, id).await {
                Ok(()) => Ok(A::Readable),
                Err(Error::NotFound) => Ok(A::NotFound),
                Err(Error::Forbidden) => Ok(A::Forbidden),
                Err(error) => Err(error),
            }
        })
    }
}
bilibi_records::declare!(
    "core/document",
    RecordKind::new(RECORD, "documents", &RECORDS).page("/documents/{id}")
);
// The records each document links to, so `referrers` counts documents per
// record.
bilibi_records::holder!(
    "core/document",
    Holder {
        table: "document_links",
        kind_field: "record_kind",
        id_field: "record_id"
    }
);

// Pages: the list and a document's record page.
inventory::submit! { Contribution::new(ROUTES, "core/document.list", provenance!(), &Route {
    company_scope: CompanyScope::Company,
    method: Method::Get, path: "/documents", service: crate::web::SERVICE, operation: "list",
    access: Access::Capability(VIEW), framed: true,
}) }
inventory::submit! { Contribution::new(ROUTES, "core/document.detail", provenance!(), &Route {
    company_scope: CompanyScope::Company,
    method: Method::Get, path: "/documents/{id}", service: crate::web::SERVICE, operation: "detail",
    access: Access::Capability(VIEW), framed: true,
}) }

// Downloads: a revision's file, read through base/artifacts on every
// request, and the list's metadata as CSV.
inventory::submit! { Contribution::new(DOWNLOADS, "core/document.file", provenance!(), &Download {
    path: "/documents/{id}/revisions/{revision}/file", service: crate::web::FILES, operation: "file",
    access: Access::Capability(DOWNLOAD),
}) }
inventory::submit! { Contribution::new(DOWNLOADS, "core/document.export", provenance!(), &Download {
    path: "/documents/export.csv", service: crate::web::FILES, operation: "export",
    access: Access::Capability(VIEW),
}) }

bilibi_menu::contribute!(
    "core/document",
    bilibi_menu::MenuItem::page("core.document", "Documents", "/documents")
        .company_scoped()
        .icon("document-text")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(50)
);

// Every write on the pages and panels calls the contract agents use (ADR 01).
// Receiving a file is the one gap: it stores the bytes with `store_file`.
inventory::submit! { Contribution::new(PAGE_WRITES, "core/document.pages", provenance!(), &PageWrites {
    service: crate::web::SERVICE,
    contracts: &[
        ("create", "core.document.create"),
        ("save_field", "core.document.update"),
        ("add_revision", "core.document.add_revision"),
        ("link", "core.document.link"),
        ("unlink", "core.document.unlink"),
        ("delete", "core.document.delete"),
        ("share", "core.document.share"),
        ("unshare", "core.document.unshare"),
    ],
    pending: Pending::Events { events: &["upload"], reason: "Receiving a file stores it as a private artifact: Artifacts takes bytes outside a contract, whose input is bounded at 1 MiB. Agent-supplied files come later (issue 133)." },
}) }
