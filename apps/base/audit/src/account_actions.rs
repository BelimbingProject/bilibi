//! Account-linked action evidence for the owning account module's privacy handler.
use bilibi_database::{Error, Result};
use bilibi_personal_data::Disclosure;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::GenericClient;

// Account IDs belong to the actor's tenant, which can differ on oversight reads.
// Other principal types use a different identity namespace and never match.
const MATCH: &str = "FROM base_audit_actions a WHERE a.tenant_id=$1 \
    AND coalesce(a.actor_tenant_id,a.tenant_id)=$1 \
    AND ((a.actor_type IN ('user','agent') AND a.actor_id=ANY($2)) OR a.impersonator_id=ANY($2))";
const PAGE: i64 = 1000;
const MAX_ROWS: usize = 100_000;

/// Counts actions of tenant-local accounts for privacy discovery and retained
/// evidence reporting. The account owner resolves `users` from the subject,
/// including inactive accounts; a matching email alone is never sufficient.
pub async fn count_account_actions(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    users: &[i64],
) -> Result<i64> {
    Ok(client
        .query_one(
            &format!("SELECT count(*) {MATCH}"),
            &[&scope.tenant(), &users],
        )
        .await?
        .get(0))
}

/// Exports all actions of tenant-local accounts for an approved disclosure.
/// The account owner checks the grant's subject and resolves `users` from it.
/// Includes impersonation and person-backed agents, with each action once.
/// Reads bounded pages; refuses an oversized bundle rather than truncating it.
/// Erased values stay erased. Retention and bookmarks are not changed.
pub async fn export_account_actions(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    grant: &Disclosure,
    users: &[i64],
) -> Result<Vec<Value>> {
    grant.check(scope)?;
    let mut records = Vec::new();
    let mut after = 0_i64;
    loop {
        let rows = client
            .query(
                &format!(
                    "SELECT a.id,a.actor_type,a.actor_id,a.impersonator_id,a.event,\
                to_char(a.occurred_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),\
                host(a.ip_address),a.url,a.user_agent,a.payload,\
                to_char(a.erased_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),\
                coalesce((SELECT m.is_retained FROM base_audit_action_metadata m \
                    WHERE m.partition_month=a.partition_month AND m.action_id=a.id),a.is_retained),\
                a.actor_role,a.agent_self_report,a.client_identity,a.trace_id,a.company_id,\
                to_char(a.recorded_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                {MATCH} AND a.id>$3 ORDER BY a.id LIMIT $4"
                ),
                &[&scope.tenant(), &users, &after, &PAGE],
            )
            .await?;
        let done = rows.len() < PAGE as usize;
        for row in rows {
            after = row.get(0);
            records.push(json!({
                "id": after, "actor_type": row.get::<_, String>(1),
                "actor_id": row.get::<_, i64>(2), "impersonator_id": row.get::<_, Option<i64>>(3),
                "event": row.get::<_, String>(4), "occurred_at": row.get::<_, String>(5),
                "ip_address": row.get::<_, Option<String>>(6), "url": row.get::<_, Option<String>>(7),
                "user_agent": row.get::<_, Option<String>>(8), "payload": row.get::<_, Option<Value>>(9),
                "erased_at": row.get::<_, Option<String>>(10), "retained": row.get::<_, bool>(11),
                "actor_role": row.get::<_, Option<String>>(12),
                "agent_self_report": row.get::<_, Option<String>>(13),
                "client_identity": row.get::<_, Option<String>>(14), "trace_id": row.get::<_, Option<String>>(15),
                "company_id": row.get::<_, Option<i64>>(16), "recorded_at": row.get::<_, Option<String>>(17),
            }));
        }
        if records.len() > MAX_ROWS {
            return Err(Error::Invalid);
        }
        if done {
            return Ok(records);
        }
    }
}
