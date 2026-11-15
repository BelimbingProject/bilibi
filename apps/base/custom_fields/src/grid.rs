//! The tenant's custom fields as Grid fields, so lists, reports and agents
//! that read through Grid reach them like built-in fields (issue #685). An
//! owner names the Grid table a record type's rows are read from and the
//! hidden JSON field that holds their values (`GridSource`); every fresh
//! scoped catalog then asks this module for the tenant's active fields of
//! those tables (`bilibi_grid::CustomColumns`). Values are read with each key
//! as a validated literal, never request text.
use crate::{CustomFields, RECORD_TYPES, definitions::Kind};
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
    pub(crate) sources: Vec<&'static GridSource>,
}

impl CustomColumns for Columns {
    fn tables<'a>(&'a self, client: &'a dyn ReadClient, scope: &'a Scope) -> CustomFuture<'a> {
        Box::pin(async move {
            if self.sources.is_empty() {
                return Ok(Vec::new());
            }
            let record_types: Vec<&str> = self.sources.iter().map(|s| s.record_type).collect();
            let rows = client
                .query(
                    "SELECT record_type,key,label,kind,target FROM base_custom_fields WHERE tenant_id=$1 AND record_type=ANY($2) AND archived_on IS NULL ORDER BY record_type,position,id",
                    &[&scope.tenant(), &record_types],
                )
                .await?;
            let mut tables: BTreeMap<&str, Vec<CustomColumn>> = BTreeMap::new();
            for row in &rows {
                let record_type: &str = row.get("record_type");
                let key: String = row.get("key");
                let Some(kind) = Kind::parse(row.get("kind")) else {
                    continue;
                };
                // Keys are validated when defined; one that is not shaped never
                // reaches SQL.
                if !bilibi_grid::key_shaped(&key) {
                    continue;
                }
                let target: Option<&str> = row.get("target");
                let reference = match (kind, target) {
                    (Kind::Reference, Some(target)) => match self.fields.records.kind(target) {
                        Some(kind) => Some(kind.key),
                        None => continue,
                    },
                    _ => None,
                };
                let (ty, value) = value(kind, &key);
                let Some(source) = self.sources.iter().find(|s| s.record_type == record_type)
                else {
                    continue;
                };
                tables.entry(source.table).or_default().push(CustomColumn {
                    key,
                    label: row.get("label"),
                    ty,
                    value,
                    reference,
                    searchable: matches!(
                        kind,
                        Kind::Text | Kind::LongText | Kind::Link | Kind::Phone | Kind::Email
                    ),
                });
            }
            Ok(self
                .sources
                .iter()
                .filter_map(|source| {
                    tables.remove(source.table).map(|columns| CustomTable {
                        table: source.table,
                        column: source.column,
                        columns,
                    })
                })
                .collect())
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
