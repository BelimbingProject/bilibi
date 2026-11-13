//! A list result's question as the run resolved it for the reader: the
//! record type, filters, sorts, columns, read bounds and time zone. A run
//! reads its one page through it; a file keeps it and reads the remaining
//! pages with only the keys and the cells (`export::tabulate`), never a
//! whole run per page.
use crate::{grid, Outcome};
use bilibi_database::Db;
use bilibi_grid::{Bounds, Column, Filter, PageSpec, Sort};
use bilibi_tenancy::Scope;
use serde_json::Value;

pub(crate) struct Listing {
    pub record_type: String,
    pub filters: Vec<Filter>,
    pub sorts: Vec<Sort>,
    /// Each column once, as the projection draws it.
    pub specs: Vec<String>,
    pub bounds: Bounds,
    pub timezone: String,
}

/// One page of a list: the rows the reader may read, each its key and one
/// cell per column, and how many rows the question matched when it was read.
pub(crate) struct ListPage {
    pub total: i64,
    pub columns: Vec<Column>,
    pub rows: Vec<(Value, Vec<Value>)>,
}

impl Listing {
    /// Rows `offset..offset + limit` in sort order: the keys, then their
    /// cells under the same bounds the keys were read under.
    pub(crate) async fn page(
        &self,
        db: &Db,
        scope: &Scope,
        offset: usize,
        limit: usize,
    ) -> Outcome<ListPage> {
        let keys = bilibi_grid::page_keys(
            db,
            scope,
            &self.record_type,
            &PageSpec {
                bounds: self.bounds,
                filters: self.filters.clone(),
                sorts: self.sorts.clone(),
                timezone: self.timezone.clone(),
                offset,
                limit,
            },
        )
        .await
        .map_err(grid)?;
        let specs: Vec<&str> = self.specs.iter().map(String::as_str).collect();
        let projection = bilibi_grid::project_within(
            db,
            scope,
            &self.record_type,
            &keys.keys,
            &specs,
            self.bounds,
        )
        .await
        .map_err(grid)?;
        let rows = projection
            .rows
            .into_iter()
            .map(|row| {
                let cells = projection
                    .columns
                    .iter()
                    .map(|c| row.cells.get(&c.id).cloned().unwrap_or(Value::Null))
                    .collect();
                (row.key, cells)
            })
            .collect();
        Ok(ListPage {
            total: keys.total,
            columns: projection.columns,
            rows,
        })
    }
}
