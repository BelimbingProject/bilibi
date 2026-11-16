# Tenant-defined custom fields stay "lite" and live in the owner's row

**Document Type:** Architecture decision record
**Status:** Accepted; implemented
**Agents:** claude-code/sonnet-5.5-high, codex/gpt-6.1-sol-medium (implementation), claude/claude-opus-5-5-high (structured values, reports and search), amp/claude-opus-5.5 (restrictions and rule conditions, #730)
**Scope:** Fields a tenant administrator adds to a record type without a release (issues #131, #685)
**Last Updated:** 2026-11-16

A hosted Bilibi with many customers cannot ship a release for every attribute a customer tracks. Static fields from an Extension (Company's field contribution contract) cover one customer per instance. The owner's direction on #131 (2026-10-26) is the scope here: simplicity now, open for later options. A tenant administrator defines **text, number, date, pick-list, several choices, yes/no, long text, link, phone, email and record** fields, shown on the record page and as list columns, imported from CSV ([Bulk Data](../../../apps/base/bulk_data/README.md#tenant-defined-fields-in-csv-import)), and reached by reports and global search like built-in fields. A field is as visible as its record unless an operator withholds it from roles on Field Restrictions (#730): withhold only, with no masking and no permissions of its own. A tenant may also offer a field to Base Rules conditions ([Custom Fields](../../../apps/base/custom_fields/README.md#rule-conditions)). Custom record types, calculated fields, dependent choices and unique fields are not part of it.

**Definitions are Base data, values are the owner's.** `base/custom_fields` owns one row per tenant, record type and key: label, help text, type, the pick-list a choice field uses, position, archived date and version. The key and the type never change and a key is never reused, so a value stored under it is always read as the type it was written as. A record's owner adds `custom_fields jsonb NOT NULL DEFAULT '{}'` to its own table (with an object check and a `gin (… jsonb_path_ops)` index) and checks every value against the tenant's definitions inside its own write. The tenant predicate, soft delete, version check and one audited `Change` then cover the values with the row. Tenants never run DDL.

**Opting in is a contribution.** An owner declares a `RecordType` to `base/custom_fields.record_types`. Nothing becomes extensible by accident, and a module that is not mounted offers nothing.

**Choice fields reuse governed pick-lists.** A choice field names a standalone tenant pick-list (`base/picklists`, ADR 07) instead of keeping options of its own, so renaming, retiring and translating values has one door. A retired value stays valid on records that hold it.

**Values have stable text representations.** A number is stored as a decimal string, never a float, so no digit is lost; dates are `YYYY-MM-DD` and yes/no uses `"true"` or `"false"`. The additional scalar types use strings and the same owner validation and JSON storage.

**Structured values stay in the same JSON, designed once.** A value is a JSON string, except several choices, which is a JSON array of pick-list codes in the order chosen, each once. A record field names its target kind (`target`, a Base Records kind such as `company`) at creation, like a choice field names its pick-list, so the value is just the record's id text and never needs a kind beside it. The owner's existing `gin (custom_fields jsonb_path_ops)` index answers "holds this code" (`@>`) for list filters. No tenant schema changes and no values table is added.

**A record field follows the target's access.** Choosing a record goes through Base Records (`check`, ADR 11): a record the person cannot see is refused exactly as a missing one, and a held one is named with the reader's own access (`resolve`) and shown unavailable otherwise, never by a raw id. A held record stays valid when another field is saved, even for a person who may no longer select it. Merges are followed on read.

**Reports and search read custom fields through Grid.** The owner contributes its JSON source once; each fresh scoped catalog exposes the tenant's active fields under the table's own capability, record reach and tenant predicate. Tenant definitions remain data rather than Grid declarations. The contracts live in [Custom Fields](../../../apps/base/custom_fields/README.md#reports-lists-and-search), [Grid](../../../apps/base/grid/README.md#tenant-custom-fields), [Reports](../../../apps/base/reports/README.md#the-rule) and [Search](../../../apps/base/search/README.md#make-a-table-searchable).

**One door, many keys.** Page and agent share the `base.custom_fields.*` operations (ADR 01); the page calls the same contracts as an agent. Archiving is the only removal: values stay on records and the field can be restored.

**Limits are settings.** Fields per record type (default 50) is a tenant setting with bounds, edited in System Settings; a text length limit arrives with value editing. Lowering a limit never rewrites stored values.

## Why not the alternatives

- **A values table (one row per record and field).** Typed indexes, but a join per shown field, a second write per save and values outside the record's version.
- **Real columns per field.** Tenants would run DDL, which is refused.
- **Full runtime fields** (per-field permissions, masking, choice options of their own). The first attempt (#174) showed what it costs across Grid, Authz, audit views and the agent schema; it is not planned. Withholding alone came with #730, when rules made a field's visibility matter: Authz offers the tenant's fields through one hook, and Grid, record pages, writes and audit views drop a withheld field. References reuse Base Records instead of a reference model of their own.
- **A Grid table or link per custom field.** Grid's declarations are code; a tenant's fields are data. One hook that adds the tenant's fields to each fresh catalog keeps Grid free of the definitions module and keeps one read path.
- **Search through a query-time scan of the JSON.** Unindexed, so a large tenant's search would exceed the bounds and return nothing; the generated vector keeps search index-backed.
- **Fields as code only.** Right for one customer per instance (the static Company field contribution stays), wrong for a hosted many-customer instance.

The contract and the runtime proof are in [Custom Fields](../../../apps/base/custom_fields/README.md).
