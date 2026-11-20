use bilibi_database::Result;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::GenericClient;

pub async fn customisation_receipts(
    client: &impl GenericClient,
    scope: &Scope,
    kinds: &[&str],
    filters: &[Option<Value>],
    before: Option<i64>,
    limit: i64,
) -> Result<(Vec<Value>, Option<i64>)> {
    let limit = limit.clamp(1, 100);
    if kinds.len() != filters.len() {
        return Err(bilibi_database::Error::Invalid);
    }
    let rows = client.query(
        "WITH sources AS (SELECT * FROM unnest($2::text[], $3::jsonb[]) AS s(kind, filter)),
         scan AS MATERIALIZED (
           SELECT m.id,m.trace_id FROM sources s CROSS JOIN LATERAL (
             SELECT id,trace_id FROM base_audit_mutations m
             WHERE m.tenant_id=$1 AND m.auditable_type=s.kind AND m.trace_id IS NOT NULL AND ($4::bigint IS NULL OR m.id<$4)
               AND (s.filter IS NULL OR m.old_values @> s.filter OR m.new_values @> s.filter)
             ORDER BY m.id DESC LIMIT ($5::bigint + 1)
           ) m ORDER BY m.id DESC LIMIT ($5::bigint + 1)
         ), candidates AS (SELECT id,trace_id FROM scan ORDER BY id DESC LIMIT $5),
         traces AS (
           SELECT DISTINCT c.trace_id FROM candidates c
           WHERE $4::bigint IS NULL OR NOT EXISTS (
             SELECT 1 FROM base_audit_mutations prior JOIN sources s ON s.kind=prior.auditable_type
             WHERE prior.tenant_id=$1 AND prior.trace_id=c.trace_id AND prior.id >= $4
               AND (s.filter IS NULL OR prior.old_values @> s.filter OR prior.new_values @> s.filter))
         )
         SELECT m.id,m.trace_id,m.auditable_type,m.auditable_id,m.actor_type,m.actor_id,m.impersonator_id,
                m.system_principal,to_char(m.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),
                m.old_values,m.new_values,
                cursor.next_id
         FROM (SELECT CASE WHEN (SELECT count(*) FROM scan)>$5 THEN (SELECT min(id) FROM candidates) END AS next_id) cursor
         LEFT JOIN (traces t JOIN base_audit_mutations m ON m.trace_id=t.trace_id JOIN sources s ON s.kind=m.auditable_type
           AND m.tenant_id=$1 AND (s.filter IS NULL OR m.old_values @> s.filter OR m.new_values @> s.filter)) ON true
         ORDER BY m.id DESC NULLS LAST",
        &[&scope.tenant(), &kinds, &filters, &before, &limit],
    ).await?;
    let next = rows.first().and_then(|row| row.get(11));
    Ok((rows.iter().filter(|row| row.get::<_, Option<i64>>(0).is_some()).map(|row| json!({
        "mutation_id": row.get::<_, i64>(0), "trace": row.get::<_, String>(1), "kind": row.get::<_, String>(2),
        "id": row.get::<_, String>(3), "actor_type": row.get::<_, String>(4),
        "actor_id": row.get::<_, i64>(5), "impersonator_id": row.get::<_, Option<i64>>(6),
        "system_principal": row.get::<_, Option<String>>(7), "when": row.get::<_, String>(8),
        "before": row.get::<_, Option<Value>>(9), "after": row.get::<_, Option<Value>>(10)
    })).collect(), next))
}

pub async fn is_current_customisation_mutation(
    client: &impl GenericClient,
    scope: &Scope,
    kind: &str,
    subject: &Value,
    receipt: i64,
) -> Result<bool> {
    Ok(client.query_one(
        "SELECT EXISTS (SELECT 1 FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type=$2 AND id=$4 AND (old_values @> $3 OR new_values @> $3)) AND NOT EXISTS (SELECT 1 FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type=$2 AND id>$4 AND (old_values @> $3 OR new_values @> $3))",
        &[&scope.tenant(), &kind, &subject, &receipt],
    ).await?.get(0))
}
