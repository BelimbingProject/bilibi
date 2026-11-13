# Documents

`core/document` is the shared library for named business documents, their revisions and the records they relate to.

## What this module owns

- One document record with a name, filing company, publication date, category, optional subcategory, type, status, optional tenant-unique integration sync ID and optional relation to a document the editor may view (supersedes or replaces).
- Numbered revisions with their own date, change log, optional label, optional reference to an earlier revision of that document, and internal file or external HTTPS link.
- Links to registered records. A link helps people find an already-authorized document; it never grants access.
- Explicit user grants for view, download, manage and share permissions.
- Shares with portal customers: one customer Person, or a Company for every business customer who signs in for it, and the portal's own read of them (`/portal/documents` and the `portal.documents` panel).
- Personal favourite and follow marks: a person's own flags, never shared. A follower is notified of a new revision while they can still see the document.
- Library actions: quick add, find duplicates, copy, merge and delete one or many.
- The file ledger and the `core.document` Artifact owner.
- One Documents list, one document record page, metadata CSV export, file downloads and agent operations.
- The Documents panel on Core company and Core person pages, and on any Domain record page that renders the `record.documents` slot.

Base owns private bytes, malware scanning, retention and legal holds. Domains own their approval, signature, employment, purchasing, fulfilment and customer-portal workflows.

## Access

Every metadata read starts with `core.document.view` and an explicit grant for the signed-in user. A file download also needs `core.document.download` and the grant's download permission; the record page links a revision's file only for a reader who holds both. Changes need `core.document.manage` and the grant's manage permission. Access changes need `core.document.manage` and the grant's share permission.

Creating a document gives its creator view, download, manage and share permissions. Sharing accepts only an active account; there are no whole-company, role or team grants.

**A sharer gives at most what their own grant holds.** Share refuses:

- your own account: nobody changes their own grant, so a sharer cannot lift their own end date or add download;
- download or manage when your own grant lacks it;
- no end date, or a later one, when your own access ends (**Access until** must be no later than your own last day);
- the creator's account, unless you are the creator; `unshare` likewise removes the creator's grant only for the creator, and nobody removes their own.

Links, filing-company membership and access to a linked record do not expose a document. List results, record reads, related-record filters, search, CSV export, revision metadata and Artifact reads all apply the same explicit grant.

## Sharing by email, and when access ends

- **Share** gives one active account an explicit grant. With **Access until** (`until`, the last day, UTC) the grant stops working after that day: every read, download and edit rechecks it, so an ended grant hides the document like no grant at all.
- **Email them a link** (`email`) mails the grantee a link to `/documents/{id}`. The link carries no token and no file: it opens only for that account, only while the grant lasts, and the sharer needs the share permission as before. Emailing a name out is a disclosure, so it is durable, limited and audited:
  - The request is one audited `core_document_emails` row (`core.document.email` changes, ids and a reason code only, never an address, name or link), written in the share's own transaction, with a durable Queue job (`core/document-email`) for a queued one.
  - Limits, counted under a lock: `core.document.email_actor_per_hour` (20), `core.document.email_recipient_per_day` for one person and one document (3) and `core.document.email_tenant_per_hour` (200). Over a limit the grant is still saved and the request is recorded `rate_limited`; the page and `core.document.share` say so.
  - Delivery rereads everything: the sender may still share the document, the document is not being deleted, and the recipient still holds an unended view grant and the view capability. Otherwise nothing is sent and the row settles `suppressed`. The message carries the document's name as it is *now*.
  - Outcomes `sent`, `failed` (after five attempts) and `suppressed` are audited. A crash between sending and recording sends once more.
  - A job Queue ends without settling the row (the sender lost the right to share first, or it ran out of attempts) is told to the owner by Queue's ending hook, as `document-monitor` (standing grant of `core.document.email.fail`): the row settles `failed` with `cancelled_email_refused` or `discarded_…` as its reason, fenced by the row's `job_id`, so it never waits in `queued` or counts against the limits for ever. Share again with **Email them a link** to send it once more; a failed row counts for no limit.
- Still named people only: an email address gives no access by itself, there are no public links, and merge carries a source's live grants (an ended one is dropped; a grant with no end beats one with an end).

## Shared with portal customers

A customer signed in to the portal (ADR 16) sees only the documents staff shared with their own record, and downloads their latest file.

- **Who shares:** someone with `core.document.manage` and the document's share permission, as for an account grant, on the record page's **Customer sharing** card or with `core.document.customer_share`. The recipient is a contact (`person`, one customer) or a company (`company`, every business customer who signs in for it); it must be a record the sharer may see (`base/records`). Only a document that keeps a file can be shared: a link is refused.
- **Available until** (`until`, the last day, UTC) ends the share after that day. Sharing again with the same customer replaces the end. **Stop sharing** (`core.document.customer_unshare`) revokes it at once.
- **What the customer sees** (`Documents::shared_with_me`, `portal.document.view`, which every invited customer holds through `portal_customer`): documents with an unended share to their Person, or to the Company they sign in for (a private individual has none), in their tenant, not being deleted and not past their expiry date. Name, publish date, last update, the latest revision's file name and size, and when the share ends; never the filing company, grants, links, other revisions or change logs. Nothing else is listed or counted.
- **Download** (`/portal/documents/{id}/file`, `Documents::read_shared_file`): the latest revision's file, only when its scan is clean (a pending file says "Being checked"), read through `Artifacts::for_portal` so Base audits the read as the customer and the Artifacts owner rechecks the share, the scan and the document inside that read. The route opens on `portal.account.view`, so the facade decides and audits every refusal.
- **Legal hold and retention** keep a file; they never hide or show it. A held document stays shared until a share ends or is revoked, and cannot be deleted while held. Deleting a document removes its customer shares with it; a merge moves a duplicate's live shares the survivor lacks; a copy has none.
- **A Domain's portal page** (a live page) mounts the panel: a `PanelSpec` whose `embed` is `document.portal-documents` (slot `portal.documents`, `EmbedContext::Portal`) and whose path holds `panel` (its anchor) and, to narrow to its own record, `kind` and `id`, as staff record pages pass them to `record.documents`. The panel lists at most 10 shared documents related to that record, newest first, and **View all** opens `/portal/documents?related=<kind>:<id>`. The record filter only narrows what is shared with the customer; it never shows more.
- **Dates** on the portal show in UTC: a customer has no company zone.
- **Proof:** `apps/core/document/examples/customer-share-walkthrough.rs` (below).

```sh
BILIBI_DOCUMENT_CUSTOMER=disposable BILIBI_DOCUMENT_CUSTOMER_ROOT=/an/empty/folder cargo run -p bilibi-document --example customer-share-walkthrough --locked
```

On a migrated, fixture-holding, seeded disposable database (as for the audit coverage check), it shares files with an individual customer, a business customer's company and nobody, then checks that each customer lists and downloads only their own, a pending and an infected file are refused, a revoked or ended share disappears, another customer, another tenant's customer and a customer without the capability are refused, and every download and refusal is audited as the customer.

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
- **Merge:** A source with an integration sync ID must have it cleared before merging. Incoming document relations to a source point to the survivor; a survivor relation to a source is cleared. Revision replacement IDs remain stable when revisions move. `merge` folds one or more duplicates into a survivor. Each source's revisions are renumbered after the survivor's latest with the change log saying where they came from, so no file is lost; links, marks and the explicit grants of active accounts are combined (permissions OR together); each source is removed along with its expiry reminder task (the survivor keeps its own date and reminder) and `base/records` redirects references to it. It needs `core.document.merge`, `core.document.manage` and the explicit manage and share grants on every document, because merging unites who may read them. The documents must all keep files or all keep links, and be filed under one company. A source revision that is retained or under legal hold keeps its document, number, change log and file (ADR 23), so the merge is refused, in the preview and again under the commit's locks (`Artifacts::ensure_unprotected`). A pending scan on any merge participant also refuses preview and commit until it settles; the worker still targets the stable revision ID.

## Quick add

- **Quick add** (`quick_create`, with `core.document.manage`) files a document in one step, from the list header and from a record's Documents panel. One form takes a file or an https link, an optional name, a category and a type, and one **Add document** receives the file, keeps it and creates the document through `core.document.create`.
- The name is the file's name when left blank (a link needs one). The category and type start at the pick-lists' defaults (`other` out of the box; an operator changes or clears them on the pick-lists page) and can be changed in the form. Status, publish date and the rest take the same defaults as any create.
- From the panel, or from the list filtered to one record (`related=<kind>:<id>`), the new document is related to that record in the same write. The New document dialog on a filtered list does the same.
- A refusal (size, type, scan, a missing name) stays in the form with the file kept, so Add document can be pressed again. A file another document you may see already holds is filed anyway; the message says how many hold it.
- Nothing else changes: the New document dialog stays the place for a publish date, status, sub-category and an arbitrary related record.

## Preview

- A row's **Preview** icon (with `core.document.download`) opens the document's latest file in place, in a dialog with a Download link beside it, for a revision whose type is a PNG, JPEG, GIF or WebP picture, a PDF or plain text. Other types (Office files, CSV, a link) show no icon: the browser cannot show them in place.
- The icon shows only for a document the reader holds an unended download grant on. The dialog frames `/documents/{id}/revisions/{revision}/preview`, a download route like the file download that calls `Documents::read_file`: the same read, the same `core.document.download` and grant checks and the same audit as the file download, never a second path to the bytes. It answers through `FileBody::preview` (`base/ui`), which shows only those types (anything else is not found), with the download's response policy: no scripts, framing by this origin only, `nosniff` and `no-store`.
- The page reads nothing itself, so a grant ended or a file quarantined after the list drew shows the frame the same refusal a download gets.

## Files and scanning

`store_file` validates the configured size and file types and stages the upload through `base/media`. A quick clean verdict copies bytes into private `base/artifacts`; a pending verdict may become an internal revision while its bytes remain in Media and unreadable. Infected, failed and unscanned verdicts also become held revisions. The owner-issued `artifact_id` input is opaque: for a held file it is an `upload:` token, and no placeholder Artifact is stored. The ledger retains Media's public attachment ID for a held revision, never its bytes or disk path.

A declared `document-scanner` Queue job rechecks a pending attachment under the owner-scoped Media inspection grant. Only a clean verdict copies bytes to an Artifact and makes the revision downloadable; an infected, failed or unscanned verdict settles its status without publishing bytes. Queue's ending hook marks any still-pending revision failed if the job is cancelled or discarded. The uploader and current document managers receive notices when a file is held and when its verdict settles. List, record, agent and CSV views show the status; file links, preview and download work only for clean internal revisions. A direct download rechecks this state. Copying a file document requires a clean latest revision. Merge preserves each moved revision's file/status and its queued job continues to target the revision ID. Deletion waits for pending scans to settle, then removes held ledger rows after Base checks every published Artifact; Media's draft cleanup owns remaining staged bytes.

### Upload progress and resume

The New document, New revision and Quick add dialogs take the file through one resumable input (`UploadSpec::resumable`), so a large upload shows its progress and survives a dropped connection; the contract is `docs/live-pages.md` (Resumable uploads) and `apps/base/media/README.md` (Resumable uploads).

- **Progress:** the chosen file's row shows the percentage received while it arrives; Upload (Add document in Quick add) waits until every byte is there.
- **Resume:** the bytes wait on Media's staging disk (`media.upload.staging_disk`). While the New document dialog is open the list's address carries `new=1`, so the remount after a dropped connection opens it again and the file carries on from the bytes already there. After a reload, or in a dialog the remount does not reopen (New revision, Quick add), choosing the same file again continues it, until `media.upload.staging_hours` passes.
- **Limit:** the largest file is Media's staging limit (`media.max_bytes`, never below 64 MiB, at most 512 MiB); `store_file` still applies `core.document.max_upload_bytes` and `artifacts.max_bytes`. Without a staging disk the input holds the bytes in memory, at most 64 MiB, and a drop starts again.
- **Scan:** a resumed file is stored like any other: `store_file` stages it through Media, and it stays held, unreadable, until the scan calls it clean.
- **Proof:** `apps/core/document/examples/upload-resume-proof.rs` serves the real host with a fixture scanner that holds files pending until released, for a browser run of progress, resume and the hold.

Internal revisions store the ledger file ID and either a clean Artifact ID or a held Media attachment ID; external revisions store one HTTPS link. The file ledger records reservations, stored files and attached files without recording content or storage paths.

## Tenant retention and due documents

Settings › Documents offers **Keep new document files for (days)**
(`core.document.retention_days`, 0 by default). With zero, each private file
revision is kept without an expiry (`Artifacts::keep_in`) when a clean Artifact
is attached or a pending revision is promoted: it stays readable until its
document is deleted, whatever the installation's `artifacts.retention_days`
(which bounds exports, and a file stored but never attached). When a tenant chooses a positive period
(1–36500 days), every newly attached **private file revision** receives an
irreversible Base retain-until when a clean Artifact is attached or a pending
revision is promoted. The owner calls `Artifacts::retain_in` inside the same
mutation as the revision attachment or scan settlement; a failed retention
rejects that write. Changing the
setting affects future revisions only and cannot shorten an existing retain-until.
External HTTPS links have no bytes in Base Artifacts, so this file-retention
rule does not claim to retain a remote file.

The Documents list's **Show › Retention due** view is for a person with
`core.document.manage` and an explicit manage grant on each document. It shows
private documents only when *every* file revision is clean, its Base
retain-until has passed and no revision is under legal hold; the latest retain-until appears in the list
and filtered CSV/agent list output. The record page shows each revision's Base
retention and hold state to its manager. Due means the document may be considered
for an explicit reviewed deletion; nothing is deleted automatically. Base still
rechecks every revision under lock in delete and merge, so a new hold refuses
those actions even after the list was read.

## Retention-safe deletion

Deleting a document first checks every revision's file. If any revision is retained or under legal hold, `preview_delete` and `delete` refuse with `Retained` or `LegalHold` before anything changes (the check reads the Artifacts rows `FOR SHARE` inside the commit, so a hold placed meanwhile waits). The document stays readable with its complete revision history.

Otherwise `delete` marks the document as deleting while preserving the document, every revision, every file-ledger row, every link, every mark and every access grant. After that transaction commits, `Artifacts::delete_batch` locks and checks the complete revision set again before it tombstones any file, and metadata is removed only after every Artifact accepts deletion and every ledger row is safely removed. That finalization first clears incoming document relations with audited changes. Repeating `delete` at the current version retries an interrupted byte or metadata finalization; a document already being deleted skips the first check, because it is unreadable and its retry is the batch check.

### Delete from the list

- Every row has a **Delete** icon, and a tick at the start of the row opens the selection bar's **Delete…**, both only for a reader with `core.document.manage`. A row shows its icon only for a document the reader holds an unended manage grant on. Ticks follow the list's selection: by id, across pages, or "every row the filters match" less those unticked, and they clear when the filters or sort change.
- A confirmation says how many documents go and that each leaves every record it relates to. Confirm runs `core.document.delete` once per document at the version it reads then, each its own reviewed write, so one refusal never undoes the others. At most 100 documents go at once, counting rows unticked since "select all" (the dialog refuses more, and says to narrow the filters or tick fewer; a filter matching over 300 rows cannot be listed in one read and is refused the same way). A database error on one document is that document's result in the report, and the rest still go.
- One row's Delete reports with a flash ("Document deleted.", or the refusal's reason). Several documents, or any refusal, open a **Delete result**: how many were deleted and, for each document kept, its name and why (retained, under legal hold, no manage grant, changed since, gone). The same refusals come back from `core.document.delete` to an agent.

## Audit trail

Every action that changes or reads a document leaves audited evidence, so there is no unrecorded way in. A change is a `base/audit` mutation written in the action's own transaction; a file read is an Artifacts action. Refused and failed actions write nothing to the document; a refused download or preview also leaves one audit action.

| Action | Evidence |
| --- | --- |
| Create, Quick add, Copy | `core.document.document` created, `core.document.revision` created, the creator's `core.document.access`, a `core.document.link` per related record |
| Update, in-place edit, expiry date, custom field | `core.document.document` updated (a reminder task is Core Activity's own change) |
| New revision | `core.document.revision` created, `core.document.document` updated |
| Link, Unlink | `core.document.link` created or deleted |
| Share, Unshare, Access until | `core.document.access` created, updated or deleted |
| Share with a customer, Stop sharing | `core.document.customer_share` created, updated or deleted (ids and the end, never a name) |
| Email them a link | `core.document.email` created, then updated when it is sent, failed, suppressed or rate limited |
| Favourite, Follow | `core.document.mark` created or updated (the reader's own flags) |
| Merge | the survivor's revisions, links, access and marks created or updated; every source's rows deleted |
| Delete, one or from the list | `core.document.document` updated (marked as deleting), then every row of the document deleted once the files are gone |
| Upload, discard a stored file | `core.document.file` ledger changes |
| Download, Preview | an `artifacts.read` action on the file, `owner_id` `core.document`, with the actor |
| Refused download or preview | a `core.document.download_refused` action with the actor, `document_id`, `revision` and `reason`: `no_grant`, `grant_ended`, `no_download_permission`, `no_download_capability` or `no_file_access` |
| Portal download | an `artifacts.read` action as above, its actor the customer (`actor_role` `customer`) |
| Refused portal download | a `core.document.download_refused` action as the customer with `"portal": true` and `reason` `not_shared` (no share to them, another tenant's or no such document), `share_ended`, `document_expired`, `no_file`, `scan_pending`, `not_clean` or `no_portal_capability` |

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

SELECT id, occurred_at, actor_type, actor_id, payload ->> 'revision' AS revision, payload ->> 'reason' AS reason
FROM base_audit_actions
WHERE tenant_id = 1 AND event = 'core.document.download_refused' AND payload ->> 'document_id' = '42'
ORDER BY id;
```

One person's document trail, newest first, is a page of each table by actor (here user 7):

```sql
SELECT id, occurred_at, auditable_type, auditable_id, event
FROM base_audit_mutations
WHERE tenant_id = 1 AND actor_type = 'user' AND actor_id = 7 AND auditable_type LIKE 'core.document.%'
ORDER BY occurred_at DESC, id DESC
LIMIT 100;

SELECT id, occurred_at, event, payload
FROM base_audit_actions
WHERE tenant_id = 1 AND actor_type = 'user' AND actor_id = 7
  AND (event LIKE 'core.document.%' OR (event LIKE 'artifacts.%' AND payload ->> 'owner_id' = 'core.document'))
ORDER BY occurred_at DESC, id DESC
LIMIT 100;
```

The coverage check is `apps/core/document/examples/audit-coverage.rs`. On a disposable database that is migrated, holds the walkthrough fixture and is seeded (`bilibi-web migrate`, `bilibi-walkthrough-fixture`, then `bilibi-web seed-production`, which gives the fixture's tenants their pick-lists) it performs each action above through the facade, then fails unless the action wrote at least one audited row and the first query above finds every document row it wrote:

```sh
BILIBI_DOCUMENT_AUDIT=disposable BILIBI_DOCUMENT_AUDIT_ROOT=/an/empty/folder cargo run -p bilibi-document --example audit-coverage --locked
```

A new document action belongs in that example in the same change. It also fails if a delegated sharer can change their own grant, give download, manage or a later end than their own grant holds, or change or remove the creator's grant, in the preview or the commit.

## List and audit latency

`apps/core/document/examples/list-latency.rs` seeds a large library through the facade and times the list, its filters and the audit queries above.

- **Fixture.** 50,000 link documents filed by the administrator, each related to one walkthrough company. The viewer holds view and download on 40%, view without download on 10% and an ended grant on 10%, and favourites 1,000 of them. The contractor sees 1% and the manager and staff a third and a fifth. Every twentieth document has expired.
- **Run.** On a migrated database that holds the walkthrough fixture and is seeded (as for the coverage check). Seeding takes about ten minutes once; a rerun only measures. `BILIBI_DOCUMENT_LATENCY_DOCUMENTS` and `BILIBI_DOCUMENT_LATENCY_RUNS` (50) change the size and repetitions.

```sh
BILIBI_DOCUMENT_LATENCY=disposable cargo run -p bilibi-document --example list-latency --release --locked
```

- **Output.** One Markdown row per case and reader: rows on the page and in total, median, p95 and maximum in milliseconds, timed around `Documents::list` (authorization and both statements) or the audit statement.
- **Shape of the reads.** A record or mark filter is a top-level `EXISTS`, so the planner starts from the record's links or the reader's marks; the default order has an index (`core_documents_last_revision_idx`) so a page walks documents newest first and checks each grant. The total still counts every document the reader may see, and name search scans names. The per-document audit queries filter on JSON values and scan the audit tables; the per-actor queries use Base Audit's actor index.

## Public facade

`Documents` is the only public service. Its main calls are:

| Call | Result |
| --- | --- |
| `store_file` | A private file token and scan status ready for a revision; held files stay unreadable |
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
| `customer_shares`, `preview_share_customer`, `share_customer`, `preview_unshare_customer`, `unshare_customer` | Read and change shares with portal customers |
| `shared_with_me`, `read_shared_file` | A portal customer's own read, with their `PortalScope` |

All writes use the caller's `MutationTransaction`, return audited changes and recheck authority inside the transaction. Inputs take a sealed `Scope` (a `PortalScope` for the customer's reads); callers never supply tenant, actor, Person or Company IDs for a read.

## Contributions

- Capabilities: `core.document.view`, `core.document.download`, `core.document.manage` and `core.document.merge`; the portal capability `portal.document.view`, added to Core Portal's `portal_customer` role.
- Pick-lists: `core.document.category`, `core.document.sub_category`, `core.document.type`, and `core.document.status`; category, type and status each start with a default (`other`, `other`, `active`).
- Settings: `core.document.max_upload_bytes`, `core.document.allowed_types`, `core.document.expiry_reminder_days` and `core.document.retention_days`.
- Custom field record type `document`.
- Record kind: `core.document.document`; holder table `document_links`.
- Routes: `/documents`, `/documents/{id}`, revision downloads and previews, and `/documents/export.csv`. Downloads and previews open on `core.document.view`, so the facade decides and audits a refusal; it requires `core.document.download` itself.
- Portal routes: `/portal/documents` (`portal.document.view`) and `/portal/documents/{id}/file` (opens on `portal.account.view`; the facade requires `portal.document.view` and audits a refusal). Portal embed `document.portal-documents` (slot `portal.documents`, `EmbedContext::Portal`).
- Menu items: `Documents`, under Core, and `Documents` in the portal menu.
- Media subject: `core.document.upload`.
- Record panels (embeds), the one Documents panel of each record kind:

| Embed id | Slot | Context | Access |
| --- | --- | --- | --- |
| `company.documents` | `company.documents` | `Record { kind: "core/company" }` | `core.document.view` |
| `person.documents` | `person.documents` | `Record { kind: "core/person" }` | `core.document.view` |
| `document.record-documents` | `record.documents` | `Record { kind: "*" }` | `core.document.view` |

Core company and Core person discover their panel by kind and pass `company_id` or `person_id`. A Domain record page (a case, an opportunity) renders the `record.documents` slot with `kind` and `id` in the panel path; it never registers a Documents panel of its own. The panel shows at most 10 documents linked to the record, newest first (name, category, revision, publish date), and **View all** opens `/documents?related=<kind>:<id>`. **Quick add** (with `core.document.manage`) files a document related to the record without leaving the page, and **Add document** opens the same filtered list, where New document files and relates it; the panel writes nothing else. The panel reads through `Documents::list`: a document with no explicit grant for the reader is absent, with no hidden count.

Run `core.document.list`, `.get`, `.access`, `.duplicates`, `.create`, `.update`, `.add_revision`, `.link`, `.unlink`, `.share` (with `until` and `email`), `.unshare`, `.customer_shares`, `.customer_share`, `.customer_unshare`, `.mark`, `.copy`, `.merge` and `.delete` through the agent API. Customers have no agents (ADR 16): their read is the portal only. Pages call the same reviewed write operations; uploading bytes (and the `copy_file` step of Copy, which then calls `core.document.copy`) is the declared page-write exception because operation input is bounded; an agent copies a file document by naming a file the page stored.

## Database records

A sync ID is exact-match unique within the tenant when set. The Documents search matches it exactly as well as matching name text. Creating or updating a relation requires view access to the target; a related document's grant is still needed to open it. A revision may replace only an earlier revision of its own document. The metadata CSV includes the sync ID and document relation, while revision fields appear in get and on the record page.

The CSV export writes every document that matches the list's search and filters, by name, up to 5,000. When more match, the file is saved as `documents-not-complete.csv` and its last line says how many were written and how many match, so a partial file is never mistaken for the whole list. Narrow the filters and export again for the rest. `examples/export-rows-proof.rs` checks this with more documents than one list page holds.

The module owns `core_documents`, `core_document_revisions`, `core_document_links`, `core_document_access`, `core_document_customer_shares`, `core_document_marks`, `core_document_files` and `core_document_emails`; documents also hold `expires_on`, the reminder task id and `custom_fields`, and a grant its `expires_at`. Every query binds `tenant_id`. Applied migrations are immutable.
