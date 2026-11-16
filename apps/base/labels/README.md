# Tenant labels

**Use it for:** a tenant renaming the words a module shows for a menu item, a record type or a field ("Companies" as "Clients"), stored as plain text against the stable ID.

**Not for:** renaming operation keys, routes or permission keys, translating a language (use `base/locale`), or a tenant's own value lists (use `base/picklists`).

A label override is data (ADR 28): a short plain text kept per tenant against a stable subject, so shared servers need no release and no customer code. It never changes an operation key, route or capability key, so agents, links and grants keep working under the old names.

| Kind | Subject ID | Shown by |
| --- | --- | --- |
| `menu` | A staff menu item ID (`base/menu`); portal items are excluded | The sidebar, in place of the item's own label. The workspace page picker and the Menu Inspector still show the module's word. |
| `record` | A Grid table ID (`base/grid`) | The record types Reports offers, in place of the table's own label. |
| `field` | `<table>.<field>`: a Grid table ID, a dot and a Grid field ID. Hidden fields are excluded | Every Grid column the tenant's readers see: list column heads, the add-a-column picker, filters, saved and shared views, Reports and exports. Record pages, forms and inline editors resolve the same words through `RecordWords`. |

A field label replaces the field's own word only; a column reached through a link still names the link in front (`Company › Name`), and a record field read through a link uses the target field's override, falling back to its own declared word. Admin screens that name a field by its stable identity (field restrictions) keep the module's word.

## Show a label (owner)

Read the tenant's overrides once per render with `Labels::labels(client, scope)` (`Arc<Labels>` from `bilibi_labels::SERVICE`), then ask the set for each word: `set.menu(id, default)`, `set.record(table, default)` and `set.field(table, field, default)` return the override or `default`. Never cache a set across requests or tenants, and never read `base_labels` yourself.

Grid reads field labels itself when it builds a catalog, so a column you resolve through `Catalog::resolve` already carries the tenant's word. A record page that draws labels from `RecordField` loads `bilibi_grid::RecordWords::load(client, scope, table)` once and calls `words.get(&field)` instead of `field.label`. `base/grid` contributes the renamable record types and fields to a `SubjectSource` (`SUBJECTS`), so this module never depends on Grid.

## Change a label

| Who | How |
| --- | --- |
| A person | Administration › Labels (`/admin/labels`), editing the **Your label** column in place; clearing it restores the module's word. Needs `base.labels.manage`; `base.labels.view` reads. |
| An agent | `base.labels.list`, `base.labels.set` and `base.labels.clear` (`kind` is `menu`, `record` or `field`), the same contracts the page calls. Writes are reviewed first. |

A label is 1 to 60 characters of plain text; Askama escapes it wherever it is shown. A subject that no module declares is refused, and every change is audited as kind `label`.

Editors carry the `identity:version` token returned by `base.labels.list` (`0:0` for an unset override). Changed or replaced overrides and concurrent first writes are refused rather than overwritten.
