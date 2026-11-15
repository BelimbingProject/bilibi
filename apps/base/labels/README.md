# Tenant labels

**Use it for:** a tenant renaming the words a module shows for a menu item or a record type ("Companies" as "Clients"), stored as plain text against the stable ID.

**Not for:** renaming operation keys, routes or permission keys, translating a language (use `base/locale`), or a tenant's own value lists (use `base/picklists`).

A label override is data (ADR 28): a short plain text kept per tenant against a stable subject, so shared servers need no release and no customer code. It never changes an operation key, route or capability key, so agents, links and grants keep working under the old names.

| Kind | Subject ID | Shown by |
| --- | --- | --- |
| `menu` | A staff menu item ID (`base/menu`); portal items are excluded | The sidebar, in place of the item's own label. The workspace page picker and the Menu Inspector still show the module's word. |
| `record` | A Grid table ID (`base/grid`) | The record types Reports offers, in place of the table's own label. |

Field labels are not overridable yet: a Grid field's label is a `&'static str` that columns, filters and pickers copy before a tenant is known.

## Show a label (owner)

Read the tenant's overrides once per render with `Labels::labels(client, scope)` (`Arc<Labels>` from `bilibi_labels::SERVICE`), then ask the set for each word: `set.menu(id, default)` and `set.record(table, default)` return the override or `default`. Never cache a set across requests or tenants, and never read `base_labels` yourself.

## Change a label

| Who | How |
| --- | --- |
| A person | Administration › Labels (`/admin/labels`), editing the **Your label** column in place; clearing it restores the module's word. Needs `base.labels.manage`; `base.labels.view` reads. |
| An agent | `base.labels.list`, `base.labels.set` and `base.labels.clear`, the same contracts the page calls. Writes are reviewed first. |

A label is 1 to 60 characters of plain text; Askama escapes it wherever it is shown. A subject that no module declares is refused, and every change is audited as kind `label`.

Editors carry the `identity:version` token returned by `base.labels.list` (`0:0` for an unset override). Changed or replaced overrides and concurrent first writes are refused rather than overwritten.
