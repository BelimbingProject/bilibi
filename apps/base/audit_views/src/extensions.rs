//! Audit kinds that extend another module's record in its Record History: a
//! module that keeps its own row beside a record (a CRM profile beside a Core
//! company, keyed by the same id) declares one, and the record's History then
//! lists that row's changes too, with no change to the record's page. Typed
//! contributions, validated at boot, discovered with no central list; a module
//! that is not mounted declares none.
use bilibi_module_registry::{Catalog, Consumer, Entry, RegistryError, Result};
use std::collections::{BTreeMap, BTreeSet};

pub const HISTORY_KINDS: &str = "base/audit_views.history_kinds";
/// The most kinds other modules add to one record kind: the embed takes 8,
/// and a page passes its own kind and perhaps a legacy one.
pub const HISTORY_KINDS_PER_RECORD: usize = 6;

/// One audit kind written under the same id as `record`'s rows.
#[derive(Debug)]
pub struct HistoryKind {
    /// The record's own audit kind, as its page passes it to the History
    /// embed (`company`, `person`).
    pub record: &'static str,
    /// The extending module's audit kind for its row beside that record,
    /// keyed by the record's id (`cx.profiles.company`).
    pub kind: &'static str,
}

fn shaped(kind: &str) -> bool {
    !kind.trim().is_empty() && kind.len() <= 255 && !kind.contains(',')
}

fn validate(entries: &[Entry]) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut per_record: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in entries {
        let declared = entry.get::<HistoryKind>()?;
        let at = format!(
            "history kind {} at {}:{}",
            entry.id, entry.provenance.source, entry.provenance.line
        );
        if !shaped(declared.record) || !shaped(declared.kind) || declared.record == declared.kind {
            return Err(RegistryError::Contribution(format!(
                "{at}: record and kind must each be an audit kind of at most 255 characters without ',', and differ"
            )));
        }
        if !seen.insert((declared.record, declared.kind)) {
            return Err(RegistryError::Contribution(format!(
                "{at}: declared twice for record {}",
                declared.record
            )));
        }
        let count = per_record.entry(declared.record).or_default();
        *count += 1;
        if *count > HISTORY_KINDS_PER_RECORD {
            return Err(RegistryError::Contribution(format!(
                "{at}: record {} has more than {HISTORY_KINDS_PER_RECORD} kinds added; its History would not open",
                declared.record
            )));
        }
    }
    Ok(())
}
inventory::submit! { Consumer::new::<HistoryKind>(HISTORY_KINDS, "base/audit_views", validate) }

/// The linked modules' kinds by the record kind they extend.
pub(crate) fn by_record() -> Result<BTreeMap<&'static str, Vec<&'static str>>> {
    let catalog = Catalog::discover(&bilibi_module_registry::discover()?)?;
    let mut kinds: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for entry in catalog.entries::<HistoryKind>(HISTORY_KINDS)? {
        let declared = entry.get::<HistoryKind>()?;
        kinds
            .entry(declared.record)
            .or_default()
            .push(declared.kind);
    }
    Ok(kinds)
}
