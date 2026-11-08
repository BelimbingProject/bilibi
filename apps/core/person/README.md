# Person identity and personal contacts

`PersonService` (`core/person.persons`) and the transaction APIs the crate root exports from the private `src/identity.rs` (`read_person`, `search_persons`, `preview_person`, `preview_person_with_code`, `create_person`, `create_person_with_code`, `preview_update_person`, `update_person`, `lock_for_account_binding`, `account_standing`) own `persons`. They use K0's `PersonInput`, `PersonKind` and `PersonId`; an identity is never an employee or login account. Core works with People absent. `/contacts` and `/contacts/{id}` provide the capability-gated directory and inline record/contact editor. Creating a contact opens a dialog on the list; `/contacts/new` redirects there with that dialog open. The earlier `/persons` addresses answer with a permanent redirect to the Contacts addresses (`Moved` contributions in `src/contributions.rs`).

Every person has a fixed Core `code`, unique within their tenant, including retired identities. The default uses up to 12 characters from whole name words, omitting source-spelled connectors bin, binti, a/l, a/p, s/o and d/o. It truncates the first word only when needed to fit the limit. For example, `Kam Pui Yue` becomes `kam_pui_yue` and `Muhammad Hafiz bin Ismail` becomes `muhammad`. If the suggestion is taken, it adds `_2`, `_3` and so on within 12 characters. A name without ASCII slug characters starts with `person`. The form suggests a code while the name is entered, and the person can replace it with a code up to 255 characters, including an imported code. If they clear the field, Bilibi chooses the code. Updates cannot change a code. `preview_person_with_code` and `create_person_with_code` accept an optional explicit code and the person's custom field values, while `preview_person` and `create_person` use the default code and set none. Preview reserves nothing; creation checks availability under a tenant lock. Renames and retirement retain the code, and a database trigger refuses a code edit. Contacts and global search find a person by code. Downstream records should display this Core code rather than assign a customer or supplier code.

The module migration creates tenant-scoped bigint identities, fixed codes, personal
email/phone, explicit human/agent kind, version and creation/change metadata.
It changes no company or account ID. No HR table exists to map in Bilibi.
No employee number, employment state/dates/type, department, supervisor, account,
password or grant can be supplied in a Person input. It rejects unknown keys.

Names are trimmed and contain 1–160 characters; optional email/phone are trimmed
and blank becomes null. Email permits the source's `local@domain` shape, forbids
whitespace/control characters and is limited to 254 UTF-8 bytes; phone has a
64-character limit. Email/name are deliberately non-unique. `duplicates` matches
exact normalized name/email (case insensitive) or trimmed phone, bounds results
to 1–100, can exclude an existing ID and includes retired identities. It proposes
candidates without choosing, merging or linking them.

Every public read and write gates on contributed `admin.person.*` capabilities.
The tenant-owner role receives all Person commands; the walkthrough fixture's
manager does too, its staff can create and edit and its viewer can read/list. Existing installations run explicit
`bilibi-web authz-reconcile` after migration; the walkthrough explicitly
reconciles its disposable fixture before making calls.
`admin.person.system.manage` additionally requires the explicit platform operator.
No Person identity, agent kind or role declaration grants capabilities itself.

`persons` is registered with Grid and Authz. Personal email/phone restrictions
remove values from every read result, including duplicates and agent output.
General search only searches visible contacts; duplicate lookup refuses a
restricted contact criterion to avoid existence probes. A read model's
`restricted` names hidden fields; agent JSON returns `{"restricted":true}`.
The same catalog maps restrictions to Person audit evidence.

`update` replaces visible identity/contact fields and requires the expected
version. A missing/null/blank visible contact clears it. A restricted contact
must be missing/null/blank and is preserved, so an editor can rename a person
without learning or erasing hidden data. Row locks serialize writes; stale
versions fail. `retire` retains the identity, contacts and history while refusing
new edits/proofs/bindings. It is independent of People employment lifecycle.

Writes use `MutationTransaction::mutate`; old/new snapshots and the action
commit atomically. Audit failure rejects the write. `PersonService::subscribe`
receives tenant/Person ID notifications only after commit; dropping/rolling back
a transaction publishes nothing. Consumers filter tenant and reauthorize their
own reads; notifications contain no contacts. Broadcast is process local.

`core.person.list`, `.get`, `.duplicates`, `.create`, `.update`, `.retire` and
the `core.person` guide are contributed to Agent API. Record results include their `path` for review cards and other callers; the agent edge also supplies the absolute `url`. Writes use E2's reviewed
plans and their outer transaction, including replay protection. Agent input
schemas use the currently supported schema dialect: omit optional contacts on
create and use empty strings to clear visible contacts on update.

`PersonService::duplicates_on(client, scope, query)` runs the same bounded duplicate lookup as `duplicates` on a supplied PostgreSQL client or transaction. It re-proves the sealed Scope on that client, retaining actor, agent and impersonation evidence, then applies the existing authorization, restricted-criterion refusal and result masking. Earlier uncommitted Person writes in that transaction are visible. It opens no pooled connection or inner transaction, reserves no identity and leaves commit or rollback to the caller. The existing `duplicates` convenience method still acquires its own connection. The registered duplicate read uses its supplied `OperationClient` directly for reproof, lookup and custom-field projection, including the `Registry::call` transaction adapter.

## Update on a caller's transaction

**`update_person(tx, scope, id, &Update)` changes a person inside another module's own `MutationTransaction`, so that module's check and the person change commit, or roll back, together.**

- **One implementation.** `PersonService::update`, the `core.person.update` agent write and `update_person` run the same owner code: the `core/person.update` capability, version check, validation, field restrictions and masking, custom fields, the protected system identity and retired refusals, and one audited `person` change in the caller's transaction.
- **Who may call it.** A module whose operation is a user's request. Begin the transaction with that user's sealed `scope.mutation_context(...)`; a transaction for another tenant, actor or a system context is refused. The user still needs `core/person.update`: the caller's own capability and checks (such as a case's identity assurance gate) come first and never replace it.
- **Refuse before the write.** Run your own check first and return its error; the caller then rolls back and nothing is written or audited. A later failure in the caller's operation rolls the person change back too, and no change notification is published.
- **Review first.** `preview_update_person(client, scope, id, &Update)` runs the same checks on a read connection or transaction (a contract's preview) and returns the person before, the identity it would store and the custom values after. It locks and writes nothing.
- **Not for system principals.** A system-context owner uses `PersonService::update_provisioned_contact_in` (below).

## Exact contact lookup on a caller client

`PersonService::match_contact_in(client, scope, &ContactAddress, limit)` compares personal contacts; [Affiliation's lookup](../affiliation/README.md#exact-current-work-contact-lookup) compares current work contacts. Call both owners when both sources matter. The crate root exports `ContactAddress` and `ContactMatches<T>`. Neither API infers, merges, binds or reserves an identity.

- Scope's live account/tenant standing is reproved on the supplied client, preserving impersonator, agent and company facets. Caller HTTP/session and bearer adapters also revalidate their credential for each call, as in the ordinary owner facades. Live Person list and detail access are required. A hidden or masked criterion is `Forbidden`, even for a nonexistent value. Missing access is an error, never a complete empty answer.
- Email uses the existing Person validator and PostgreSQL case-insensitive `lower` comparison after trimming input. There is no provider-specific dot/plus rewriting. Stored spelling is preserved.
- Phone accepts only an explicit international spelling: remove ASCII spaces, hyphens, parentheses and dots, then require a leading `+` or `00` followed by 6–15 ASCII digits starting with a nonzero digit. Bare digits are refused, whether national (with or without a trunk zero, such as `(212) 555-1234`) or country-coded without a prefix (`60 12 345 6790`): no country is inferred from the digits, actor, tenant, company, postal address or locale. Extensions and other unsupported spellings are not canonical numbers either. Unsupported input is `Invalid`; any non-null unsupported stored phone sets `uncomparable`, so it cannot produce a complete negative or match another country's number. Email lookups never set `uncomparable`. Display values, write validation and Core Consent's own policy are unchanged.
- Limits are 1–100, with stable ID order. `overflow` means additional exact matches exist; `uncomparable` means some source values could not be checked. Only `complete()` (both flags false) makes the result exhaustive under this comparison policy. Preserve both flags when composing owners or criteria. Known matches can coexist with incompleteness.
- Retired identities remain candidates, consistently with duplicate lookup and detail reads. Erasure clears the contacts, including their derived index entries; an erased stub has no old address to match. A match is never a live Person proof.
- SQL stays in the storage owner. Nonunique expression indexes derive from the original values, following every normal, provisioning and privacy write without another stored contact copy. Candidate and incompleteness reads use one statement snapshot on the caller's client; earlier uncommitted owner writes are visible, and rollback leaves no lookup state.

### Concurrency boundary

This is a read prerequisite, **not an intake uniqueness reservation**. The existing mutation boundary uses PostgreSQL's configured transaction isolation (normally Read Committed); the lookup neither changes it nor opens a nested transaction. Separate owner lookups can therefore observe different committed snapshots. A caller needing one read snapshot can begin its transaction at Repeatable Read before any query, while retaining visibility of its own writes; that snapshot still reserves no absent address.

Person row/version locks protect existing records, and the tenant code lock protects code allocation. Neither coordinates contact intake with all independent Person/Affiliation/provisioning/privacy writers. A caller-only advisory lock does not fix this. Making only the intake transaction Serializable also cannot establish a reservation against writers that do not participate in that isolation contract. There is currently no supported bounded contact-reservation/retry contract across those writers. Intake that requires concurrent uniqueness must first establish a writer-wide coordination contract; this lookup does not claim that prerequisite is solved. Existing deadlock/lock-timeout retries roll back the entire outer operation, never just the lookup or its last write.

The [contact matching walkthrough](../affiliation/README.md#contact-matching-walkthrough) exercises both real owners on a disposable database.

## System identity and optional account binding

`ensure_platform_orchestrator` explicitly provisions Lara Example as an agent
in the operator tenant, resolved by `platform-orchestrator`, never a numeric ID
or employee number. Provisioning serializes concurrent calls and returns the
same record without overwriting edits. Its system key is not a general create
field, its kind cannot become human, and it cannot retire or use the generic
account-binding seam. Generic agents are ordinary contacts. Provisioning creates
no credentials/account, no automatic login and no Authz system-principal grant.
The bootstrap administrator works in the operator's company through an explicit desk on an ordinary agent Person under its own name. Core User (`bilibi_user::bootstrap`) creates that Person, its business affiliation and its first desk once per bootstrap receipt through `create`; the walkthrough fixture links its AI Agent account the same way.
The reserved key is globally unique. Moving the operator marker to another
tenant requires explicit identity/reference reconciliation; provisioning refuses
to manufacture a replacement in that tenant.

User owns optional staff account links (K9); Person does not add competing account storage or depend on User. `bilibi_person::lock_for_account_binding` supplies the small integration seam: a scoped, live, non-system Person locked on the caller's mutation transaction. It returns a non-deserializable `BindingIdentity` borrowed from that transaction with tenant, Person ID and kind, containing no contacts. The caller authorizes its account operation, then stores the tenant-scoped optional Person FK. Hold the Person lock through the later User write. A first desk is a separate Company → Person → User write owned by Core User. The seam never creates or implies an account link or desk, and retired/system identities refuse. Person creation never writes users, sessions, credentials or grants.

Tenancy's `PartyProofs::person` uses this owner to prove live scoped Person IDs;
that proof conveys existence, not a capability or account binding. `files`
(K15) is the private-file owner: the Media attachment subject `core.person`
and the Artifacts owner `core.person` (`bilibi_person::FILES`), deciding on the sealed
proof connection with `admin.person.view` for reads, `.update` for writes and
`.retire` for purge, a person of the actor's tenant (unretired for a write),
and the filing company reproved through `PartyProofs`. Use these, not a
hand-rolled check, for any file attached to a person. Principal
Directory providers name only bounded, live Person/agent candidates within the
request tenant. Agent candidates must have `kind=agent`. Base never reads Core
tables directly; the providers contain the owning queries.

`bilibi_person::read_person(client, scope, id)` accepts `&dyn bilibi_tenancy::ReadClient`. Composed reads, including billing-address owner checks, keep the caller's connection and the same `admin.person.view` check and contact masks. Retired Persons remain readable.

## Tenant-defined custom fields

**A tenant administrator adds text, number, date and pick-list fields to contacts without a release; Core Person keeps the values on the person and checks them in its own writes.** Contacts are the `base/custom_fields` record type `person` ([Custom Fields](../../base/custom_fields/README.md), ADR 21); values live in `persons.custom_fields` (migration `core-person-006-custom-fields`), keyed by each field's stable key.

- **One check, every door.** `Update.custom` (key to value, `null` clears; keys left out are kept) and `create_person_with_code`'s values go through `CustomFields::validate` in a preview and `validate_locked` in the write, so a changed pick-list code is held until commit. A key that is not an active field, a value of the wrong shape or a choice the list does not offer is refused on `cf_<key>`; a retired choice the person already holds stays valid.
- **Page.** The record page shows the active fields among the identity facts and edits each in place (`save_custom_field`, through `core.person.update` with the identity as it stands, so only that value changes). A date reads in the reader's format, a choice by its label (marked Retired when retired). The create dialog shows them after the contact details and posts `cf_<key>`; a refusal stays on its field and in the summary.
- **Agents.** `core.person.create` and `core.person.update` take `custom` as a list of `{key, value}` (the input dialect has no free-form object). Reads (`get`, `list`, `duplicates`) and write results return `custom` as `{key, label, type, value}` for the active fields. Find the keys with `base.custom_fields.list`.
- **Archived fields** leave the page and agent output, and a write naming one is refused; the value stays on the person and returns if the field is restored.
- **Not in this part.** List columns, sorting and filters on custom fields (as Company has), and per-field restrictions, which ADR 21 leaves out: a reader who may open the person sees the values.
- **Previews** of `core.person.update` and `.retire` read on the plan's own transaction, so concurrent plans never wait for a second pooled connection.

## Personal data

**Erasure anonymises the person to a stub that keeps its ID, kind and links, and marks it erased (`erased_at`).** Store `core/person.people` under [`base/personal_data`](../../base/personal_data/README.md), [ADR 05](../../../docs/architecture/decisions/05-personal-data-registry.md).

- Declares kind `person` (name, code, email, phone and custom field values; the record is a person) and erases `person`, `base/media/attachment` and `base/media/asset` evidence. Erasure clears `custom_fields`, and audit erasure clears `custom` in the person's history.
- The code, which may contain personal data, becomes `erased-<id>`, the only change the fixed-code trigger admits. The `erased-` prefix is reserved at creation and in the database, so an imported code cannot occupy another person's erasure stub.
- Erasure invalidates all cached Contacts CSV exports in the tenant in the same transaction as anonymisation, including exports whose filters or linked columns may contain the person. Concurrent export reading/publication is fenced against that transaction; later exports read the anonymised facts. After commit, all saved copies (including files of interrupted attempts) are removed through Artifacts, with retention and legal holds reported. Invalidated downloads fail immediately, even if physical cleanup must retry.
- Files are read and removed only through the grant-checked Media and Artifacts paths: a retired person's documents and those filed under another company are reached; no capability opens them otherwise. Each document deletion is recorded as it happens.
- Other people sharing an email or phone are counted, never touched.
- **Masking.** A masked `email` or `phone` holds its partial value, named in `masked`. The record page offers Reveal (the page contract `base.privacy.field.reveal`) to readers holding `privacy.field.reveal`; the full value stays only in that view while the record is unchanged and the reader may still reveal. Embedded contact panels show no Reveal. Saving another field leaves a masked contact as stored.

## Runtime walkthrough without unit tests

Use a freshly migrated disposable database, then run:

```sh
BILIBI_PERSON_DEMO=disposable cargo run -p bilibi-person --example person-walkthrough --locked
```

The example calls real app services and prints records/refusals for normalization,
duplicate candidates, concurrent stale saves, cross-tenant IDs, masked contacts,
live capability revocation, retirement, system-agent protection, optional binding
locks, transactional rollback and a deliberately refused audit action. A
Domain-style operation calls `update_person` on its own transaction: its check
refuses before the write (nothing written or audited), a user without
`core/person.update` and another user's scope are refused, a rollback leaves the
person unchanged, and a permitted change commits audited under the caller's
action. It also
executes create/update/retire through the real safe-write plan service and prints
write refusal without printing credentials or 2FA codes. It creates a temporary
operator marker only in the disposable DB and removes it. No People module,
unit tests, test runner or mocks are involved.

`person-custom-fields` proves custom fields through the same doors, each line `ok` or `FAIL`: definitions, refusals on `cf_<key>`, create and update through reviewed plans, audit, a stale version, the viewer and another tenant refused, a retired choice kept, an archived field, and 40 concurrent previews. It then serves the app for the browser (`APP_ORIGIN`, `BIND_ADDR`; `serve` serves only):

```sh
BILIBI_PERSON_DEMO=disposable cargo run -p bilibi-person --example person-custom-fields --locked
```

On a fresh disposable database, `BILIBI_PERSON_PROOF=disposable cargo run -p bilibi-person --example person-duplicates --locked` proves caller-transaction visibility and rollback, tenant separation, live denial/reproof, restrictions/masking and the original bounds. It exhausts a single-connection pool and calls the actual registered duplicate read handler with an `OperationClient` exposing that transaction, including its custom-field projection. It also calls the registered duplicate read through `Registry::call` on the bare transaction; that generic adapter need not expose `transaction()` for the duplicate read to retain its connection.

## Person screens and record panels (K8)

The Contacts menu entry (`admin.contacts`, under Core) is a Core contact directory. It has no employee number,
employment type/status/dates, supervisor, department or automatic account
controls. Human/agent is identity kind; current/retired is identity availability.
Retirement preserves history and is independent of HR employment.

The directory composes shared `ListState` and Grid `PageColumns`, with bounded 25/50/100/300 row pages, search through visible contacts, kind and retired filters, name/code sorting, accepted kind/identity/creation-date URL sorts and stable ID ties. Search, filters, sort, pagination and Grid `cols/lens/z/since` state survive reload and Back; column arrangements use shared per-account Grid persistence. The default columns are Name, Code, Work email, Role, Company, Personal email and Personal phone. Scalar identity/contact facts share their detail labels with Grid through the record-field schema; Affiliations contributes the work fields, including optional Work phone. Work details follow the affiliation owner's discovered Grid paths (`work.work_email:list`, `work.title:list`, `work.company.name:list`); multiple current affiliations roll up distinct values. Personal contacts come from the redacted Person read model, without an implicit work-contact fallback. Search also matches visible current work email, phone, role and company before pagination. Fresh restrictions prune both built-in and walked contact columns and suggestions, including on empty pages.

Creation opens a dialog on the Contacts list, the same shared modal that legal entity types use. The fields are the shared floating-label controls. It uses the owner validator, shared form errors and non-binding duplicate suggestions, and creates no login. A refused save stays in the dialog. A saved contact opens its page. Detail fields use shared inline text/choice
editors and per-fact commit states. Enter/blur commits, Escape cancels, nullable
contacts can clear, stale versions refuse, and lost write access keeps the page
with readonly facts and an explanatory outcome. Retirement uses a held record
version and shared confirmation dialog. History is the shared audit embed.

`person.contacts` is a reusable discovered embed. A company, affiliation or
account owner first resolves its relation through its public owner API, then
mounts it with `{panel: <stable anchor>, id: <Person ID>}`. It reauthorizes Person
reads and writes, masks restricted contacts, and edits only personal email and
phone. Never pass a company ID as a Person ID. K19 owns the company affiliation
list/editor and work contacts; no guessed company-to-person link exists here.

Person detail automatically mounts contributed embeds whose context is
`Record { kind: "core/person" }`, except its own `person.contacts`. The path
carries `panel`, `id` and `person_id`; anchors are stable and browser-owned.
Affiliation, Address, User and relationship owners can contribute their panels
without Person importing them or requiring People. Each panel is separately
guarded by the host. HTTP rendering leaves child anchors for live mounting.

The manual `person-contact-panel` example serves an independent record host at
`/person-contact-reference/{id}` so the shared contact embed can be tried outside
Person detail. Run it with a migrated disposable `DATABASE_URL` (it installs the walkthrough
fixture):

```sh
APP_ORIGIN=http://127.0.0.1:4499 BIND_ADDR=127.0.0.1:4499 \
  cargo run -p bilibi-person --example person-contact-panel --locked
```

Sign in, open the reference route with a Person ID created with the New contact dialog on `/contacts`,
edit a personal contact, and verify it in the ordinary Person page. The reference
host resolves the Person through its public API on every update; a production
company host additionally resolves its owned affiliation before supplying that ID.

The directory uses the FlexTable configuration, retaining the approved Person model's identity filters, name sort and additional 300-row page size. It omits HR columns; company work contacts come only from explicit current affiliations. Large pages load walked columns in
batches bounded by Grid's `query::MAX_ROWS`; they retain every visible row.
The customization notch offers show/hide, drag reorder, row-height steps,
Compact/Normal presets and Reset. The account's column order, lenses and row
height are saved automatically by shared `PageViews`; an explicit column URL
wins on load. The persisted
account arrangement and shareable URL are the saved-view contract here.

## Step-up

- Retiring a person is the Base Step-up operation `core.person.retire` (`src/step_up.rs`), off by default.
- A tenant that turns it on in `/authz/step-up` gets the account holder's code asked for both the page facade (`PersonService::retire`) and the agent write (`core.person.retire`).
- See `apps/base/step_up/README.md`.

## Postal addresses

Companies and contacts have linked postal addresses, owned by [Core Address](../address/README.md). The `core.address` guide explains saving an office or billing address and attaching it to a company or person. Use `core.address.company.list` or `core.address.person.list` to read those links. Address creation and attachment are reviewed writes; a namecard contact can be created first, then linked to its office address. An affiliation holds a position and work email/phone, not a postal address.

`PersonService::create_in` composes the same authorized creation inside a caller-owned `MutationTransaction`, so atomic account provisioning never opens an inner transaction. It retains validation, field policy and after-commit notifications.

`PersonService::provision_in` is the same creation for a system principal (`Context::system`) that has authorized it itself, such as company sign-in creating an account. It checks no capability and refuses a user context.

`PersonService::update_provisioned_contact_in` updates a provisioned Person in the same caller-owned system transaction as account provisioning. The caller proves its ownership; the owner retains validation, retirement checks, audit capture and notifications.

For the fixed code and name of an already authorized linked record, and bounded code lookup across Person and Company, use the [shared party identity facade](../../base/tenancy/README.md#linked-party-identities-and-code-lookup). Person supplies only identity and retirement status there; personal contacts still require the existing view permission and field masks.

## Linked agent steps

`core.person.create` declares its newly created person for later same-plan references. An affiliation or address step can use that ID in the same reviewed approval. See [same-plan record references](../../base/agent_api/README.md#new-records-in-later-steps) for the wire format and owner contract.
