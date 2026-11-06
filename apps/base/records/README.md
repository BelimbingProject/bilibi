# Base Records

One way for any module to point at a record that another module owns, including a record in another Domain, without depending on that module.
- **A holder stores `(record_kind, record_id)`, both text.** No foreign key crosses a module boundary, and the owner never learns who points at its records.
- **An owner registers each kind of record once:** its key, Grid table, page path and an access check.
- **Names follow the reader's own permissions.** A record the reader cannot see stays unnamed; the pill, the picker and linking all follow the owner's own access check.
- **Every page shows a reference the same way,** with the `RelatedRecord` pill and `RecordPicker` from `base/ui`. Why a Base module, and why one key per kind: [ADR 11](../../../docs/architecture/decisions/11-record-references.md).

## Register a kind (owner)

Two steps in the owning module, nothing anywhere else.

1. Implement the access check on your own type, like Media's `SubjectOwner`: load the live record with the caller's sealed scope and check your own view capability. Answer `Readable`, `NotFound` (missing, removed, another tenant's) or `Forbidden`.

   ```rust
   impl RecordOwner for PersonRecords {
       fn authorize<'a>(&'a self, client: &'a dyn ProofClient, scope: &'a Scope, id: &'a str) -> ProofFuture<'a, Access> { … }
   }
   ```

2. Declare the kind (discovered like capabilities, validated at boot):

   ```rust
   bilibi_records::declare!("core/person", RecordKind::new("person", "persons", &RECORDS).page("/contacts/{id}"));
   ```

   - Add `base/records` to the descriptor `dependencies` and `bilibi-records.workspace = true` to `Cargo.toml`.
   - Core Person (`person`), Core Company (`company`), Core Address (`address`), Core User (`user`, staff accounts only) and Core Product (`product`) are registered.

## Boot refusals

Each refusal names the kind and its source line.

| Refused | Rule |
| --- | --- |
| Key | The audit kind the owner writes in `Change.kind`, lower case (`person`, `finance.receivables.document`); registered once. |
| Dependency | The contributing module depends on `base/records`. |
| Grid table | Exists, claims the key in `record_types`, and names a `label_field` (checked by the service factory). |
| Page path | Exactly one `{id}`, and a contributed GET page route (checked by the service factory). |
| Holder | Its Grid table exists with both named fields (checked by the service factory). |

An owner may override `RecordOwner::select` to refuse a new choice while keeping earlier references readable through `authorize`. `check` and picker search use `select`; `resolve` uses `authorize`. Core Product uses this for retirement. The default selection rule is the existing read rule.

## Hold references (holder)

- **Storage:** your own table and migration, with `record_kind text NOT NULL`, `record_id text NOT NULL` and an index on `(tenant_id, record_kind, record_id)`.
- **Declare the table** so `referrers` can count through it: `bilibi_records::holder!("domain/notes", Holder { table: "notes", kind_field: "record_kind", id_field: "record_id" })`. Its Grid table must exist.
- **Use the facade** (`Arc<Records>` from `bilibi_records::SERVICE`):

| Need | Call |
| --- | --- |
| Accept a new reference | `check(client, scope, kind, id)` on the connection you already hold: your write's `tx.sql()` in a commit, and in a preview the `OperationClient` the plan hands you. Store the `Reference` it returns. |
| Show references | `resolve(scope, &references)` → `Resolved { label, path, … }`, in the order given; `resolved.pill(id, framed)` gives the `RelatedRecord`. |
| Pick a record | `search(scope, &kinds, text)` → hits; `records.picker_hits(&hits)` feeds `RecordPicker`. |
| Filter your own counts and pages by an existing reference | `matching(tx, scope, kind, id)` → an owner-built `ReferenceMatch` subquery, or `None` for an unavailable source; see below. |
| Who points here | `referrers(scope, kind, id)` → a count per holder table the reader may read, through Grid's bounded `summarize`, for the record `id` names now (a merged id counts its survivor's referrers); a malformed id of a mounted kind is refused. |
| Merge two records (owner) | `redirect(tx, scope, kind, from, to)` in the owner's merge transaction, at `READ COMMITTED`. |

- A refusal from `check` (`UnknownKind`, `MalformedId`, `NotFound`, `Forbidden`) is your field error: `refusal.message()`.
- Your holder row is your own write through `MutationBoundary`, with a `Change` on your own kind. The owner is not written to.

## Filter by a surviving record

Use `matching` when your own indexed count or list must include references stored before a merge. Canonicalizing the requested source with `check` and comparing its survivor with stored `record_id` by equality loses those earlier references.

```rust
let tx = client.build_transaction()
    .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
    .start().await?;
tx.batch_execute(bilibi_grid::BOUNDS).await?;
let Some(matched) = records.matching(&tx, scope, kind, id).await? else {
    // Your ordinary unavailable-source response, identical for every denial.
    return Err(bilibi_database::Error::NotFound);
};
let (tenant, kind, survivor) = matched.parameters();
let predicate = format!(
    "(tenant_id, record_kind, record_id) IN ({})",
    matched.sql(),
);
// Your authorized holder read uses this same predicate in both statements:
let count = format!("SELECT count(*), count(*) FILTER (WHERE owner_id IS NOT NULL) FROM notes WHERE {predicate}");
let page = format!("SELECT id FROM notes WHERE {predicate} ORDER BY id LIMIT $4 OFFSET $5");
// Admit each complete statement with EXPLAIN against Grid's
// MAX_ESTIMATED_COST, then bind tenant/kind/survivor to $1/$2/$3.
// Bind the page's limit/offset to $4/$5; propagate a query failure.
```

- **Owner boundary:** `ReferenceMatch::sql()` is a complete owner-built membership subquery with the three columns `(tenant_id, record_kind, record_id)`. Compare all three holder columns with it as one row; an ID-only or otherwise partial membership test is invalid. Embed it whole; do not inspect its implementation or query Records storage yourself. `reference()` identifies the readable survivor. All values are parameters; holder table/column names come from your code, never request input.
- **Exact membership:** the survivor and every predecessor from transitive merges participate in one SQL membership test. There is no alias cap, client-side enumeration, chunk arithmetic or holder-row multiplication. Keep the holder index on `(tenant_id, record_kind, record_id)` and apply membership before pagination and aggregation, including your own conditional counts.
- **Uniform source denial:** unknown/unmounted kinds, malformed ids, missing/removed/other-tenant sources and unreadable survivors all return `None`. The survivor passes the owner's read check (`authorize`, as in `resolve`), so a retired record may still filter its existing references. This grants no holder access or write authority; the holder authorizes its own read, and new selections still use `check`.
- **Connection and snapshot:** pass your existing PostgreSQL transaction. `matching` takes no pooled connection and starts, commits or rolls back nothing. It refuses `READ COMMITTED`/`READ UNCOMMITTED` with `Error::Invalid`; use `REPEATABLE READ` or `SERIALIZABLE`, then execute the subquery immediately on that same transaction and scope. Source access, redirects, count and pages see that transaction's snapshot, including its own holder writes. Merges never run in such a transaction (`redirect` requires `READ COMMITTED`); concurrent merges become visible in a fresh transaction. Do not cache a match or reuse it after the transaction. The transaction must allow owner authorization decision-log writes; use `READ WRITE` even for these business reads.
- **Cost:** the reverse redirect index serves the predecessor relation; no holder or tenant scan is required to discover aliases. The holder owns admission for its complete SQL: use Grid's `BOUNDS` (statement/lock timeouts), `MAX_ESTIMATED_COST`, `MAX_ROWS` and `MAX_OFFSET`, as in [`text_search_sql`](../grid/README.md). A timeout, cost refusal or database error must fail the read, never produce a partial count/page. `matching` does not change transaction settings or admit arbitrary holder SQL for you.

## Behaviour

- **Ids are text,** at most 64 bytes in one path segment (`workspace::record_id`). A kind whose Grid key is a number only accepts numeric ids, so a forged id never reaches the owner's source as an error.
- **One spelling per id:** a number key is read as plain decimal everywhere. `check` returns `01` as `1` for the holder to store, and `resolve`, `redirect` and `referrers` read `01` as `1` too.
- **Unknown kinds fail closed:** `check` refuses them; `resolve` shows them as unavailable, never as an error, so unmounting a Domain leaves holders working.
- **Names with the reader's permissions:** `resolve` projects each kind's `label_field` through `bilibi_grid::project` with the reader's scope, at most 100 ids per kind per call, then puts each named record to the owner's `RecordOwner::authorize`, the read check (new selections also pass `select`). A row Grid drops (no read capability on the table, a restricted label, a removed or soft-deleted record) and a record the owner refuses (a reader who may list companies but not view one) show as unavailable, so the pill, the picker and saving agree.
- **Previews take no connection:** a plan holds one pooled transaction while the owner previews, so a holder's preview checks on the client the plan hands it and calls nothing that takes another connection, `resolve` and `search` included. Show the checked reference and its page (`kind.path_of`); agents name it with `base.records.resolve`.
- **Connections:** no facade method holds a pooled connection while Grid takes one, and the agent dispatcher holds none while a read runs (`apps/base/agent_api/README.md`), so concurrent resolves and searches, direct or through `/api/v1` and `/mcp`, cannot fill the pool and wait on each other.
- **Removed records** are never cascaded and holders' rows are never deleted.
- **Merges:** a redirect `(tenant, kind, from) → to` is followed by `check`, `resolve` and the pill, so every holder repoints on read. Chains collapse on write, and each redirect row is an audited `Change` on `record_redirect`. `redirect` takes a transaction lock per tenant and kind, so overlapping merges run one after the other and never commit a chain (`a → b` beside `b → c`). This needs the merge transaction at `READ COMMITTED`, the `MutationBoundary` default: an older snapshot would miss the merge it waited for. `redirect` refuses any other isolation with `Error::Invalid` before it locks or writes. Chains already committed are not repaired.
- **Picker:** `search` matches each allowed kind's `label_field` inside the owner's own source (`text_search_sql`), so tenant, soft delete and permissions apply. Matches are read in key order, 50 at a time, and named as `resolve` names them: records merged into one survivor are one hit and records the owner refuses are skipped. Each kind gathers up to the tenant setting `records.picker_hits` distinct records (default 8, Settings › Records), reading at most 500 matches. Each kind is searched once, however often it is named, and an unmounted kind not at all. A choice is still `check`ed on save; a forged id is refused.
- **Long names:** the pill stays on one line, at most 16rem (60% of a phone's width), and a picker hit within its row; a longer name ends in "…" with the whole name as the title, so a table cell never grows to fit it.
- **Workspace:** the pill renders through `RecordLink` with the registering module as its workspace kind, so on a framed page it selects the record in a following tile.
- **Isolation:** every read runs through the owner's tenant-scoped source or check; redirect and holder statements carry the tenant predicate.

## Drop targets

A page can take records dropped from another tile as a shortcut for an action it already has (issue 265): contacts onto Customer portal opens the invitation review on that contact. A drop grants nothing: it opens the target's own review, and the operation that review commits decides again. The same shortcut is always an "Actions…" menu item (a link), so phones, the keyboard, screen readers and agents need no drag.

**Declare a target (the page's owner),** beside the page's `PageWrites`:

   ```rust
   bilibi_records::drop_target!("core/portal", bilibi_records::DropTarget {
       id: "portal.invite", service: PAGES, contract: operations::INVITE,
       capability: MANAGE, path: STAFF_PATH, label: "Invite as customer",
       accepts: &["person"], max: 1,
   });
   ```

- `contract` is an operation the `service` already declares in its `PAGE_WRITES`; `capability` is what that contract needs, so only people who can finish the review are offered the target (display only; the contract decides). `max` is how many records one drop takes (1 to 100).
- Boot refuses a target that accepts a kind no mounted module declares, whose `path` is no contributed GET page route of its `service`, or whose contract the service does not declare.
- **The page** wraps itself with `target.zone().wrap(id, html)` only while its own button shows, and handles two inputs the same way: the live event `drop_records` (`records` = `kind:id,kind:id`) and the address parameter `?drop=` (`DROP_EVENT`, `DROP_PARAM`). Both call `Records::receive(scope, target_id, text)`, which checks the count, the kinds and every record (read as `resolve` reads them, so a hidden, removed or forged record is refused), then the page opens its review with the record chosen, as its button and picker would. Show a refusal with `DropRefusal::message`.

**Offer a source (a list's owner):** `Records::add_to(scope, kind)` lists the targets that accept the kind and that the person holds the capability for. When it is not empty, set `FlexRow::record(kind, id)` on each row and `AddTo::href(&records)` in a `bilibi_ui::AddToMenu` on the row and on the selection bar (a target that takes fewer records than are ticked stays in the menu, disabled with the reason). Dragging a ticked row takes every ticked row of its table that is on screen. Contacts (`person`, with a selection bar) and Companies (`company`, one row at a time) are the Platform's sources; Finance's Customers (`receivables.customer`) and Vendors (`payables.vendor`) accept both, up to 20 at a time, and Customer portal (`portal.invite`) takes one contact.

The browser half is `apps/base/ui/assets/drops.js`: the source frame tells the workspace what it carries (`assets/tiling.js` relays it) so every accepting tile lights up, the drop raises `drop_records` on the page, and a drag from another site carries none of the types it reads and is ignored.

## Which kind an agent operation acts on

`Records::kind_of_operation(key)` answers from the operation's registry key, with no declaration on the operation: `<module id with dots>.<verb>` belongs to the module's only kind (`core.company.get` is `company`), and `<module id with dots>.<kind>.<verb>` to the kind of that name (`finance.receivables.document.list`). Anything deeper (`core.company.relationship.list`) or from a module with several kinds and no kind segment belongs to no kind. The host edge records the answer with each agent request (`apps/web/README.md`), and the AI panel names it with `kind_nouns` (the Grid table's label, singular and plural). Name an operation's key after its kind and verb to have its requests counted.

## Agents

| Key | Capability | Does |
| --- | --- | --- |
| `base.records.resolve` | `base.records.view` | Names up to 100 references `{kind, id}`: `label` (null when unavailable), `path`, `stored_id`. |
| `base.records.search` | `base.records.view` | Finds records of up to 20 kinds by name: `{kind, id, label, path}`. |

- The agent edge adds an absolute `url` beside every record reference's `path`, on both `/api/v1` (operation calls, plan previews and commits, refusals included) and `/mcp`.
- Writes that store a reference are the holder's own reviewed writes, which `check` the target in preview and commit.
- `base.records.view` is granted to Authz's built-in roles; the guide `base.records` explains references to agents.

## Storage

`base_records_redirects` (migration `base-records-001-redirects`): `(tenant_id, kind, from_id) → to_id`, unique per tenant, kind and `from_id`; `from_id` never equals `to_id`.

## Runtime check

`apps/web/examples/records-reference/` declares a notes holder that points at people and companies. Run it on a fresh disposable database:

```sh
DATABASE_URL=… cargo run -p bilibi-web --example records-reference
```

Add `APP_ORIGIN` and `BIND_ADDR` to serve the app afterwards, with the notes page at `/reference/notes`.

| Case | Expected observation |
| --- | --- |
| Boot refusals | Each invalid registration is refused with its source line: registered twice, key shape, two `{id}`, no `base/records` dependency, missing table, unclaimed key, no `label_field`, no page route. |
| Check | A readable person is accepted; a forged id, another tenant's company and a soft-deleted company do not exist; a malformed id and an unmounted kind are refused; a company after the viewer's grant was revoked is not one they may view; a merged company is accepted as its survivor. |
| Reviewed writes | Notes about a person and a company save through plan preview and commit; the viewer's note about a company and a forged id are refused in the preview. |
| Resolve | A retired person keeps its name; a soft-deleted company and an unmounted kind are unavailable; a merged company reads as its survivor; for the viewer without company grants, every company is unavailable and people stay named. |
| Referrers | The administrator counts the notes table; staff, who cannot read it, get no holders. |
| Search | The administrator finds companies; the viewer without company grants finds none. |
| Agent edge | `base.records.resolve`, `base.records.search` and the holder's list answer over HTTP with a real agent connection: `label`, `path` and an absolute `url`; an unmounted kind has a null label and no url. |
| Agent doors under load | On the pool of 16 connections, 16 synchronized `base.records.resolve` and 16 `base.records.search` calls each through `/api/v1` and through MCP's `read` tool (a real MCP connection from a PKCE code) answer within 8 s with the expected result, three rounds; then 128 mixed calls through both doors at once all answer. |
| Browser | Pick a record by name, save, the pill opens the record's page; in a workspace the pill selects the record in a following tile; the viewer sees company pills unavailable and a forged pick is refused under the picker; no horizontal scroll at 390 px. |
| Audit | `record_redirect` and the holder's note are each one `created` mutation by the actor. |
| One spelling | `01` is accepted as company `1`, resolves to its name (also over HTTP) and counts the same referrers as `1`. |
| Overlapping merges | Merging 52 into 53 waits while merging 51 into 52 is uncommitted; both then point at 53. |
| Connection pool | 64 concurrent resolves on the pool of 16 connections all finish. |
| Picker limit | With the first eight matches (companies 1 to 8) all pointing to company 1, searching `0` gives 8 distinct companies. |
| Repeated kinds | `["company"]` named twice or 32 times gives the same 8 distinct hits in the same time as once; 21 kinds are refused. |
| Plan references | A notes plan preview's record carries the same absolute `url` through `/api/v1/plans` and MCP's `plan`, and so does a plan refused at commit (`plan_rejected`) through both doors. |
| Merged referrers | After 61 → 1, `referrers` of 61 counts the same notes as of 1. |
| Long names | A 160-character company name: the pill, the picker hit and the chosen pill stay within a 390 px page, ending in "…" with the whole name as their title. |
| Synchronized plans | 16 plans of the notes write at once through `/api/v1/plans` and through MCP's `plan` answer within 8 s on the pool of 16, three rounds, and 16 commits of previewed plans at once all finish. |
| List but not view | For the manager allowed `admin.company.list` and denied `admin.company.view`, `check` refuses company 1, `resolve` shows it unavailable and `search` offers no company, directly and through `/api/v1` and MCP; in the browser their company pills are Restricted and the picker offers no company. |

### Native equivalence proof

`apps/web/examples/records-equivalence/` runs the complete Platform app with a disposable audited holder and the walkthrough fixture, without serving or changing UI. On a clean committed checkout, use the supported real-Git composition recipe and a fresh disposable database:

```sh
python3 scripts/discover-modules.py --check
python3 scripts/discover-modules.py --pin
DATABASE_URL=… cargo run --locked -p bilibi-web --example records-equivalence
```

The executable requires a current compiled-in pin and asserts unmerged and chained merges, canonical source spelling, the equality defect, total/taken/owned counts, every ordered page, more than 100 predecessors, agreement with `referrers`, uniform tenant/source denial, unchanged complete holder rows and mutation history, concurrent merge snapshots, refusal of redirects at `REPEATABLE READ` and `SERIALIZABLE` before any write, a caller-owned uncommitted `READ COMMITTED` redirect and its rollback, refusal of `matching` at `READ COMMITTED`, and successful reads with every pool connection already held. Complete consumer statements use Grid's cost admission and timeout bounds. Fixture-only source setup is explicit SQL; holder writes and redirects use `MutationBoundary`. Any failed assertion exits unsuccessfully.
