use crate::{query::ident, Bounds, Catalog, FieldType, QueryError, Table};
use bilibi_module_registry::RegistryError;
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio_postgres::{types::ToSql, Transaction};

pub trait SubjectAccess: Send + Sync {
    fn integer_key(&self, kind: &str) -> bool;
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
    let integer = survivors.access.integer_key(subject.kind);
    let raw = if integer && ty != FieldType::Integer {
        format!(
            "CASE WHEN ({raw}) ~ '^[+-]?[0-9]+$' AND octet_length({raw}) <= 64 AND pg_input_is_valid({raw}, 'bigint') THEN (({raw})::bigint)::text END"
        )
    } else {
        format!("({raw})::text")
    };
    let id = format!(
        "COALESCE((SELECT grid_merged.to_id FROM ({}) grid_merged WHERE grid_merged.tenant_id = $1 AND grid_merged.kind = '{}' AND grid_merged.from_id = ({raw})), ({raw}))",
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
    (head, rows, params): (&str, &str, &[&(dyn ToSql + Sync)]),
    bounds: Bounds,
) -> Result<Value, QueryError> {
    let (Some(scope), Some(records)) = (&catalog.subject_scope, SURVIVORS.get()) else {
        return Ok(Value::Null);
    };
    let mut candidates = BTreeSet::new();
    if let Some((subject, ty, raw)) = field(root, "r") {
        let id = survivor(subject, ty, &raw);
        candidates.insert(format!(
            "SELECT '{}' AS kind, ({id})::text AS id {rows}",
            subject.kind
        ));
    }
    for spec in specs {
        let Ok(column) = catalog.resolve(root, spec) else {
            continue;
        };
        let links: Vec<_> = column
            .hops
            .iter()
            .chain(&column.many)
            .chain(&column.tail)
            .collect();
        for (i, link) in links.iter().enumerate() {
            let target = catalog.table(link.to()).expect("resolved target");
            if target.subject().is_none() {
                continue;
            }
            let walked = crate::query::walk_links(catalog, root, &links[..=i], false);
            if let Some((subject, ty, raw)) = field(walked.table, &walked.alias) {
                let id = survivor(subject, ty, &raw);
                candidates.insert(format!(
                    "SELECT '{}' AS kind, candidate.id FROM (SELECT r.* {rows}) r CROSS JOIN LATERAL (SELECT ({id})::text AS id {}) candidate",
                    subject.kind, walked.suffix,
                ));
            }
        }
    }
    if candidates.is_empty() {
        return Ok(Value::Null);
    }
    const MAX_SUBJECTS: usize = 1000;
    let candidates = candidates
        .into_iter()
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let sql = format!(
        "{head} SELECT DISTINCT kind, id FROM ({candidates}) candidates WHERE id IS NOT NULL LIMIT {}",
        MAX_SUBJECTS + 1
    );
    tokio::time::timeout(
        Duration::from_millis(u64::from(bounds.statement_ms)),
        async {
            let explain: Value = tx
                .query_one(&format!("EXPLAIN (FORMAT JSON) {sql}"), params)
                .await?
                .try_get(0)?;
            if !explain[0]["Plan"]["Total Cost"]
                .as_f64()
                .is_some_and(|cost| cost <= bounds.max_cost)
            {
                return Err(QueryError::TooHeavy(
                    "the estimated cost of subject checks exceeds its bound",
                ));
            }
            let candidates = tx.query(&sql, params).await?;
            if candidates.len() > MAX_SUBJECTS {
                return Err(QueryError::TooHeavy(
                    "the number of subjects exceeds the read bound",
                ));
            }
            let mut readable: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for row in candidates {
                let kind: String = row.try_get(0)?;
                let id: String = row.try_get(1)?;
                if records.access.readable(tx, scope, &kind, &id).await? {
                    readable.entry(kind).or_default().insert(id);
                }
            }
            Ok(serde_json::json!(readable))
        },
    )
    .await
    .map_err(|_| QueryError::TooHeavy("subject checks exceeded the time bound"))?
}
