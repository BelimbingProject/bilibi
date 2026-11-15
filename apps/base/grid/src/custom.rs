//! Tenant-defined fields in the scoped catalog (issue #685). A record owner
//! keeps a tenant's custom values in one JSON column of its own row; the
//! definitions are data, so they cannot be declared in code. `base/custom_fields`
//! binds the one [`CustomColumns`] hook, and every fresh catalog asks it for the
//! tenant's active fields, which then join the owner's table as ordinary
//! fields named `cf_<key>`: columns, filters, groupings, sorts and measures of
//! lists and reports, under the table's own capability, reach and tenant
//! predicate. Grid never reads the definitions itself, so it does not depend
//! on the module that owns them.
use crate::FieldType;
use bilibi_database::Result;
use bilibi_module_registry::RegistryError;
use bilibi_tenancy::{ReadClient, Scope};
use std::{
    collections::HashSet,
    future::Future,
    pin::Pin,
    sync::{Mutex, OnceLock},
};

/// One tenant-defined field of a table, as its definitions module states it.
#[derive(Clone, Debug)]
pub struct CustomColumn {
    /// The field's stable key: a lower-case letter, then letters, digits and
    /// `_`. The catalog field is `cf_<key>`.
    pub key: String,
    /// The tenant's current label.
    pub label: String,
    pub ty: FieldType,
    /// The value as SQL over the row's JSON column, written `{json}`. Built by
    /// the definitions module from validated keys and declared pick-list keys
    /// only; it may bind the tenant as `$1`, as every Grid statement does.
    pub value: String,
    /// For a reference to another record: the Grid table of its kind. The
    /// column shows that record's name through the reader's own scoped source
    /// of that table, and exists only when the reader may read it.
    pub reference: Option<&'static str>,
    /// Whether people find a record by this field's text in global search
    /// (free text, links, phone numbers and email addresses).
    pub searchable: bool,
}

/// The tenant-defined fields one Grid table takes, kept in its JSON field
/// `column` (a hidden field the table declares).
#[derive(Clone, Debug)]
pub struct CustomTable {
    pub table: &'static str,
    pub column: &'static str,
    pub columns: Vec<CustomColumn>,
}

pub type CustomFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<CustomTable>>> + Send + 'a>>;

/// What the module owning tenant definitions answers for each fresh catalog:
/// the tenant's active fields of every table that takes them, read on the
/// catalog's own connection.
pub trait CustomColumns: Send + Sync {
    fn tables<'a>(&'a self, client: &'a dyn ReadClient, scope: &'a Scope) -> CustomFuture<'a>;
}

static HOOK: OnceLock<&'static dyn CustomColumns> = OnceLock::new();

/// Binds the hook once at boot (`base/custom_fields` does).
pub fn bind_custom_columns(hook: &'static dyn CustomColumns) -> bilibi_module_registry::Result<()> {
    HOOK.set(hook)
        .map_err(|_| RegistryError::Service("duplicate Grid custom-column binding".into()))
}

pub(crate) fn hook() -> Option<&'static dyn CustomColumns> {
    HOOK.get().copied()
}

/// Whether `key` is a custom field key: the only text that enters a custom
/// field's id and its SQL literal.
pub fn key_shaped(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && key.len() <= 40
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A field id `cf_<key>` as the `&'static str` every catalog field carries.
/// Keys never change and are never reused, so each distinct key text is kept
/// once for the process however many catalogs name it; labels, which tenants
/// rename, are not kept here (`Table::field_label`).
pub(crate) fn field_id(key: &str) -> &'static str {
    static IDS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let id = format!("cf_{key}");
    let mut ids = IDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(held) = ids.get(id.as_str()) {
        return held;
    }
    let held: &'static str = Box::leak(id.into_boxed_str());
    ids.insert(held);
    held
}
