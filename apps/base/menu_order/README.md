# Tenant menu order

**Use it for:** arranging staff menu items among siblings for one tenant.

**Not for:** granting access, changing menu IDs, ordering the customer portal, or editing a menu in a separate screen.

A tenant override stores item IDs for one parent (the empty parent is the root). Listed items come first in the saved order. Unlisted or newly contributed siblings follow in the [product order](../menu/README.md). Missing and moved items are ignored. The shell filters by each reader's current permissions before applying the order, so an override cannot expose a hidden item.

| Agent operation | Purpose |
| --- | --- |
| `base.menu_order.list` | Read visible product siblings, their saved IDs and a version token for each visible group. |
| `base.menu_order.preview` | Read the current reader's visible menu in its saved order, or pass `parent` and `item_ids` to preview a proposed order without saving; an empty array previews product order. |
| `base.menu_order.set` | Submit a nonempty list of sibling IDs for a parent, using the token just read. |
| `base.menu_order.restore` | Restore product order for the caller's visible siblings, using the token just read. |

Writes need `base.menu-order.manage`. Reads need `base.menu-order.view`. Orders target the staff root or a visible staff sibling group. Proposed IDs must be unique visible siblings. Setting or restoring order preserves saved IDs for siblings that still belong to the group and are outside the caller's view. The version token binds the saved override and which siblings the caller can see, including for groups without an override. A change to either requires a fresh list and review. Agent writes use the shared review and approval flow and recheck authorization and the token inside the audited mutation.

Set and restore reviews use the [shared audience preview](../agent_api/README.md#audience-previews). The audience is everyone in the tenant. `visible_to_current_reader` carries the ordered menu after filtering by the caller's grants. Set and restore reviews include that preview alongside the before and after sibling IDs.

Use the agent operations to inspect and restore menu order. This module contributes a **Menu order** [customisation section](../labels/README.md#what-can-be-changed-agents), so `base.labels.customisations.discover` lists each visible sibling group with its product order, effective order and version token. Its rows on the [Customisations page](../../../docs/architecture/decisions/33-pages-for-business-actions-and-customisations.md) are planned.

## Walkthrough

On a disposable PostgreSQL installation, run `DATABASE_URL=postgresql://…/postgres scripts/run-walkthroughs.sh menu-order`. It checks proposed and saved order, stale and invalid writes, audit, reader visibility, removed IDs, restore and a second tenant through the real contracts.
