# Base Custom Fields

**Use it for:** fields a tenant administrator adds to your records without a release.

**Not for:** a field-definitions table, page or operation of your own, or values your code branches on (keep those in code; a tenant's own routing on a field is a [watcher condition](#watcher-conditions)).

Typed fields (text, number, date, pick-list, several choices, yes/no, long text, link, phone, email and record) a tenant administrator adds to the records of a module that opts in, without a release. The decision and the rejected alternatives are in [ADR 21](../../../docs/architecture/decisions/21-tenant-defined-custom-fields.md).

- **This module owns the definitions, not the values.** A record's owner keeps values in its own row and checks them in its own write.
- **A tenant that defines no fields, or a module that does not opt in, sees no change.**


## Offer custom fields on a record type

Four steps in the owning module.

1. Declare the record type (discovered like capabilities, validated at boot):

   ```rust
   inventory::submit! {
       bilibi_module_registry::Contribution::new(
           bilibi_custom_fields::RECORD_TYPES, "company",
           bilibi_module_registry::Provenance { module: "core/company", source: file!(), line: line!() },
           &bilibi_custom_fields::RecordType { id: "company", label: "Companies" },
       )
   }
   ```

   - Add `base/custom_fields` to the module's descriptor `dependencies` and `bilibi-custom-fields.workspace = true` to its `Cargo.toml`.
   - The id is lower-case letters, digits and `_` (at most 32), unique, and never changes.
2. Add the storage in your own migration, once:

   ```sql
   ALTER TABLE companies
    ADD COLUMN custom_fields jsonb NOT NULL DEFAULT '{}'::jsonb,
    ADD CONSTRAINT companies_custom_fields_object CHECK (jsonb_typeof(custom_fields) = 'object');
   CREATE INDEX companies_custom_fields_idx ON companies USING gin (custom_fields jsonb_path_ops);
   ```
3. Read the fields a record offers with `CustomFields::active(client, scope, "company")` (service `bilibi_custom_fields::SERVICE`). It needs no custom-field capability: authorize your own page or write first.
4. Let reports, lists and search reach them: select `custom_fields` in your Grid table's source, declare it `Field::new("custom_fields", "Custom fields", FieldType::String).hidden()`, and contribute where Grid reads them (boot refuses a record type of another module, a second source, or a field that is not hidden text):

   ```rust
   inventory::submit! { bilibi_module_registry::Contribution::new(bilibi_custom_fields::GRID_SOURCES, "company", provenance,
       &bilibi_custom_fields::GridSource { record_type: "company", table: "companies", column: "custom_fields" }) }
   ```

   For global search, add a stored generated vector and its index in your migration, select and declare it hidden the same way, and name it on your `Searchable` with `.custom("custom_search_vector")` (`../search/README.md`):

   ```sql
   ALTER TABLE companies ADD COLUMN custom_search_vector tsvector
     GENERATED ALWAYS AS (jsonb_to_tsvector('simple', custom_fields, '["string"]')) STORED;
   CREATE INDEX companies_custom_search ON companies USING gin (custom_search_vector);
   ```

## Behaviour

| Rule | Detail |
| --- | --- |
| Key | A lower-case letter, then letters, digits and `_`, at most 40. Fixed at creation, unique per tenant and record type, **never reused**, not even by an archived field. |
| Type | `text`, `number`, `date`, `choice`, `choices`, `boolean`, `long_text`, `link`, `phone`, `email` or `reference`. Fixed at creation. |
| Stored value | Text as a JSON string; number as an exact decimal string, never a float; date as `YYYY-MM-DD`; choice as the pick-list value's code; several choices as a JSON array of codes in the order chosen, each once; yes/no as `"true"` or `"false"`; record as the record's id text; the other kinds as strings. |
| Choice, several choices | Names a standalone tenant pick-list that carries no capability of its own (`choice_lists()`); renaming, retiring and translating values happen in Pick-lists. Fixed at creation. |
| Record | Names the Base Records kind it points at (`target`, from `reference_kinds()`), fixed at creation. A new value goes through `Records::check` with the writer's own access, so a hidden, removed or forged record is refused; the held record stays valid when other fields are saved, even for a writer who may no longer select it. A field whose kind is no longer mounted leaves forms, columns and agents, as a choice field whose list is gone does. |
| Archive, not delete | An archived field leaves forms, columns and agents; its values stay on records; restoring brings it back. |
| Value checks | `CustomFields::validate` (reviewed preview, on a read connection) and `validate_locked` (the owner's write transaction) check a patch of key to value against the active fields and return the values as they will be stored. An empty or null value clears a key; a key that is not an active field is refused; text is limited (`base.custom_fields.max_text_length`, default 1,000), a number is up to 18 digits and 6 decimals, a date is `YYYY-MM-DD`, a pick-list code must be an active choice unless the record already holds it (for several choices, each newly chosen code). |
| Additional checks | Long text uses the tenant text length limit and a multiline editor. A link must be an absolute HTTP or HTTPS URL within that limit. Phone takes digits, spaces and `+ ( ) - . /` with at least one digit, then an optional extension (`x` or `ext` and its digits), up to 64 characters; email requires an address up to 254 bytes. Yes/no uses an on/off form switch and accepts only `"true"` or `"false"`. Several choices takes an array of codes (an empty array clears it) and refuses a single string; every other kind refuses an array. Null clears any field. |
| Showing values | Owners use the facade's helpers so every page reads the same: `choice_text` (labels, retired ones marked), `choice_options` (what an editor offers, held retired codes included), `references` (names with the reader's access, for the `RelatedRecord` pill), `reference_hits` (for `RecordPicker`), `codes_value` and `picked` (posted editor values as stored values). A several-choices fact edits in place with a `MultiSelect` inside `InlineChoice` (`docs/inline-editing.md`). |
| Limit | Active fields per record type: tenant setting `base.custom_fields.max_fields` (default 50, 1–200). Archived fields do not count, restoring one is refused at the limit. |
| Watcher condition | Off by default. `watcher_condition` on define or update offers a text, number, date, pick-list or yes/no field to Base Watchers conditions; other kinds are refused. See [Watcher conditions](#watcher-conditions). |
| Restriction | An operator withholds a field from roles on Field Restrictions as `cf_<key>` of the record type's Grid table (withhold only: none is personal). `active` then leaves it out for that reader, so pages hide it and writes refuse it, and Grid leaves it out of lists, reports and search. Audit views take it out of the `custom_fields` object an owner audits its values under, showing a change as a withheld `custom_fields.<key>`, so audit custom values under that one key. |
| Audit | Each definition write is one `custom_field` change with before and after. |
| Capabilities | `base.custom-fields.view` reads definitions and `base.custom-fields.manage` defines, edits, archives and restores them. The Tenant Owner system role includes both. |

## Administration page

Use **Administration › Custom fields** (`/admin/custom-fields`, `base.custom-fields.view`) to manage the tenant's fields. The record type and the status are a filter and a column. Label and help text edit in place; archive and restore confirm first; **+ Field** opens the create dialog. Every change goes through the `base.custom_fields.*` contracts below, so the page and agents share one check and one audit trail. For the shared inventory, see [Customisations](../labels/README.md#customisations-page).

## Agent operations

| Key | Capability | Does |
| --- | --- | --- |
| `base.custom_fields.record_types` | `view` | The record types that take fields with their registered list addresses, the active-field limit, the pick-lists a choice field may use and the kinds of record a record field may point at (`record_kinds`). |
| `base.custom_fields.list` | `view` | The tenant's fields, optionally for one record type; archived ones with `include_archived`. |
| `base.custom_fields.define` | `manage` | Add a field; a choice or several-choices field names its `picklist`, a record field its `target`. |
| `base.custom_fields.update` | `manage` | Change label, help text or `watcher_condition` (omitted keeps it); needs `version`. |
| `base.custom_fields.archive` / `.restore` | `manage` | Needs `version`. |

Each write is a reviewed safe write: the preview runs the facade's checks without locking or writing, and the commit writes on the plan's audited transaction. The guide `base.custom_fields` explains keys, types and archiving to agents.

Owner operations take values as `{key, value, values}` pairs: `value` (text or null) for every kind but several choices, `values` (an array of codes) for several choices.

## Watcher conditions

**A tenant decides which of its fields its watchers may read** (#730). A field is a Base Watchers condition only while it is offered (`watcher_condition`, the **Watcher condition** switch on the page), on a record type with a `GridSource`. The watcher reads it on the trigger's own record (`cf_segment`) or on a record that one links to (`company.cf_segment`, #862); the switch is the same one, on the field's own record type.

- **Offered:** the field joins the watcher vocabulary under its label, and a watcher on it is drafted and approved like one on a declared field. Grid carries the flag (`Table::custom_watcher_condition`).
- **Withdrawn or archived:** an approved watcher that reads it, on its own record or through a link, is refused at run time (`catalog_changed`) and marked Needs review; a new draft on it is refused. A missing field never reads as an empty value.
- **Restricted:** the `watchers` principal holds every restriction in its tenant, so a restricted field refuses approval ("hidden from automation") and each run (`restricted_field`).
- Core invariants stay in the owner's code: a condition routes, it never validates a save.

## CSV import

The owner opt-in and mapping contract lives in [Bulk Data](../bulk_data/README.md#tenant-defined-fields-in-csv-import).

## Reports, lists and search

A fresh Grid catalog asks this module (the `bilibi_grid::CustomColumns` hook it binds at boot) for the tenant's active fields of each `GridSource` table using `CustomFields::active` on the catalog's connection, and they join that table as `cf_<key>` fields: report columns, filters, groupings, sorts and measures, and columns a list adds from its picker (except record fields), under the table's own capability, reach and tenant predicate, with no grant of their own.

| Kind | In Grid |
| --- | --- |
| Number, date, yes/no | Decimal, date and yes/no values; a stored value of another shape reads as empty |
| Choice, several choices | The code, or the codes joined by `, ` in the order chosen |
| Record | Grid projects its stored id and carries its Base Records kind. [Grid](../grid/README.md#tenant-custom-fields) owns projection and filter rules; its [measure rules](../grid/README.md#grouped-summaries-and-row-pages-g5) limit reference aggregates to count. [Reports](../reports/README.md#the-rule) owns naming and reference freshness on pages and exports. |
| Text, long text, link, phone, email | Text, and searchable |

Global search uses the owner's generated `custom_search_vector` for indexed candidates and the active searchable text for matching; [Search](../search/README.md#make-a-table-searchable) owns the query and ranking contract.

## Storage

`base_custom_fields` (migrations `base-custom_fields-001-definitions`, `base-custom_fields-002-more-types`, `base-custom_fields-003-structured-values`, `base-custom_fields-004-rule-conditions`, `base-custom_fields-005-watcher-conditions`): one row per tenant, record type and key, with the pick-list or record kind a field takes. Every statement binds the tenant.

## Runtime check

On a fresh disposable database (no unit tests):

```sh
BILIBI_CUSTOM_FIELDS_DEMO=disposable BILIBI_CUSTOM_FIELDS_ROOT="$PWD/.local/structured-fields-artifacts" \
  DATABASE_URL=… APP_ORIGIN=… \
  cargo run -p bilibi-custom-fields --example structured-fields-walkthrough
```

| Case | Expected |
| --- | --- |
| Define | A record field must name a mounted kind; contacts take several choices and a record field pointing at companies |
| Values | Several choices are one array of codes, each once; a record field stores the id in its one spelling (`01` is `1`); both live in the contact's own JSON |
| Refusals | An unknown code, a single string for several choices, an array for text and a forged record id are each refused on `cf_<key>` |
| Target's access | A person who may not see companies cannot choose one, can still save another field of a contact that holds one, and sees it unnamed |
| Reports | Each active field is a column; a record field stays offered with its name withheld when the target is unreadable; none in another tenant; a rows report filters, sorts and shows text, codes and the partner's name; a summary groups by several choices |
| Search | A word of a custom text field finds the contact; another tenant and a pick-list code do not; an archived field's words stop finding it and its column leaves reports |
