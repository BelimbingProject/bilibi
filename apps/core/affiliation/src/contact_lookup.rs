//! Current work contact comparison, owned here rather than joined by callers.
use crate::{
    contributions::{LIST, VIEW},
    AffiliationId, AffiliationService, PersonId,
};
use bilibi_authz::{authorize_key, restrictions_for};
use bilibi_database::{Error, Result};
use bilibi_person::{ContactAddress, ContactMatches};
use bilibi_tenancy::Scope;
use serde::Serialize;
use tokio_postgres::GenericClient;

#[derive(Clone, Debug, Serialize)]
pub struct WorkContactMatch {
    pub affiliation: AffiliationId,
    pub person: PersonId,
}

impl AffiliationService {
    /// Current means linked and within inclusive UTC calendar dates. Retired
    /// Persons and archived companies retain readable work history; this is
    /// not a proof that either endpoint is writable. Reproves the caller on
    /// this client and refuses missing source/list/detail or field access.
    pub async fn match_contact_in(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        address: &ContactAddress,
        limit: i64,
    ) -> Result<ContactMatches<WorkContactMatch>> {
        let scope = self.fresh(client, scope).await?;
        for capability in [LIST, VIEW, bilibi_person::LIST, bilibi_person::VIEW] {
            authorize_key(client, &scope, capability).await?;
        }
        let restrictions = restrictions_for(client, &scope, "person_affiliations").await?;
        let (field, expression, uncomparable) = match address {
            ContactAddress::Email(_) => ("work_email", "lower(work_email)", "false"),
            ContactAddress::Phone(_) => (
                "work_phone",
                "core_person_phone_key(work_phone)",
                "EXISTS(SELECT 1 FROM source WHERE original IS NOT NULL AND key IS NULL)",
            ),
        };
        if restrictions.contains(field) {
            return Err(Error::Forbidden);
        }
        if !(1..=100).contains(&limit) {
            return Err(Error::Invalid);
        }
        let key = address.canonical_in(client).await?;
        let rows = client.query(&format!(
            "WITH source AS NOT MATERIALIZED (
                SELECT id,person_id,{field} AS original,{expression} AS key FROM person_affiliations
                WHERE tenant_id=$1 AND unlinked_at IS NULL
                AND (effective_from IS NULL OR effective_from <= (CURRENT_TIMESTAMP AT TIME ZONE 'UTC')::date)
                AND (effective_to IS NULL OR effective_to >= (CURRENT_TIMESTAMP AT TIME ZONE 'UTC')::date)),
             candidates AS (SELECT id,person_id FROM source WHERE key=$2 ORDER BY id LIMIT $3),
             status AS (SELECT {uncomparable} AS uncomparable)
             SELECT candidates.id,candidates.person_id,status.uncomparable FROM status LEFT JOIN candidates ON true ORDER BY candidates.id"
        ), &[&scope.tenant(), &key, &(limit + 1)]).await?;
        let uncomparable = rows[0].get("uncomparable");
        let mut entries = Vec::new();
        for row in &rows {
            if let Some(id) = row.get::<_, Option<i64>>("id") {
                entries.push(WorkContactMatch {
                    affiliation: AffiliationId::new(id).map_err(|_| Error::Internal)?,
                    person: PersonId::new(row.get("person_id")).map_err(|_| Error::Internal)?,
                });
            }
        }
        let overflow = entries.len() > limit as usize;
        entries.truncate(limit as usize);
        Ok(ContactMatches {
            entries,
            overflow,
            uncomparable,
        })
    }
}
