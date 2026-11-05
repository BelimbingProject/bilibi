# Documents

`core/document` is the shared library for named business documents, their revisions and the records they relate to.

## What this module owns

- One document record with a name, filing company, publication date, category, optional subcategory, type and status.
- Numbered revisions with their own date, change log and internal file or external HTTPS link.
- Links to registered records. A link helps people find an already-authorized document; it never grants access.
- Explicit user grants for view, download, manage and share permissions.
- Personal favourite and follow marks.
- The file ledger and the `core.document` Artifact owner.
- One Documents list, one document record page, metadata CSV export, file downloads and agent operations.
- The Documents panel on Core company and Core person pages, and on any Domain record page that renders the `record.documents` slot.

Base owns private bytes, malware scanning, retention and legal holds. Domains own their approval, signature, employment, purchasing, fulfilment and customer-portal workflows.

## Access

Every metadata read starts with `core.document.view` and an explicit grant for the signed-in user. A file download also needs `core.document.download` and the grant's download permission. Changes need `core.document.manage` and the grant's manage permission. Access changes need `core.document.manage` and the grant's share permission.

Creating a document gives its creator view, download, manage and share permissions. Sharing accepts only an active account; there are no whole-company, role or team grants. The creator's own grant cannot be removed through `unshare`.

Links, filing-company membership and access to a linked record do not expose a document. List results, record reads, related-record filters, search, CSV export, revision metadata and Artifact reads all apply the same explicit grant.

## Files and scanning

`store_file` validates the configured size and file types, stages the upload through `base/media`, and waits for a clean malware result before copying it into private `base/artifacts`. Pending, infected, failed or unscanned content is refused and never becomes a readable revision.

Internal revisions store one Artifact ID in `core_document_revisions`; external revisions store one HTTPS link. The file ledger records reservations, stored files and attached files without recording content or storage paths.

## Retention-safe deletion

Deleting a document first marks it as deleting while preserving the document, every revision, every file-ledger row, every link, every mark and every access grant. After that transaction commits, `Artifacts::delete_batch` locks and checks the complete revision set before it tombstones any file.

If any revision is retained or under legal hold, the whole batch is refused and the document with its full revision history remains. Metadata is removed only after every Artifact accepts deletion and every ledger row is safely removed. Repeating `delete` at the current version retries an interrupted byte or metadata finalization.

## Public facade

`Documents` is the only public service. Its main calls are:

| Call | Result |
| --- | --- |
| `store_file` | A scanned private file ready for a revision |
| `preview_create`, `create` | A checked draft, then a document with revision 1 |
| `preview_update`, `update` | A checked field patch, then the updated document |
| `preview_revision`, `add_revision` | A checked revision, then the stored revision |
| `link`, `unlink` | Add or remove a record reference |
| `access`, `preview_share`, `share`, `preview_unshare`, `unshare` | Read and change explicit access |
| `preview_delete`, `delete` | Review and start retention-safe deletion |
| `get`, `list`, `revisions`, `links`, `file` | Permission-checked reads |

All writes use the caller's `MutationTransaction`, return audited changes and recheck authority inside the transaction. Inputs take a sealed `Scope`; callers never supply tenant or actor IDs.

## Contributions

- Capabilities: `core.document.view`, `core.document.download`, `core.document.manage`, and the reserved future capability `core.document.merge`.
- Pick-lists: `core.document.category`, `core.document.sub_category`, `core.document.type`, and `core.document.status`.
- Settings: `core.document.max_upload_bytes` and `core.document.allowed_types`.
- Record kind: `core.document.document`; holder table `document_links`.
- Routes: `/documents`, `/documents/{id}`, revision downloads and `/documents/export.csv`.
- Menu item: `Documents`, under Core.
- Media subject: `core.document.upload`.
- Record panels (embeds), the one Documents panel of each record kind:

| Embed id | Slot | Context | Access |
| --- | --- | --- | --- |
| `company.documents` | `company.documents` | `Record { kind: "core/company" }` | `core.document.view` |
| `person.documents` | `person.documents` | `Record { kind: "core/person" }` | `core.document.view` |
| `document.record-documents` | `record.documents` | `Record { kind: "*" }` | `core.document.view` |

Core company and Core person discover their panel by kind and pass `company_id` or `person_id`. A Domain record page (a case, an opportunity) renders the `record.documents` slot with `kind` and `id` in the panel path; it never registers a Documents panel of its own. The panel shows at most 10 documents linked to the record, newest first (name, category, revision, publish date), and **View all** opens `/documents?related=<kind>:<id>`. **Add document** (with `core.document.manage`) opens the same filtered list, where New document files and relates it, so the panel writes nothing. The panel reads through `Documents::list`: a document with no explicit grant for the reader is absent, with no hidden count.

Run `core.document.list`, `.get`, `.access`, `.create`, `.update`, `.add_revision`, `.link`, `.unlink`, `.share`, `.unshare` and `.delete` through the agent API. Pages call the same reviewed write operations; uploading bytes is the declared page-write exception because operation input is bounded.

## Database records

The module owns `core_documents`, `core_document_revisions`, `core_document_links`, `core_document_access`, `core_document_marks` and `core_document_files`. Every query binds `tenant_id`. Applied migrations are immutable.
