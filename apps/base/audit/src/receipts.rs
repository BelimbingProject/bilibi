//! Bounded audit evidence for owner-authorized customisation receipts.
use bilibi_database::Result;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::GenericClient;

/// Read recent changes of the given owner-authorized kinds. The caller must
/// authorize every kind and any attribution and values it exposes.
pub async fn customisation_receipts(
    client: &impl GenericClient,
    scope: &Scope,
    kinds: &[&str],
) -> Result<Vec<Value>> {
    let rows = client.query("SELECT id, trace_id, auditable_type, auditable_id, actor_type, actor_id, impersonator_id, system_principal, occurred_at::text, old_values, new_values FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type=ANY($2) AND trace_id IS NOT NULL AND occurred_at >= now() - interval '30 days' ORDER BY occurred_at DESC,id DESC LIMIT 501", &[&scope.tenant(), &kinds]).await?;
    Ok(rows.iter().map(|row| json!({
        "mutation_id": row.get::<_, i64>(0), "trace": row.get::<_, String>(1), "kind": row.get::<_, String>(2),
        "id": row.get::<_, String>(3), "actor_type": row.get::<_, String>(4),
        "actor_id": row.get::<_, i64>(5), "impersonator_id": row.get::<_, Option<i64>>(6),
        "system_principal": row.get::<_, Option<String>>(7), "when": row.get::<_, String>(8),
        "before": row.get::<_, Option<Value>>(9), "after": row.get::<_, Option<Value>>(10)
    })).collect())
}
