# Audit evidence contract

**Use it for:** the evidence of who changed what; every `MutationBoundary` write is captured automatically, plus recorded action facts.

**Not for:** an audit table, change log or history rows of your own; never insert audit rows by hand.

`base/audit` records durable mutation and action facts in `base_audit_mutations`,
`base_audit_actions`, the mutable bookmark table `base_audit_action_metadata`
and the diagnostics table `base_audit_capture_failures` (migration
`006-audit-evidence`). Rows outlive actors and subjects: no foreign keys to
tenants, accounts or records. PostgreSQL refuses UPDATE, DELETE and TRUNCATE on
evidence (the only changes it admits are the chain link, approved erasure and
action retention, below), stamps `recorded_at` from `clock_timestamp()` and fills a missing
`occurred_at`. The frozen legacy ledgers `audit_mutations` and
`bilibi_action_commits` were carried over once as `source = 'legacy'` rows and
receive nothing further.

## Capturing business writes

Do not insert audit rows from an owner. Write through
`bilibi_database::MutationBoundary` (`base/database.mutations`) and return every
affected record as a `Change`; `AuditCapture` is bound to that boundary during
service installation and runs inside the owner's transaction. A refused audit
row, an unrecognised principal or a failing commit-time action row refuses the
business commit: there is no best-effort mode. Boot fails when nothing is bound.
Before refusing, the capture writes a diagnostics row on another connection with
identity and error type only, never values or messages.

A `Change` becomes `created`, `updated` or `deleted` from its old/new objects.
An empty side is `{}` or JSON `null`: an empty old side is a create, an empty new
side a delete, and a change with two empty sides writes nothing. Rows are
append-only and `event` is inside the hash chain, so a wrong event can never be
corrected afterwards; `person-walkthrough` (`core/person`) checks that a create
reported with `null` is recorded as `created`. Bulk writes arrive as one slice
through `Capture::mutations` and are inserted in chunks of 1,000 rows per
statement. Values are stored in full, with NUL stored as `␀`. Redaction is by
field name on every captured record (`GLOBAL_REDACTIONS`: `password`,
`password_hash`, `remember_token`, `secret`, `api_key`, `token`, `payload`),
plus per-record-kind fields an owner contributes with
`Contribution::new(REDACTIONS, ..., &Redaction { kind, fields })`. The change is
recorded; the value appears as `[redacted]`. Widen the policy rather than masking
at a call site, and name non-secret fields differently.

## Attribution

`record_oversight_read` accepts only Tenancy's sealed parent proof and child grant. It rechecks live consent before inserting the child action and sets `actor_tenant_id` to the parent's tenant, while retaining the real user or person-backed agent actor. The nullable column is reserved for cross-tenant oversight actions; ordinary actions leave it null. Child evidence includes reader/parent display names, grant version, table, level, column/filter names and row count, never filter operands or result values. Grid returns no child data when this insert fails. `oversight_activity` reads at most 20 recent actions in the caller's tenant; the shared Actions list renders the oversight attribution and parent fan-out exclusions.

`Attribution::from_context` maps the F2 `Context` principal kinds `user`,
`agent`, `guest`, `console`, `scheduler`, `queue`, `system` and `unknown`.
`actor_id` names the account acted as; `impersonator_id` names the operator
behind an impersonated session and must be a user; a `system` principal with a
non-zero id becomes `system/0` plus `system_principal`. Any other kind, or a
non-numeric user/agent id, is refused. `Attribution::from_scope` is the sealed
session identity, operator included under impersonation (K14); Q2 supplies job
rehydration contexts.
`Context::actor_role` / `Attribution::actor_role` is recorded as the row's `actor_role`. `Scope::mutation_context` and `Attribution::from_scope` set it to `customer` for a portal account (ADR 16), and the Audit lists label that actor "Customer #N" instead of "User #N".
`agent_self_report` (migration `base-audit-007-agent-self-report`, on mutations
and actions) is the agent's self-reported `harness/model-version-effort`,
taken from `Context::agent_self_report` or the scope's agent facet. It is
recorded beside the actor, never replaces it, and is shown as "Model (as reported)" on the Actions and Data Mutations lists and in record history.
The committed action payload records `Context::agent_connection` when an agent acts; its shared trace links every nested mutation to that durable grant. The actor remains the person.
The committed action row carries `event = Context::action`, so name actions as
`owner.operation` (`company.update`).

## Explicit actions and the console

`record_action(client, &attribution, request, event, payload, retained)` records
attempted, refused and failed operations on the caller's own connection, outside
any rolled-back transaction. `RequestInfo` carries IP, URL and a user agent cut
to 80 characters; free text in payloads goes through `bounded_text` (2,000
characters with a marker).

`console::execute_readonly(db, &scope, request, info)` is the read-only SQL
console engine: operator proof, empty/statement/keyword/table text guards, then a
PostgreSQL `READ ONLY` transaction with a 10 s statement timeout, count, column
names and at most 1,000 rows per page. Every command is one
`database_query.executed|refused|failed` action row with the SQL as typed and the
outcome; result rows never reach the record, and a command whose record cannot
be written returns no answer. Database error details are returned to the operator
but the recorded failure/refusal message is generic: PostgreSQL messages may
quote parameter values or data raised by a function.

Each command runs on a connection taken out of the pool and closed when it
ends. A read-only transaction still allows functions with session effects (a
session advisory lock such as `pg_advisory_lock`, a session setting through
`set_config`); closing the connection ends them, so one command cannot hold a
tenant's audit chain lock or change the session of the next pooled caller.
Known gap: the console still runs as the application's database role, so
functions such as `pg_terminate_backend` are not refused; a SELECT-only role
for the console is a later change.

The table guard keeps the `sessions` table out of the console. It refuses a query that names it, plain or double-quoted, with or without a schema; a `U&"..."` identifier, which it does not decode; and functions that run SQL text or print whole tables (`query_to_xml` and the other `*_to_xml*` functions, `ts_stat`, `ts_rewrite`, `dblink*`). Like the other text guards it is advisory: the session token is safe because `base/session` stores only its digest (`apps/base/session/README.md#token-at-rest`). A column alias named `sessions` is refused too; rename it.

`execute_readonly_with_options` also accepts named parameters and output sorting.
Its lexical binder skips strings, quoted identifiers, dollar quotes, casts and
nested comments; values use PostgreSQL's text bind format for the inferred type,
never string substitution. A missing value binds NULL. The transaction uses a
repeatable-read snapshot and is explicitly rolled back even on SQL error.
Arbitrary-precision JSON numbers preserve PostgreSQL numeric values without
floating-point rounding. Unique positional aliases preserve duplicate output
names in JSON conversion; paging arithmetic is checked. Connection/setup errors also go through the recorder on
a fresh pool checkout. `record_outcome` allows the owning service to record
capability/actor refusals through this same rows-free payload builder.

C9's `core/user.database_queries` service owns saved queries and the live
capability/account checks; C10 owns the screen. Both reuse this executor rather
than running editor SQL themselves.

`src/read.rs` holds the bounded tenant reads the Audit screens use:
`list_actions` (period, search, actor type, event family, result, diagnostics,
bounded page), `toggle_retained` (the mutable bookmark; the recorded bit never
changes), `recent_mutations`, `list_mutations` (period, search, event, bounded
page) and `subject_history` (one record's latest mutations under any of its
kinds). `MutationEntry::withhold` takes a reader's withheld fields out of an
entry and names them instead. They never ask who may read: this crate sits
below Authz, so the screens in `base/audit_views` own the capability vocabulary,
the checks and which fields are withheld.

### List bounds

**Both ledgers can be large, so `list_actions` and `list_mutations` never read a tenant's whole ledger.**

| Bound | Rule |
| --- | --- |
| Period | Every query names `[since, until)` by event time; `ActionQuery::default()` and `MutationQuery::default()` read the last 30 days. The tenant timeline index (`tenant_id, occurred_at DESC, id DESC`) serves it |
| Count | Matches are counted to `MAX_COUNT` (10,000); `more` says there are more. A page beyond the count reads nothing |
| Order | Event time only, newest or oldest first (`descending`): the one order an index gives. The other sorts were removed |
| Timeout | The count and the page run in one read-only transaction under a 5 s local statement timeout. A read that outlasts it returns an empty page with `timed_out`, not an error |

- Both take `&mut tokio_postgres::Client`, because the timeout is local to a transaction.
- `examples/bounded-evidence.rs` (`base/audit_views`) checks each bound.

## Personal-data erasure

**One narrow exception to append-only evidence, opened only by a sealed `base/personal_data` grant** ([ADR 05](../../../docs/architecture/decisions/05-personal-data-registry.md), migration `base-audit-009-erasure`).

- `erase_record(tx, &Erasure, kind, id, person, &Minimums)` clears the personal fields that the kind's owner declared (`RECORD_KINDS`) in every mutation row of that record, and the subject identifier only when declared. An undeclared kind is `Invalid`; the caller never names fields.
- **The database holds the same authority:** the fields must be declared in `base_personal_data_kinds` (written by the kind's owning module's migration; `Internal` when code and row disagree). The cause must be confirmed, for the person named, by the checker its owner registered in `base_audit_erasure_causes` (`base/privacy`: the request's own person while it runs or still owes cleanup, or a sweep still running). The target must be a record the calling transaction itself changed, or a due deferral the sweep is finishing. Anything else is refused, even when called directly as the owner role, so a request never authorises erasure of another person's or an untouched record.
- A store whose live record has nothing left to clear calls `holds_values(tx, kind, id)` first: it writes its erasure change (which lets the history be cleared) only while that history still holds a declared value, so a repeat erasure of a clean record writes nothing. A declared field `*` matches every top-level key.
- Declarations and checkers change only in a transaction that also records a migration in the ledger (checked at commit). This guards against mistakes, such as an ad hoc `UPDATE`; it is not a boundary against code running as the application's database role, which can also write the ledger.
- `Minimums` are legal minimums per company: a row of a listed company stays while newer than that company's days; a row of no company follows the strictest. Kept rows get one open `base_audit_erasure_deferrals` row naming the person; the erasure's own rows (its trace) are always cleared.
- `due_evidence(client, tenant, after, limit)` lists due deferrals; the caller leaves out held records and persons under the hold guard and passes the rest to `erase_deferred(tx, &Erasure, ids, &Minimums)` (sweep grants only), which erases them with today's minimums.
- Each call writes one append-only `base_audit_erasures` row in the same statement: request or sweep, kind, ID, fields, rows erased and kept, actor.
- No session setting opens the path: the guard admits a change only while `base_audit_erase_values` runs as the evidence's owner (call stack and `current_user`), and `EXECUTE` is revoked from `PUBLIC`. Every other update, delete and truncate is refused.
- Audit's own store, `base/audit.history`, exports the evidence history of the subject's person record.

## Action retention

**An action row past its retention period loses its address, URL, browser and payload values; who acted, when and the event stay** (migration `base-audit-011-bounded-evidence`).

| Part | Rule |
| --- | --- |
| Period | No default: a company turns the record class `base/privacy.audit_actions` on and sets its days (`apps/base/privacy/README.md`, Retention). Action rows name no company, so they follow the tenant's primary company |
| Exemption | A retained action (its bookmark on the Actions screen, else the `is_retained` bit recorded with it) is passed over and counted as kept |
| What goes | `ip_address`, `url`, `user_agent` become NULL and each top-level payload value becomes `[erased]`. `erased_at` says when |
| What stays | ID, tenant, actor, impersonator, event, time, trace and the payload's keys: the hash chain's facts, so the chain holds |
| Facade | `erase_actions(tx, &Erasure, cutoff, companies, after, limit)`, sweep grants only, one batch of at most 1,000 rows in ID order |
| Database | `base_audit_erase_actions` is refused unless the sweep is open in the tenant (`base_audit_erasure_causes`). The guard itself clears the row, so the function can change nothing else; every other update is refused |
| Ledger | One append-only `base_audit_action_erasures` row per batch: sweep, cutoff, first and last ID, rows erased and kept, actor |

- Whole-row removal follows the partition policy below; clearing personal values and removing evidence are separate operations.
- `retention-walkthrough` (`base/privacy`) runs it through the scheduled sweep.

## Partition retention

**An operator can remove a whole expired month from either ledger while retained rows stay readable and the chain still verifies.** For example, removing January moves bookmarked actions to the protected partition, records the removed chain ranges, and drops the January table in the same transaction.

### Storage and identity

- Both evidence tables use PostgreSQL range partitions on `partition_month`, initially the first day of the UTC month in which the row was recorded. A released protected action returns to the current month, as described below. Event time stays separate: a backdated event cannot expire newly recorded evidence.
- The primary key is `(partition_month, id)`. One identity sequence per ledger still issues IDs across all partitions. The unique chain index includes `(coalesce(tenant_id, 0), chain_seq, partition_month)`, as PostgreSQL requires the partition key in a partitioned unique index.
- The bookmark references `(partition_month, action_id)` with `ON UPDATE CASCADE` and `ON DELETE RESTRICT`. Moving a retained row preserves the bookmark and its foreign key.
- Capture prepares the current and following month before inserting. A removed month cannot be recreated. The special `infinity` partition holds retained evidence and is never eligible for partition removal.
- This pre-production schema replacement drops the old evidence and chain heads in a new migration. It does not convert existing rows; development and walkthrough databases are disposable.

### Period and exemptions

| Part | Rule |
| --- | --- |
| Period | The global setting `audit.partition_retention_days` has no enabled default. The operator must choose a positive number of days in System Settings before removal is available |
| Eligibility | The end of the whole UTC month must be at or before now minus the configured days; the current month and the protected partition are always refused |
| Exemption | An action follows its bookmark when present, otherwise its recorded `is_retained` bit. Mutations have an immutable `is_retained` bit. Retained rows move unchanged, except for the physical partition key, into the protected partition |
| Released bookmarks | Releasing a protected action moves it into the current month, with its original evidence and chain link unchanged. It becomes eligible only after that whole month has passed the configured period |
| Authority | The public operation requires platform-operator standing and the partition-removal capability, rechecked in its mutation transaction. The database function is executable only by the evidence owner and enforces age, exemptions and chain continuity itself |
| Ledger | One immutable `base_audit_partition_removals` row records the ledger, month, chosen period, removed and retained counts, operator, reason and trace. Its checkpoints contain only sequence bounds and hashes, never row values |

### Checkpoints and guards

- Rows link in commit order, which can cross a recording-month boundary. The removal ledger therefore records every contiguous removed range for each tenant: the sequence before the range and its hash, the final sequence and hash, and the number of rows removed.
- Before removal, each row in the partition must have a valid link, including the incoming link from the preceding live row or checkpoint. A broken or unsealed partition refuses the whole transaction.
- Verification merges surviving rows with checkpoints in chain order. It checks the incoming checkpoint hash before advancing to the final hash. Removing a checkpoint, changing a surviving fact or losing a tail still breaks verification; removed values cannot be recomputed and are covered by the guarded removal record.
- A running verification blocks partition removal, so a saved verification cursor never lands inside a newly removed range. A later verification starts from the beginning and follows the checkpoints.
- The removal transaction locks the selected ledger and bookmark metadata before examining exemptions. It moves only retained rows, removes obsolete bookmark rows, writes the ledger and checkpoints, then drops the old partition. Failure rolls everything back, including the DDL.
- Direct DELETE and TRUNCATE remain refused on parents and leaves. The evidence guard admits only the retained-row move inside removal and the return to the current month when a protected bookmark is released. Neither move changes evidence or chain links. There is no session variable that opens the guard. As with erasure, these guards prevent accidental SQL changes; a database owner who disables triggers or changes functions remains outside this guarantee.

## Hash chain

**Every mutation and action row is linked to the one before it in its tenant over its ID, tenant, time, actor, event, record and field names; values are excluded, so approved erasure keeps the chain whole.**

- **Scope of the guarantee:** an unrecorded removal, or a changed ID, tenant, time, actor, event, record or field name, is detected. A changed value is not: values are outside the hash so that erasure can clear them, and only the evidence guard (no update outside the erasure functions) protects them.
- **One chain per tenant and ledger** (`base_audit_chain_heads`: chain, tenant, last link). Rows of no tenant form chain 0. A commit takes only its own tenant's lock (`pg_advisory_xact_lock(841332, base_audit_chain_key(chain, tenant))`), so tenants do not queue behind each other.
- **Linked in one pass per commit.** A deferred constraint trigger fires as the transaction commits, so no committed row is ever unlinked. Its first firing for a ledger computes the facts of every row the transaction wrote there, then takes the tenant's lock, links them in ID order and stores the links with one UPDATE; the other firings find their row linked and return.
- **Heads are numbered** (`born`) in the order chains began, without gaps, so a removed head (a tenant's whole chain) is found, except the newest-born one.
- The partition replacement starts fresh evidence chains; later partition removal preserves the heads and bridges removed ranges with checkpoints.
- Measured on PostgreSQL 18 with `pgbench` (8 clients over 8 tenants, one mutation and one action row per commit): 275 commits a second before, 735 after; beside one client looping a 5,000-change commit, 15 before (515 ms each), 691 after (12 ms each).

### Verification

**`request_verification` starts one verification; `advance_verification` takes it one bounded step; a queued job in `base/audit_views` drives the steps.**

| Part | Rule |
| --- | --- |
| Step | One short transaction (60 s statement timeout): choose the next tenant's chain, or recompute at most `STEP_LINKS` (10,000) links of the current one (`base_audit_verify_range`), or close a ledger with its report |
| Resume | `base_audit_verifications` stores the ledger, tenant, link and hash the next step starts from. A stopped job resumes there; a repeated job finds nothing to redo |
| One at a time | A second request while one runs is `Conflict`. A verification with no progress for 30 minutes is closed as `abandoned` by the next request |
| Breaks | Per ledger, the first of `missing` (a link was removed), `changed` (a row's facts differ from its link), `tail` (the last links were removed), with the tenant, link and row; then `head` (a tenant's whole chain was removed) and `unsealed` (a committed row has no link: its trigger was bypassed) |
| Result | `whole` or `broken` on the row, and one `audit.chain.verified` action written by the step that finishes it |
| Snapshot | One range is one statement, so the head it compares is the head of the rows it read. Rows committed while a verification walks are checked when it reaches them |

- The stored row is working state, not evidence, and is written outside the mutation boundary; the request and the result are audited.
- `base/audit_views` offers **Verify audit log** at `/audit/integrity` to platform operators.

## Runtime evidence

`examples/partition-retention.rs` in `base/audit_views` loads three historical recording months on a disposable database. It removes two months from both ledgers, verifies every tenant chain in two-item steps, preserves retained evidence and bookmarks, releases a protected bookmark, checks rollback and lock conflicts, and proves that ordinary deletion, truncation and surviving-row tampering cannot pass unnoticed:

```sh
BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes cargo run -p bilibi-audit-views --example partition-retention
```


`examples/bounded-evidence.rs` in `base/audit_views` checks the chain per tenant, the list bounds and verification as a queued job, and fails when one does not hold:

```sh
BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes cargo run -p bilibi-audit-views --example bounded-evidence
```

Demonstrated without unit tests on a disposable database upgraded from the
pre-P5 schema; see Firstmate task data `data/bilibi-p5/` (reset script, proof
fixture, logs): legacy carry-over, service save with shared trace, redaction and
NUL handling, 2,500-row bulk capture, system/impersonator/queue attribution,
refused unknown kinds, refused mutation and action rows leaving company and
notification state unchanged with diagnostics written, append-only evidence, every
console outcome recorded, and an HTTP/browser smoke of the running app.

`client_identity` (migration `base-audit-008-client-identity`) records the transport client separately on actions and mutations, via the sealed scope or trusted `Context`. It is shown as "Client identity", never authorization or a replacement for "Model (as reported)".
