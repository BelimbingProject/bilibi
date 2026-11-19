//! Bounded attribution reads for owner-authorized inventories.
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use tokio_postgres::GenericClient;

/// Attribution of the latest recorded change to a requested record.
#[derive(Clone, Debug)]
pub struct LatestChange {
    pub kind: String,
    pub id: i64,
    pub actor_type: String,
    pub actor_id: i64,
    pub system_principal: Option<String>,
    pub occurred_at: std::time::SystemTime,
}

/// Latest evidence for a bounded set of records in the caller's tenant.
/// Callers authorize access to the records and the attribution they display.
pub async fn latest_changes(
    client: &impl GenericClient,
    scope: &Scope,
    records: &[(&str, i64)],
) -> Result<Vec<LatestChange>> {
    if records.len() > 100 || records.iter().any(|(kind, id)| kind.is_empty() || *id <= 0) {
        return Err(Error::Invalid);
    }
    let kinds: Vec<&str> = records.iter().map(|(kind, _)| *kind).collect();
    let ids: Vec<String> = records.iter().map(|(_, id)| id.to_string()).collect();
    let rows = client.query("SELECT wanted.kind, wanted.id, m.actor_type, m.actor_id, m.system_principal, m.occurred_at FROM unnest($2::text[], $3::text[]) AS wanted(kind,id) CROSS JOIN LATERAL (SELECT actor_type, actor_id, system_principal, occurred_at FROM base_audit_mutations WHERE tenant_id=$1 AND auditable_type=wanted.kind AND auditable_id=wanted.id ORDER BY occurred_at DESC, id DESC LIMIT 1) m", &[&scope.tenant(), &kinds, &ids]).await?;
    rows.iter()
        .map(|row| {
            Ok(LatestChange {
                kind: row.get(0),
                id: row
                    .get::<_, String>(1)
                    .parse()
                    .map_err(|_| Error::Internal)?,
                actor_type: row.get(2),
                actor_id: row.get(3),
                system_principal: row.get(4),
                occurred_at: row.get(5),
            })
        })
        .collect()
}
