# Watchers

**Use it for:** a tenant's own "when this happens, do that" with no release: offer an event of yours as a watcher trigger and a write contract of yours as a watcher action.

**Not for:** a check that must stop a save (keep it in your operation), per-tenant code branches, or a workflow or approval engine of your own.

A watcher is data ([ADR 28](../../../docs/architecture/decisions/28-tenant-customisation-is-data-on-shared-servers.md), [ADR 31](../../../docs/architecture/decisions/31-tenant-watchers-run-after-commit-as-one-bounded-principal.md), #682). It says: when a trigger event happens to a record, if these conditions over the record's fields hold, run these actions. Omit `if` to act on every event of the watched kind. A person drafts it on the Watchers page or through an AI agent; an administrator approves one fixed version; that version runs on every new event, with no AI beyond an optional declared [AI observation](#ai-observations).

## What a watcher is

A draft uses `watch` for the event, optional `if` for its filter, and `do` for operation steps, for example `{"watch":"cx.cases.opened","if":{"match":"all","conditions":[...]},"do":[{"operation":"cx.cases.route","input":{"queue":"vip"}}]}`. Draft metadata and examples accompany this definition.

| Part | What it holds | Who supplies it |
| --- | --- | --- |
| Trigger | An event an owner records in its own transaction (`watcher_cases/cases.opened`) | An owner's `Trigger` contribution |
| Conditions | When `if` is present, `all` or `any` of one or more conditions, each with a field, operator and any required value | The watcher |
| Actions | 1 to 10 owner write contracts, each with approved parameters | Owners' `Action` contributions; the watcher selects actions and supplies their parameters |
| Examples | 1 to 10 fact sets for a filtered watcher; up to 10 otherwise with expected `match` or `no_match` outcomes | The watcher's author |

- **Fields** are Grid column paths on the trigger's table (`priority`, or `company.name` through a one-link), read as the `watchers` principal. No aggregates.
- **Tenant custom fields** (`cf_<key>`, [Custom Fields](../custom_fields/README.md#watcher-conditions)) can be used in conditions on the trigger's own record (`cf_segment`) or on a record one direct link away (`company.cf_segment`). Drafts that reference an unoffered or archived field are refused. For an offered linked field, the vocabulary gives the full path and a label such as `Company › Segment`.
- **A linked record's field is read with the trigger's record**, in the same bounded statement and facts budget, as the `watchers` principal: it needs the linked table's read capability and reach to that record. Approval of a linked custom condition requires automation to read the field through both the trigger and linked tables. A run refuses a linked custom field when automation loses access to its table (`read_refused`), or a linked record when it is out of reach (`subject_unavailable`). A record with no linked record reads the field as missing.
- Undoing a custom-field change that would withdraw an approved watcher's condition is refused after checking the active definitions; the correction preview names the dependency when the reader may see it.
- **Operators**: `eq`, `ne`, `in`, `not_in`, `lt`, `le`, `gt`, `ge` (numbers and dates), `empty`, `not_empty`.
- **Values compare exactly**: numbers as exact decimals (integer JSON numbers or decimal strings; fractional JSON numbers and values beyond decimal precision are refused), dates and times as instants, text as written, true/false as such. A missing value matches only `empty`; every other comparison with it is false.
- **No "update any record" action.** An action is one owner's contract with its own schema, preview, checks and audit. The runner binds the action's subject field to the trigger's record; a watcher never names a record, tenant, principal or operation outside the installed set.

## JavaScript steps

A `do` step can carry a named JavaScript function body under `script`. The connector drafts it as part of the watcher version. Its approval pins the source, language, SHA-256 source digest, fields, examples, permitted calls and inputs together. Each invocation receives only its declared fields from the scoped trigger record. A script can branch, calculate and return an ordered array of calls. An empty array skips the actions. [ADR 34](../../../docs/architecture/decisions/34-tenant-scripts-run-through-operation-contracts.md)

```json
{
  "script": {
    "name": "Route urgent cases",
    "source": "const desk = lookup('watcher_cases.cases.lookup', {case_id: 7}); return facts.priority === 'urgent' ? [{operation:'watcher_cases.cases.route',input:{queue:desk.title}}] : [];",
    "fields": ["priority"],
    "lookups": [{"operation":"watcher_cases.cases.lookup","input":{"case_id":7}}],
    "calls": [{"operation":"watcher_cases.cases.route","input":{"queue":"priority"}}],
    "examples": [
      {"facts":{"priority":"normal"},"lookup_fixtures":[{"operation":"watcher_cases.cases.lookup","input":{"case_id":7},"reply":{"title":"priority"}}],"expect":[]},
      {"facts":{"priority":"urgent"},"lookup_fixtures":[{"operation":"watcher_cases.cases.lookup","input":{"case_id":7},"reply":{"title":"priority"}}],"expect":[{"operation":"watcher_cases.cases.route","input":{"queue":"priority"}}]}
    ]
  }
}
```

- The tenant grants `watchers.script.execute` separately in Automation access. Approval also records permission for the named watcher principal to run each exact script declaration in that version. Runtime checks both before starting that script, so permission for one digest never admits another. The draft and approval reviewer needs the execution capability to run that particular version's examples. The record owner binds the subject and checks each write through its normal operation.
- An approved version also records an exact-digest permit for person-initiated calls. `base.watchers.script.run` takes the approved watcher, version, script position and target record. It reads the declared facts and current record version under the caller's live reach, evaluates the same sandbox, validates the complete selected plan, then commits every owner operation in the caller's one transaction. The Watcher page calls it as a page action. MCP delegates it as the connected person, and `bilibi-axi plan/commit base.watchers.script.run ...` reaches the same API contract. A Schedule worker calls it through `JobContracts` as the `watchers` principal; the queued attempt rechecks the principal's live grant and that exact script permit before any effect.
- An owner offers one of its read contracts with `LOOKUPS`. A script declares an exact operation and input; its examples carry captured replies. Approval fingerprints the schemas and requires the approver's authority. Runtime calls each declaration through Agent API as the current caller, with its live capability, field restrictions and record reach, then exposes only the reply to `lookup(operation, input)`. Another operation or input is refused.
- The host accepts only complete operation and input pairs in the declaration. An owner certifies a watcher action as local through its reviewed write handler, which declares that its effects stay within the installation. The private Activities follow-up handler is eligible. The worker receives no connector credentials and has no filesystem, network or database interface.
- **Outbound terms.** An owner marks an action that sends data outside Bilibi as `outbound` in its write handler (`apps/base/agent_api/README.md`); the vocabulary lists it with its destination and data fields. Only a script step may select it, and the script names the one destination and the data fields it may send under `outbound`, for example `"outbound":[{"operation":"watcher_cases.cases.notify","destination":"desk@partner.example","data":["title","priority"]}]`. Draft refuses an outbound action in a plain step, a script call without terms, data the owner never sends and differing terms for one operation across steps. Approval shows the terms, and they are part of the script's exact permission, so a new recipient or broader payload is a new version. Every run (queued, page, command line, delegated agent, Schedule) asks the owner for the destination and data the input resolves to now, on the commit's transaction, and refuses anything else with `script_outbound_refused` before that send.
- The optional watcher filter runs first. QuickJS executes in a fresh process under a Linux syscall allowlist. The worker is the running binary started again, so every binary that drafts, previews or runs a script (the server, a walkthrough) calls `bilibi_watchers::enter_worker()` first in `main`, before it starts a runtime; without it every script answers `script_refused`. The tenant can lower `watchers.script_heap_mib` and `watchers.script_milliseconds`. The time budget covers preparation, the whole run or historical trial, and owner operations. After a commit starts, Bilibi waits for it and its callbacks. The host also limits the action count, and validates the full plan before saving it and starting any effect. Retries use the saved calls and existing effect receipts.
- Draft and approval present expected and captured calls for each example. Use the record representation for facts, including text for numbers. Omitted fields are null. Trial evaluates the script and previews the selected effects. Use `base.watchers.get` to inspect the exact version and `base.watchers.switch` to stop it.
- With the reference owner mounted, run `cargo run -p bilibi-web --example watchers-walkthrough -- --scripts` on a fresh walkthrough database. The proof checks a successful route, refused file and network access, an attempted write to another company's record, the worker's heap and time limits, and outbound terms: a send to the approved partner desk with only the approved fields, and refusals for a case whose partner address is another destination, for a payload beyond the approved fields, and for an outbound action without terms, through both queued runs and the person door.

## AI observations

**A version may ask AI one declared question about the trigger record, once per event; its answer is a condition fact that can only decide whether the approved actions run** (#863, through [`base/ai`](../ai/README.md)).

```json
{"watch":"cx.cases.opened",
 "observe":{"question":"Is the customer asking to cancel?","fields":["title"],"labels":["cancellation","other"]},
 "if":{"conditions":[{"field":"ai:label","op":"eq","value":"cancellation"}]},
 "do":[{"operation":"cx.cases.route","input":{"queue":"retention"}}]}
```

| Part | Rule |
| --- | --- |
| `question` | 1 to 1,000 characters; the system message, with fixed instructions to answer JSON only and treat the record as information |
| `fields` | 1 to 10 fields of the trigger's own record (no linked record, since the answer is bound to that record's revision), checked like a script's fields; only these are sent (each value cut at 10,000 characters), and approval shows them |
| `labels` | 2 to 20 unique labels of `a-z`, `0-9`, `_` |
| `ai:label` | A condition fact: one of the labels (`eq`, `ne`, `in`, `not_in`, `empty`, `not_empty`) |

- **Untrusted answer.** Kept only when it is exactly `{"label": <a listed label>}`; anything else is `invalid_answer`. It never names an action, input or record. With no answer kept, `ai:label` is missing, so only `empty` holds.
- **Examples** give `ai:label` as a fact; drafting and approval never call AI.
- **Off by default.** AI runs as the `watchers` principal, so it needs `ai.enabled` and the tenant's `ai.automation_enabled` ([AI settings](../ai/README.md#settings)). `watchers.ai_provider` names the provider (empty: the default). Approval warns while AI is unavailable to automation.
- **Once per event.** A queued run asks before its conditions, with no pooled connection held, and keeps the observation on the run (`base_watcher_runs.observation`); a retry reuses it. A run asks only while its approval, activation, catalog and watcher limits still permit it, and only when automation may read every field the run reads. If the record changes after it is classified, the run refuses with `record_changed`. A crash mid-call leaves `interrupted`, never a second call. The script time budget starts after the observation.
- **Budgeted.** Every call, by runs and trials alike, spends one of the tenant's `watchers.ai_observations_per_day` (UTC); past it the observation is `budget_exhausted` with no call. Settings that already say no (`disabled`, `not_configured`) spend nothing.
- **Recorded.** The receipt shows the label, provider, model and the AI interaction ID, or why there is none. Each call also leaves its metadata row in AI Interactions. No record text is stored.
- **Reasons for no answer:** `disabled`, `not_configured`, `timeout`, `provider_error`, `invalid_answer`, `budget_exhausted`, `interrupted`; a field automation cannot read refuses the run (`read_refused`, `restricted_field`, `subject_unavailable`).
- **Receipts and privacy.** A reader sees an observation, and which conditions held, only when they may read every field it sent.
- **On the page.** The version shows the question, labels and fields sent; Draft next version keeps the observation and offers AI label as a condition field; an agent drafts a change to the observation itself.

## Versions and approval

**A watcher changes only by a new version; one approved version runs.**

| Step | Contract | Capability | What it checks |
| --- | --- | --- | --- |
| Draft | `base.watchers.draft` | `admin.watchers.manage` | Installed trigger; fields the drafter may read, values of their type; installed actions on the trigger's record with inputs their contract accepts; examples come out as stated |
| Approve | `base.watchers.approve` | `admin.watchers.approve` | The draft checks again as the approver, plus: it is the latest version; the approver holds All reach for the trigger table, every table its conditions walk and every action's capability and declared human prerequisites; no condition field is restricted or masked for the `watchers` principal; its actions close no cycle in the active watchers' declared events; the tenant's limits hold |
| Restore | `base.watchers.restore` | `admin.watchers.approve` | An earlier approved version repeats its current permissions, bindings, examples, limits and cycle checks before it replaces the active version |
| Switch | `base.watchers.switch` | `admin.watchers.approve` | On again re-runs the approval checks on the running version |

- **Versions never change.** Each stores its definition, examples, the person's words, a SHA-256 fingerprint, how it was drafted (`person` or `agent`), who drafted it and who approved it.
- **Drafting runs nothing.** A new version of a running watcher runs only once approved; until then the earlier version keeps running.
- **A queued run keeps its pinned version.** If another version is approved, the watcher is switched off, or it is switched off and on before the run acts, the run is refused (`stale_version`, `watcher_off`, `stale_activation`); nothing silently swaps in the new watcher or activation.
- **Installed bindings are pinned.** Approval stores a fingerprint of the condition fields' types, choices and source mappings (for a custom field, of the trigger's record or a linked one, its type and value SQL while it is offered to watchers), every linked table's join and key bindings, the trigger bindings and the action schemas. A changed catalog refuses execution (`catalog_changed`) and marks the watcher **Needs review** until a new version is approved.
- **Earlier approved versions can be restored.** Undo an activation from Customisations. A refusal leaves the version running now active.
- **Approval never delegates more.** The approver must hold All reach for every capability the watcher reads or acts on, including linked tables; Own or Team reach cannot approve or switch on tenant-wide automation; a watcher cannot reach a field the principal may not read.

## How a run works

**Recorded with the trigger's save, run after it commits, never inside it.**

1. The owner records its event in its `MutationTransaction` (`base/events`).
2. Events hands it to the `base/watchers.triggers` subscriber. In the delivery transaction it writes one run per approved watcher for that event in the tenant, pinned to its version and current activation. An event recorded after both approval and the latest switch-on queues work for the `base/watchers-admit` worker (`Subscribe::handoff`). An event delivered while off, or recorded at or before approval or switch-on, writes a refusal directly to the run log.
3. The worker runs as the `watchers` principal. Per run it checks the chain budget, the version and the switch, reads the conditions' fields through Grid, evaluates them and records `matched` (with the facts and each condition's outcome) or `no_match`.
4. Before each outstanding action, including after a retry, the worker rechecks the principal's current record and field access. The evaluated record version is bound into the action; its owner checks it under the record lock and refuses a changed record (`record_changed`). Successful actions advance the next action's expected version in the same transaction; the original version remains in the saved facts. Each action calls the owner's contract through the job door (`JobContracts::commit`) in a transaction that also writes its receipt, keyed `(tenant, version, event, position)`. A retry after a crash finds the receipt and does nothing twice.
5. The run ends `done`, `partial` or `refused`, with a bounded reason code.

| Reason | Means |
| --- | --- |
| `chain_budget` | Another watcher's action started this run and the chain reached `watchers.chain_depth` |
| `stale_version`, `watcher_off` | The watcher changed before the run acted |
| `event_before_enabled`, `stale_activation` | The event predates switching on, or the watcher was switched off and on after the run was queued |
| `event_before_approval` | The event was already recorded when this watcher version was approved |
| `read_refused`, `restricted_field` | The principal may not read the record or a condition's field |
| `subject_unavailable` | The record is gone or out of reach |
| `observation_missing` | The run reached its conditions without its AI observation |
| `action_limit` | The watcher is over the configured action count or actions-per-run limit |
| `watcher_limit` | The watcher is over a configured size or fact limit |
| `attempt_limit` | The job exhausted its configured attempt budget; waiting runs are refused |
| `actor_refused` | Queue refused the principal (a suspended tenant, a withdrawn grant); told through Queue's `EndingBinding` |
| `catalog_changed` | Installed fields, trigger bindings or action schemas changed, a condition's custom field was archived or withdrawn from watchers, or an owner was unmounted; draft and approve a new version |
| `record_changed` | Skipped: the record changed after the watcher matched |
| `action_refused` | An action's owner refused it; the effect's receipt keeps the owner's own words |
| `script_call_refused` | A script requested an operation outside its approved list |
| `script_lookup_refused` | A script requested a read outside its approved operation and input, or the read owner refused it |
| `script_permission_refused` | The caller has no permission for this exact script declaration and version |
| `script_outbound_refused` | An outbound action resolved a destination or data outside the script's approved terms, or has none |

## Authority

**One named principal, `watchers`, under declared bounds and the tenant's live grants.**

- Its declaration holds `watchers.run.execute` and `watchers.run.settle` (a standing grant: recording why a run ended is the way out, never an effect).
- Owners use `bilibi_authz::OfferedCapabilities` to offer the Grid table's read key for a trigger or linked record and the action contract's key for an effect. Base names no owner.
- A tenant administrator grants each key on Administration › Authorization › **Automation access**, as for every principal, and allows event delivery (`domain-events`) there too. Until then runs are refused, visibly (or, without delivery, never queued: the Events page shows the waiting deliveries). A direct deny wins.
- Each owner admits the principal to its contract with `bilibi_agent_api::job::PrincipalWrite`; the contract then decides its capability live, on the principal's own grants, as at every door.
- **Never automated**: contracts of `base/authz`, `base/authz_agent`, `base/workflow`, `base/step_up`, `base/watchers`, `base/tenancy`, `base/session`, `core/user`, `core/tenant` and `core/agent`, and any contract that declares step-up. Boot refuses such an action.
- The principal receives every field restriction in its tenant (`apps/base/authz/README.md`), so a restricted field is refused at the source, at approval and at run time.

## Limits

**Every bound an operator could reasonably set is a setting** (System Settings, group Watchers, tenant or global; editable with `admin.watchers.approve`).

| Setting | Default | Range | Bounds |
| --- | --- | --- | --- |
| `watchers.actions_per_run` | 3 | 1–10 | Actions one run takes; a watcher with more is not approved |
| `watchers.chain_depth` | 3 | 1–10 | Watchers in a row one first change may start (the causal-chain budget) |
| `watchers.runs_per_minute` | 600 | 1–10,000 | Tokens refilled per minute and maximum tokens held by a tenant |
| `watchers.active_per_trigger` | 20 | 1–100 | Watchers switched on for one trigger |
| `watchers.conditions` | 20 | 1–20 | Conditions in a watcher |
| `watchers.actions` | 10 | 1–10 | Actions in a watcher; `watchers.actions_per_run` also applies |
| `watchers.examples` | 10 | 1–10 | Examples stored on a watcher version |
| `watchers.values_per_list` | 50 | 1–50 | Values in one condition list |
| `watchers.facts_read` | 21 | 1–21 | Distinct condition field paths and the record version |
| `watchers.max_attempts` | 25 | 1–25 | Attempts per watchers job, including the first |
| `watchers.trial_days` | 30 | 1–365 | Longest past window a trial can examine |
| `watchers.trial_events` | 100 | 1–500 | Most readable events returned in a trial |
| `watchers.ai_observations_per_day` | 100 | 1–10,000 | AI observations runs and trials ask per UTC day |
| `watchers.trial_observations` | 20 | 1–100 | Events a trial of an observing draft asks AI about (also its event limit) |

`watchers.ai_provider` (text, default empty) names the provider observations use.

Each tenant starts with a full bucket shared by its watchers. A new run spends one token. An admitted run keeps its token across retries. Tokens refill continuously up to one minute of the current rate, which also applies to the time elapsed since the last debit. When the bucket is empty, Queue keeps the same head job scheduled for the next token with its attempt budget intact. Admission jobs wait in tenant enqueue order and admit an event's runs by run ID. Each debit and its action job commit together. Action jobs execute in order within their event, and action retries leave later events free to admit and execute.

The definition's hard ceilings stay fixed; settings can lower them. The facts budget counts distinct condition paths plus the bound record version (once). Draft review, approval, switch-on and execution apply the current size and fact limits, including before each outstanding action. Queue keeps a hard ceiling of 25 attempts; the watchers worker allows the configured number of attempts, including the first, and cancels with `attempt_limit` when exhausted. Queue’s ending hook refuses the event’s unadmitted runs when an admission job ends, or the individual run when its action job ends.

The chain follows causes, not traces: the worker names its run as the cause of the action's transaction (`Events::cause`), Events stores it on every event recorded there (`Event::cause`), and the next run's depth is its cause's plus one. Independent events of one import keep depth 1.

## Offering a trigger and an action

```rust,ignore
inventory::submit! { Contribution::new(bilibi_watchers::TRIGGERS, CASE_OPENED, provenance!(), &bilibi_watchers::Trigger {
    on: On { event: CASE_OPENED, versions: &[1] }, title: "A case is opened", table: "cases", subject: "/case", logical_event: None, version: "version",
}) }
inventory::submit! { Contribution::new(bilibi_watchers::ACTIONS, "cx.cases.route", provenance!(), &bilibi_watchers::Action {
    title: "Send the case to a queue", table: "cases", subject: "case_id", version: "expected_version", records: &[CASE_ROUTED], approval_capabilities: &[],
}) }
inventory::submit! { Contribution::new(bilibi_watchers::LOOKUPS, "cx.cases.get", provenance!(), &bilibi_watchers::Lookup {
    title: "Read a case",
}) }
inventory::submit! { Contribution::new(bilibi_agent_api::job::PRINCIPAL_WRITES, "cx.cases.route", provenance!(), &PrincipalWrite { principals: &["watchers"] }) }
inventory::submit! { Contribution::new(bilibi_authz::OFFERED_CAPABILITIES, "watchers:cx/cases", provenance!(), &OfferedCapabilities {
    principal: "watchers", capabilities: &[CASES_VIEW, CASES_ROUTE],
}) }
```

Boot refuses a trigger that is not the contributor's own declared event or names no Grid table the principal may be offered; an action that is not the contributor's reviewed write, does not admit `watchers`, lacks the subject or expected-version input, records an undeclared event, or would grant access or decide an approval. The owner's contract must accept a principal scope (`scope.user()` is `0`) and recheck its capability, expected record version and record rules in its commit; a company's write also holds `require_writable`.

`Action::approval_capabilities` declares additional capabilities a human needs to perform the action, including nested writes that use a different permission for the principal. Approval and re-enabling require the operation's own capability and every prerequisite with All reach; direct denies win. Boot checks the keys against the Authz vocabulary, and the approval fingerprint includes the declaration. This adds no principal bounds or consent: the executor and owner still check the principal's own permissions live. Use `&[]` when the operation capability suffices. Shared follow-up registration requirements belong to [Activities](../../core/activity/README.md#follow-up-operations-offered-by-record-owners).

`Watchers::action_identity(client, scope)` returns the executing watcher version, action position and captured logical event key as an opaque identity, only inside a Watchers action transaction under the `watchers` principal. An owner may combine it with its source kind and ID to deduplicate multipart events without merging independent watchers or manual requests. This identity grants no authority; owner checks still apply.

`Trigger::logical_event` is an optional JSON pointer to the owner's immutable snapshot key in the event payload. Set it for multipart events: every part of one occurrence carries the same nonempty string (at most 200 bytes) or integer, and separate occurrences carry different keys. `None` means each recorded event ID is a separate occurrence. A declared key that is absent or invalid refuses delivery; it never falls back to the live record revision. Watchers captures the key, namespaced by event name, on each run during delivery, so later source edits and event retention cannot change it. The pointer is part of the approval fingerprint. `expected_revision` remains solely the current write fence. Runs predating capture refuse with `event_identity_missing`; development databases are disposable.

## Trying a draft on past events

**Call `base.watchers.trial` with a draft, optional `days` and `limit`; it never acts.** It requires both `admin.watchers.view` and `admin.watchers.manage`. It checks the draft as its reader, scans retained, unerased events in recorded order in the bounded window and evaluates their subjects with the live condition checker. The response identifies event-time evidence (event ID, recorded time and subject) separately from the condition facts and revision read from the current record. A near miss is an event whose accessible current record does not satisfy the conditions. Deleted, unreachable or restricted records and unsupported event versions do not count toward the return limit. Truncation indicates another readable event or the response size limit. A trial examines at most 5,000 candidate events, including inaccessible ones; if it reaches that work limit, it says the scan stopped early and asks the reader to narrow the window, without reporting how many events were hidden or examined. Event retention may leave gaps.

For a draft with an [AI observation](#ai-observations), the trial first picks the events it will show, asks each once as the `watchers` principal within the daily budget (one at a time, with no connection held), and shows each event's `observation`. The audited start receipt is written before the first call. Each call requires `admin.watchers.approve`, current access to the event, automation's access to every field the trial evaluates, and AI available to automation. Without approval permission, trial rows show `not_permitted` and spend no observation budget. A changed record revision cannot reuse the earlier answer. The event limit is at most `watchers.trial_observations`. For a match, the watcher principal calls the action owner's preview at the current revision. An unrestricted reader with All reach for the action and its prerequisites receives the owner's effect details. The trial checks preview visibility in the same snapshot as the preview. Follow-ups reuse the owner's logical occurrence deduplication. Each preview uses the current record independently of changes an earlier action would make. The trial takes no action and leaves the token bucket untouched. The audited start receipt records the actor, trigger, definition digest, window and return limit, and survives interruption. Individual facts larger than 4 KiB are omitted and the response is capped at 512 KiB. See [receipt and trial evidence](#pages-and-agents) for linked custom fields.

## Pages and agents

- **Administration › Watchers** (`/watchers`, `admin.watchers.view`): every watcher, its trigger, whether it is on, the running version and the last run. **New watcher** (`admin.watchers.manage`) opens the drafting dialog; `/watchers/{id}` has **Draft next version** (same dialog, filled from the latest version; a watcher keeps its trigger). The watcher page shows the latest version and, when different, the running version with their declared fields, lookups and actions. An administrator can approve the latest draft, use the on/off switch and read the latest 25 receipts. Restore an earlier activation from Customisations.
- **Drafting on the page** builds the input of `base.watchers.draft` (page events `draft_review` and `draft_save`, the one write path) and nothing else: the trigger and actions are searchable comboboxes from `base.watchers.vocabulary` (actions limited to the trigger's table), conditions pick a field path from the trigger's vocabulary with the operators and a value control of the field's type (a choice list for enums, Yes/No for true/false, a date, a number; lists take one value per line), and each example is a row of the condition fields' values with matches or does not match. **Review** previews the draft through the contract and shows its conditions in words, actions, each example's result, warnings and problems; **Save draft** is enabled only for a reviewed draft, and any edit discards the review. Saving lands on the watcher, where a holder of `admin.watchers.approve` approves it.
- **Receipts and privacy**: a receipt's facts and condition outcomes are the record's values, except that stored run facts and trial results use `{"omitted":"linked custom field"}` in place of the value at each linked custom-field path; condition outcomes are reported separately from facts. Current catalog bindings, record reach and field restrictions apply to every read, including values collected before a restriction changed. A reader who cannot read the subject or any recorded field sees the evidence as restricted. Run and action statuses and reasons remain visible. Protected facts and condition outcomes are kept out of audit metadata.
- **Agents and command line**: `base.watchers.vocabulary` (installed triggers with their fields, lookups, actions, operators and limits), `base.watchers.list`, `base.watchers.get`, `base.watchers.runs`, `base.watchers.trial`, `base.watchers.draft`, `base.watchers.approve`, `base.watchers.restore`, `base.watchers.switch`, `base.watchers.script.run`, and the guide `base.watchers`. An agent's draft records `drafted_via: agent`; it runs only once a person approves it. A delegated agent and `bilibi-axi` run a script only through the reviewed run operation and retain their person's authority.

## Setup packs

Export a latest unapproved version and preview it as a new destination draft through [Setup packs](../setup_packs/README.md). A script step travels once its source and examples pass the customer-value scan described there (`src/transfer_scan.rs`). Reverse an initial unapproved draft through Customisations. The draft discard operation refuses a watcher after it has been revised or approved.

## Tables

`base_watchers`, `base_watcher_versions`, `base_watcher_runs`, `base_watcher_effects`, `base_watcher_buckets`, `base_watcher_trials`, `base_watcher_script_permissions` and `base_watcher_ai_budget`. Account IDs on versions and script permissions are evidence, not foreign keys: Base names no Core table.

## Seeing it work

`apps/web/examples/watchers-walkthrough.rs` on a fresh disposable database, migrated and seeded first (its header has the commands). It calls startup validation with a customer Extension in the supplied composition, proves refusal in Shared service and admission in Isolated installation, and calls the protected owner mutation to prove that switching to shared refuses and preserves the saved mode. A tenant administrator is refused by the same mutation. Unmounted, the engine offers no trigger and refuses a draft on an unknown one. With the reference owner `fixtures/containers/watcher_cases` mounted (never a Platform member), it runs the same approved watcher in both installation modes: an urgent 1500.00 case goes to the priority desk exactly once, and a normal case remains unchanged. It also proves two tenants with the same facts and different destinations, a restart and a crash-retry with one effect, an agent's draft approved by a person, a person's draft in the page's input shape and its next version, refusals for a denied capability, a restricted field, an archived company, a suspended tenant, a stale version and a switched-off watcher, a condition on a tenant custom field (refused until offered, then routed; refused when the field is restricted or archived), approval, routing, trial and receipt checks for a [linked custom condition](#what-a-watcher-is), self-triggering and multi-watcher cycles refused at approval and a noncyclic chain stopped at the chain budget, a threshold change as a new version, and a burst that shares the tenant bucket across watchers and resumes in order as tokens refill while another tenant runs independently.

With the reference owner mounted, `--observations` proves AI observations against a local fake OpenAI-compatible server: refused drafts on an undeclared observation or an unlisted label; approval showing the question, labels and the only field sent, and warning while AI for automation is off; no call while off; a likely cancellation routed once with only the declared field sent and one AI Interactions row; another request and an injected unlisted label acting on nothing; an interrupted call never asked twice; the daily budget; another tenant staying off; a trial showing each event's observation with no run or effect; and the receipt.

The official Domain companion walkthrough is `apps/web/examples/watchers-domains-walkthrough.rs`. Commerce offers order creation and status changes as single events and proves a two-line order triggers one status action; its status-change trigger also offers the follow-up task. See the [companion handoff](../../../docs/watchers-logical-event-handoff.md) for mounts, source prerequisites and execution instructions.

## Not yet

- Standalone script records are covered by [ADR 34](../../../docs/architecture/decisions/34-tenant-scripts-run-through-operation-contracts.md). Script versions currently remain inside watcher versions; watcher events, pages, scheduled jobs, the command line and delegated agents share their execution contract.
