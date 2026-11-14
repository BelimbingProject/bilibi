# Bulk data: CSV import, export and bulk updates

**Use it for:** CSV import, CSV export and setting one field on many records of your type, through your own contracts.

**Not for:** an import page, CSV parser or bulk-update handler of your own.

`base/bulk_data` lets people bring a module's records in from a CSV file through that module's own write contracts, take a list's records out to CSV through its own list contract, and set one field on many records through its own update contract, for any module that opts in ([ADR 10](../../../docs/architecture/decisions/10-queued-jobs-call-contracts.md), issue 133). Lists tick rows with `FlexTable`'s `Selection` (`apps/base/ui`).
## Opting a record type in

Declare a dependency on `base/bulk_data` and contribute one `RecordType` per type, with id `<module>.<name>` (the type's key in runs and saved mappings; keep it stable):

```rust
inventory::submit! { Contribution::new(bilibi_bulk_data::RECORD_TYPES, "core/person.contacts", provenance!(), &RecordType {
    label: "Contacts", table: "persons", path: Some("/contacts/{id}"),
    import: Import {
        capability: IMPORT, create: "core.person.create", id: "id",
        duplicates: Some(Duplicates { read: "core.person.duplicates", fields: &["email", "phone"], update: "core.person.update", update_id: "person_id", update_version: "version", update_fields: "identity" }),
        references: &[],
    },
    export: Some(Export { capability: EXPORT, list: "core.person.list" }),
    bulk_update: Some(BulkUpdate {
        capability: BULK_UPDATE, list: "core.person.list", get: "core.person.get", get_id: "person_id",
        update: "core.person.update", update_id: "person_id", update_version: "version", update_fields: "identity",
        fields: &["kind"],
    }),
}) }
```

| Field | Meaning |
| --- | --- |
| `label`, `path` | What the records are called; their page with `{id}`, for links from row results |
| `table` | The owner's Grid table: a field restricted there for the person is not offered |
| `import.capability` | The owner's tenant capability to import this type (verb `import`, from its own vocabulary); checked for every row |
| `import.create`, `import.id` | The owner's write contract creating one record, and where the record's id is in its result |
| `import.duplicates` | Optional. The owner's read answering candidates for the match `fields`, and its update contract with the id, version and fields properties. A field the file leaves out keeps the record's value |
| `import.references` | Create fields holding another record's id: a `parent` record type whose earlier import names it by source key, and/or a `lookup` read that must find exactly one record |
| `export` | Optional; `None` keeps the type out of export. The owner's tenant capability to export it (verb `export`) and its list read contract, which takes integer `page` and `page_size` and returns the records with their `import.id` |
| `bulk_update` | Optional; `None` keeps the type out of bulk updates. The owner's tenant capability (verb `bulk-update`), its paged list read (records with `import.id` and `version`), its read of one record by `get_id`, its update contract with the id, version and fields properties, and the `fields` people may set: scalar properties of the update's fields (choices, dates, flags, numbers, or an owner's id) |

- **Mappable fields are data:** the create contract's top-level string, number, integer and boolean properties, with its `enum` as choices and `required` as required.
- **Boot refuses** an unknown or foreign contract, a write a job cannot call (step-up declared, or `Channel::Job` left out), a list read a job cannot call or that does not page, an undeclared or foreign capability, a match field, reference, lookup property or parent that does not exist, and a bulk field that is not a scalar of the update's fields.
- A lookup read must match exactly (Core Person's duplicates read matches email exactly); a scoring read such as Core Company's duplicates is not a lookup. Several matches are always a row error.

Adopters: Core Person (`core/person.contacts`, matched on email or phone; exported from the Contacts list with `admin.person.export`; kind set in bulk with `admin.person.bulk-update`) and Core Affiliation (`core/affiliation.affiliations`, the contact by source key of a contacts import or by email; the organisation as a column of ids or one fixed value).

## Owner business results

Ordinary contributors need no changes. An owner whose create operation can instead perform a business action contributes an optional `ImportResults` under `IMPORT_RESULTS`, with the **same id and module provenance** as its `RecordType`. `interpret: fn(&serde_json::Value) -> bilibi_database::Result<ImportResult>` receives the owner's `result`, without the operation envelope, in both preview and commit. It projects the owner's already authorized answer; it does not perform matching, reads or writes. Unknown result shapes must return an error rather than guess.

`ImportResult` contains `ImportOutcome::{Create, Update, Action}`, optional `record_id`, optional plain `message`, and `Vec<ResultLink>`. `Action` means a successful business action without creating the imported record (for example another enquiry and task on an existing record); it is counted separately from create/update/skip/error. The message states what the action did or would do. Only this imported type's id belongs in `record_id`, because child imports may use it. Foreign records and tasks use links (`label`, application-local `path`). The result may carry at most four links, labels of 100 characters, paths of 1,000 characters and a message of 2,000 characters; ids are at most 100 characters. External, protocol-relative, backslash, whitespace and control-character paths are refused. Malformed projections roll back the row and retain normal job retry behavior.

The owner must return only safe outcome facts and record/task identifiers, without personal names, restricted fields or secrets. These facts persist with the run under its normal person/company visibility; destination pages recheck their own access. Result links grant no access. Generic Bulk Data never reads another owner's records to reconstruct an answer.

The saved check is advisory. Each real row calls the owner again, interprets **the actual commit answer**, and saves that result beside the owner mutation in the same transaction. A record created between check and import may therefore change a checked `create` to an actual `action`. The optional projection applies to the create contract; existing duplicate-read/update policies retain their own outcomes. No whole-file transaction is promised: stop/cancel retains earlier committed rows.

Existing `ContractError::Backend(bilibi_database::Error::Refused(message))` is an actionable failed row, preserving the owner's message (within the existing row-message bound). Commit refusals drop the owner transaction before reopening to record failure and advance `next_row`; genuine backend failures still retry, and Queue lease and row fences still apply.

The same refusal mapping is shared by bulk updates: a `Refused` answer from the update contract is recorded as a **Not updated** row with the owner's message (within the same bound) instead of retrying the job, and a `Refused` answer when a ticked record is first read is reported on its row like a record that was not found.

The action count in the finish notice, the run's action fact and its **Action** result filter appear only for record types whose owner contributes `ImportResults`, or for a run that already recorded actions; the Imports list's **Actions completed** column appears only when a mounted record type contributes it, with `—` on other types' rows. Ordinary imports keep their created/updated/skipped/not-imported wording.

This seam supports [CX lead import 117](https://github.com/cx2027/bilibi-dom-cx/issues/117); consumer registration and its application adoption remain separate.

## A run

| State | Meaning |
| --- | --- |
| `draft` | Uploaded; map it, check it or import it |
| `previewing`, `previewed` | The dry run is working, or done: every row checked by the owner's preview, nothing written |
| `running`, `completed` | Rows are being written, or all are done; the person gets a notice |
| `cancelled` | Stopped during the import; rows already written stay. A cancelled check returns to `draft` |
| `stopped` (read only) | The job ended without finishing (its person lost access, or every attempt failed); cancel and start again |

- **Upload** (`/imports/new`): the record type and a file. Refused in words: not UTF-8 (a byte-order mark is fine), not CSV, no header, a blank or repeated column, a row wider than the header, larger than `bulk_data.import_max_bytes`, more rows than `bulk_data.import_max_rows`.
- **Mapping**: for each field, a column or a fixed value. A reference field's column is looked up, or names a row of a completed parent import (one of the person's own, in the same company) by its source key. `source_key` names the column a later child import uses. `duplicates`: `skip` (default), `update` or `create`.
- **Check file** (dry run) and **Import** run in the background as the person, through `bilibi_agent_api::job`. Each real row is one transaction: the owner's preview and commit, with the row's result beside it; a refusal is recorded in words and the run goes on. A row locks its run with a single-row read (`store::lock`) and never counts the run's recorded rows, so an import's cost grows with its rows, not with their square; the counts are read for the pages and once when the run ends. Every row the run writes shares its trace ID.
- **Slices**: an attempt works for two minutes, then queues the rest from the next unrecorded row. A retried attempt never writes a row twice.
- **Access**: each row rechecks the type's import capability and the contract's own; losing `admin.import.create` stops the job.
- **Failed rows** download from `/imports/{id}/failed-rows` (`?pass=preview` for the dry run's): the file's columns plus an Error column, UTF-8 with a byte-order mark, and a cell a spreadsheet would run as a formula is prefixed with `'`.
- **Saved mappings** are saved by name per record type, shared in the company, and offered when the file's header has every column they read.

- **A restarted check**: a pass records only for the job its run names. A check cancelled and started again has a new job; the old one stops at its next batch instead of recording rows planned with the earlier mapping.
- **Personal-data erasure**: the uploaded file is the person's data as typed, and a row's source key and message can quote a cell. The owner's `erase_exports_in` therefore also marks every run of its record type in the tenant, and of each type with a field naming it as `parent` (an affiliations file names contacts). A marked run reads as `erased`: its file name is `[erased]`, its header and mapping are empty, its file is not offered, its rows answer no source key or message, it is no longer a parent, and a run still working ends as `cancelled`. Fixed mapping values cannot be matched safely to one subject, so the same audited transaction deletes every company-shared saved mapping for the affected record types. `remove_erased_exports` then blanks the rows (5,000 row numbers per transaction) and deletes the file through Artifacts, and sets `cleaned_at`. Outcomes, record ids and result links stay; to import the file again, upload it again.

A run is visible only to the person who started it while working at the desk where it was opened.

## An export

- **Start** from the list (Contacts: **Export CSV** in the header for the filtered list, or in the selection bar for the ticked rows) or `base.bulk_data.export.start`: the list's filters (its list contract's input without `page` and `page_size`, checked against that contract's schema) or the ticked `ids`, and the columns it shows (Grid specs, 1 to 64, each once). It needs `admin.import.view` and the type's export capability, and is filed under the selected desk. Queue captures that company and re-proves desk access for every attempt and business transaction. Artifacts re-proves the person's access to the file's company before creation and download. Switching desks after enqueue does not move the job. Losing the captured desk stops it without running the work in another company. The same rule applies to imports and bulk updates ([ADR 19](../../../docs/architecture/decisions/19-company-work-access-and-page-scope.md)).
- **Runs** in the background as the person (`base/bulk_data-export`, up to 5 attempts): it first purges the company's expired run files, rechecks the capabilities, pages through the list contract, then projects the records through Grid in the list's order (ticked ids go straight to Grid, in the order given). A field restricted for the person is blank; another tenant's or company's record never appears, since the list contract and Grid both read as the person.
- **No gaps**: offset pages are not a snapshot, so the list is read in full until two reads in a row hold the same records (at most three reads). A record moved, added or dropped while it is paged is never skipped; a list whose members keep changing fails in words. Edits that keep a record in the list do not count against it.
- **Bounded memory**: the file is written a row at a time and stops once it is larger than `artifacts.max_bytes`, so an export never holds more than that much of its file (plus one Grid batch of 100 rows).
- **Refused in words**, with no file: more records than `bulk_data.export_max_rows` (nothing is cut off), a list that takes longer than ten minutes to read, a revoked grant, a column the list no longer has, no shown column the person may read, a file too large for private documents. A file stored by an attempt that then fails is deleted, not left to expire.
- **The file**: UTF-8 with a byte-order mark, the columns' labels as its header, a cell a spreadsheet would run as a formula prefixed with `'`. It downloads from `/exports/{id}/file`, read through Artifacts every time, and expires after `bulk_data.file_days`.
- **Access when downloading**: when the file is made, the export saves what the person could see (the export, list and Imports grants, which columns Grid lets them read, and the type's restricted fields). The download and the export's page compare it every time, as Reports' prepared exports do: a revoked grant or a newly restricted field refuses a file made before it, and they export again.
- **Personal-data erasure**: an owner calls `erase_exports_in` with its sealed erasure grant and record-type key in the same transaction as clearing source facts, then `remove_erased_exports` during its handler's after-commit cleanup. This conservatively invalidates every cached export of that type in the tenant (filters and linked columns need not name the person's ID), and marks the type's import runs (see [A run](#a-run)). A dedicated connection's shared session guard covers each export's reading through publication and closes on completion or cancellation; erasure takes the exclusive transaction guard. Invalidated files cannot be downloaded and all published attempt copies are deleted through Artifacts, reporting retention/holds through `Progress`. Contacts uses this contract; this does not enable Contacts oversight or classify any new Grid fields.
- **An erasure never waits long on an export**: it asks for the guard for two seconds, then answers `Conflict` and its job retries with Queue's backoff. Its transaction locks Queue's row for the job, so a longer wait would cost the attempt its lease.
- **An erasure touches only what still holds something**: once the clean-up has removed an export's files it sets `cleaned_at`, and later erasures leave that export alone. One whose file is retained or on legal hold stays unmarked, so each later erasure reports it too.
- **Expiry**: the export keeps its file's expiry; past it, its page and the Imports list show it Expired and offer no download.
- **Stopped jobs**: on its last attempt the job ends the export as failed. A job Queue ends without running it (its person lost the Imports pages first) cannot do that, so the company's next export first ends such exports as failed, drops their filters and tells their persons.
- **Round trip**: an import drops the apostrophe an export puts before a formula-like cell (`'+60 12…`), so a file exported, edited and imported back keeps its values; any other apostrophe stays.
- **Retention**: an hourly scheduled task (`base/bulk_data-file-sweep`, enabled on the Schedule board like every task) purges expired run files in every tenant through `Artifacts::sweep_expired`, so they go even where nobody uploads or exports again; each upload and each export also purges the company's expired files first. A file that cannot be purged is logged by its code, and Artifacts holds it for an operator after its attempt limit.
- **`/exports/{id}`** shows its state (Preparing, Ready, Expired, No longer available, Not finished, Stopped) and the download while it may be taken; the person gets a notice when it ends. An export is visible only to the person who started it while working at the desk where it was opened.

## A bulk update

| State | Meaning |
| --- | --- |
| `resolving` | The job is finding the records, once, with the version each has now |
| `running`, `completed` | Records are being updated, or all are done; the person gets a notice |
| `cancelled` | Stopped by its person; records already updated stay and are listed |
| `failed` | Ended before updating anything (too many records, none matched, access lost), in words |
| `stopped` (read only) | The job ended without finishing; records already updated stay |

- **Start** from the list's selection bar (Contacts: tick rows, or tick the page and **Select all N matching**, then **Set kind…**) or `base.bulk_data.update.start`: the field, the value as text (converted as an import converts a cell), and either the ticked `ids` or the list's `filter` (`{}` for every record; naming neither, or both, is refused), with `except_ids` for the rows unticked after "Select all N matching", which the bar counts as "599 of 600". Unticked rows are left out before `bulk_data.update_max_records` counts, and are bounded only by what a selection holds (`Selection::MAX_KEYS`). It needs `admin.import.view` and the type's bulk-update capability, and is filed under the company the person works in now; fields restricted for the person are not offered. More than `bulk_data.update_max_records` records are refused.
- **Resolve once**: the job reads the list (until two full reads hold the same records, as an export does) or each ticked record, as the person, and keeps each record's id and its version in the last read; the filter's values are then dropped. A record edited after that is reported stale on its row, so a busy list still resolves. Records of another tenant or company are never found.
- **Each record** is one transaction: the owner's read of it, then its update contract with the resolved version and every other field as read (a hidden field is left out, so the owner keeps it), with the result beside it. A record already set is left alone (`skip`); one changed since it was resolved is refused by the owner's version fence and reported, never overwritten. Every record shares the update's trace ID.
- **Access**: each record rechecks the type's bulk-update capability; a revoked grant refuses the rest in words.
- **Slices**: an attempt works for two minutes, then queues the rest from the next unrecorded record; a retried or resumed attempt never writes a record twice.
- **Stopped jobs**: a job Queue ends before the update has found its records (its person lost the desk or the Imports pages first) cannot drop the filter, so the company's next update first ends such updates as `failed`, drops their filter and ids and tells their persons. An update stopped after it resolved keeps no filter and stays `stopped`.
- **`/updates/{id}`** shows what it sets, its state and how many records are done, **Stop update** (with a confirmation) while it works, and each record's result in a `FlexTable`. `/imports` lists the person's newest exports and bulk updates for the current desk. A bulk update is visible only to the person who started it while working at the desk where it was opened.

## Contracts

Capabilities `admin.import.view` (pages and reads) and `admin.import.create` (start, check, cancel, save mappings), plus each record type's own.

| Operation | Kind | Does |
| --- | --- | --- |
| `base.bulk_data.types` | read | Record types the person may import, with their fields |
| `base.bulk_data.import.list`, `.get`, `.rows` | read | Their runs, one run (with fitting saved mappings and possible parents), a pass's row results |
| `base.bulk_data.mapping.list` | read | Saved mappings of a record type |
| `base.bulk_data.import.preview`, `.start` | write | Map a run and start the dry run or the real run |
| `base.bulk_data.import.cancel` | write | Stop a working run |
| `base.bulk_data.mapping.save`, `.delete` | write | Saved mappings |
| `base.bulk_data.export.get`, `.list` | read | One of the person's exports, with its download path once ready; their newest exports |
| `base.bulk_data.export.start` | write | Start an export of a list (filters as JSON object text, optionally `except_ids` to leave out) or of ticked ids |
| `base.bulk_data.update.fields` | read | The fields of a record type the person may set in bulk |
| `base.bulk_data.update.get`, `.rows`, `.list` | read | One bulk update, its records' results, the person's newest |
| `base.bulk_data.update.start`, `.cancel` | write | Start a bulk update of ticked ids or a list's filters (optionally `except_ids` to leave out); stop one |

Receiving the file is the page's `receive` event, a listed allowance: Artifacts takes bytes outside a contract, and a plan's input is bounded at 1 MiB. Agents work on runs their person uploaded until agent-supplied files land.

## Storage and audit

`base_bulk_data_runs`, `base_bulk_data_rows` and `base_bulk_data_mappings` (migration `base-bulk-data-001-imports`), `base_bulk_data_exports` (`base-bulk-data-002-exports`), and `base_bulk_data_updates` and `base_bulk_data_update_rows` (`base-bulk-data-003-updates`), every statement carrying the tenant. Owner result links and the `action` outcome are added by `base-bulk-data-005-import-results`; the erasure marks of a run (`erased_for`, `cleaned_at`) and an export's `cleaned_at` by `base-bulk-data-006-import-erasure`. Changes are audited as `base/bulk_data.run`, `.row`, `.mapping`, `.export`, `.update` and `.update_row`, never with a file's data or a filter's values; the owner audits each record it writes. An erasure's invalidations and clean-up are audited under the same kinds (`.export`, `.run`), as counts and ids. An export keeps its filters only while it runs, and a bulk update only until it has found its records: a search can quote a person's name or email.

Not pruned yet: the rows of ended runs, exports and updates stay until a retention period for them is decided (issue 600).

Settings (operator UI, group Bulk data): `bulk_data.import_max_bytes`, `bulk_data.import_max_rows`, `bulk_data.export_max_rows`, `bulk_data.update_max_records` and `bulk_data.file_days`. The uploaded file (kind `import-source`) and the exported file (kind `export`, subject `export:{id}`) are private artifacts of owner `base.bulk_data` that expire after `bulk_data.file_days`.

## Walkthrough

On a disposable database, set `DATABASE_URL`, `BILIBI_BULK_DATA_DEMO=disposable`, `BILIBI_SETTINGS_KEY` and `BILIBI_USER_SIGNING_KEY`, then run `cargo run -p bilibi-bulk-data --example import-walkthrough` from a scratch directory: an unseeded database names no private document folder, so it keeps uploads under `./data/artifacts`. It migrates, installs the walkthrough fixture and, as its manager (`manager@example.test`, whose fixture role holds the import capabilities), imports 1,000 contacts through the contracts and a real Queue executor: a saved mapping reused, a dry run that writes nothing and reports errors and duplicates, the three duplicate policies and a row matching two contacts, affiliations finding their contacts by source key, a revoked grant, a worker stopped mid-run, a file refused for each reason, tenant isolation, the audit trail under the run's trace ID, and a check cancelled and started again with another mapping, whose results must all come from the second (it fails otherwise).

`cargo run -p bilibi-bulk-data --example export-walkthrough`, with the same variables, exports the fixture's contacts as its manager: the filtered rows and shown columns, link columns, a restricted field left blank and a formula-like name opened as text, refusals before it starts, the row limit failing it in words, a grant revoked before the job runs, ticked records only (another tenant's left out), another tenant refused, a file refused after a field is restricted or the grant revoked (and allowed again once restored), an expired file purged from disk before the next export and then shown expired, an export of only restricted columns refused, an export whose job was ended by a revoked grant finished by the next export (filter dropped, its person told), the size limit failing an export in words, a list renamed while it is read, the audit trail without filter values, the file's expiry and the person's notices.

`cargo run -p bilibi-bulk-data --example update-walkthrough`, with the same variables, changes 500 contacts' kind as the manager: refusals before it starts (no capability, a field not offered, a value outside the choices, more records than the limit), ticked records with another tenant's among them, every match with one contact edited meanwhile (reported, not overwritten) and the audit under the update's trace, already-set records, a grant revoked mid-run, a stop mid-run, a worker stopped mid-run (none written twice), another tenant refused, an update whose job ended before it resolved finished by the next one (filter dropped, its person told; it fails otherwise), and no filter values in the audit trail.

Erasure is proved by `cargo run -p bilibi-privacy --example requests-walkthrough` (`apps/base/privacy`): a Contacts export paused at publication makes the erasure give way with `conflict` and retry, never waiting on the guard; afterwards the exports are invalidated under the `base/bulk_data.export` kind and marked cleaned, and a Contacts import and an affiliations file uploaded before the erasure keep no source key, message or file.

For owner-result semantics, `cargo run -p bilibi-web --example import-results-walkthrough` uses the same disposable setup, an exact `APP_ORIGIN` and `BIND_ADDR`, and a private `BILIBI_IMPORT_RESULTS_CASES` JSON file. The case file supplies `files` (scenario keys with `file_name` and `csv`, plus `code` for rejected projections) and `codes` (the values used by the race/rollback checks); scenario expectations are fixed in the executable. Keep these inputs and raw results outside publication evidence. Its fixture-only owner creates records or adds enquiry tasks through registered operations and the real Queue runner. It fails if preview writes, commit reuses a stale preview outcome, actionable refusals keep retrying, poison rollback leaks a mutation, genuine database/lease failures become row skips, retry repeats an earlier task, malformed projections commit, cancel loses earlier rows, or an ordinary contributor loses its create/id result. `--serve` serves that reference application for browser checks of upload, mapping, results and task links; reference routes are absent from the product binary.
