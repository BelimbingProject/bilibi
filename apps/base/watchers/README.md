# Watchers

**Use it for:** a tenant's own "when this happens, do that" with no release: offer an event of yours as a watcher trigger and a write contract of yours as a watcher action.

**Not for:** a check that must stop a save (keep it in your operation), per-tenant code branches, or a workflow or approval engine of your own.

A watcher is data ([ADR 28](../../../docs/architecture/decisions/28-tenant-customisation-is-data-on-shared-servers.md), [ADR 31](../../../docs/architecture/decisions/31-tenant-watchers-run-after-commit-as-one-bounded-principal.md), #682). It says: when a trigger event happens to a record, if these conditions over the record's fields hold, run these actions. Omit `if` to act on every event of the watched kind. A person drafts it on the Watchers page or through an AI agent; an administrator approves one fixed version; that version runs on every new event, without AI.

## What a watcher is

A draft uses `watch` for the event, optional `if` for its filter, and `do` for operation steps, for example `{"watch":"cx.cases.opened","if":{"match":"all","conditions":[...]},"do":[{"operation":"cx.cases.route","input":{"queue":"vip"}}]}`. Draft metadata and examples accompany this definition.

| Part | What it holds | Who supplies it |
| --- | --- | --- |
| Trigger | An event an owner records in its own transaction (`watcher_cases/cases.opened`) | An owner's `Trigger` contribution |
| Conditions | When `if` is present, `all` or `any` of one or more conditions, each with a field, operator and any required value | The watcher |
| Actions | 1 to 10 owner write contracts, each with approved parameters | Owners' `Action` contributions; the watcher selects actions and supplies their parameters |
| Examples | 1 to 10 fact sets for a filtered watcher; up to 10 otherwise with expected `match` or `no_match` outcomes | The watcher's author |

- **Fields** are Grid column paths on the trigger's table (`priority`, or `company.name` through a one-link), read as the `watchers` principal. No aggregates.
- **Tenant custom fields** (`cf_<key>`, [Custom Fields](../custom_fields/README.md#watcher-conditions)) are fields only when the tenant offered them to watcher conditions: on the trigger's own record (`cf_segment`) or on a record its one-links reach (`company.cf_segment`). A draft on one that is not offered is refused with that reason; the vocabulary lists only offered ones, under the tenant's label, and names a field one link away by its path (label `Company › Segment`).
- **A linked record's field is read with the trigger's record**, in the same bounded statement and facts budget, as the `watchers` principal: it needs the linked table's read capability and reach to that record, and a run whose linked record is out of reach is refused (`subject_unavailable`). A record with no linked record reads the field as missing.
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
- An owner offers one of its read contracts with `LOOKUPS`. A script declares an exact operation and input; its examples carry captured replies. Approval fingerprints the schemas and requires the approver's authority. Runtime calls each declaration through Agent API as the current caller, with its live capability, field restrictions and record reach, then exposes only the reply to `lookup(operation, input)`. Another operation or input is refused.
- The host accepts only complete operation and input pairs in the declaration. An owner certifies a watcher action as local through its reviewed write handler, which declares that its effects stay within the installation. The private Activities follow-up handler is eligible. The worker receives no connector credentials and has no filesystem, network or database interface.
- The optional watcher filter runs first. QuickJS executes in a fresh process under a Linux syscall allowlist. The tenant can lower `watchers.script_heap_mib` and `watchers.script_milliseconds`. The time budget covers preparation, the whole run or historical trial, and owner operations. After a commit starts, Bilibi waits for it and its callbacks. The host also limits the action count, and validates the full plan before saving it and starting any effect. Retries use the saved calls and existing effect receipts.
- Draft and approval present expected and captured calls for each example. Use the record representation for facts, including text for numbers. Omitted fields are null. Trial evaluates the script and previews the selected effects. Use `base.watchers.get` to inspect the exact version and `base.watchers.switch` to stop it.
- With the reference owner mounted, run `cargo run -p bilibi-web --example watchers-walkthrough -- --scripts` on a fresh walkthrough database. The proof checks a successful route, refused file and network access, an attempted write to another company's record, and the worker's heap and time limits.

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
