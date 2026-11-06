//! Exact contact comparison on the caller's connection; never a reservation.
use crate::{PersonId, PersonService, LIST, VIEW};
use bilibi_authz::{authorize_key, restrictions_for};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use serde::Serialize;
use tokio_postgres::GenericClient;

/// One personal or work contact criterion. Originals remain with their owner.
#[derive(Clone, Debug)]
pub enum ContactAddress {
    Email(String),
    Phone(String),
}
impl ContactAddress {
    /// Owner comparison key on the supplied client. Email follows Person's
    /// validator and case-insensitive comparison. Phone requires an explicit
    /// `+` or `00` international prefix; bare digits are refused.
    pub async fn canonical_in(&self, client: &impl GenericClient) -> Result<String> {
        let (value, sql) = match self {
            Self::Email(value) => {
                crate::validate_person(&crate::PersonInput {
                    name: "Contact lookup".into(),
                    kind: crate::PersonKind::Human,
                    email: Some(value.clone()),
                    phone: None,
                })
                .map_err(|_| Error::Invalid)?;
                (value.trim(), "SELECT lower($1::text)")
            }
            Self::Phone(value) => {
                if value.trim().chars().count() > 64 || value.contains('\0') {
                    return Err(Error::Invalid);
                }
                (value.trim(), "SELECT core_person_phone_key($1::text)")
            }
        };
        if value.is_empty() {
            return Err(Error::Invalid);
        }
        client
            .query_one(sql, &[&value])
            .await?
            .get::<_, Option<String>>(0)
            .ok_or(Error::Invalid)
    }
}

/// Bounded exact candidates, never an inferred identity or uniqueness proof.
#[derive(Clone, Debug, Serialize)]
pub struct ContactMatches<T> {
    pub entries: Vec<T>,
    /// More exact candidates exist than the caller's limit.
    pub overflow: bool,
    /// At least one non-null source phone lacks an explicit international
    /// prefix and cannot be compared. Even an empty result is then not a complete negative.
    pub uncomparable: bool,
}
impl<T> ContactMatches<T> {
    pub fn complete(&self) -> bool {
        !self.overflow && !self.uncomparable
    }
}

impl PersonService {
    /// Personal contacts only, including retained retired identities, as in
    /// duplicate lookup. Reproves Scope and list/detail/field access on this
    /// client. Errors (including missing access) are never empty matches.
    /// Sees earlier owner writes in this transaction and reserves nothing.
    pub async fn match_contact_in(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        address: &ContactAddress,
        limit: i64,
    ) -> Result<ContactMatches<PersonId>> {
        let scope = self.fresh(client, scope).await?;
        authorize_key(client, &scope, LIST).await?;
        authorize_key(client, &scope, VIEW).await?;
        let restrictions = restrictions_for(client, &scope, "persons").await?;
        let field = match address {
            ContactAddress::Email(_) => "email",
            ContactAddress::Phone(_) => "phone",
        };
        if restrictions.contains(field) {
            return Err(Error::Forbidden);
        }
        if !(1..=100).contains(&limit) {
            return Err(Error::Invalid);
        }
        let key = address.canonical_in(client).await?;
        // Only fixed owner expressions reach SQL. The status and candidates
        // share one statement snapshot, including uncommitted caller writes.
        let (expression, uncomparable) = match address {
            ContactAddress::Email(_) => ("lower(email)", "false"),
            ContactAddress::Phone(_) => (
                "core_person_phone_key(phone)",
                "EXISTS(SELECT 1 FROM source WHERE original IS NOT NULL AND key IS NULL)",
            ),
        };
        let rows = client.query(&format!(
            "WITH source AS NOT MATERIALIZED (SELECT id,{field} AS original,{expression} AS key FROM persons WHERE tenant_id=$1),
             candidates AS (SELECT id FROM source WHERE key=$2 ORDER BY id LIMIT $3),
             status AS (SELECT {uncomparable} AS uncomparable)
             SELECT candidates.id,status.uncomparable FROM status LEFT JOIN candidates ON true ORDER BY candidates.id"
        ), &[&scope.tenant(), &key, &(limit + 1)]).await?;
        let uncomparable = rows[0].get("uncomparable");
        let mut entries = rows
            .iter()
            .filter_map(|row| row.get::<_, Option<i64>>("id"))
            .map(|id| PersonId::new(id).map_err(|_| Error::Internal))
            .collect::<Result<Vec<_>>>()?;
        let overflow = entries.len() > limit as usize;
        entries.truncate(limit as usize);
        Ok(ContactMatches {
            entries,
            overflow,
            uncomparable,
        })
    }
}
