# Documents

`core/document` is the shared library for named business documents, their revisions and the records they relate to.

## What this module owns

- One document record with a name, filing company, publication date, category, optional subcategory, type and status.
- Numbered revisions with their own date, change log and internal file or external HTTPS link.
- Links to registered records. A link helps people find an already-authorized document; it never grants access.
- Explicit user grants for view, download, manage and share permissions.
- Personal favourite and follow marks: a person's own flags, never shared. A follower is notified of a new revision while they can still see the document.
- Library actions: quick add, find duplicates, copy, merge and delete one or many.
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
- **Copy:** `copy_file` stores a private duplicate of the latest file (bytes cannot go through a contract, like `store_file`), then `copy` creates a new document named "Copy of …" with the same publish date, category, sub-category and type, the list's default status, and one revision whose change log is "Document created". A link is copied without a file. Links, grants and marks are not copied: only the person who copies can see the copy until they share it.
- **Merge:** `merge` folds one or more duplicates into a survivor. Each source's revisions are renumbered after the survivor's latest with the change log saying where they came from, so no file is lost; links, marks and the explicit grants of active accounts are combined (permissions OR together); each source is removed along with its expiry reminder task (the survivor keeps its own date and reminder) and `base/records` redirects references to it. It needs `core.document.merge`, `core.document.manage` and the explicit manage and share grants on every document, because merging unites who may read them. The documents must all keep files or all keep links, and be filed under one company. A source revision that is retained or under legal hold keeps its document, number, change log and file (ADR 23), so the merge is refused, in the preview and again under the commit's locks (`Artifacts::ensure_unprotected`).

## Quick add

- **Quick add** (`quick_create`, with `core.document.manage`) files a document in one step, from the list header and from a record's Documents panel. One form takes a file or an https link, an optional name, a category and a type, and one **Add document** receives the file, keeps it and creates the document through `core.document.create`.
- The name is the file's name when left blank (a link needs one). The category and type start at the pick-lists' defaults (`other` out of the box; an operator changes or clears them on the pick-lists page) and can be changed in the form. Status, publish date and the rest take the same defaults as any create.
- From the panel, or from the list filtered to one record (`related=<kind>:<id>`), the new document is related to that record in the same write. The New document dialog on a filtered list does the same.
- A refusal (size, type, scan, a missing name) stays in the form with the file kept, so Add document can be pressed again. A file another document you may see already holds is filed anyway; the message says how many hold it.
- Nothing else changes: the New document dialog stays the place for a publish date, status, sub-category and an arbitrary related record.

## Preview

- A row's **Preview** icon (with `core.document.download`) opens the document's latest file in place, in a dialog with a Download link beside it, for a revision whose type is a PNG, JPEG, GIF or WebP picture, a PDF or plain text. Other types (Office files, CSV, a link) show no icon: the browser cannot show them in place.
- The icon shows only for a document the reader holds an unended download grant on. The dialog frames `/documents/{id}/revisions/{revision}/preview`, a download route with the same `core.document.download` capability that calls `Documents::read_file`: the same read, the same grant check and the same `artifacts.read` audit as the file download, never a second path to the bytes. It answers inline only for the types above (anything else is not found), with the download's response policy: no scripts, framing by this origin only, `nosniff` and `no-store`.
- The page reads nothing itself, so a grant ended or a file quarantined after the list drew shows the frame the same refusal a download gets.

## Files and scanning

`store_file` validates the configured size and file types, stages the upload through `base/media`, and waits for a clean malware result before copying it into private `base/artifacts`. Pending, infected, failed or unscanned content is refused and never becomes a readable revision.

Internal revisions store one Artifact ID in `core_document_revisions`; external revisions store one HTTPS link. The file ledger records reservations, stored files and attached files without recording content or storage paths.

## Retention-safe deletion

Deleting a document first checks every revision's file. If any revision is retained or under legal hold, `preview_delete` and `delete` refuse with `Retained` or `LegalHold` before anything changes (the check reads the Artifacts rows `FOR SHARE` inside the commit, so a hold placed meanwhile waits). The document stays readable with its complete revision history.

Otherwise `delete` marks the document as deleting while preserving the document, every revision, every file-ledger row, every link, every mark and every access grant. After that transaction commits, `Artifacts::delete_batch` locks and checks the complete revision set again before it tombstones any file, and metadata is removed only after every Artifact accepts deletion and every ledger row is safely removed. Repeating `delete` at the current version retries an interrupted byte or metadata finalization; a document already being deleted skips the first check, because it is unreadable and its retry is the batch check.

### Delete from the list

- Every row has a **Delete** icon, and a tick at the start of the row opens the selection bar's **Delete…**, both only for a reader with `core.document.manage`. A row shows its icon only for a document the reader holds an unended manage grant on. Ticks follow the list's selection: by id, across pages, or "every row the filters match" less those unticked, and they clear when the filters or sort change.
- A confirmation says how many documents go and that each leaves every record it relates to. Confirm runs `core.document.delete` once per document at the version it reads then, each its own reviewed write, so one refusal never undoes the others. At most 100 documents go at once (the dialog refuses more, and says to narrow the filters or tick fewer).
- One row's Delete reports with a flash ("Document deleted.", or the refusal's reason). Several documents, or any refusal, open a **Delete result**: how many were deleted and, for each document kept, its name and why (retained, under legal hold, no manage grant, changed since, gone). The same refusals come back from `core.document.delete` to an agent.

## Audit trail

Every action that changes or reads a document leaves audited evidence, so there is no unrecorded way in. A change is a `base/audit` mutation written in the action's own transaction; a file read is an Artifacts action. Refused and failed actions write nothing to the document.

| Action | Evidence |
| --- | --- |
| Create, Quick add, Copy | `core.document.document` created, `core.document.revision` created, the creator's `core.document.access`, a `core.document.link` per related record |
| Update, in-place edit, expiry date, custom field | `core.document.document` updated (a reminder task is Core Activity's own change) |
| New revision | `core.document.revision` created, `core.document.document` updated |
| Link, Unlink | `core.document.link` created or deleted |
| Share, Unshare, Access until | `core.document.access` created, updated or deleted |
| Email them a link | `core.document.email` created, then updated when it is sent, failed, suppressed or rate limited |
| Favourite, Follow | `core.document.mark` created or updated (the reader's own flags) |
| Merge | the survivor's revisions, links, access and marks created or updated; every source's rows deleted |
| Delete, one or from the list | `core.document.document` updated (marked as deleting), then every row of the document deleted once the files are gone |
| Upload, discard a stored file | `core.document.file` ledger changes |
| Download, Preview | an `artifacts.read` action on the file, `owner_id` `core.document`, with the actor |

Every row of a document's parts carries its `document_id`, so one query finds the whole trail. Run it on **Administration › System › Database Queries** with the tenant and document IDs; **Audit Log › Data Mutations** lists the same mutations tenant-wide (search `core.document`).

```sql
SELECT id, occurred_at, auditable_type, event, actor_type, actor_id
FROM base_audit_mutations
WHERE tenant_id = 1 AND auditable_type LIKE 'core.document.%'
  AND ((auditable_type = 'core.document.document' AND auditable_id = '42')
    OR new_values ->> 'document_id' = '42' OR old_values ->> 'document_id' = '42')
ORDER BY id;

SELECT id, occurred_at, event, actor_type, actor_id
FROM base_audit_actions
WHERE tenant_id = 1 AND event LIKE 'artifacts.%' AND payload ->> 'owner_id' = 'core.document'
  AND payload ->> 'artifact_id' IN (SELECT artifact_id FROM core_document_revisions WHERE tenant_id = 1 AND document_id = 42)
ORDER BY id;
```

The coverage check is `apps/core/document/examples/audit-coverage.rs`. On a disposable database that is migrated and seeded (`bilibi-web migrate`, then `bilibi-web seed-production`, which carries the pick-lists) it performs each action above through the facade, then fails unless the action wrote at least one audited row and the first query above finds every document row it wrote:

```sh
BILIBI_DOCUMENT_AUDIT=disposable BILIBI_DOCUMENT_AUDIT_ROOT=/an/empty/folder cargo run -p bilibi-document --example audit-coverage --locked
```

A new document action belongs in that example in the same change.

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
| `preview_delete`, `delete` | Review (refusing a retained or held revision) and start retention-safe deletion |
| `mark`, `preview_mark`, `marks` | The reader's own favourite and follow marks |
| `duplicates`, `duplicates_of_file` | Look-alikes the reader may see |
| `copy_file`, `preview_copy`, `copy` | A copy with its latest revision only |
| `preview_merge`, `merge`, `may_merge` | Fold duplicates into a survivor |
| `get`, `list`, `revisions`, `links`, `file` | Permission-checked reads |

All writes use the caller's `MutationTransaction`, return audited changes and recheck authority inside the transaction. Inputs take a sealed `Scope`; callers never supply tenant or actor IDs.

## Contributions

- Capabilities: `core.document.view`, `core.document.download`, `core.document.manage` and `core.document.merge`.
- Pick-lists: `core.document.category`, `core.document.sub_category`, `core.document.type`, and `core.document.status`; category, type and status each start with a default (`other`, `other`, `active`).
- Settings: `core.document.max_upload_bytes`, `core.document.allowed_types` and `core.document.expiry_reminder_days`.
- Custom field record type `document`.
- Record kind: `core.document.document`; holder table `document_links`.
- Routes: `/documents`, `/documents/{id}`, revision downloads and previews, and `/documents/export.csv`.
- Menu item: `Documents`, under Core.
- Media subject: `core.document.upload`.
- Record panels (embeds), the one Documents panel of each record kind:

| Embed id | Slot | Context | Access |
| --- | --- | --- | --- |
| `company.documents` | `company.documents` | `Record { kind: "core/company" }` | `core.document.view` |
| `person.documents` | `person.documents` | `Record { kind: "core/person" }` | `core.document.view` |
| `document.record-documents` | `record.documents` | `Record { kind: "*" }` | `core.document.view` |

Core company and Core person discover their panel by kind and pass `company_id` or `person_id`. A Domain record page (a case, an opportunity) renders the `record.documents` slot with `kind` and `id` in the panel path; it never registers a Documents panel of its own. The panel shows at most 10 documents linked to the record, newest first (name, category, revision, publish date), and **View all** opens `/documents?related=<kind>:<id>`. **Quick add** (with `core.document.manage`) files a document related to the record without leaving the page, and **Add document** opens the same filtered list, where New document files and relates it; the panel writes nothing else. The panel reads through `Documents::list`: a document with no explicit grant for the reader is absent, with no hidden count.

Run `core.document.list`, `.get`, `.access`, `.duplicates`, `.create`, `.update`, `.add_revision`, `.link`, `.unlink`, `.share` (with `until` and `email`), `.unshare`, `.mark`, `.copy`, `.merge` and `.delete` through the agent API. Pages call the same reviewed write operations; uploading bytes (and the `copy_file` step of Copy, which then calls `core.document.copy`) is the declared page-write exception because operation input is bounded; an agent copies a file document by naming a file the page stored.

## Database records

The module owns `core_documents`, `core_document_revisions`, `core_document_links`, `core_document_access`, `core_document_marks`, `core_document_files` and `core_document_emails`; documents also hold `expires_on`, the reminder task id and `custom_fields`, and a grant its `expires_at`. Every query binds `tenant_id`. Applied migrations are immutable.
