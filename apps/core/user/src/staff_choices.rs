//! Tenant-wide staff pick for another owner's assignment control. The caller
//! names the capability its own operation needs; Core User decides it for the
//! actor, so no `admin.user.*` grant is required and no id is typed.
use crate::UserService;
use bilibi_authz::authorize_key;
use bilibi_database::{Error, Result};
use bilibi_security::Scope;
use tokio_postgres::GenericClient;

/// The most staff one search returns.
pub const STAFF_CHOICE_MAX: usize = 20;
/// The longest search text, in bytes.
pub const STAFF_SEARCH_MAX: usize = 255;

/// A staff account offered in a picker: the id a caller stores and the name a
/// person recognises. No email, credential or desk leaves this module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaffChoice {
    pub id: i64,
    pub name: String,
}

/// Live staff only, as `standing` proves them on every sign-in: an active
/// identity, an active and not SCIM-deactivated account, and a linked Person
/// that has not been retired. The name test is a word-start match through
/// `position`, so typed `%` and `_` stay literal and no pattern is built.
const SEARCH: &str = "SELECT u.id,u.name FROM users u JOIN user_identities i ON i.id=u.identity_id AND i.status='active' AND i.deleted_at IS NULL WHERE u.tenant_id=$1 AND u.kind='staff' AND u.deleted_at IS NULL AND u.status='active' AND NOT EXISTS(SELECT 1 FROM user_scim_users s WHERE s.tenant_id=u.tenant_id AND s.user_id=u.id AND NOT s.active) AND (u.person_id IS NULL OR EXISTS(SELECT 1 FROM persons p WHERE p.tenant_id=u.tenant_id AND p.id=u.person_id AND p.retired_at IS NULL AND p.system_key IS NULL)) AND ($2='' OR starts_with(lower(u.name),$2) OR position(' '||$2 in lower(u.name))>0) ORDER BY lower(u.name),u.id LIMIT $3";

async fn search(
    client: &impl GenericClient,
    scope: &Scope,
    capability: &str,
    text: &str,
) -> Result<Vec<StaffChoice>> {
    let text = text.trim();
    if text.len() > STAFF_SEARCH_MAX {
        return Err(Error::Invalid);
    }
    // Decided and logged for the actor, failing closed for an unknown key.
    authorize_key(client, scope, capability).await?;
    Ok(client
        .query(
            SEARCH,
            &[
                &scope.tenant(),
                &text.to_lowercase(),
                &(STAFF_CHOICE_MAX as i64),
            ],
        )
        .await?
        .iter()
        .map(|row| StaffChoice {
            id: row.get(0),
            name: row.get(1),
        })
        .collect())
}

impl UserService {
    /// Same-connection staff search for a caller holding a transaction.
    pub async fn search_staff_on(
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        capability: &str,
        text: &str,
    ) -> Result<Vec<StaffChoice>> {
        search(client, scope, capability, text).await
    }
    /// Up to [`STAFF_CHOICE_MAX`] active staff of the scope's tenant whose
    /// name has a word starting with `text` (all, in name order, when it is
    /// empty). `capability` is the caller's own declared capability for the
    /// operation the choice feeds: the actor must hold it now, or the answer
    /// is `Forbidden`. A choice is still re-proved by the caller's write.
    pub async fn search_staff(
        &self,
        scope: &Scope,
        capability: &str,
        text: &str,
    ) -> Result<Vec<StaffChoice>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        Self::search_staff_on(&**client, &scope, capability, text).await
    }
}
