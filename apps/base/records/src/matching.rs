//! Survivor-equivalent membership for a holder's own SQL reads.
use crate::{Access, Records, Reference};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use tokio_postgres::Transaction;

// Redirects collapse on write (`redirect` runs only at READ COMMITTED), so the
// survivor and its direct predecessors are the complete equivalence class. The reverse index serves this lookup.
pub(crate) const MEMBERSHIP_SQL: &str = "SELECT $1::bigint AS tenant_id,$2::text AS record_kind,$3::text AS record_id UNION ALL SELECT tenant_id,kind,from_id FROM base_records_redirects WHERE tenant_id=$1 AND kind=$2 AND to_id=$3";

/// An owner-built membership subquery for existing stored references.
/// Execute immediately on the transaction and scope passed to
/// [`Records::matching`]; do not cache it or treat it as write authority.
pub struct ReferenceMatch {
    tenant: i64,
    reference: Reference,
}

impl ReferenceMatch {
    /// The readable survivor, in its one spelling.
    pub fn reference(&self) -> &Reference {
        &self.reference
    }

    /// Three columns containing `(tenant_id, record_kind, record_id)` for the
    /// survivor and every merged predecessor. Embed the whole relation as
    /// `(tenant_id, record_kind, record_id) IN (...)`. This is owner SQL: use
    /// it whole, without inspecting or rewriting it. It has no result cap and
    /// does not multiply holder rows.
    pub fn sql(&self) -> &'static str {
        MEMBERSHIP_SQL
    }

    /// Bind these values to `$1` (tenant), `$2` (kind), `$3` (survivor).
    /// The holder's other parameters start at `$4`.
    pub fn parameters(&self) -> (i64, &str, &str) {
        (self.tenant, &self.reference.kind, &self.reference.id)
    }
}

impl Records {
    /// Match existing references to a readable survivor, including references
    /// stored before any transitive merge. Unknown/unmounted kinds, malformed
    /// ids, missing/removed/other-tenant records and unreadable survivors all
    /// return `None`, without disclosing predecessor membership.
    ///
    /// All checks use the caller's transaction, with no pool checkout or
    /// transaction control. It must use REPEATABLE READ or SERIALIZABLE
    /// (`Error::Invalid` otherwise): the owner check, redirects and holder's
    /// count/pages then see one snapshot, even across concurrent merges.
    /// Read access is checked as in `resolve`, including retired records;
    /// new references must still use `check` and its selection rule.
    ///
    /// The caller owns holder authorization and query admission: apply Grid's
    /// `BOUNDS` and `MAX_ESTIMATED_COST` to its complete statements, paginate
    /// rows within Grid's limits, and propagate failures rather than returning
    /// partial counts/pages. No holder rows or audit history are rewritten.
    pub async fn matching(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        kind: &str,
        id: &str,
    ) -> Result<Option<ReferenceMatch>> {
        let isolation: String = tx
            .query_one("SHOW transaction_isolation", &[])
            .await?
            .get(0);
        if !matches!(isolation.as_str(), "repeatable read" | "serializable") {
            return Err(Error::Invalid);
        }
        let Some(record) = self.kind(kind) else {
            return Ok(None);
        };
        let Some(id) = Self::canonical(record, id) else {
            return Ok(None);
        };
        let id = Self::redirected(tx, scope.tenant(), kind, std::slice::from_ref(&id))
            .await?
            .remove(&id)
            .unwrap_or(id);
        if record.owner.authorize(tx, scope, &id).await? != Access::Readable {
            return Ok(None);
        }
        Ok(Some(ReferenceMatch {
            tenant: scope.tenant(),
            reference: Reference::new(kind, id),
        }))
    }
}
