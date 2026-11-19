# Setup packs

**Use it for:** copying selected label overrides and active custom field definitions between tenants through agent operations.

**Not for:** copying records, credentials, grants, approvals, record values, pick-list values, shared views or rules.

## Transfer setup

| Operation | Tenant | Capability | Result |
| --- | --- | --- | --- |
| `base.setup_packs.export` | Source | `base.setup-packs.view` | Select overrides by `{kind, subject}` and fields by `{record_type, key}` to export one pack. |
| `base.setup_packs.preview` | Destination | `base.setup-packs.view` | Compare the pack to the destination and see the proposed additions and any refusals without changing setup. |
| `base.setup_packs.apply` | Destination | `base.setup-packs.manage` | Dry-run, review, and commit the unchanged operation to add the reviewed entries in one audited transaction. |

Export also needs each selected owner's view capability (`base.labels.view`, `base.custom-fields.view`). Preview and apply need each included owner's manage capability (`base.labels.manage`, `base.custom-fields.manage`). The authenticated scope identifies the tenant for every call. Export and import use separate connections when the caller has distinct accounts in the two tenants.

Choose entries with `base.labels.list` and `base.custom_fields.list`. Export requires each selection to exist as an override or an active definition. Every selection and pack carries at most 100 entries in total. Duplicate stable keys are refused.

## Version 1 format

```json
{
  "version": 1,
  "labels": [
    {"kind": "menu", "subject": "admin.labels", "label": "Vocabulary"}
  ],
  "custom_fields": [
    {
      "record_type": "company",
      "key": "contract_stage",
      "label": "Contract stage",
      "help": "The stage of the contract",
      "kind": "choice",
      "picklist": "core.document.status",
      "watcher_condition": false
    }
  ]
}
```

The labels use the [label owner's stable subject IDs](../labels/README.md). Field definitions use the [custom field owner's field kinds and stable keys](../custom_fields/README.md#behaviour). `help` is optional. `picklist` is required for choice and several-choices fields. `target` is required for reference fields and names a stable record kind. `watcher_condition` carries whether the field is offered to rules. No rule or condition using it travels. Tenant and row IDs, edit versions, archived definitions and absolute positions stay with the source; exported field order becomes append order within each destination record type.

Preview takes `{pack, picklist_map?}`. Version 1 maps pick-lists only. Each mapping is `{from, to}` and names a source key used in the pack and an eligible destination key. An unmapped list uses the same stable key. Maintain the destination list's values in [Pick-lists](../picklists/README.md).

## Review and apply

- `additions` holds the overrides and definitions that would be added, including destination pick-list keys. Identical entries leave no difference.
- `collisions` lists a different override at the same subject, or a different or archived definition at the same record type and key. A collision must be resolved before any changes can apply.
- `missing` identifies unavailable label subjects, record types, destination pick-lists and reference kinds by their keys. Preview reports each unavailable dependency of a field separately. Resolve those dependencies before preview can compare the field with a destination definition or validate the proposed addition.
- `invalid` lists owner validation failures, including the destination's active-field limit across the proposed additions for each record type.
- When `ready` is true, copy `expected_additions` from preview into the apply input with the same pack and mapping. Dry-run the apply operation, show the additions and wait for approval before committing the unchanged plan. A new addition discovered after review requires a new preview. An addition another writer already made identically can become a no-op.
- Apply rechecks the destination and uses the ordinary label set and custom field define paths on the agent plan's transaction. Owner writes and their audit rows commit together. An empty difference writes zero changes, including when retrying an already-applied pack.

The two-tenant proof is registered as `scripts/run-walkthroughs.sh setup-packs`.

Part of [#800](https://github.com/BelimbingProject/bilibi/issues/800). See [ADR 33](../../../docs/architecture/decisions/33-pages-for-business-actions-and-customisations.md) for the planned Customisations page.
