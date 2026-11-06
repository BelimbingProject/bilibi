//! A document's tenant-defined custom fields (`base/custom_fields`, ADR 21).
//! Values live in `core_documents.custom_fields`, keyed by the field's stable
//! key, and are checked here with the owner's write; the definitions, their
//! page and their agent operations are Base's.
use crate::model::{Custom, FieldError};
use crate::store::{On, Store};
use crate::{WriteError, WriteResult};

/// The record type Documents offers to `base/custom_fields`.
pub(crate) const RECORD_TYPE: &str = "document";

/// How a custom field is named in a form control, an error and an audit row:
/// `cf_<key>`, so it never collides with a built-in field.
pub(crate) fn name(key: &str) -> String {
    format!("cf_{key}")
}

/// `current` with `stored` folded in (a `null` clears a key).
pub(crate) fn merge(current: &Custom, stored: &Custom) -> Custom {
    let mut next = current.clone();
    for (key, value) in stored {
        if value.is_null() {
            next.remove(key);
        } else {
            next.insert(key.clone(), value.clone());
        }
    }
    next
}

impl Store<'_> {
    /// Checks a patch of custom values against the tenant's active fields and
    /// returns the values as they will be stored. A commit holds changed
    /// pick-list codes; a preview only reads.
    pub(crate) async fn check_custom(
        &self,
        on: On<'_>,
        scope: &bilibi_tenancy::Scope,
        patch: &Custom,
        current: &Custom,
    ) -> WriteResult<Custom> {
        if patch.is_empty() {
            return Ok(Custom::new());
        }
        let checked = match on {
            On::Commit(tx) => {
                self.custom
                    .validate_locked(tx, scope, RECORD_TYPE, patch, current)
                    .await?
            }
            On::Preview(client) => {
                self.custom
                    .validate(client, scope, RECORD_TYPE, patch, current)
                    .await?
            }
        };
        checked.map_err(|errors| {
            WriteError::Invalid(
                errors
                    .into_iter()
                    .map(|e| FieldError::owned(name(&e.field), e.message))
                    .collect(),
            )
        })
    }
}
