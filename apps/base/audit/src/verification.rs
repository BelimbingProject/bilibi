//! Verification of the evidence hash chains in bounded steps (ADR 05).
//!
//! Each tenant has its own chain in each ledger. A verification walks every
//! chain of both ledgers, [`STEP_LINKS`] links per statement, and stores where
//! it is after each step (`base_audit_verifications`), so a queued job that
//! stops resumes there instead of starting again. One verification runs at a
//! time. The stored row is working state; the result is also recorded as an
//! `audit.chain.verified` action in the step that finishes it.
use crate::{insert_action, Attribution};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_tenancy::Scope;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::SystemTime;
use tokio_postgres::{Client, GenericClient, Row, Transaction};

/// Links one step recomputes, in one statement.
pub const STEP_LINKS: i32 = 10_000;
/// The audit kind of a verification's own change rows.
pub const VERIFICATION_KIND: &str = "base/audit.verification";
/// Each step is one short transaction under this.
const STEP_BOUNDS: &str = "SET LOCAL statement_timeout = '60s'";

/// One ledger's verification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainReport {
    /// `mutations` or `actions`.
    pub chain: String,
    /// Tenant chains walked to their end.
    pub tenants: i64,
    /// Links recomputed.
    pub checked: i64,
    /// Committed rows with no link: their chain trigger was bypassed.
    pub unsealed: i64,
    /// Where the ledger first breaks, if it does.
    pub break_at: Option<ChainBreak>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainBreak {
    /// The tenant whose chain breaks (0: rows of no tenant); `None` when the
    /// break is not in one chain.
    pub tenant: Option<i64>,
    /// The link at the break, in that tenant's chain.
    pub seq: Option<i64>,
    /// The evidence row at the break, when one is there.
    pub row: Option<i64>,
    /// `missing` (a link was removed), `changed` (a row's facts differ from
    /// its link), `tail` (the last links were removed), `head` (a tenant's
    /// whole chain was removed) or `unsealed` (a committed row was never
    /// linked).
    pub reason: String,
}

/// One verification as the screen reads it.
#[derive(Clone, Debug)]
pub struct Verification {
    pub id: i64,
    /// `running`, `whole`, `broken` or `abandoned` (it stopped progressing and
    /// a later one replaced it).
    pub state: String,
    /// The ledger being walked while it runs.
    pub chain: String,
    /// Links recomputed so far in that ledger.
    pub checked: i64,
    /// One report per finished ledger.
    pub reports: Vec<ChainReport>,
    pub requested_by: i64,
    pub requested_at: SystemTime,
    pub finished_at: Option<SystemTime>,
}
impl Verification {
    pub fn running(&self) -> bool {
        self.state == "running"
    }
    fn from_row(row: &Row) -> Self {
        Self {
            id: row.get(0),
            state: row.get(1),
            chain: row.get(2),
            checked: row.get(3),
            reports: serde_json::from_value(row.get::<_, Value>(4)).unwrap_or_default(),
            requested_by: row.get(5),
            requested_at: row.get(6),
            finished_at: row.get(7),
        }
    }
}
const COLUMNS: &str = "id,state,chain,checked,reports,requested_by,requested_at,finished_at";

/// The latest verification, running or finished. Platform-wide evidence:
/// callers must restrict it to platform operators.
pub async fn latest_verification(client: &impl GenericClient) -> Result<Option<Verification>> {
    Ok(client
        .query_opt(
            &format!("SELECT {COLUMNS} FROM base_audit_verifications ORDER BY id DESC LIMIT 1"),
            &[],
        )
        .await?
        .as_ref()
        .map(Verification::from_row))
}

/// Starts a verification in the caller's audited transaction and returns its
/// ID; the caller enqueues the job that walks it in the same transaction.
/// `Conflict` while another is running. One that has not progressed for 30
/// minutes is closed as `abandoned` first: its job is gone. Platform-wide
/// evidence: callers must restrict it to platform operators.
pub async fn request_verification(tx: &mut MutationTransaction<'_>, scope: &Scope) -> Result<i64> {
    if tx.context().tenant != scope.tenant() {
        return Err(Error::Forbidden);
    }
    tx.sql()
        .execute(
            "UPDATE base_audit_verifications SET state='abandoned',finished_at=clock_timestamp() \
             WHERE finished_at IS NULL AND progressed_at < clock_timestamp() - interval '30 minutes'",
            &[],
        )
        .await?;
    let id: i64 = tx
        .sql()
        .query_opt(
            "INSERT INTO base_audit_verifications(tenant_id,requested_by) VALUES($1,$2) \
             ON CONFLICT DO NOTHING RETURNING id",
            &[&scope.tenant(), &scope.user()],
        )
        .await?
        .ok_or(Error::Conflict)?
        .get(0);
    tx.record(Change {
        kind: VERIFICATION_KIND.into(),
        id,
        old: json!({}),
        new: json!({"state": "running"}),
    })
    .await?;
    Ok(id)
}

/// Takes a running verification one step further, in one short transaction,
/// and returns it as it now stands: the next tenant's chain is chosen, or up
/// to [`STEP_LINKS`] links of the current one are recomputed, or a ledger is
/// closed with its report. The step that closes the second ledger finishes the
/// verification and records `audit.chain.verified` as `who`, in the same
/// transaction. A verification that is not running is returned unchanged, so a
/// repeated job does nothing; `NotFound` when there is none.
pub async fn advance_verification(
    client: &mut Client,
    id: i64,
    who: &Attribution,
) -> Result<Verification> {
    let tx = client.transaction().await?;
    tx.batch_execute(STEP_BOUNDS).await?;
    let Some(row) = tx
        .query_opt(
            "SELECT chain,chain_tenant,tenant_done,seq,hash,checked,tenants,reports \
             FROM base_audit_verifications WHERE id=$1 AND state='running' FOR UPDATE",
            &[&id],
        )
        .await?
    else {
        tx.rollback().await?;
        return client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM base_audit_verifications WHERE id=$1"),
                &[&id],
            )
            .await?
            .as_ref()
            .map(Verification::from_row)
            .ok_or(Error::NotFound);
    };
    let chain: String = row.get(0);
    let tenant: i64 = row.get(1);
    let checked: i64 = row.get(5);
    let tenants: i64 = row.get(6);
    let reports: Vec<ChainReport> = serde_json::from_value(row.get(7)).unwrap_or_default();
    if row.get::<_, bool>(2) {
        let next = tx
            .query_opt(
                "SELECT tenant_id FROM base_audit_chain_heads WHERE chain=$1 AND tenant_id>$2 ORDER BY tenant_id LIMIT 1",
                &[&chain, &tenant],
            )
            .await?;
        match next {
            Some(next) => {
                tx.execute(
                    "UPDATE base_audit_verifications SET chain_tenant=$2,tenant_done=false,seq=0,hash=''::bytea,\
                     progressed_at=clock_timestamp() WHERE id=$1",
                    &[&id, &next.get::<_, i64>(0)],
                )
                .await?;
            }
            None => close_ledger(&tx, id, who, &chain, (tenants, checked), None, reports).await?,
        }
    } else {
        let range = tx
            .query_one(
                "SELECT checked,last_seq,last_hash,finished,break_seq,break_id,reason \
                 FROM base_audit_verify_range($1,$2,$3,$4,$5)",
                &[
                    &chain,
                    &tenant,
                    &row.get::<_, i64>(3),
                    &row.get::<_, Vec<u8>>(4),
                    &STEP_LINKS,
                ],
            )
            .await?;
        let checked = checked + range.get::<_, i64>(0);
        match range.get::<_, Option<String>>(6) {
            Some(reason) => {
                let broken = ChainBreak {
                    tenant: Some(tenant),
                    seq: range.get(4),
                    row: range.get(5),
                    reason,
                };
                close_ledger(
                    &tx,
                    id,
                    who,
                    &chain,
                    (tenants, checked),
                    Some(broken),
                    reports,
                )
                .await?;
            }
            None => {
                let finished: bool = range.get(3);
                tx.execute(
                    "UPDATE base_audit_verifications SET seq=$2,hash=$3,checked=$4,tenant_done=$5,tenants=$6,\
                     progressed_at=clock_timestamp() WHERE id=$1",
                    &[
                        &id,
                        &range.get::<_, i64>(1),
                        &range.get::<_, Vec<u8>>(2),
                        &checked,
                        &finished,
                        &(tenants + i64::from(finished)),
                    ],
                )
                .await?;
            }
        }
    }
    let now = tx
        .query_one(
            &format!("SELECT {COLUMNS} FROM base_audit_verifications WHERE id=$1"),
            &[&id],
        )
        .await?;
    tx.commit().await?;
    Ok(Verification::from_row(&now))
}

/// Closes the ledger being walked with its report and moves to the next, or
/// finishes the verification. A ledger whose chains all hold is still broken
/// when its heads are not numbered without gaps (a tenant's whole chain was
/// removed) or a committed row has no link.
async fn close_ledger(
    tx: &Transaction<'_>,
    id: i64,
    who: &Attribution,
    chain: &str,
    (tenants, checked): (i64, i64),
    mut break_at: Option<ChainBreak>,
    mut reports: Vec<ChainReport>,
) -> Result<()> {
    let table = match chain {
        "mutations" => "base_audit_mutations",
        "actions" => "base_audit_actions",
        _ => return Err(Error::Internal),
    };
    let heads = tx
        .query_one(
            "SELECT count(*),coalesce(max(born),0) FROM base_audit_chain_heads WHERE chain=$1",
            &[&chain],
        )
        .await?;
    if break_at.is_none() && heads.get::<_, i64>(0) != heads.get::<_, i64>(1) {
        break_at = Some(ChainBreak {
            tenant: None,
            seq: None,
            row: None,
            reason: "head".into(),
        });
    }
    let unlinked = tx
        .query_one(
            &format!("SELECT count(*),min(id) FROM {table} WHERE chain_seq IS NULL"),
            &[],
        )
        .await?;
    let unsealed: i64 = unlinked.get(0);
    if break_at.is_none() && unsealed > 0 {
        break_at = Some(ChainBreak {
            tenant: None,
            seq: None,
            row: unlinked.get(1),
            reason: "unsealed".into(),
        });
    }
    reports.push(ChainReport {
        chain: chain.to_owned(),
        tenants,
        checked,
        unsealed,
        break_at,
    });
    let stored = serde_json::to_value(&reports).map_err(|_| Error::Internal)?;
    if chain == "mutations" {
        tx.execute(
            "UPDATE base_audit_verifications SET chain='actions',chain_tenant=-1,tenant_done=true,seq=0,hash=''::bytea,\
             checked=0,tenants=0,reports=$2,progressed_at=clock_timestamp() WHERE id=$1",
            &[&id, &stored],
        )
        .await?;
        return Ok(());
    }
    let whole = reports.iter().all(|report| report.break_at.is_none());
    tx.execute(
        "UPDATE base_audit_verifications SET state=$2,reports=$3,progressed_at=clock_timestamp(),\
         finished_at=clock_timestamp() WHERE id=$1",
        &[&id, &if whole { "whole" } else { "broken" }, &stored],
    )
    .await?;
    insert_action(
        tx,
        who,
        None,
        "audit.chain.verified",
        &json!({"verification": id, "whole": whole, "chains": reports}),
        true,
        None,
    )
    .await?;
    Ok(())
}
