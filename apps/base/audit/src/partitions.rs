//! Whole-month removal. The SQL owner guards live evidence and checkpoints;
//! the upper operation owns the capability and configured retention period.
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_tenancy::{ReadClient, Scope};
use serde::Serialize;
use serde_json::json;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditLedger {
    Actions,
    Mutations,
}
impl AuditLedger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Actions => "actions",
            Self::Mutations => "mutations",
        }
    }
    fn table(self) -> &'static str {
        match self {
            Self::Actions => "base_audit_actions",
            Self::Mutations => "base_audit_mutations",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PartitionRemoval {
    pub ledger: AuditLedger,
    pub month: String,
    pub retention_days: i32,
    pub removed: i64,
    pub retained: i64,
}

/// Counts one expired recording month. Platform-wide: the caller must prove
/// operator standing and its removal capability before calling this read.
pub async fn partition_preview(
    client: &dyn ReadClient,
    ledger: AuditLedger,
    month: &str,
    days: i32,
) -> Result<PartitionRemoval> {
    if !(1..=365_000).contains(&days) || month.len() != 10 || !month.ends_with("-01") {
        return Err(Error::Invalid);
    }
    let row = client.query_opt(
        "SELECT $1::text::date < date_trunc('month',clock_timestamp() AT TIME ZONE 'UTC')::date \
         AND (($1::text::date + interval '1 month') AT TIME ZONE 'UTC') <= clock_timestamp()-make_interval(days=>$2), \
         EXISTS (SELECT 1 FROM pg_inherits WHERE inhparent=to_regclass($3) \
         AND inhrelid=to_regclass($3 || '_' || to_char($1::text::date,'YYYYMM'))), \
         EXISTS (SELECT 1 FROM base_audit_verifications WHERE state='running')",
        &[&month, &days, &ledger.table()],
    ).await?.ok_or(Error::Internal)?;
    if !row.get::<_, bool>(0) {
        return Err(Error::Invalid);
    }
    if !row.get::<_, bool>(1) {
        return Err(Error::NotFound);
    }
    if row.get::<_, bool>(2) {
        return Err(Error::Conflict);
    }
    let source = match ledger {
        AuditLedger::Actions => "SELECT coalesce(b.is_retained,a.is_retained) AS keep FROM base_audit_actions a \
            LEFT JOIN base_audit_action_metadata b ON b.action_id=a.id AND b.partition_month=a.partition_month \
            WHERE a.partition_month=$1::text::date",
        AuditLedger::Mutations => "SELECT is_retained AS keep FROM base_audit_mutations WHERE partition_month=$1::text::date",
    };
    let counts = client.query_opt(
        &format!("SELECT count(*) FILTER (WHERE NOT keep), count(*) FILTER (WHERE keep) FROM ({source}) q"),
        &[&month],
    ).await?.ok_or(Error::Internal)?;
    Ok(PartitionRemoval {
        ledger,
        month: month.to_owned(),
        retention_days: days,
        removed: counts.get(0),
        retained: counts.get(1),
    })
}

/// Removes an expired recording month in the caller's audited transaction.
/// The caller owns the capability and settings; this boundary reproves the
/// operator and the database independently checks age, retention and links.
pub async fn remove_partition(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    ledger: AuditLedger,
    month: &str,
    days: i32,
    reason: &str,
) -> Result<PartitionRemoval> {
    let scope = scope.clone();
    let month = month.to_owned();
    let reason = reason.to_owned();
    tx.coordinate(move |tx| Box::pin(async move {
        let context = scope.mutation_context("audit.partition.remove");
        if tx.context().tenant != context.tenant || (tx.context().actor.kind != context.actor.kind || tx.context().actor.id != context.actor.id) {
            return Err(Error::Forbidden);
        }
        bilibi_tenancy::prove_operator(tx.sql(), &scope).await?;
        if reason.trim().is_empty() || reason.chars().count() > 2000 {
            return Err(Error::Invalid);
        }
        tx.sql().batch_execute("SET LOCAL lock_timeout='5s'; SET LOCAL statement_timeout='60s'").await?;
        // Check malformed input before the cast in the removal function.
        partition_preview(tx.sql(), ledger, &month, days).await?;
        let row = tx.sql().query_one(
            "SELECT removal_id,removed,retained FROM base_audit_remove_partition($1,$2::text::date,$3,$4,$5,$6,$7)",
            &[&ledger.as_str(), &month, &days, &scope.tenant(), &scope.user(), &tx.context().trace_id, &reason],
        ).await?;
        let result = PartitionRemoval {
            ledger,
            month: month.to_owned(),
            retention_days: days,
            removed: row.get(1),
            retained: row.get(2),
        };
        tx.record(Change {
            kind: "base/audit.partition_removal".into(),
            id: row.get(0),
            old: json!({}),
            new: json!({"ledger":ledger,"month":month,"retention_days":days,"removed":result.removed,"retained":result.retained,"reason":reason}),
        }).await?;
        Ok(result)
    })).await
}
