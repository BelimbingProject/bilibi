# Media storage contract

**Use it for:** uploading, storing, scanning and streaming files, and private attachments on your records.

**Not for:** `std::fs` writes, a files or attachments table, or a download route of your own.

`base/media` owns private asset storage: operator-mapped disks, asset metadata
(`base_media_assets`), content-checked uploads, malware scanning with
quarantine, the staging of resumable uploads, signed single-asset streams, the
trusted subject-owner seam and private subject attachments
(`base_media_attachments`: draft/submitted state, ULID public ids,
owner-checked downloads and draft cleanup).

Owners call `base/media.store` (`Media`) **after** authorizing their own
workflow. Nothing here is a browser endpoint except the signed stream route the
Web host mounts at `/media/assets/{id}/stream`, and the Quarantined Files page.

## Operator setup

Settings (global scope, editor group Media, platform capability
`admin.system.media.manage`): `media.disks` maps durable disk names to existing
absolute directories with mode `0700`, not under a `static`, `public` or
`assets` path and with no symlink anywhere in the path. `media.max_bytes`
(default 10 MiB, at most 512 MiB), `media.attachment_allowed_types`,
`media.attachment_disk`, `media.stream_ttl_seconds` (default 300),
`media.scan.policy` and `media.scan.timeout_seconds` (see Malware scanning),
`media.upload.staging_disk` and `media.upload.staging_hours` (see Resumable
uploads).
The production seed sets `local` to `data/media` under `BILIBI_INSTANCE_DIR`, or under the working directory when no instance directory is set. The first upload creates that default folder with mode `0700`; a custom disk folder must already exist. The seed preserves an operator's saved mapping, including an empty one.

Keep disk roots stable while assets reference them; `external` is a reserved
sentinel for linked (not stored) assets and cannot be a disk name.

Boot with `BILIBI_MEDIA_SIGNING_KEY` set to 64 hex characters (32 random
bytes). It is separate from `BILIBI_SETTINGS_KEY`. Without it, uploads still
work but signing and verifying refuse (`Error::SigningUnavailable`). The host
also needs the `file` command with its libmagic database; without it uploads
refuse with `MimeDetectionUnavailable` instead of trusting the client.

## Writing assets

- `put_upload(&mut client, scope, disk, directory, Upload, metadata)` checks
  size, detects the MIME type from the bytes (never the filename or a claimed
  content type), refuses empty files and active content, writes
  `tenants/{tenant}/{directory}/{uuid}` exclusively with mode `0600`, then
  registers it in its own audited transaction. A failed registration deletes the
  file. Active types are HTML, XHTML, XML, SVG and JavaScript MIME types. libmagic
  cannot tell script from prose, so script-like text is stored as `text/plain`;
  what protects it is delivery (below), not detection.
- `put_public_image(&mut client, scope, directory, Upload, max_bytes)` stores a PNG, JPEG, GIF or WebP brand image on the attachment disk. A page with no session (a portal sign-in card) shows it with `tenant_stream_url(client, tenant, id)`, the signed link of `stream_url` for a tenant the caller took from its own record, never from the request.
- `put_public_raster_image` is the identity-image variant for company marks and account pictures: PNG, JPEG or WebP only, with the same byte inspection, size bound and signed delivery.
- `put_derivative_bytes` stores a derivative under an existing original with the
  type detected the same way.
- `register_original` / `register_derivative` record bytes an authorized owner
  already placed, inside the owner's `MutationTransaction`; they touch no file.
- Calls that write bytes take a plain `&mut Client` on purpose: files and rows
  cannot share a rollback, so Media owns the transaction. Do not start
  a transaction you might roll back around a file write.
- `delete_asset` commits the row deletions first (tree, children first), then
  removes files. A file that cannot be removed reports `CleanupFailed`; the
  operator reconciles the disk. A legal hold (`base/privacy`) on any asset of the tree refuses it (`Held`).
- A write whose registration fails removes the new file again, unless the failure was the commit itself: the server may have committed, so the file stays rather than leave a row naming a missing file.
- Every row is tenant-scoped through the sealed `Scope`; another tenant's id is
  `NotFound`. Audit records name the asset but never its storage key or metadata.

## Malware scanning and quarantine

Every stored file carries a scan state; only a readable one is ever served. Decision and rejected alternatives: [ADR 12](../../../docs/architecture/decisions/12-malware-scanning-fails-closed.md).

| State | Meaning | Served |
| --- | --- | --- |
| `unscanned` | No scanner was bound when it was stored, it predates scanning, or it was larger than the scanner accepts | Only under `when_available` |
| `pending` | Waiting for the scanner | Never |
| `clean` | The scanner found nothing | Yes |
| `infected` | Quarantined: the scanner named a signature | Never |
| `failed` | No verdict after the last scan attempt (`scanner_unavailable`, `file_unavailable`), no verdict could be recorded (`scan_error`), or too large under `required` | Never |

### The scanner seam

- An Extension implements `Scanner` (`name`, `version`, optional `max_bytes`, `scan(bytes, ScanFacts) -> Verdict`) and calls `Media::bind_scanner` once from its service factory, declaring a dependency on `base/media`. A second scanner refuses boot.
- `Verdict` is `Clean`, `Infected { signature }` or `Unavailable`. The scanner gets the bytes, the detected type and the size: never the tenant, record, filename or uploader.
- `scan` runs outside every transaction and is cut off after `media.scan.timeout_seconds` (60, 5–600). Do network IO through `base/integration` or a local socket; connection settings and secrets are the Extension's own.

### What happens to a new file

- While a scanner is bound, every file Media stores (`put_upload`, `put_derivative_bytes`, `upload_attachment`, `register_*`) starts `pending`, and the `base/media-scan` job is queued in the registering transaction.
- The job runs as system principal `media-scanner`, which holds `media.file.scan` in every tenant through a standing grant (`apps/base/authz/README.md`). A tenant that denies it keeps its files `pending`.
- `Unavailable`, a timeout or an unreadable file retries with Queue's backoff; after 8 attempts (about eight minutes) the file is `failed`.
- An attempt that cannot record its verdict (a store, audit or notice error, a crash, a lost lease) retries the same way. After the eighth, up to three more attempts (about thirty minutes) ask no scanner and settle the file `failed` with reason `scan_error`, so none is left `pending`.
- Each job carries the file's scan generation, which a rescan raises. A verdict is recorded only while the file still awaits that generation, so an older scan still running cannot decide a rescanned file.
- A file larger than the scanner's `max_bytes`, measured from the stored bytes and never from the size an owner declared, is never sent: `failed` under `required`, `unscanned` under `when_available`, with reason `too_large`.
- Each verdict is an audited `Change` on the asset by `media-scanner`: state, scanner, version, signature and reason, never filename, key or bytes.
- An infected file notifies its uploader (a person; a system principal has none) and the tenant's accounts holding `admin.media.quarantine.manage` (live accounts only, at most 500).
- An impersonated session cannot store files while a scanner is bound: Queue refuses to enqueue for an impersonator.

### Policy (`media.scan.policy`, global)

| Value | No scanner bound | Unscanned files |
| --- | --- | --- |
| `when_available` (default) | Files are stored `unscanned` | Served as before |
| `required` | Uploads refuse with `ScanningRequired` before any bytes are written | Refused (`NotScanned`), including every file stored before; rescan them first |

### Reads and owners

- `stream_url`, `display_url`, `read_signed_asset`, `open_signed_asset`, `put_derivative_bytes`, `register_derivative`, `download_attachment`, `open_attachment` and `disclosed_attachment` (as `File::Withheld`) refuse a file that is not readable: `ScanPending`, `Quarantined`, `ScanFailed` or `NotScanned`. `download_attachment` asks the owner first, so a refused reader learns nothing more.
- The Web host answers those with `423 Locked` ("File not available").
- `Asset.scan_state` and `Attachment.scan_state` say where a file is; show "Checking file…" while `Pending`. `inspect_attachment(client, scope, public_id)` returns the current private attachment metadata and verdict after the subject owner allows `Inspect`, without opening bytes or granting a tenant-wide quarantine view. `Media::scanning()` says whether a scanner is bound.
- A `pending` draft may be submitted and stays unreadable until clean; an `infected` draft cannot be submitted (`Quarantined`). Discard and draft cleanup work as before.
- Linked (`external`) assets are never stored here, so they are never scanned.

### Quarantine

Tenant capabilities `admin.media.quarantine.view` and `admin.media.quarantine.manage`.

| Call | Agent operation | What it does |
| --- | --- | --- |
| `quarantine(client, scope, QuarantineQuery)` | `base.media.quarantine.list` | This tenant's files in the given states whose name contains the search (any case), newest first: one window (offset, 1–1,000 rows) and the `total` that match |
| `quarantined_file(client, scope, id)` | (the write previews) | One file, whatever its state |
| `rescan(tx, scope, id)` | `base.media.quarantine.rescan` | A file that is not clean and not purged goes back to `pending` with a new job and scan generation; needs a bound scanner |
| `purge(tx, scope, id)` | `base.media.quarantine.purge` | Purges an `infected` or `failed` file (below); the row, verdict, attachment binding and audit stay |

- The reads take the caller's client (`&dyn ReadClient`). A write's preview passes the `OperationClient` its plan hands it, the plan's own transaction, so concurrent plans never wait on the pool for a second connection.
- Search and paging run in SQL, so a file is reachable however many purged files the tenant keeps.

Purge never deletes bytes inside the manager's transaction:

1. `purge` commits `purge_requested_at` as an audited `Change` together with the `base/media-purge` job. From then nobody can open or rescan the file. Any failure before that commit (audit, queue, commit) leaves the row and the bytes as they were.
2. The job runs as system principal `media-purger`, which holds `media.file.purge` in every tenant through a standing grant. Under its fenced lease and the row's lock it removes the bytes, then records `bytes_purged_at` as an audited `Change`. Bytes already gone count as removed, so a retry after a lost record finishes the removal.
3. A failed removal retries with Queue's backoff for about an hour (12 attempts). The file then stays purging, unreadable, and a repeat purge queues the removal again. Once the bytes are gone a repeat is a no-op. A tenant that denies `media-purger` keeps purged bytes on disk.
4. A legal hold on the file (`base/media/asset`) or on its attachment (`base/media/attachment`) keeps the bytes: the job ends `legal_hold`, the file stays purging and unreadable, and a purge repeated after the hold is released removes them.

An impersonated session cannot rescan or purge: Queue refuses to enqueue for an impersonator.

The Quarantined Files page (`/system/media/quarantine`, Administration › System) lists them by status (Needs attention, Quarantined, Scan failed, Being checked, Not scanned, and Purging or Purged once purged), with Scan again and Purge contents for managers. Purge confirms first, in a dialog that opens with the shared instance banner. A database trigger keeps a purged file's verdict fixed and both purge times final.

## Resumable uploads

A live file input declared with `UploadSpec::resumable` keeps its chunks in a staging file here while they arrive, so a dropped connection, a restarted server or a reloaded page carries on from the bytes already there. Once every byte is there and checked, the file is held in the page's memory until the page takes it, as any upload is. The browser contract is [`docs/live-pages.md`](../../../docs/live-pages.md) (Resumable uploads); the decision is [ADR 14](../../../docs/architecture/decisions/14-resumable-uploads-stage-on-a-media-disk.md).

| Setting (global) | Default | Meaning |
| --- | --- | --- |
| `media.upload.staging_disk` | `local` | The disk staging files live on. While it names no configured, safe disk, resumable inputs hold their bytes in memory, at most 64 MiB |
| `media.upload.staging_hours` | 24 (1–720) | Hours an unfinished upload is kept for the person to carry on |
| `media.max_bytes` | 10 MiB | Also the largest resumable file, though never below 64 MiB |

- The Web host is the only caller. `staging(client)` resolves the disk, the size limit (`Staging::limit`: `media.max_bytes` clamped to 64–512 MiB, the same figure the host's `UploadSpec::limit` refuses a larger file by) and the lifetime once per announcement (`None` without a safe staging disk); the connection then goes back to the pool. `Staging::stage(scope, StagingRequest)` opens a `StagedUpload` without one, which writes chunks in order (`write`), hands over the bytes once all of them hash to the announced SHA-256 (`finish`), and `remove`s the file.
- A file lives at `staging/tenants/{tenant}/accounts/{account}/{key}` with mode `0600`. The key is the SHA-256 of the impersonating operator, the page service, the input, the size and the file's SHA-256, so only the same person choosing the same file in the same input finds it. Its length is the offset to resume from; reopening it hashes the bytes already there again.
- The first hash mismatch empties the file and the browser sends it again from the start; a second refuses.
- An account keeps at most 20 staging files: opening another removes the account's files nobody wrote to within `media.upload.staging_hours`, then its least recently written.
- The `base/media-staging-sweep` schedule runs hourly once an operator enables it on the Schedule board (and grants `scheduler` its `EXECUTE`, `apps/base/schedule/README.md`). It removes every staging file nobody wrote to within `media.upload.staging_hours`, in every tenant. Files on a disk the setting no longer names are left for the operator.
- Staging files are transport, not records: nothing about them is audited, and they are never scanned. The bytes reach an owner only through `Uploads::consume`, and whatever the owner stores goes through the usual checks and scan.

## Reading bytes back

**The Web host's two media routes send a file as they read it; anything that needs the whole file waits for a load slot** (issue 569).

| Call | For | Bound |
| --- | --- | --- |
| `open_signed_asset(client, id, token)`, `open_attachment(client, scope, public_id)` | The Web host's stream and attachment routes | A `FileStream`: the file opened, its `len`, `delivery(private)` and `into_reader()`. At most 64 open at once; one more is `Busy`, which the host answers `503` with `Retry-After: 1`. The slot is freed when the reader drops |
| `read_signed_asset`, `download_attachment`, `disclosed_attachment`, the scan job | An owner that needs the bytes in memory (an image in a PDF, an export, a scanner) | At most 4 whole files in memory at once; the next waits |

- The checks are the same either way (one code path each): the token or the owner's `Download`, the private-tree rule, the scan rule, and the download audit action before anything is returned.
- The host takes the pooled connection for the checks only; it is back in the pool before the first byte is sent, and the response carries `content-length`.
- The scan job measures the stored file before reading it: one larger than the scanner's `max_bytes` is never read.

## Signed streams

`stream_url` / `display_url` return `/media/assets/{id}/stream?token=...` where
the token is `v1.{tenant}.{expires}.{hmac}` binding asset id, tenant and expiry.
The route has no session: the token alone authorizes that one public asset until
it expires. Verification is constant-time, then the row, its tenant, the
private-tree rule and the disk are rechecked. Invalid, expired or private-bound
requests answer 403; missing or unreadable assets 404.

Only JPEG, PNG, GIF and WebP render inline. Everything else is an
`application/octet-stream` attachment. Every stream sends `nosniff`,
`Content-Security-Policy: default-src 'none'; sandbox` and `private, no-store`;
build headers with `StreamFile::delivery`, never by hand. The Web host's
default security headers only fill gaps, so these win.

An asset in a tree containing a `private` row (a private attachment) is refused
by streams, derivatives, listing and deletion in either direction. The database
backs this: a private asset is always a lone original, and no row may name a
private asset as its parent.

## Subject owners

A module that needs private files for its records depends on `base/media` and
contributes `Subject { owner: &OWNER }` under a stable lowercase key
(`orders.order`) to `SUBJECTS`. `Media::authorize_subject` calls the owner's
`SubjectOwner::authorize` with the caller's sealed scope and a `SubjectAction`.
The owner resolves its live record and checks its own capability; unknown
subjects have no access. An owner whose records are about a person answers
`person_subject` too, so a privacy request reaches their files (Personal-data
erasure, below). Contributions from modules that do not depend on
`base/media` fail at boot.

A module that only needs other modules to point at its records, not to hold files for them, registers a record kind with Base Records instead: [`../records/README.md`](../records/README.md). Its `RecordOwner` follows the same access pattern as `SubjectOwner`.

## Private attachments

Attachments are files a user, or a module's background job, adds to *someone else's* record (a ticket photo, an expense receipt, a file attached to received mail). Bytes are a `private` asset and the attachment row binds them to `(subject type, subject id)` with a state: `draft`, then `submitted`. `AttachmentSubject { kind, id }` names the record; `kind` is the key the owner contributed to `SUBJECTS` and `id` is the owner's own id, passed as text.

| Call | Takes | What it does |
|---|---|---|
| `upload_attachment(&mut client, scope, subject, Upload, Rules)` | plain connection | Owner `Upload` check, size/libmagic/allowlist checks (`Rules` may only narrow `media.max_bytes` and `media.attachment_allowed_types`), the draft cap (below), private write to `media.attachment_disk`, owner `Upload` check again inside the registering transaction, then a draft with a fresh ULID. A failed registration deletes the file. |
| `submit_attachments(&mut tx, scope, subject, ids)` | owner's `MutationTransaction` | Locks the rows in id order, requires every id to exist in this tenant and belong to this subject, asks the owner for `Submit`, then marks drafts submitted. Repeating it changes nothing and adds no audit row. |
| `retain_drafts(&mut tx, scope, subject, ids)` | owner's `MutationTransaction` | Same checks with `Retain`; sets `retained_at` so cleanup spares the draft. Submitted rows pass through. |
| `download_attachment(&client, scope, public_id)` / `open_attachment` | any client | Tenant-scoped lookup, owner `Download` check, then the bytes (`open_attachment`: opened for streaming, see Reading bytes back). Each delivery records the audit action `media.attachment.downloaded` and each owner refusal `media.attachment.download_refused` (with a reason code): attachment public id, subject, actor and any impersonator, never the file name or storage key. Bytes are returned only after the action is recorded, so a download that cannot be audited is refused. Send them with `StreamFile::delivery(true)`: private files always download as `application/octet-stream`. |
| `discard_draft(&mut client, scope, public_id)` | plain connection | Uploader only (the same account, or the same system principal), drafts only, not while a legal hold names the attachment or its asset (`Held`), owner `Discard` check; commits the row deletions, then removes the file (`CleanupFailed` if it cannot). |
| `cleanup_drafts(&mut client, tenant, older_than)` | plain connection | Trusted callers only. Removes that tenant's unretained drafts older than the age, re-locking and rechecking each and passing over any a legal hold names (on the attachment or its asset). Attributed to system principal `media.attachments.cleanup`. |

Every id is a request, never proof: a public id from another tenant is
`NotFound`, malformed or lowercased ids are `NotFound`, and the owner is asked
fresh on every call, so closing the record or revoking the capability stops the
very next download. Impersonated sessions are refused for submit and retain
because the row cannot name the operator. Submitted attachments have no delete
or update path: the API refuses (`ImmutableAttachment`), a database trigger
refuses raw writes, and the asset foreign key refuses removing the bytes.
Audit records name the binding, never filename, key or bytes.

`submit_attachments` and `retain_drafts` compose into the owner's business
transaction, so a rollback undoes the binding. Everything that writes or removes
bytes needs a plain connection, as in M1.

The Web host serves `GET /media/attachments/{public_id}` for a signed-in
session only (no session redirects to `/login`; not found and wrong tenant are
404; an owner refusal is 403). It never uses signed links.

### Abandoned drafts

**Unsubmitted drafts cannot fill the disk** (issue 570).

- An uploader (an account, or a system principal) keeps at most 200 unretained drafts per tenant: one submit binds at most 200. At the cap an upload first removes that uploader's own drafts older than 24 hours, then refuses with `TooManyDrafts` before any bytes are written. Concurrent uploads by the same uploader may pass the cap together.
- The `base/media-draft-cleanup` schedule (hourly, each tenant) removes every tenant's unretained drafts older than 24 hours through `cleanup_drafts`. Like every Base schedule it runs once an operator reviews and enables it on the Schedule board and the tenant allows scheduled work (`apps/base/schedule/README.md`, Per-tenant reach).
- Remove one tenant's now with the host command:

      bilibi-web media-cleanup --tenant ID [--hours POSITIVE_INTEGER]   # default 24

### Stored by a system principal

**A declared system principal may upload, submit, retain and discard attachments exactly as a person does** (issue 434, migration `base-media-005-system-uploader`). A mail import or connector job keeps the files it receives without a person present.

- Seal the job's scope with `SystemAuthentication::for_system` and pass it to the same calls; write with `scope.mutation_context(..)`, so audit shows `System · <principal>`.
- Media refuses an anonymous system scope, a principal no module declares (even one sealed with `Authentication::for_principal` directly), and a transaction whose context is not that scope's principal. An impersonated session is still refused for submit and retain.
- Your `SubjectOwner` decides which principal may act on a subject, as it decides for a person: answer `Upload` only for the principals you accept (`scope.principal()`), or check a capability granted to the principal by name.
- Nothing is more trusted: size, libmagic type, active-content refusal, the allow-list, `Rules`, malware scanning, the `required` policy and the owner's `Download` check are the same.
- The row records `uploaded_by_type = 'system'` and `uploaded_by_principal`, with `uploaded_by_id` empty; the asset records `created_by_principal`. `Attachment::uploaded_by` is `Uploader::System(name)`.
- An infected file it stored has no account to tell, so only the tenant's quarantine managers are told. The quarantine list and `base.media.quarantine.list` show `uploaded_by_principal`.
- `cleanup_drafts` removes its abandoned drafts like a person's.
- A system principal stores only private attachments: `put_upload`, `store` and derivatives stay a person's (`Forbidden`).

### Personal-data erasure

**Only a sealed `base/personal_data` grant reaches a subject's files for a request** ([ADR 05](../../../docs/architecture/decisions/05-personal-data-registry.md), migration `base-media-004-erasure`).

- A grant reaches a subject when its kind is in some store's `person_subjects` and its ID is the request's person, or when its registered owner answers `SubjectOwner::person_subject` yes for that person: a record about a person (a case) whose ID is not a person ID. A sweep grant names no person: it reaches a subject only when its registered owner answers `SubjectOwner::swept_subject` yes, that is, the owner's own retention sweep erased that record.
- The owner vouches for the whole subject, never one file: yes reaches every attachment filed under it. Answer yes only when every file under that subject belongs to the grant's person. If one record holds different people's files, file each person's attachments under a separate subject (for example, a note) that the owner can vouch for. The default answers no.
- The owner answers from its own live record on the caller's connection (inside the erasure's transaction for `erase_attachments`), for `scope`'s tenant. A held erasure may resume much later, and `remove_erased_bytes` asks again after commit: keep the subject linked to the person until no hold keeps any of its files and their bytes are removed.
- `disclosed_attachments` / `disclosed_attachment` (with `Disclosure`): every live attachment of the subject and its bytes, for an export; the subject's owner is not asked to authorize (only, for a subject that is not a person ID, whether it is the person's). `disclosed_attachment` returns `bilibi_personal_data::File`: a file that is not readable (pending, infected, failed, purged, or unscanned under `required`) is `File::Withheld(NotCleared)` and one whose bytes are gone `File::Withheld(Missing)`, which the bundle lists instead of failing (issue 567). A disk that cannot be read is still an error, never a shorter export.
- `erase_attachments(tx, scope, &Erasure, subject, &Holds)`: each attachment no hold names, on the attachment or on its asset, becomes a tombstone and its asset row goes, through `base_media_erase_attachment` (the only path the guard admits). The asset's change records `original_filename` as `[erased]`; the caller clears both kinds' evidence (`base/media/attachment`, `base/media/asset`). `erased_by` names the user, or is empty when a system principal erased (owed cleanup after a hold). The asset kind's personal field is declared in `base_personal_data_kinds` by this migration.
- `remove_erased_bytes(client, scope, &Erasure, subject)`: one file per transaction, in id order, claimed under its row lock, removed and recorded once; `Removal.more` says files remain. A file that will not go is passed over for the rest of the call, so later files still go, and the call then answers `CleanupFailed`.
- **Retention sweeps** (`base/privacy` record classes): the owner's sweeper marks its record swept, then calls `erase_attachments` in the sweep's transaction (holds on an attachment or its asset keep it, as for a request), returning the erased attachment and asset rows as `ErasedRecord`s so their evidence is cleared. Its `Sweeper::after_sweep` then calls `remove_swept_bytes(client, scope, &Erasure, kind)`: under the sweep grant only, it removes the bytes of erased files of that kind's subjects whose owner answers `swept_subject` yes, through `remove_erased_bytes`, at most one batch per call (`more` says call again); a subject the owner does not vouch for is left for its own request. `swept_subject` answers from the owner's live record, so keep the record marked swept until no hold keeps any of its files and their bytes are removed.
- `count_attachments` counts a subject's files for a reader its owner allows.
- An erased row never changes again. `ImmutableAttachment` still refuses every other update or delete.

## Manual walkthroughs

### Scanning

`scan-walkthrough` binds a fixture scanner that lives only in that executable: a file holding the EICAR test string is infected, everything else is clean, and it answers `Unavailable` while `<BILIBI_MEDIA_DEMO_ROOT>-scanner-down` exists. While `…-scanner-slow` exists a scan takes three seconds, a scan that starts while `…-scanner-stale` exists calls the file infected (`Stale-Signature`), and it accepts at most 1 MiB. Run it like `media-walkthrough` below. `prepare` (default) runs Queue's executor in process and prints pending then clean or infected files, refused downloads and submissions, notices, the quarantine list per tenant, a stopped scanner ending `failed`, rescan, a rescan overtaking an older scan still running, the size limit read from the stored bytes, a verdict that cannot be recorded settling `failed` (`scan_error`), a purge whose audit row is refused leaving the bytes and the row as they were, purge with its removal job, a tenant that denies the standing grant, and the verdicts, purge requests and removals in the audit trail. It ends with a declared system principal (`walkthrough-mailbox`) storing a clean and an infected file, the refusals (active content, an undeclared, anonymous or unaccepted principal, another tenant, an impersonated user), submit, discard and cleanup. `no-scanner` shows both policies without a scanner. `-- serve` (with `APP_ORIGIN`, `BIND_ADDR`, the signing keys, and `BILIBI_SCAN_FIXTURE=none` for no scanner) runs the real host with the fixture scanner, the fixture ticket owner and the executor, for the Quarantined Files page and `/media/attachments/{id}`.

### Attachments

`attachments-walkthrough` covers the attachment lifecycle; its fixture subject
owner is the `walkthrough.ticket` rows in a `walkthrough_tickets` table the
example creates in the disposable database. Run it like `media-walkthrough`
below (adding `media.attachment_disk` is done for you); it prints outcomes for
uploads, ULIDs, rejected files, tenant and owner rechecks, submit, retain,
discard, immutability and audit, checks that a delivery (also one made while impersonating) and an owner refusal each record their download action without naming the file, checks that an uploader's 201st unsubmitted draft is refused until one of their drafts is older than 24 hours (which the next upload then removes), and finishes by printing ids. Then
`-- serve` (with `APP_ORIGIN` and `BIND_ADDR`) runs the real host with the
fixture owner registered: sign in with the walkthrough fixture accounts and fetch
`/media/attachments/{id}`, or change the ticket row with `psql` and watch the
next request flip to 403. Backdated drafts can then be removed with
`bilibi-web media-cleanup --tenant 1`.

### Assets and subject owners

On a disposable database run the host's `migrate`. Set
`DATABASE_URL`, `BILIBI_DISPOSABLE_MEDIA_DEMO=yes`, `BILIBI_MEDIA_SIGNING_KEY`
and `BILIBI_MEDIA_DEMO_ROOT` (a scratch directory the command makes `0700`), then
run `cargo run -p bilibi-media --locked --example media-walkthrough`. It appoints
tenant 1 operator, stores a PNG and a note, shows rejected active-content,
oversize, empty and unsafe uploads, derivative and private-tree rules, tenant
isolation, token refusals and subject owners, checks that 64 streams open at
once, a 65th is `Busy`, a stream yields exactly the stored bytes and a
dropped reader frees its slot, and prints signed paths. Start the
app with the same signing key and `curl` those paths; run again with
`-- delete <id>` for a separate-process delete. It is a demonstration, not a
test harness; its fixture subject owner lives only in the example executable.
