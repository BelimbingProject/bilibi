//! Immutable contributed workers. Transport policy belongs to trusted owner code.
use crate::execution::{EndingBinding, ENDINGS};
use bilibi_database::{Error, Result};
use bilibi_module_registry::{Catalog, Consumer, Entry, RegistryError};
use serde_json::Value;
use std::collections::BTreeMap;

pub const WORKERS: &str = "base/queue.workers";
/// Validator must enforce the owner's argument allowlist and business types.
/// Arguments must contain references, never credentials or secret payloads.
/// Execution is at least once; owners must make business effects idempotent.
#[derive(Clone, Copy)]
pub struct Worker {
    pub max_attempts: i32,
    pub validate: fn(Value) -> Result<Value>,
}
#[derive(Clone, Copy)]
pub struct RegisteredWorker {
    pub id: &'static str,
    pub module: &'static str,
    pub definition: &'static Worker,
}
pub struct Workers {
    workers: BTreeMap<&'static str, RegisteredWorker>,
    /// Workers whose owner is told when a job ends without finishing.
    told: Vec<String>,
}
impl Workers {
    pub fn from_catalog(catalog: &Catalog) -> bilibi_module_registry::Result<Self> {
        let entries = catalog.entries::<Worker>(WORKERS)?;
        validate(entries)?;
        Ok(Self {
            workers: entries
                .iter()
                .map(|entry| {
                    Ok((
                        entry.id,
                        RegisteredWorker {
                            id: entry.id,
                            module: entry.provenance.module,
                            definition: entry.get::<Worker>()?,
                        },
                    ))
                })
                .collect::<bilibi_module_registry::Result<_>>()?,
            told: catalog
                .entries::<EndingBinding>(ENDINGS)?
                .iter()
                .map(|entry| entry.id.to_owned())
                .collect(),
        })
    }
    pub fn get(&self, id: &str) -> Result<RegisteredWorker> {
        self.workers.get(id).copied().ok_or(Error::Invalid)
    }
    pub fn iter(&self) -> impl Iterator<Item = RegisteredWorker> + '_ {
        self.workers.values().copied()
    }
    pub(crate) fn told(&self) -> &[String] {
        &self.told
    }
}
fn validate(entries: &[Entry]) -> bilibi_module_registry::Result<()> {
    for entry in entries {
        let worker = entry.get::<Worker>()?;
        let bytes = entry.id.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 128
            || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
            || !bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_/-".contains(b))
            || !(1..=100).contains(&worker.max_attempts)
        {
            return Err(RegistryError::Contribution(format!(
                "invalid queue worker {} at {}:{}",
                entry.id, entry.provenance.source, entry.provenance.line
            )));
        }
    }
    Ok(())
}
inventory::submit! { Consumer::new::<Worker>(WORKERS,"base/queue",validate) }
