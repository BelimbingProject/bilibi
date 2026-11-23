//! A row report's resolved filters, sorts, columns, bounds and time zone.
use crate::{Outcome, grid};
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
    pub reference_names: Vec<crate::ReferenceName>,
    pub total: i64,
    pub totals: Vec<Value>,
    pub columns: Vec<Column>,
    pub rows: Vec<(Value, Vec<Value>)>,
}

impl Listing {
    /// Rows `offset..offset + limit` in sort order: the keys, then their
    /// cells under the same bounds the keys were read under.
    pub(crate) async fn page(
        &self,
        reports: &crate::Reports,
        scope: &Scope,
        offset: usize,
        limit: usize,
        measures: &[bilibi_grid::Measure],
        file_cap: Option<usize>,
    ) -> Outcome<ListPage> {
        let (keys, projection, totals) = bilibi_grid::report_page(
            &reports.db,
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
            &self.specs,
            measures,
            file_cap,
        )
        .await
        .map_err(grid)?;
        let mut rows: Vec<_> = projection
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
        let reference_names = reports
            .name_references(
                scope,
                &projection.columns,
                rows.iter_mut().map(|(_, cells)| cells),
                false,
            )
            .await?;
        Ok(ListPage {
            reference_names,
            total: keys.total,
            totals,
            columns: projection.columns,
            rows,
        })
    }
}
