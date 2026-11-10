//! Trusted subject owners. A module that wants private files for its records
//! contributes a `Subject` under a stable key; Media then asks that owner, with
//! the caller's sealed scope, before every attachment operation. Media
//! never learns the owner's tables: the owner resolves its live record through
//! its own public API and refuses missing, deleted or cross-tenant subjects.
use crate::{Error, Result};
use bilibi_module_registry::{Catalog, Consumer, Entry, Module, RegistryError};
use bilibi_party_ids::PersonId;
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
use std::collections::BTreeMap;

pub const SUBJECTS: &str = "base/media.subjects";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubjectAction {
    Upload,
    Submit,
    Retain,
    Download,
    Discard,
}
/// Implement on the owner's own type. Return `Ok` only for a live subject this
/// scope may perform `action` on; the owner checks its own capability. IDs are
/// requests, never proof, and an unknown subject has no access.
pub trait SubjectOwner: Send + Sync {
    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        action: SubjectAction,
        subject_id: &'a str,
    ) -> ProofFuture<'a, ()>;
    /// Whether `subject_id`, one of this owner's subjects that is not a person
    /// ID (a case about a person), belongs to `person` in `scope`'s tenant, so
    /// an approved request about that person reaches its files for export and
    /// erasure. Answer from your own record on `client`, the caller's
    /// connection (inside the erasure's transaction for `erase_attachments`).
    /// A yes vouches for the whole subject, not one attachment: answer yes only
    /// when every file under it belongs to this person. Use separate subjects
    /// (for example, notes) if one record holds different people's files.
    /// Keep the subject linked to the person after erasure until no hold keeps
    /// any of its files and their bytes are removed; held erasures resume later.
    /// It is never an authorization: Media asks it only under a grant.
    /// Default: no.
    fn person_subject<'a>(
        &'a self,
        _client: &'a dyn ProofClient,
        _scope: &'a Scope,
        _person: PersonId,
        _subject_id: &'a str,
    ) -> ProofFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }
}
/// Contribution payload: `Contribution::new(SUBJECTS, "orders.order", provenance, &Subject { owner: &OWNER })`.
pub struct Subject {
    pub owner: &'static dyn SubjectOwner,
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 100
        && key.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        })
}
fn validate(entries: &[Entry]) -> bilibi_module_registry::Result<()> {
    for entry in entries {
        entry.get::<Subject>()?;
        if !valid_key(entry.id) {
            return Err(RegistryError::Contribution(format!(
                "invalid media subject key {} at {}:{}",
                entry.id, entry.provenance.source, entry.provenance.line
            )));
        }
    }
    Ok(())
}
inventory::submit! { Consumer::new::<Subject>(SUBJECTS, "base/media", validate) }

/// Immutable lookup built once. The contributing module must depend on
/// `base/media`, so a stray registration cannot borrow Media's trust.
#[derive(Default)]
pub struct Subjects {
    owners: BTreeMap<&'static str, &'static dyn SubjectOwner>,
}
impl Subjects {
    pub fn from_catalog(
        catalog: &Catalog,
        modules: &[&Module],
    ) -> bilibi_module_registry::Result<Self> {
        let mut owners = BTreeMap::new();
        for entry in catalog.entries::<Subject>(SUBJECTS)? {
            let depends = entry.provenance.module == "base/media"
                || modules.iter().any(|module| {
                    module.id == entry.provenance.module
                        && module.dependencies.contains(&"base/media")
                });
            if !depends {
                return Err(RegistryError::Contribution(format!(
                    "media subject {} at {}:{} is contributed by a module that does not depend on base/media",
                    entry.id, entry.provenance.source, entry.provenance.line
                )));
            }
            owners.insert(entry.id, entry.get::<Subject>()?.owner);
        }
        Ok(Self { owners })
    }
    pub async fn authorize(
        &self,
        client: &dyn ProofClient,
        scope: &Scope,
        key: &str,
        action: SubjectAction,
        subject_id: &str,
    ) -> Result<()> {
        if subject_id.is_empty() || subject_id.len() > 255 {
            return Err(Error::UnknownSubject);
        }
        let owner = self.owners.get(key).ok_or(Error::UnknownSubject)?;
        owner.authorize(client, scope, action, subject_id).await?;
        Ok(())
    }
    /// Whether the registered owner of `key` says `subject_id` belongs to
    /// `person`. An unknown key or an out-of-range ID is no.
    pub(crate) async fn person_subject(
        &self,
        client: &dyn ProofClient,
        scope: &Scope,
        key: &str,
        person: PersonId,
        subject_id: &str,
    ) -> Result<bool> {
        if subject_id.is_empty() || subject_id.len() > 255 {
            return Ok(false);
        }
        let Some(owner) = self.owners.get(key) else {
            return Ok(false);
        };
        Ok(owner
            .person_subject(client, scope, person, subject_id)
            .await?)
    }
}
