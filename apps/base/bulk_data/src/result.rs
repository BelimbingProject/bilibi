//! Optional owner projection of a create contract's business result.
use bilibi_database::{Error, Result};
use bilibi_module_registry::{Consumer, Entry, RegistryError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const IMPORT_RESULTS: &str = "base/bulk_data.import_results";

/// Optional contribution with the same id and owner as its `RecordType`.
/// Receives only the create contract's `result` (without its envelope), for
/// both preview and commit. It must project facts already authorized by the
/// owner, without reads or writes. An unknown/malformed result must fail,
/// never guess that a record was created. Commit's answer is authoritative.
#[derive(Debug)]
pub struct ImportResults {
    pub interpret: fn(&Value) -> Result<ImportResult>,
}

/// A successful owner business outcome. Refusals remain contract errors.
#[derive(Clone, Copy, Debug)]
pub enum ImportOutcome {
    Create,
    Update,
    /// A business action completed without creating the imported record,
    /// such as recording another enquiry against an existing record.
    Action,
}

#[derive(Clone, Debug)]
pub struct ImportResult {
    pub outcome: ImportOutcome,
    /// Only an id of this imported record type, usable by a child import.
    /// Other kinds of records belong in `links`, not here.
    pub record_id: Option<String>,
    /// Safe business outcome text, without personal or restricted fields.
    pub message: Option<String>,
    /// Up to four safe record/task links. These are saved under the run's
    /// normal person/company visibility; destination owners enforce access.
    pub links: Vec<ResultLink>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResultLink {
    /// Plain text, for example "Enquiry task 42"; no personal record names.
    pub label: String,
    /// Application-local absolute path, never an external URL.
    pub path: String,
}

impl ImportResult {
    pub(crate) fn validate(&self) -> Result<()> {
        let text = |s: &str, max| !s.trim().is_empty() && s.chars().count() <= max;
        if self.record_id.as_ref().is_some_and(|id| !text(id, 100))
            || self.message.as_ref().is_some_and(|s| !text(s, 2000))
            || self.links.len() > 4
            || self.links.iter().any(|link| {
                !text(&link.label, 100)
                    || !text(&link.path, 1000)
                    || !link.path.starts_with('/')
                    || link.path.starts_with("//")
                    || link
                        .path
                        .chars()
                        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
            })
        {
            return Err(Error::Internal);
        }
        Ok(())
    }
}

fn shape(entry: &Entry) -> bilibi_module_registry::Result<()> {
    entry.get::<ImportResults>().map(|_| ())
}
inventory::submit! { Consumer::new::<ImportResults>(IMPORT_RESULTS, "base/bulk_data", |entries| entries.iter().try_for_each(shape)) }

pub(crate) fn invalid(entry: &Entry) -> RegistryError {
    RegistryError::Contribution(format!(
        "import results {} at {}:{} must name a record type of the same module",
        entry.id, entry.provenance.source, entry.provenance.line
    ))
}
