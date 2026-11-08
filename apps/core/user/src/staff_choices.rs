//! Tenant-wide staff pick for another owner's assignment control. The caller
//! names the capability its own operation needs, which its owner declared a
//! staff-assignment capability; Core User decides it for the actor, so no
//! `admin.user.*` grant is required and no id is typed.
use crate::UserService;
use bilibi_authz::authorize_key;
use bilibi_database::{Error, Result};
use bilibi_module_registry::{Catalog, Consumer, Entry, Module, RegistryError};
use bilibi_security::Scope;
use std::collections::BTreeSet;
use tokio_postgres::GenericClient;

pub const ASSIGNMENTS: &str = "core/user.staff_assignments";

/// A capability its owner declares as the authority to assign staff. Only a
/// declared key can ask for the tenant's staff.
#[derive(Debug)]
pub struct StaffAssignment {
    pub key: &'static str,
}
impl StaffAssignment {
    pub const fn new(key: &'static str) -> Self {
        Self { key }
    }
}

/// Declares one staff-assignment capability from module `$module`, which must
/// depend on `core/user` and declare the capability in its own vocabulary:
/// `bilibi_user::declare_staff_assignment!("domain/module", StaffAssignment::new("domain.module.thing.assign"));`
#[macro_export]
macro_rules! declare_staff_assignment {
    ($module:literal, $assignment:expr) => {
        $crate::inventory::submit! {{
            const ASSIGNMENT: $crate::StaffAssignment = $assignment;
            $crate::Contribution::new(
                $crate::ASSIGNMENTS,
                ASSIGNMENT.key,
                $crate::Provenance { module: $module, source: file!(), line: line!() },
                &ASSIGNMENT,
            )
        }}
    };
}

fn validate(entries: &[Entry]) -> std::result::Result<(), RegistryError> {
    let mut keys = BTreeSet::new();
    for entry in entries {
        let assignment = entry.get::<StaffAssignment>()?;
        if entry.id != assignment.key || !keys.insert(assignment.key) {
            return Err(RegistryError::Contribution(format!(
                "staff assignment {} at {}:{} is keyed wrongly or declared twice",
                entry.id, entry.provenance.source, entry.provenance.line
            )));
        }
    }
    Ok(())
}

inventory::submit! { Consumer::new::<StaffAssignment>(ASSIGNMENTS, "core/user", validate) }

/// The declared staff-assignment capabilities of the mounted modules.
pub(crate) struct Assignments(BTreeSet<&'static str>);
impl Assignments {
    /// Each key must be a capability its declaring module owns, and that
    /// module must depend on `core/user`, so the graph shows who asks.
    pub(crate) fn from_catalog(
        catalog: &Catalog,
        modules: &[&Module],
    ) -> std::result::Result<Self, RegistryError> {
        let vocabulary = bilibi_authz::vocabulary::Vocabulary::from_catalog(catalog)?;
        let mut keys = BTreeSet::new();
        for entry in catalog.entries::<StaffAssignment>(ASSIGNMENTS)? {
            let module = entry.provenance.module;
            let at = format!(
                "staff assignment {} at {}:{}",
                entry.id, entry.provenance.source, entry.provenance.line
            );
            if module != "core/user"
                && !modules
                    .iter()
                    .any(|m| m.id == module && m.dependencies.contains(&"core/user"))
            {
                return Err(RegistryError::Contribution(format!(
                    "{at} comes from {module}, which does not depend on core/user"
                )));
            }
            if vocabulary
                .capabilities
                .get(entry.id)
                .is_none_or(|owned| owned.provenance.module != module)
            {
                return Err(RegistryError::Contribution(format!(
                    "{at} is not a capability {module} declares"
                )));
            }
            keys.insert(entry.get::<StaffAssignment>()?.key);
        }
        Ok(Self(keys))
    }
}

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
    assignments: &Assignments,
    client: &impl GenericClient,
    scope: &Scope,
    capability: &str,
    text: &str,
) -> Result<Vec<StaffChoice>> {
    let text = text.trim();
    if text.len() > STAFF_SEARCH_MAX {
        return Err(Error::Invalid);
    }
    if !assignments.0.contains(capability) {
        return Err(Error::Forbidden);
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
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        capability: &str,
        text: &str,
    ) -> Result<Vec<StaffChoice>> {
        search(&self.staff_assignments, client, scope, capability, text).await
    }
    /// Up to [`STAFF_CHOICE_MAX`] active staff of the scope's tenant whose
    /// name has a word starting with `text` (all, in name order, when it is
    /// empty). `capability` is the caller's own capability for the
    /// operation the choice feeds, declared with `declare_staff_assignment!`:
    /// an undeclared key, or one the actor does not hold now, is `Forbidden`. A choice is still re-proved by the caller's write.
    pub async fn search_staff(
        &self,
        scope: &Scope,
        capability: &str,
        text: &str,
    ) -> Result<Vec<StaffChoice>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        self.search_staff_on(&**client, &scope, capability, text)
            .await
    }
}
