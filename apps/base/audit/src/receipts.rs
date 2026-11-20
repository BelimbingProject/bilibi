use bilibi_database::Result;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::GenericClient;

pub async fn customisation_receipts(
    client: &impl GenericClient,
    scope: &Scope,
    kinds: &[&str],
    before: Option<i64>,
    limit: i64,
) -> Result<(Vec<Value>, Option<i64>)> {
    let limit = limit.clamp(1, 100);
    let rows = client.query(
        "WITH candidates AS (
             SELECT trace_id,max(id) AS last_id FROM base_audit_mutations
             WHERE tenant_id=$1 AND auditable_type=ANY($2) AND trace_id IS NOT NULL
             GROUP BY trace_id HAVING ($3::bigint IS NULL OR max(id)<$3)
             ORDER BY last_id DESC LIMIT ($4::bigint + 1)
         ), page AS (
             SELECT trace_id,last_id FROM candidates ORDER BY last_id DESC LIMIT $4
         )
         SELECT m.id,m.trace_id,m.auditable_type,m.auditable_id,m.actor_type,m.actor_id,m.impersonator_id,
                m.system_principal,to_char(m.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),
                m.old_values,m.new_values,
                CASE WHEN (SELECT count(*) FROM candidates)>$4 THEN (SELECT min(last_id) FROM page) END
         FROM page JOIN base_audit_mutations m ON m.trace_id=page.trace_id
         WHERE m.tenant_id=$1 AND m.auditable_type=ANY($2)
         ORDER BY page.last_id DESC,m.id DESC",
        &[&scope.tenant(), &kinds, &before, &limit],
    ).await?;
    let next = rows.first().and_then(|row| row.get(11));
    Ok((rows.iter().map(|row| json!({
        "mutation_id": row.get::<_, i64>(0), "trace": row.get::<_, String>(1), "kind": row.get::<_, String>(2),
        "id": row.get::<_, String>(3), "actor_type": row.get::<_, String>(4),
        "actor_id": row.get::<_, i64>(5), "impersonator_id": row.get::<_, Option<i64>>(6),
        "system_principal": row.get::<_, Option<String>>(7), "when": row.get::<_, String>(8),
        "before": row.get::<_, Option<Value>>(9), "after": row.get::<_, Option<Value>>(10)
    })).collect(), next))
}

pub async fn latest_customisation_mutation(
    client: &impl GenericClient,
    scope: &Scope,
    kind: &str,
    subject: &Value,
) -> Result<Option<i64>> {
    Ok(client
        .query_one(
            "SELECT max(id) FROM base_audit_mutations
         WHERE tenant_id=$1 AND auditable_type=$2 AND (old_values @> $3 OR new_values @> $3)",
            &[&scope.tenant(), &kind, &subject],
        )
        .await?
        .get(0))
}
