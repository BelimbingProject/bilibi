# Setup packs

**Use it for:** copying selected label overrides, active custom field definitions, shared list views and watcher drafts between tenants through agent operations.

**Not for:** copying records, credentials, grants, record values, pick-list contents, watcher approvals or run evidence.

## Transfer setup

| Operation | Tenant | Capability | Result |
| --- | --- | --- | --- |
| `base.setup_packs.export` | Source | `base.setup-packs.view` | Export the selected setup in one versioned pack. |
| `base.setup_packs.preview` | Destination | `base.setup-packs.view` | Compare with the destination and see proposed additions and any refusals. |
| `base.setup_packs.apply` | Destination | `base.setup-packs.manage` | Add the reviewed entries in one audited transaction. |

The authenticated scope identifies the tenant. Use a source connection for export and a destination connection for preview and apply.

| Setup | Export selection | Source access | Destination access |
| --- | --- | --- | --- |
| Labels | `labels` entries of `{kind, subject}` from `base.labels.list` | `base.labels.view` | `base.labels.manage` |
| Custom fields | `custom_fields` entries of `{record_type, key}` from `base.custom_fields.list` | `base.custom-fields.view` | `base.custom-fields.manage` |
| Shared views | `views` entries of `{page, id}` from `base.grid.view.pages` | Page access, view visibility and `grid.views.share` | Page access and `grid.views.share` |
| Watcher drafts | `watchers` IDs from `base.watchers.list` | `admin.watchers.view` and access to the condition fields | `admin.watchers.view`, `admin.watchers.manage` and access to the condition fields |

## Version 3

A pack has `version: 3` and the four arrays `labels`, `custom_fields`, `views` and `watchers`. At most 100 entries travel in one pack. Export a new pack when using an earlier version.

- Labels use the [label owner's stable subject IDs](../labels/README.md). Field definitions use the [custom field owner's field kinds and stable keys](../custom_fields/README.md#behaviour). Field order becomes append order within each destination record type.
- A view carries `{page, name, view, params, roles}`. The arrangement contains columns, lenses, row height and change-since date. Portable parameters include declared fixed choices, sort and page size. Clear free-text searches and record-specific filters before exporting. An empty `roles` array means everyone. Other audiences carry role keys, with `code:` for system roles and `name:` for custom roles. The destination caller owns the added view.
- A watcher carries `{name, watch, definition, warnings?}` from its latest unapproved version. It contains authored conditions and declared action parameters. Replace conditions or parameters that identify source records before exporting. The pack omits examples, the source explanation, account IDs, receipts and approval state. A watcher can refer to a custom field supplied by the same pack. An imported watcher must pass the destination's separate approval before it can run.

## Review and apply

Preview takes `{pack, picklist_map?, role_map?, watcher_examples?}`. Use `picklist_map` entries of `{from, to}` with eligible pick-list keys. Unmapped lists use the same stable key. Use `role_map` entries of `{from, to}` with an exported role key and an existing destination role ID from `base.authz_agent.role.list`. Unmapped system roles match their code. Custom role names require explicit mapping. Maintain the destination role memberships and [pick-list values](../picklists/README.md) through their owners.

- Supply destination-local examples for new watchers with conditions in `watcher_examples`, keyed by exact watcher name. Each example has a `facts` object and `expect` set to `match` or `no_match`. Export flags free-text condition operands and action inputs. Destination preview recomputes those warnings and identifies each literal by its path. Review the warnings and remove any source record values before applying.
- `additions` shows what will be added, including mapped pick-list keys and destination view audiences.
- `collisions` identifies different entries using the same key or name. Archived fields and ambiguous watcher names also collide.
- `missing` identifies unavailable subjects, record kinds, pick-lists, pages, columns, roles, watcher triggers and actions. `invalid` reports owner validation failures, including capacity limits.
- When `ready` is true, copy `expected_additions` into apply with the same pack, mappings and destination examples. Dry-run the operation, show the additions and obtain approval before committing the unchanged plan. A newly required addition or changed role binding needs another preview.
- All additions and their audit rows commit together. The same pack and mappings yield an empty difference when the destination matches, even when retrying the original review.
- Inspect and reverse a saved view or an initial unapproved watcher draft through Administration → Customisations. Watcher review and approval remain on Watchers.

The two-tenant walkthroughs are `scripts/run-walkthroughs.sh setup-packs` and `scripts/run-walkthroughs.sh setup-packs-watchers`. See [ADR 33](../../../docs/architecture/decisions/33-pages-for-business-actions-and-customisations.md).
