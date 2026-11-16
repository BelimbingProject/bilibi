//! The tenant's custom fields as Grid fields, so lists, reports and agents
//! that read through Grid reach them like built-in fields (issue #685). An
//! owner names the Grid table a record type's rows are read from and the
//! hidden JSON field that holds their values (`GridSource`); every fresh
//! scoped catalog then asks this module for the tenant's active fields of
//! those tables (`bilibi_grid::CustomColumns`). Values are read with each key
//! as a validated literal, never request text.
use crate::{definitions::Kind, CustomFields, RECORD_TYPES};
use bilibi_grid::{CustomColumn, CustomColumns, CustomFuture, CustomTable, FieldType};
use bilibi_module_registry::{Catalog, Consumer, RegistryError};
use bilibi_tenancy::{ReadClient, Scope};
use std::collections::BTreeMap;

/// Contribute one per record type whose rows a Grid table reads, from the
/// module that declares the record type.
pub const GRID_SOURCES: &str = "base/custom_fields.grid_sources";

/// Where Grid reads a record type's custom values: the owner's Grid `table`
/// and the hidden field (`Field::new(column, …).hidden()`) its source selects
/// the `custom_fields` JSON column under.
#[derive(Debug)]
pub struct GridSource {
    pub record_type: &'static str,
    pub table: &'static str,
    pub column: &'static str,
}

inventory::submit! { Consumer::new::<GridSource>(GRID_SOURCES, "base/custom_fields", |_| Ok(())) }

/// The validated sources: one per record type, from its declaring module,
/// naming an installed Grid table with that hidden field.
pub(crate) fn sources(
    catalog: &Catalog,
) -> bilibi_module_registry::Result<Vec<&'static GridSource>> {
    let record_types = catalog.entries::<crate::RecordType>(RECORD_TYPES)?;
    let installed = bilibi_grid::installed()?;
    let mut seen = BTreeMap::new();
    let mut out = Vec::new();
    for entry in catalog.entries::<GridSource>(GRID_SOURCES)? {
        let source = entry.get::<GridSource>()?;
        let refuse = |why: &str| {
            RegistryError::Contribution(format!(
                "custom-field Grid source {} at {}:{}: {why}",
                entry.id, entry.provenance.source, entry.provenance.line
            ))
        };
        if !record_types.iter().any(|record| {
            record.id == source.record_type && record.provenance.module == entry.provenance.module
        }) {
            return Err(refuse("names no record type its module declares"));
        }
        if seen.insert(source.record_type, ()).is_some() {
            return Err(refuse("is the second source for its record type"));
        }
        let table = installed
            .table(source.table)
            .ok_or_else(|| refuse("names no installed Grid table"))?;
        if table.owner != entry.provenance.module {
            return Err(refuse("names a Grid table of another module"));
        }
        if !table
            .field(source.column)
            .is_some_and(|field| field.hidden && field.ty == FieldType::String)
        {
            return Err(refuse("names no hidden text field of its table"));
        }
        out.push(source);
    }
    Ok(out)
}

/// Grid's hook: the tenant's active fields of every source, in their order.
pub(crate) struct Columns {
    pub(crate) fields: std::sync::Arc<CustomFields>,
}

impl CustomColumns for Columns {
    fn tables<'a>(&'a self, client: &'a dyn ReadClient, scope: &'a Scope) -> CustomFuture<'a> {
        Box::pin(async move {
            let mut tables = Vec::with_capacity(self.fields.sources.len());
            for source in &self.fields.sources {
                let definitions = self
                    .fields
                    .defined(client, scope, source.record_type)
                    .await?;
                let columns = definitions
                    .into_iter()
                    .filter(|definition| bilibi_grid::key_shaped(&definition.key))
                    .map(|definition| {
                        let reference = definition
                            .target
                            .as_deref()
                            .and_then(|target| self.fields.records.kind(target))
                            .map(|kind| kind.key);
                        let (ty, value) = value(definition.kind, &definition.key);
                        CustomColumn {
                            key: definition.key,
                            label: definition.label,
                            ty,
                            value,
                            reference,
                            searchable: matches!(
                                definition.kind,
                                Kind::Text
                                    | Kind::LongText
                                    | Kind::Link
                                    | Kind::Phone
                                    | Kind::Email
                            ),
                            rule_condition: definition.rule_condition
                                && definition.kind.rule_condition_kind(),
                        }
                    })
                    .collect();
                tables.push(CustomTable {
                    table: source.table,
                    column: source.column,
                    columns,
                });
            }
            Ok(tables)
        })
    }
}

/// Authz's hook: every active field of every source, restrictable as
/// `cf_<key>` on the source's table (withheld only; none is personal).
pub(crate) struct Restrictable {
    pub(crate) fields: std::sync::Arc<CustomFields>,
}

impl bilibi_authz::field_catalog::TenantFields for Restrictable {
    fn fields<'a>(
        &'a self,
        client: &'a dyn ReadClient,
        scope: &'a Scope,
    ) -> bilibi_authz::field_catalog::TenantFieldsFuture<'a> {
        Box::pin(async move {
            let installed =
                bilibi_grid::installed().map_err(|_| bilibi_database::Error::Internal)?;
            let mut out = Vec::new();
            for source in &self.fields.sources {
                let Some(table) = installed.table(source.table) else {
                    continue;
                };
                for definition in self
                    .fields
                    .defined(client, scope, source.record_type)
                    .await?
                {
                    out.push(bilibi_authz::field_catalog::TenantField {
                        table: source.table,
                        table_label: table.label(),
                        record_types: table.decl.record_types,
                        id: format!("cf_{}", definition.key),
                        label: definition.label,
                    });
                }
            }
            Ok(out)
        })
    }
}

/// A kind's Grid type and its value as SQL over `{json}`. Numbers, dates and
/// yes/no cast only a value of their stored shape, so a value written before
/// the field existed reads as empty rather than failing the statement.
fn value(kind: Kind, key: &str) -> (FieldType, String) {
    let text = format!("({{json}} ->> '{key}')");
    match kind {
        Kind::Number => (
            FieldType::Decimal,
            format!(
                "(CASE WHEN {text} ~ '^-?[0-9]{{1,18}}(\\.[0-9]{{1,6}})?$' THEN {text}::numeric END)"
            ),
        ),
        Kind::Date => (
            FieldType::Date,
            format!(
                "(CASE WHEN {text} ~ '^[0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}$' THEN {text}::date END)"
            ),
        ),
        Kind::Boolean => (
            FieldType::Boolean,
            format!("(CASE {text} WHEN 'true' THEN true WHEN 'false' THEN false END)"),
        ),
        // Several codes read as one text, in the order chosen.
        Kind::Choices => (
            FieldType::String,
            format!(
                "(SELECT string_agg(code, ', ' ORDER BY n) FROM jsonb_array_elements_text(CASE WHEN jsonb_typeof({{json}} -> '{key}') = 'array' THEN {{json}} -> '{key}' END) WITH ORDINALITY AS codes(code, n))"
            ),
        ),
        Kind::Text
        | Kind::LongText
        | Kind::Link
        | Kind::Phone
        | Kind::Email
        | Kind::Choice
        | Kind::Reference => (FieldType::String, text),
    }
}
