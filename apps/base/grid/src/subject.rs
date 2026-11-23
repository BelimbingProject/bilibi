use crate::{Bounds, Catalog, FieldType, QueryError, Table, query::ident};
use bilibi_module_registry::RegistryError;
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio_postgres::Transaction;

pub trait SubjectAccess: Send + Sync {
    fn readable<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        kind: &'a str,
        id: &'a str,
    ) -> ProofFuture<'a, bool>;
}

/// What the module owning record merges states: `relation` is one static
/// `SELECT` of `tenant_id` (bigint), `kind`, `from_id` and `to_id` (text),
/// a row per merged-away record with the survivor it reads as now. A survivor
/// is never itself merged, so one lookup reaches it.
pub struct Survivors {
    pub relation: &'static str,
    pub access: Arc<dyn SubjectAccess>,
}

static SURVIVORS: OnceLock<Survivors> = OnceLock::new();

/// Binds the merge relation once at boot (`base/records` does).
pub fn bind_survivors(survivors: Survivors) -> bilibi_module_registry::Result<()> {
    SURVIVORS
        .set(survivors)
        .map_err(|_| RegistryError::Service("duplicate Grid survivor binding".into()))
}

pub(crate) fn survivor(subject: crate::Subject, ty: FieldType, raw: &str) -> String {
    let Some(survivors) = SURVIVORS.get() else {
        return raw.into();
    };
    let id = format!(
        "COALESCE((SELECT grid_merged.to_id FROM ({}) grid_merged WHERE grid_merged.tenant_id = $1 AND grid_merged.kind = '{}' AND grid_merged.from_id = ({raw})::text), ({raw})::text)",
        survivors.relation, subject.kind
    );
    if ty == FieldType::Integer {
        format!(
            "(SELECT CASE WHEN pg_input_is_valid(grid_id.id, 'bigint') THEN grid_id.id::bigint END FROM (SELECT {id} AS id) grid_id)"
        )
    } else {
        id
    }
}

pub(crate) fn readable(table: &Table, alias: &str) -> Option<String> {
    let (subject, ty, raw) = field(table, alias)?;
    if SURVIVORS.get().is_none() {
        return Some(format!("{raw} IS NULL"));
    }
    let survivor = survivor(subject, ty, &raw);
    Some(format!(
        "({raw} IS NULL OR COALESCE((SELECT subjects FROM actor)->'{}' ? ({survivor})::text, false))",
        subject.kind
    ))
}

fn field(table: &Table, alias: &str) -> Option<(crate::Subject, FieldType, String)> {
    let subject = table.subject().filter(|_| table.report_subjects)?;
    let field = table.decl.fields.iter().find(|f| f.id == subject.field)?;
    Some((
        subject,
        field.ty,
        format!("{alias}.{}", ident(field.source_key())),
    ))
}

pub(crate) async fn proofs<'a>(
    tx: &Transaction<'_>,
    catalog: &Catalog,
    root: &Table,
    specs: impl IntoIterator<Item = &'a str>,
    bounds: Bounds,
) -> Result<Value, QueryError> {
    let (Some(scope), Some(records)) = (&catalog.subject_scope, SURVIVORS.get()) else {
        return Ok(Value::Null);
    };
    let mut tables = BTreeSet::from([root.id()]);
    for spec in specs {
        if let Ok(column) = catalog.resolve(root, spec) {
            tables.extend(
                column
                    .hops
                    .iter()
                    .chain(&column.many)
                    .chain(&column.tail)
                    .map(|link| link.to()),
            );
        }
    }
    tokio::time::timeout(Duration::from_millis(u64::from(bounds.statement_ms)), async {
        const MAX_SUBJECTS: usize = 1000;
        let mut checked = BTreeSet::new();
        let mut readable: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for table in tables.into_iter().filter_map(|id| catalog.table(id)) {
            let Some((subject, ty, raw)) = field(table, "holder") else {
                continue;
            };
            let survivor = survivor(subject, ty, &raw);
            let sql = format!("WITH actor AS (SELECT $1::bigint AS tenant, $2::bigint AS usr) SELECT DISTINCT id FROM (SELECT ({survivor})::text AS id FROM {} holder CROSS JOIN actor WHERE {raw} IS NOT NULL) candidates WHERE id IS NOT NULL LIMIT {}", table.scoped_source(), MAX_SUBJECTS + 1);
            let params: &[&(dyn tokio_postgres::types::ToSql + Sync)] = &[&scope.tenant(), &scope.user()];
            let explain: Value = tx.query_one(&format!("EXPLAIN (FORMAT JSON) {sql}"), params).await?.try_get(0)?;
            if !explain[0]["Plan"]["Total Cost"].as_f64().is_some_and(|cost| cost <= bounds.max_cost) {
                return Err(QueryError::TooHeavy("the estimated cost of subject checks exceeds its bound"));
            }
            for row in tx.query(&sql, params).await? {
                let id: String = row.try_get(0)?;
                if !checked.insert((subject.kind, id.clone())) {
                    continue;
                }
                if checked.len() > MAX_SUBJECTS {
                    return Err(QueryError::TooHeavy("the number of subjects exceeds the read bound"));
                }
                if records.access.readable(tx, scope, subject.kind, &id).await? {
                    readable.entry(subject.kind).or_default().insert(id);
                }
            }
        }
        Ok(serde_json::json!(readable))
    })
    .await
    .map_err(|_| QueryError::TooHeavy("subject checks exceeded the time bound"))?
}
