# Tenant menu order

**Use it for:** arranging staff menu items among siblings for one tenant.

**Not for:** granting access, changing menu IDs, ordering the customer portal, or editing a menu in a separate screen.

A tenant override stores item IDs for one parent (the empty parent is the root). Listed items come first in the saved order; unlisted or newly contributed siblings follow in product order. Missing and moved items are ignored. The shell filters by each reader's current permissions before applying the order, so an override cannot expose a hidden item. Labels still come from `base/labels`.

| Agent operation | Purpose |
| --- | --- |
| `base.menu_order.list` | Read visible product siblings, their saved IDs and the `identity:version` token for each visible group. |
| `base.menu_order.preview` | Read the current reader's visible menu in its saved order, or pass `parent` and `item_ids` to preview a proposed order without saving; an empty array previews product order. |
| `base.menu_order.set` | Set sibling IDs for a parent, using the token just read. |
| `base.menu_order.restore` | Restore product order for the caller's visible siblings, using the token just read. |

Writes need `base.menu-order.manage`. Reads need `base.menu-order.view`. Orders target the staff root or a visible staff sibling group. Proposed IDs must be unique visible siblings. Setting or restoring order preserves saved IDs for siblings outside the caller's view. Writes recheck authorization, visibility and the version inside the audited mutation and refuse a stale write. Agent writes use the shared review and approval flow. A `0:0` token means there is no override.

The Customisations page is being built separately; its menu-order section is a follow-up.

## Walkthrough

On a disposable PostgreSQL installation, run `DATABASE_URL=postgresql://…/postgres scripts/run-walkthroughs.sh menu-order`. It checks proposed and saved order, stale and invalid writes, audit, reader visibility, removed IDs, restore and a second tenant through the real contracts.
