//! Planner statistics after a large batch (#719). Until autovacuum analyzes
//! a table, the planner keeps its old row count, so a search straight after
//! an import or update that wrote hundreds of rows can pick a plan that runs
//! for minutes (#717). A work slice counts the rows each of its transactions
//! wrote, per table, and analyzes every table it wrote enough rows into once
//! the slice stops writing.
use bilibi_database::Result;
use std::collections::HashMap;
use tokio_postgres::{Client, Transaction};

/// Rows one slice writes into a table before that table is analyzed: below
/// it the planner's estimate is near enough, and autovacuum catches up.
const ANALYZE_AFTER: i64 = 200;

/// The rows a work slice has written so far, per table.
#[derive(Default)]
pub(crate) struct Written(HashMap<String, i64>);

impl Written {
    /// Adds the rows `tx` wrote, the owner's tables included; call it just
    /// before `tx` commits.
    pub(crate) async fn add(&mut self, tx: &Transaction<'_>) -> Result<()> {
        for row in tx
            .query(
                "SELECT relid::regclass::text, n_tup_ins+n_tup_upd+n_tup_del FROM pg_stat_xact_user_tables WHERE n_tup_ins+n_tup_upd+n_tup_del>0",
                &[],
            )
            .await?
        {
            *self.0.entry(row.get(0)).or_default() += row.get::<_, i64>(1);
        }
        Ok(())
    }

    /// Analyzes the tables given at least [`ANALYZE_AFTER`] rows, on `client`
    /// outside any transaction. The writes are committed already, so a
    /// failure (a role that may not analyze) is reported and not the run's.
    pub(crate) async fn analyze(self, client: &Client) {
        let mut tables: Vec<String> = self
            .0
            .into_iter()
            .filter(|(_, rows)| *rows >= ANALYZE_AFTER)
            .map(|(table, _)| table)
            .collect();
        if tables.is_empty() {
            return;
        }
        tables.sort();
        // The names come from the catalog (`regclass` quotes them), never
        // from the run's input.
        let tables = tables.join(", ");
        if let Err(error) = client.batch_execute(&format!("ANALYZE {tables}")).await {
            eprintln!("bulk data: could not analyze {tables} after a large batch: {error}");
        }
    }
}
