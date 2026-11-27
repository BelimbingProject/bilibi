//! Discovery and validation of the installed Grid catalog.

use super::{invalid, link_into, where_, GridContribution, Join, Table, MAX_TABLES};
use crate::Field;
use bilibi_module_registry::{Entry, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Every installed table, validated into one catalog.
#[derive(Debug, Default)]
pub struct Installed {
    tables: BTreeMap<&'static str, Table>,
}
impl Installed {
    /// Validates the complete sorted set. Any defect fails the snapshot build, so a
    /// broken catalog never reaches a page. Nothing here runs a source.
    pub fn from_entries(entries: &[Entry]) -> Result<Self> {
        let mut tables: BTreeMap<&'static str, Table> = BTreeMap::new();
        let mut pending = Vec::new();
        let mut types: BTreeMap<&str, &str> = BTreeMap::new();
        for entry in entries {
            let contribution = entry.get::<GridContribution>()?;
            let owner = entry.provenance.module;
            let at = where_(&entry.provenance);
            for decl in contribution.tables {
                let table = Table::new(decl, owner, entry.provenance, &at)?;
                for kind in decl.record_types {
                    if let Some(first) = types.insert(kind, decl.id) {
                        return Err(invalid(format!(
                            "grid record type {kind} claimed by {first} and {} from {at}",
                            decl.id
                        )));
                    }
                }
                if tables.insert(decl.id, table).is_some() {
                    return Err(invalid(format!(
                        "duplicate grid table {} from {at}",
                        decl.id
                    )));
                }
                if tables.len() > MAX_TABLES {
                    return Err(invalid(format!("more than {MAX_TABLES} grid tables")));
                }
            }
            pending.extend(
                contribution
                    .links
                    .iter()
                    .map(|link| (link, entry.provenance)),
            );
        }
        for (link, provenance) in pending {
            link_into(&mut tables, link, &provenance)?;
        }
        for table in tables.values() {
            let source = table.source();
            for extra in source.extra_tables {
                if !tables.contains_key(extra) {
                    return Err(invalid(format!(
                        "grid source {} on table {} names unknown extra source table {extra}",
                        table.id(),
                        table.id()
                    )));
                }
            }
            let mut derived = BTreeSet::new();
            for column in source.derived {
                if !derived.insert(*column)
                    || table.field(column.column).is_none()
                    || table.is_protected(column.column)
                    || !source.extra_tables.contains(&column.table)
                    || tables
                        .get(column.table)
                        .and_then(|source| source.field(column.field))
                        .is_none()
                {
                    return Err(invalid(format!(
                        "grid source {} has invalid or repeated derived field {} <- {}.{}",
                        table.id(),
                        column.column,
                        column.table,
                        column.field
                    )));
                }
            }
            for extra in source.extra_tables {
                if !source.derived.iter().any(|column| column.table == *extra) {
                    return Err(invalid(format!(
                        "grid source {} has no derived field for extra source table {extra}",
                        table.id()
                    )));
                }
            }
            for (id, source) in table
                .links()
                .iter()
                .filter_map(|link| match link.decl.join {
                    Join::Edge(source) => Some((link.id(), source)),
                    Join::On { .. } => None,
                })
            {
                for extra in source.extra_tables {
                    if !tables.contains_key(extra) {
                        return Err(invalid(format!(
                            "grid source {id} on table {} names unknown extra source table {extra}",
                            table.id()
                        )));
                    }
                }
                if !source.derived.is_empty() {
                    return Err(invalid(format!(
                        "grid edge source {id} on table {} cannot declare projected fields",
                        table.id()
                    )));
                }
            }
        }
        for entry in entries {
            let contribution = entry.get::<GridContribution>()?;
            for schema in contribution.record_fields {
                let table = tables.get_mut(schema.table).ok_or_else(|| {
                    invalid(format!("record fields name unknown table {}", schema.table))
                })?;
                if table.owner != entry.provenance.module
                    && !contribution
                        .links
                        .iter()
                        .any(|link| link.from == schema.table)
                {
                    return Err(invalid(format!(
                        "record fields for {} must own the table or a panel link",
                        schema.table
                    )));
                }
                for field in schema.fields {
                    if field.id.is_empty()
                        || field.label.is_empty()
                        || field.column.is_none() && field.exclusion.is_none()
                    {
                        return Err(invalid(format!(
                            "invalid record field for {}",
                            schema.table
                        )));
                    }
                    if let Some(column) = field.column {
                        if table
                            .record_fields
                            .iter()
                            .any(|previous| previous.column == Some(column))
                        {
                            return Err(invalid(format!(
                                "duplicate record column {column} on {}",
                                schema.table
                            )));
                        }
                    }
                    table.record_fields.push(*field);
                }
            }
        }
        for table in tables.values() {
            for field in table.fields() {
                if let Some(vocabulary) = field.vocabulary {
                    let canonical =
                        bilibi_ui::record_field(vocabulary.vocabulary).ok_or_else(|| {
                            invalid(format!("unknown vocabulary {}", vocabulary.vocabulary))
                        })?;
                    let expected = Field::record(*canonical);
                    if field.id != expected.id
                        || field.label != expected.label
                        || field.ty != expected.ty
                        || field.values != expected.values
                        || field.picklist != expected.picklist
                        || field.personal != expected.personal
                        || field.classified != expected.classified
                    {
                        return Err(invalid(format!(
                            "field {} on {} disagrees with {}",
                            field.id,
                            table.id(),
                            vocabulary.vocabulary
                        )));
                    }
                }
            }
            for field in &table.record_fields {
                if let Some(path) = field.column {
                    if path == field.id
                        && table.field(path).is_some_and(|column| {
                            column
                                .vocabulary
                                .is_none_or(|reference| reference.vocabulary != field.vocabulary)
                        })
                    {
                        return Err(invalid(format!(
                            "record field {} on {} must reference its vocabulary",
                            field.id,
                            table.id()
                        )));
                    }
                    crate::column::resolve(path, table, &|id| tables.get(id)).map_err(|error| {
                        invalid(format!(
                            "record field {} on {}: {error}",
                            field.id,
                            table.id()
                        ))
                    })?;
                }
            }
        }
        Ok(Self { tables })
    }
    pub fn from_inventory() -> Result<Self> {
        let catalog =
            bilibi_module_registry::Catalog::discover(&bilibi_module_registry::discover()?)?;
        Self::from_entries(catalog.entries::<GridContribution>(crate::TABLES)?)
    }
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.tables.values()
    }

    pub fn restricted_record_field_columns<'a>(
        &'a self,
        restricted: impl Fn(&str, &str) -> bool + 'a,
    ) -> impl Iterator<Item = (&'static str, &'static str, &'static str, &'static str)> + 'a {
        self.tables()
            .flat_map(|root| root.record_fields().iter().map(move |field| (root, field)))
            .filter_map(move |(root, field)| {
                if !restricted(root.id(), field.id) {
                    return None;
                }
                let column =
                    crate::column::resolve(field.column?, root, &|id| self.table(id)).ok()?;
                Some((root.id(), field.id, column.table, column.field?.id))
            })
    }

    pub fn table(&self, id: &str) -> Option<&Table> {
        self.tables.get(id)
    }
    pub(crate) fn map(&self) -> &BTreeMap<&'static str, Table> {
        &self.tables
    }
}
