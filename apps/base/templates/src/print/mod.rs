//! The owner contributes the schema and samples. A layout only arranges those facts.
mod facts;
mod layout;
mod render;

pub use facts::{PrintFacts, PrintField, PrintKind, PrintValue};
pub use layout::CompiledPrint;

use crate::{catalogue, Audience, Origin, Problem};
use bilibi_module_registry::{Consumer, Entry, RegistryError};
use std::collections::BTreeSet;

pub const PRINT_CATALOGUE: &str = "base/templates.print_catalogue";
const MAX_FIELDS: usize = 128;
const MAX_LAYOUT_BYTES: usize = 48 * 1024;

type Checked<T> = Result<T, Problem>;

pub struct PrintContract {
    pub key: &'static str,
    pub label: &'static str,
    pub audience: Audience,
    pub version: &'static str,
    pub states: &'static [&'static str],
    pub fields: &'static [PrintField],
    pub base_language: &'static str,
    pub defaults: &'static [PrintDefault],
}

pub struct PrintDefault {
    pub language: &'static str,
    pub version: &'static str,
    pub layout: &'static str,
}

#[derive(Clone, Debug)]
pub struct ResolvedPrint<'a> {
    pub language: String,
    pub origin: Origin,
    pub layout: &'a str,
}

impl PrintContract {
    /// Try the requested language, its parent, the business fallback and the packaged base.
    pub fn packaged(
        &self,
        requested: &str,
        fallback: Option<&str>,
    ) -> Checked<ResolvedPrint<'static>> {
        catalogue::languages(requested, fallback, self.base_language)?
            .into_iter()
            .find_map(|language| {
                let default = self
                    .defaults
                    .iter()
                    .find(|item| item.language == language)?;
                Some(ResolvedPrint {
                    language,
                    origin: Origin::Packaged {
                        version: default.version.to_owned(),
                    },
                    layout: default.layout,
                })
            })
            .ok_or_else(|| Problem::new(self.key, "The print contract has no packaged layout."))
    }

    pub fn compile(&self, source: &str) -> Checked<CompiledPrint<'_>> {
        layout::compile(self, source)
    }

    fn validate(&self, id: &str, owner: &str) -> Checked<()> {
        if self.key != id
            || !id
                .strip_prefix(owner)
                .and_then(|suffix| suffix.strip_prefix('.'))
                .is_some_and(catalogue::identifier)
            || self.label.trim().is_empty()
            || self.version.trim().is_empty()
            || self.states.is_empty()
        {
            return Err(Problem::new(
                id,
                "Declare an owner-prefixed key, label, version and states.",
            ));
        }
        let mut states = BTreeSet::new();
        if self
            .states
            .iter()
            .any(|state| !catalogue::identifier(state) || !states.insert(state))
        {
            return Err(Problem::new(id, "Print states need unique identifiers."));
        }
        facts::validate_fields(self.fields, true)?;
        let mut languages = BTreeSet::new();
        for default in self.defaults {
            if catalogue::language(default.language).as_deref() != Some(default.language)
                || !languages.insert(default.language)
                || default.version.trim().is_empty()
            {
                return Err(Problem::new(
                    id,
                    "Packaged layouts need unique canonical languages and versions.",
                ));
            }
            self.compile(default.layout)?.preview()?;
        }
        if !languages.contains(self.base_language) {
            return Err(Problem::new(id, "Package a layout for the base language."));
        }
        Ok(())
    }
}

fn validate(entries: &[Entry]) -> bilibi_module_registry::Result<()> {
    for entry in entries {
        let contract = entry.get::<PrintContract>()?;
        contract
            .validate(entry.id, &entry.provenance.module.replace('/', "."))
            .map_err(|problem| {
                RegistryError::Contribution(format!(
                    "print contract {} at {}:{}: {problem}",
                    entry.id, entry.provenance.source, entry.provenance.line
                ))
            })?;
    }
    Ok(())
}

inventory::submit! { Consumer::new::<PrintContract>(PRINT_CATALOGUE, "base/templates", validate) }
