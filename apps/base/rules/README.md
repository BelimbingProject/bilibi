# Rules

**Use it for:** a tenant's own "when this happens, do that" with no release: offer an event of yours as a rule trigger and a write contract of yours as a rule action.

**Not for:** a check that must stop a save (keep it in your operation), tenant scripts or per-tenant code branches, or a workflow or approval engine of your own.

A rule is data ([ADR 28](../../../docs/architecture/decisions/28-tenant-customisation-is-data-on-shared-servers.md), [ADR 31](../../../docs/architecture/decisions/31-tenant-rules-run-after-commit-as-one-bounded-principal.md), #682). It says: when a trigger event happens to a record, if these conditions over the record's fields hold, run these actions. An AI agent drafts it from a person's words; an administrator approves one fixed version; that version runs on every new event, without AI.

## What a rule is

| Part | What it holds | Who supplies it |
| --- | --- | --- |
| Trigger | An event an owner records in its own transaction (`rule_cases/cases.opened`) | An owner's `Trigger` contribution |
| Conditions | `all` or `any` of: a field of the trigger's record, an operator, a value | The rule |
| Actions | 1 to 10 owner write contracts, each with approved parameters | Owners' `Action` contributions; the rule selects actions and supplies their parameters |
| Examples | 1 to 10 fact sets with expected `match` or `no_match` outcomes | The rule's author |

- **Fields** are Grid column paths on the trigger's table (`priority`, or `company.name` through a one-link), read as the `rules` principal. No aggregates.
- **Operators**: `eq`, `ne`, `in`, `not_in`, `lt`, `le`, `gt`, `ge` (numbers and dates), `empty`, `not_empty`.
- **Values compare exactly**: numbers as exact decimals (integer JSON numbers or decimal strings; fractional JSON numbers and values beyond decimal precision are refused), dates and times as instants, text as written, true/false as such. A missing value matches only `empty`; every other comparison with it is false.
- **No "update any record" action.** An action is one owner's contract with its own schema, preview, checks and audit. The runner binds the action's subject field to the trigger's record; a rule never names a record, tenant, principal or operation outside the installed set.

## Versions and approval

**A rule changes only by a new version; one approved version runs.**

| Step | Contract | Capability | What it checks |
| --- | --- | --- | --- |
| Draft | `base.rules.draft` | `admin.rules.manage` | Installed trigger; fields the drafter may read, values of their type; installed actions on the trigger's record with inputs their contract accepts; examples come out as stated |
| Approve | `base.rules.approve` | `admin.rules.approve` | The draft checks again as the approver, plus: it is the latest version; the approver holds All reach for the trigger table, every table its conditions walk and every action's capability; no condition field is restricted or masked for the `rules` principal; its actions close no cycle in the active rules' declared events; the tenant's limits hold |
| Switch | `base.rules.switch` | `admin.rules.approve` | On again re-runs the approval checks on the running version |

- **Versions never change.** Each stores its definition, examples, the person's words, a SHA-256 fingerprint, how it was drafted (`person` or `agent`), who drafted it and who approved it.
- **Drafting runs nothing.** A new version of a running rule runs only once approved; until then the earlier version keeps running.
- **A queued run keeps its pinned version.** If another version is approved or the rule is switched off before the run acts, the run is refused (`stale_version`, `rule_off`); nothing silently swaps in the new rule.
- **Installed bindings are pinned.** Approval stores a fingerprint of the condition fields' types, choices and source mappings, every linked table's join and key bindings, the trigger bindings and the action schemas. A changed catalog refuses execution (`catalog_changed`) and marks the rule **Needs review** until a new version is approved.
- **Approval never delegates more.** The approver must hold All reach for every capability the rule reads or acts on, including linked tables; Own or Team reach cannot approve or switch on tenant-wide automation; a rule cannot reach a field the principal may not read.

## How a run works

**Recorded with the trigger's save, run after it commits, never inside it.**

1. The owner records its event in its `MutationTransaction` (`base/events`).
2. Events hands it to the `base/rules.triggers` subscriber. In the delivery transaction it writes one `queued` run per rule switched on for that event in the tenant, pinned to the approved version, and hands the event to the `base/rules-run` worker (`Subscribe::handoff`).
3. The worker runs as the `rules` principal. Per run it checks the chain budget, the version and the switch, reads the conditions' fields through Grid, evaluates them and records `matched` (with the facts and each condition's outcome) or `no_match`.
4. Before each outstanding action, including after a retry, the worker rechecks the principal's current record and field access. The evaluated record version is bound into the action; its owner checks it under the record lock and refuses a changed record (`record_changed`). Successful actions advance the next action's expected version in the same transaction; the original version remains in the saved facts. Each action calls the owner's contract through the job door (`JobContracts::commit`) in a transaction that also writes its receipt, keyed `(tenant, version, event, position)`. A retry after a crash finds the receipt and does nothing twice.
5. The run ends `done`, `partial` or `refused`, with a bounded reason code.

| Reason | Means |
| --- | --- |
| `chain_budget` | Another rule's action started this run and the chain reached `rules.chain_depth` |
| `stale_version`, `rule_off` | The rule changed before the run acted |
| `read_refused`, `restricted_field` | The principal may not read the record or a condition's field |
| `subject_unavailable` | The record is gone or out of reach |
| `action_limit` | The run is over the tenant's action limit |
| `actor_refused` | Queue refused the principal (a suspended tenant, a withdrawn grant); told through Queue's `EndingBinding` |
| `catalog_changed` | Installed fields, trigger bindings or action schemas changed, or an owner was unmounted; draft and approve a new version |
| `record_changed` | Skipped: the record changed after the rule matched |
| `action_refused` | An action's owner refused it; the effect's receipt keeps the owner's own words |

## Authority

**One named principal, `rules`, under declared bounds and the tenant's live grants.**

- Its declaration holds `rules.run.execute` and `rules.run.settle` (a standing grant: recording why a run ended is the way out, never an effect).
- Trigger and action owners add their own keys to its bounds with `bilibi_authz::OfferedCapabilities` (the Grid table's read key, each action contract's key). Base names no owner.
- A tenant administrator grants each key on Administration › Authorization › **Automation access**, as for every principal, and allows event delivery (`domain-events`) there too. Until then runs are refused, visibly (or, without delivery, never queued: the Events page shows the waiting deliveries). A direct deny wins.
- Each owner admits the principal to its contract with `bilibi_agent_api::job::PrincipalWrite`; the contract then decides its capability live, on the principal's own grants, as at every door.
- **Never automated**: contracts of `base/authz`, `base/authz_agent`, `base/workflow`, `base/step_up`, `base/rules`, `base/tenancy`, `base/session`, `core/user`, `core/tenant` and `core/agent`, and any contract that declares step-up. Boot refuses such an action.
- The principal receives every field restriction in its tenant (`apps/base/authz/README.md`), so a restricted field is refused at the source, at approval and at run time.

## Limits

**Every bound an operator could reasonably set is a setting** (System Settings, group Rules, tenant or global; editable with `admin.rules.approve`).

| Setting | Default | Range | Bounds |
| --- | --- | --- | --- |
| `rules.actions_per_run` | 3 | 1–10 | Actions one run takes; a rule with more is not approved |
| `rules.chain_depth` | 3 | 1–10 | Rules in a row one first change may start (the causal-chain budget) |
| `rules.runs_per_minute` | 60 | 1–10,000 | Runs one tenant starts in a minute; more wait for a later minute, so a burst never holds the shared queue |
| `rules.active_per_trigger` | 20 | 1–100 | Rules switched on for one trigger |

Fixed by the record's shape: 20 conditions, 10 actions, 10 examples, 50 values in a list.

The chain follows causes, not traces: the worker names its run as the cause of the action's transaction (`Events::cause`), Events stores it on every event recorded there (`Event::cause`), and the next run's depth is its cause's plus one. Independent events of one import keep depth 1.

## Offering a trigger and an action

```rust,ignore
inventory::submit! { Contribution::new(bilibi_rules::TRIGGERS, CASE_OPENED, provenance!(), &bilibi_rules::Trigger {
    on: On { event: CASE_OPENED, versions: &[1] }, title: "A case is opened", table: "cases", subject: "/case", version: "version",
}) }
inventory::submit! { Contribution::new(bilibi_rules::ACTIONS, "cx.cases.route", provenance!(), &bilibi_rules::Action {
    title: "Send the case to a queue", table: "cases", subject: "case_id", version: "expected_version", records: &[CASE_ROUTED],
}) }
inventory::submit! { Contribution::new(bilibi_agent_api::job::PRINCIPAL_WRITES, "cx.cases.route", provenance!(), &PrincipalWrite { principals: &["rules"] }) }
inventory::submit! { Contribution::new(bilibi_authz::OFFERED_CAPABILITIES, "rules:cx/cases", provenance!(), &OfferedCapabilities {
    principal: "rules", capabilities: &[CASES_VIEW, CASES_ROUTE],
}) }
```

Boot refuses a trigger that is not the contributor's own declared event or names no Grid table the principal may be offered; an action that is not the contributor's reviewed write, does not admit `rules`, lacks the subject or expected-version input, records an undeclared event, or would grant access or decide an approval. The owner's contract must accept a principal scope (`scope.user()` is `0`) and recheck its capability, expected record version and record rules in its commit; a company's write also holds `require_writable`.

## Pages and agents

- **Administration › Rules** (`/rules`, `admin.rules.view`): every rule, its trigger, whether it is on, the running version and the last run. `/rules/{id}`: the rule, the version awaiting approval (with the approval review's problems and warnings, and Approve through a confirmation), the running version, the on/off switch and the latest 25 receipts (which conditions held, what each action did or why it was refused). Both call the contracts agents call.
- **Receipts and privacy**: a receipt's facts and condition outcomes are the record's values; current record reach and field restrictions apply to every read, including values collected before a restriction changed. A reader who cannot read the subject or any recorded field sees the evidence as restricted. Protected facts and condition outcomes are kept out of audit metadata.
- **Agents**: `base.rules.vocabulary` (installed triggers with their fields, actions with their inputs, operators, limits), `base.rules.list`, `base.rules.get`, `base.rules.runs`, `base.rules.draft`, `base.rules.approve`, `base.rules.switch`, and the guide `base.rules`. An agent's draft records `drafted_via: agent`; it runs only once a person approves it.

## Tables

`base_rules`, `base_rule_versions`, `base_rule_runs` and `base_rule_effects` (migrations `base-rules-001-rules` and `base-rules-002-approval-bindings`). Account IDs on versions are evidence, not foreign keys: Base names no Core table.

## Seeing it work

`apps/web/examples/rules-walkthrough.rs` on a fresh disposable database, migrated and seeded first (its header has the commands). Unmounted, the engine offers no trigger and refuses a draft on an unknown one. With the reference owner `fixtures/containers/rule_cases` mounted (never a Platform member), it proves two tenants with the same facts and different destinations, a restart and a crash-retry with one effect, an agent's draft approved by a person, refusals for a denied capability, a restricted field, an archived company, a suspended tenant, a stale version and a switched-off rule, self-triggering and multi-rule cycles refused at approval and a noncyclic chain stopped at the chain budget, a threshold change as a new version, and a burst held to its tenant's bound while the other tenant runs at once.

## Not yet

- Drafting and changing rules on the page (today an agent drafts through `base.rules.draft`): [#728](https://github.com/BelimbingProject/bilibi/issues/728).
- Triggers and actions offered by the official Domains: [#729](https://github.com/BelimbingProject/bilibi/issues/729).
- Tenant custom fields as conditions: [#730](https://github.com/BelimbingProject/bilibi/issues/730).
- The installation setting that admits customer code (ADR 28): [#731](https://github.com/BelimbingProject/bilibi/issues/731).
- Shadow runs against past records, configuration packs, a bounded text formula, AI observations as inputs and script or Wasm tooling for isolated installations: later items of [#682](https://github.com/BelimbingProject/bilibi/issues/682).
