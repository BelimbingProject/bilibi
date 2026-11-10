# Documents

`core/document` is the shared library for named business documents, their revisions and the records they relate to.

## What this module owns

- One document record with a name, filing company, publication date, category, optional subcategory, type, status, optional tenant-unique integration sync ID and optional relation to a document the editor may view (supersedes or replaces).
- Numbered revisions with their own date, change log, optional label, optional reference to an earlier revision of that document, and internal file or external HTTPS link.
- Links to registered records. A link helps people find an already-authorized document; it never grants access.
- Explicit user grants for view, download, manage and share permissions.
- Personal favourite and follow marks: a person's own flags, never shared. A follower is notified of a new revision while they can still see the document.
- Library actions: find duplicates, copy and merge.
- The file ledger and the `core.document` Artifact owner.
- One Documents list, one document record page, metadata CSV export, file downloads and agent operations.
- The Documents panel on Core company and Core person pages, and on any Domain record page that renders the `record.documents` slot.

Base owns private bytes, malware scanning, retention and legal holds. Domains own their approval, signature, employment, purchasing, fulfilment and customer-portal workflows.

## Access

Every metadata read starts with `core.document.view` and an explicit grant for the signed-in user. A file download also needs `core.document.download` and the grant's download permission. Changes need `core.document.manage` and the grant's manage permission. Access changes need `core.document.manage` and the grant's share permission.

Creating a document gives its creator view, download, manage and share permissions. Sharing accepts only an active account; there are no whole-company, role or team grants. The creator's own grant cannot be removed through `unshare`.

Links, filing-company membership and access to a linked record do not expose a document. List results, record reads, related-record filters, search, CSV export, revision metadata and Artifact reads all apply the same explicit grant.

## Sharing by email, and when access ends

- **Share** gives one active account an explicit grant. With **Access until** (`until`, the last day, UTC) the grant stops working after that day: every read, download and edit rechecks it, so an ended grant hides the document like no grant at all. Your own access never ends this way.
- **Email them a link** (`email`) mails the grantee a link to `/documents/{id}`. The link carries no token and no file: it opens only for that account, only while the grant lasts, and the sharer needs the share permission as before. Emailing a name out is a disclosure, so it is durable, limited and audited:
  - The request is one audited `core_document_emails` row (`core.document.email` changes, ids and a reason code only, never an address, name or link), written in the share's own transaction, with a durable Queue job (`core/document-email`) for a queued one.
  - Limits, counted under a lock: `core.document.email_actor_per_hour` (20), `core.document.email_recipient_per_day` for one person and one document (3) and `core.document.email_tenant_per_hour` (200). Over a limit the grant is still saved and the request is recorded `rate_limited`; the page and `core.document.share` say so.
  - Delivery rereads everything: the sender may still share the document, the document is not being deleted, and the recipient still holds an unended view grant and the view capability. Otherwise nothing is sent and the row settles `suppressed`. The message carries the document's name as it is *now*.
  - Outcomes `sent`, `failed` (after five attempts) and `suppressed` are audited. A crash between sending and recording sends once more.
- Still named people only: an email address gives no access by itself, there are no public links, and merge carries a source's live grants (an ended one is dropped; a grant with no end beats one with an end).

## Expiry and its reminder

- A document may have an **expiry date** (`expires_on`), edited in place on its page. Past it, the page and the list show it as expired whatever its status says (the status list stays the operator's); the list filters **Expired** and **Expiring in 30 days**.
- Setting the date creates one Core Activity task, "Document <id> expiry", related to the document and due `core.document.expiry_reminder_days` (default 14, Settings › Documents) before the date at 09:00 in the actor's zone. It is private to the person who set the date (`expiry_user_id`) and rings their bell through Core Activity's reminder sweep; there is no second reminder system. The task is made only when that person holds `core.activity.manage`; otherwise the date is saved without one.
- The task names no document and no date, so a bell or email it sends discloses nothing to someone who has lost access; its related-record pill names the document only to a reader who may still view it.
- Changing or clearing the date, deleting or merging the document, or removing the reminder person's access removes the old task in the same transaction through `Activities::delete_related`, whoever created it. A task that cannot be removed fails the change.

## Custom fields

Documents is a `base/custom_fields` record type (`document`): an administrator adds text, number, date and pick-list fields without a release (ADR 21). Values live in `core_documents.custom_fields`, checked with `CustomFields::validate_locked` in the same write as every other change.

- **Page:** the active fields show as facts that edit in place (`save_custom_field`, through `core.document.update`).
- **Agents:** `core.document.create` and `.update` (inside `patch`) take `custom` as `{key, value}` pairs; `core.document.get` returns `custom` as `{key, label, type, value}` for the active fields. Find keys with `base.custom_fields.list`.
- **Copy** carries the values over; **merge** keeps the survivor's.
- Not yet: a custom field as a list column or filter, and a field in the New document dialog.

## Library actions

- **Marks:** `mark` sets the reader's own favourite and follow flags on a document they may see. The list filters on them (`show=favourites` or `show=following`). Deleting or merging a document moves or removes its marks.
- **Duplicates:** `duplicates` lists other documents the reader may see that share a file hash with any revision, the same latest link, or the same name. A document the reader has no grant on is never named or counted. After an upload the New document dialog warns with `duplicates_of_file`.
- **Copy:** The sync ID and document/revision lineage are cleared on a copy. `copy_file` stores a private duplicate of the latest file (bytes cannot go through a contract, like `store_file`), then `copy` creates a new document named "Copy of …" with the same publish date, category, sub-category and type, the list's default status, and one revision whose change log is "Document created". A link is copied without a file. Links, grants and marks are not copied: only the person who copies can see the copy until they share it.
- **Merge:** A source with an integration sync ID must have it cleared before merging. Incoming document relations to a source point to the survivor; a survivor relation to a source is cleared. Revision replacement IDs remain stable when revisions move. `merge` folds one or more duplicates into a survivor. Each source's revisions are renumbered after the survivor's latest with the change log saying where they came from, so no file is lost; links, marks and the explicit grants of active accounts are combined (permissions OR together); each source is removed along with its expiry reminder task (the survivor keeps its own date and reminder) and `base/records` redirects references to it. It needs `core.document.merge`, `core.document.manage` and the explicit manage and share grants on every document, because merging unites who may read them. The documents must all keep files or all keep links, and be filed under one company. A source revision that is retained or under legal hold keeps its document, number, change log and file (ADR 23), so the merge is refused, in the preview and again under the commit's locks (`Artifacts::ensure_unprotected`).

## Files and scanning

`store_file` validates the configured size and file types, stages the upload through `base/media`, and waits for a clean malware result before copying it into private `base/artifacts`. Pending, infected, failed or unscanned content is refused and never becomes a readable revision.

Internal revisions store one Artifact ID in `core_document_revisions`; external revisions store one HTTPS link. The file ledger records reservations, stored files and attached files without recording content or storage paths.

## Retention-safe deletion

Deleting a document first marks it as deleting while preserving the document, every revision, every file-ledger row, every link, every mark and every access grant. After that transaction commits, `Artifacts::delete_batch` locks and checks the complete revision set before it tombstones any file. Once the files are accepted for deletion, finalization clears incoming document relations with audited changes before removing metadata.

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
| `mark`, `preview_mark`, `marks` | The reader's own favourite and follow marks |
| `duplicates`, `duplicates_of_file` | Look-alikes the reader may see |
| `copy_file`, `preview_copy`, `copy` | A copy with its latest revision only |
| `preview_merge`, `merge`, `may_merge` | Fold duplicates into a survivor |
| `get`, `list`, `revisions`, `links`, `file` | Permission-checked reads |

All writes use the caller's `MutationTransaction`, return audited changes and recheck authority inside the transaction. Inputs take a sealed `Scope`; callers never supply tenant or actor IDs.

## Contributions

- Capabilities: `core.document.view`, `core.document.download`, `core.document.manage` and `core.document.merge`.
- Pick-lists: `core.document.category`, `core.document.sub_category`, `core.document.type`, and `core.document.status`.
- Settings: `core.document.max_upload_bytes`, `core.document.allowed_types` and `core.document.expiry_reminder_days`.
- Custom field record type `document`.
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

Run `core.document.list`, `.get`, `.access`, `.duplicates`, `.create`, `.update`, `.add_revision`, `.link`, `.unlink`, `.share` (with `until` and `email`), `.unshare`, `.mark`, `.copy`, `.merge` and `.delete` through the agent API. Pages call the same reviewed write operations; uploading bytes (and the `copy_file` step of Copy, which then calls `core.document.copy`) is the declared page-write exception because operation input is bounded; an agent copies a file document by naming a file the page stored.

## Database records

A sync ID is exact-match unique within the tenant when set. The Documents search matches it exactly as well as matching name text. Creating or updating a relation requires view access to the target; a related document's grant is still needed to open it. A revision may replace only an earlier revision of its own document. The metadata CSV includes the sync ID and document relation, while revision fields appear in get and on the record page.

The module owns `core_documents`, `core_document_revisions`, `core_document_links`, `core_document_access`, `core_document_marks`, `core_document_files` and `core_document_emails`; documents also hold `expires_on`, the reminder task id and `custom_fields`, and a grant its `expires_at`. Every query binds `tenant_id`. Applied migrations are immutable.
