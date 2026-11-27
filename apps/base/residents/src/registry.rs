//! Immutable contributed readers. A reader is declared once by its owner; the
//! keys it reads are data (`Residents::keep`).
use bilibi_database::{Error, Result};
use bilibi_module_registry::{Catalog, Consumer, Entry, RegistryError};
use std::collections::BTreeMap;

pub const READERS: &str = "base/residents.readers";
/// A long-lived reader, contributed under `READERS` with the reader ID as the
/// entry ID. `service` belongs to the same module and resolves to
/// [`crate::ReaderService`]. Every reading runs as `principal`, a system
/// principal the same module declares, in the key's tenant, and is decided on
/// `capability`, a key inside that principal's declaration.
#[derive(Clone, Copy, Debug)]
pub struct Reader {
    pub service: &'static str,
    pub principal: &'static str,
    pub capability: &'static str,
}
#[derive(Clone, Copy, Debug)]
pub struct RegisteredReader {
    pub id: &'static str,
    pub module: &'static str,
    pub definition: &'static Reader,
}
/// The readers this composition declares.
pub struct Readers {
    readers: BTreeMap<&'static str, RegisteredReader>,
}
impl Readers {
    pub(crate) fn from_catalog(catalog: &Catalog) -> bilibi_module_registry::Result<Self> {
        let entries = catalog.entries::<Reader>(READERS)?;
        validate(entries)?;
        Ok(Self {
            readers: entries
                .iter()
                .map(|entry| {
                    Ok((
                        entry.id,
                        RegisteredReader {
                            id: entry.id,
                            module: entry.provenance.module,
                            definition: entry.get::<Reader>()?,
                        },
                    ))
                })
                .collect::<bilibi_module_registry::Result<_>>()?,
        })
    }
    pub fn get(&self, id: &str) -> Result<RegisteredReader> {
        self.readers.get(id).copied().ok_or(Error::Invalid)
    }
    pub fn iter(&self) -> impl Iterator<Item = RegisteredReader> + '_ {
        self.readers.values().copied()
    }
}
fn validate(entries: &[Entry]) -> bilibi_module_registry::Result<()> {
    for entry in entries {
        let reader = entry.get::<Reader>()?;
        let bytes = entry.id.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 128
            || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
            || !bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_/-".contains(b))
            || !reader
                .service
                .starts_with(&format!("{}.", entry.provenance.module))
            || [reader.principal, reader.capability]
                .iter()
                .any(|key| key.is_empty() || key.len() > 200)
        {
            return Err(RegistryError::Contribution(format!(
                "invalid resident reader {} at {}:{}",
                entry.id, entry.provenance.source, entry.provenance.line
            )));
        }
    }
    Ok(())
}
inventory::submit! { Consumer::new::<Reader>(READERS, "base/residents", validate) }

/// A key is a reference such as a record ID, shown to operators: lowercase
/// letters, digits and `._:/-`, at most 128 bytes, never a person's data.
pub(crate) fn valid_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._:/-".contains(b))
}
/// A failure code operators see: `[a-z][a-z0-9_]{0,63}`.
pub(crate) fn valid_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}
