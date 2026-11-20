# Tenant labels

**Use it for:** a tenant renaming the words a module shows for a menu item, a record type or a field ("Companies" as "Clients"), stored as plain text against the stable ID; and contributing your module's own tenant customisation to the Customisations page and to the agents' "what can I change here?" read.

**Not for:** renaming operation keys, routes or permission keys, translating a language (use `base/locale`), or a tenant's own value lists (use `base/picklists`).

A label override is data (ADR 28): a short plain text kept per tenant against a stable subject, so shared servers need no release and no customer code. It never changes an operation key, route or capability key, so agents, links and grants keep working under the old names.

| Kind | Subject ID | Shown by |
| --- | --- | --- |
| `menu` | A staff menu item ID (`base/menu`); portal items are excluded | The sidebar and the [menu-order reader preview](../menu_order/README.md). The workspace page picker, Menu Inspector and menu-order list show the contributed labels. |
| `record` | A Grid table ID (`base/grid`) | The record types Reports offers, in place of the table's own label. |
| `field` | `<table>.<field>`: a Grid table ID, a dot and a Grid field ID. Hidden fields are excluded | Every Grid column the tenant's readers see: list column heads, the add-a-column picker, filters, saved and shared views, Reports and exports. Record pages, forms and inline editors resolve the same words through `RecordWords`. |

A field label replaces the field's own word only; a column reached through a link still names the link in front (`Company › Name`), and a record field read through a link uses the target field's override, falling back to its own declared word. Admin screens that name a field by its stable identity (field restrictions) keep the module's word.

## Show a label (owner)

Read the tenant's overrides once per render with `Labels::labels(client, scope)` (`Arc<Labels>` from `bilibi_labels::SERVICE`), then ask the set for each word: `set.menu(id, default)`, `set.record(table, default)` and `set.field(table, field, default)` return the override or `default`. Never cache a set across requests or tenants, and never read `base_labels` yourself.

For column and record-field rendering, follow [Grid's label integration](../grid/README.md#shared-label-rendering). `base/grid` contributes the renamable record types and fields to a `SubjectSource` (`SUBJECTS`), so this module never depends on Grid.

## Change a label

| Who | How |
| --- | --- |
| A person | Administration › Labels (`/admin/labels`), editing the **Your label** column in place; clearing it restores the module's word. Needs `base.labels.manage`; `base.labels.view` reads. |
| An agent | `base.labels.list`, `base.labels.set` and `base.labels.clear` (`kind` is `menu`, `record` or `field`), the same contracts the page calls. Writes are reviewed first. Each listed word also carries `page` (the address a menu item opens) and `record_kinds` (the Grid table and record types a record or field belongs to). |

A label is 1 to 60 characters of plain text; Askama escapes it wherever it is shown. A subject that no module declares is refused, and every change is audited as kind `label`.

Editors carry the `identity:version` token returned by `base.labels.list` (`0:0` for an unset override). Changed or replaced overrides and concurrent first writes are refused rather than overwritten.

The Tenant Owner system role includes `base.labels.view`, `base.labels.manage` and `base.labels.customisations.view`.

## Agent setup guide

With `base.labels.customisations.view`, search for **Customise my tenant** (`base.labels.customise`) for a worked setup, review, verification and recovery path. The guide includes [delegated tenant settings](../settings_editor/README.md#tenant-agent-operations). Its operation contracts and plan limit are generated at read time. For watcher reviews, read the installed automation vocabulary for tenant limits.

## Customisations page

Open **Administration › Customisations** (`/admin/customisations`, `base.labels.customisations.view`) to see tenant label overrides and active and archived custom fields. Use **Restore default** on a label override, **Archive** on an active field or **Restore** on an archived field. Each action requires the owning module's write permission. A notice names any sections you lack permission to read.

The page shows the recorded change time when [Audit can match the current revision](../audit/README.md#record-revision-attribution). The **Changed by** column requires `admin.audit.log.list` and otherwise displays **Restricted**. Use the owner’s editor or agent operations to create or edit a customisation.

A module contributes one `CustomisationSection` under `SECTIONS` for each kind of customisation it owns. The section has two parts and declares at least one:

| Part | Feeds | Declares |
| --- | --- | --- |
| `undo` (`Undo`) | The page's rows and their one action | The read operation and input, row projection, reviewed write operation and page event |
| `changes` (`Changes`) | [What can be changed](#what-can-be-changed-agents) | Purpose, limits, guide, the reads that list every choice, and the change, preview, verify and restore operations |

Each page row supplies a stable audit kind, record ID and revision plus the versioned input for the write. The page retains that input server-side and accepts confirmation only for the displayed action, record and revision. The confirmed action calls `PageContracts::submit`, then the page reads the sections again. Register the event and write contract with a [contributed page binding](../agent_api/README.md#shared-pages-with-contributed-owner-contracts) for `base/labels.customisations`. Project rows through the owner's read operation so its access checks apply.

## What can be changed (agents)

**One read answers "what can I change here?":** `base.labels.customisations.discover`, for any signed-in person's agent. It adds no page.

| Input | Meaning |
| --- | --- |
| `on_page` | The exact page path, such as `/companies` |
| `record_kind` | A kind of record by any stable ID its owners know it by (`company`, `companies`); given with `on_page`, the choice must match both |
| `purpose` | A few words such as `rename column` matched against the choice’s name, ID and purpose terms |
| `page`, `page_size` | A bounded page of choices: 25 unless asked, at most 100 |

The answer holds:

- **`choices`:** one page of what may be changed, whether or not the tenant has changed it. Each has its section, subject, title, `on_page`, `record_kinds`, `target` (`tenant`), the value in effect, default, `source`, `customised`, `version` and any choice-specific `limits`. Use `input` for the choice's identifying write fields, `verify_input` for the verify operation, and `available_writes` for the write operations supported for the choice and caller. A value that may not be read (a secret) is `{"restricted":true}`.
- **`sections`:** for each section with a match, its purpose, target, limits, guide and its read, change, preview, verify and restore operations, with `available` based on the operation’s capability and channel. Planning a change operation as a dry run is always a preview; `preview` names an extra read when the owner has one.
- **`meta.page`:** the page, page size and total number of choices.

Permissions are the owners' own. The read takes one capability snapshot, then calls each section's read operations through `Registry::call_on` on the caller's connection, so every owner applies its capability, tenant scope and field restrictions, and the response omits unreadable sections. Nothing is read from another tenant, and no scope, tenant or actor comes from input.

Contribute `changes` with the section. Boot refuses a section whose reads are missing, are not reads or need a transaction, whose change or restore operation is not a reviewed write, or whose guide is not registered. `choices` projects the read results into `Choice` rows: fill `page` and `record_kinds` with what the owner knows and give each choice `keywords` for the actions and subjects it covers, and leave `value` as `None` when it must not be read.

| Section | Owner | Choices |
| --- | --- | --- |
| Labels | `base/labels` | Every menu item, record type and field that may be renamed |
| Custom fields | [`base/custom_fields`](../custom_fields/README.md) | Every kind of record that takes fields, then each field |
| Shared list views | [`base/grid`](../grid/README.md) | Every list page the caller may open and its visible saved views |
| Tenant settings | [`base/settings_editor`](../settings_editor/README.md) | Every setting delegated to tenant administrators |
| Menu order | [`base/menu_order`](../menu_order/README.md) | Every sibling group of the staff menu |

Watchers and setup packs do not contribute a section yet.

## Customisation receipts

`base.labels.receipts` and the Customisations page show recent label and custom-field audit changes grouped by trace, including the actor, recorded values and the owner's current result. They require the Customisations capability, audit-list access and the relevant owner's read capability. Changes from owners the reader cannot access are omitted. Audit redactions remain in force.

The read covers the last 30 days and at most 500 changes. `more` reports the limit; when true, a trace may have additional changes outside the returned window. Audit retention can remove older evidence. The page offers an undo for a current version where the owner supplies a correction; it previews and then commits the same versioned owner operation. A stale version is marked **Changed since**. Restored defaults with no remaining version are not offered for undo. Undo is one change at a time, not a trace-wide or historical rollback.

An owner contributes a `ReceiptSource` under `RECEIPT_SOURCES`: its audit kind, read capability, authorized current read, projection and correction to an existing reviewed write. The source must compare stable identity as well as revision. Bind every correction event to its owner contract with `PAGE_BINDINGS`. Receipt reads use Audit evidence, not another history store.
