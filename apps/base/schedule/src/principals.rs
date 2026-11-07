//! Optional owner-declared authority for each-tenant work. The scheduler keeps
//! its directory/fan-out authority; business work runs as the owner's principal.
use crate::{Definition, Reach, EXECUTE, TENANT_WORK};
use bilibi_authz::{SystemPrincipalDefinition, Vocabulary, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{Catalog, Consumer, Entry, RegistryError, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Optional contribution keyed by the schedule definition id, from its owner.
pub const EXECUTION_PRINCIPALS: &str = "base/schedule.execution-principals";

/// Select a principal declared by this definition's module in Authz vocabulary.
/// It must declare EXECUTE and TENANT_WORK as possible grants, plus the exact
/// business capabilities the worker needs. Declaration grants nothing. Use a
/// dedicated principal for work with a different authority bound.
#[derive(Clone, Copy, Debug)]
pub struct ExecutionPrincipal {
    pub principal: &'static str,
}

fn invalid(entry: &Entry, reason: &str) -> RegistryError {
    RegistryError::Contribution(format!(
        "schedule execution principal {} from {} at {}:{}: {reason}",
        entry.id, entry.provenance.module, entry.provenance.source, entry.provenance.line
    ))
}

fn validate(entries: &[Entry]) -> Result<()> {
    for entry in entries {
        let selected = entry.get::<ExecutionPrincipal>()?;
        if !crate::definition::valid_key(entry.id) || selected.principal.is_empty() {
            return Err(invalid(entry, "definition key and principal are required"));
        }
    }
    Ok(())
}
inventory::submit! { Consumer::new::<ExecutionPrincipal>(EXECUTION_PRINCIPALS, "base/schedule", validate) }

pub(crate) fn from_catalog(
    catalog: &Catalog,
) -> Result<BTreeMap<&'static str, SystemPrincipalDefinition>> {
    let vocabulary = Vocabulary::from_catalog(catalog)?;
    let definitions = catalog.entries::<Definition>(crate::DEFINITIONS)?;
    let declarations = catalog.entries::<VocabularyContribution>(VOCABULARY)?;
    let mut result = BTreeMap::new();
    for entry in catalog.entries::<ExecutionPrincipal>(EXECUTION_PRINCIPALS)? {
        let selected = entry.get::<ExecutionPrincipal>()?;
        let definition = definitions
            .iter()
            .find(|definition| definition.id == entry.id)
            .ok_or_else(|| invalid(entry, "definition is not installed"))?;
        if definition.provenance.module != entry.provenance.module
            || definition.get::<Definition>()?.reach != Reach::EachTenant
        {
            return Err(invalid(
                entry,
                "requires an each-tenant definition owned by this module",
            ));
        }
        let worker = definition.get::<Definition>()?.worker;
        let binding = catalog
            .entries::<bilibi_queue::HandlerBinding>(bilibi_queue::HANDLERS)?
            .iter()
            .find(|binding| binding.id == worker)
            .map(|binding| binding.get::<bilibi_queue::HandlerBinding>())
            .transpose()?;
        if binding.is_none_or(|binding| binding.capability != EXECUTE) {
            return Err(invalid(
                entry,
                "worker must bind EXECUTE for operator-tenant consent",
            ));
        }
        let principal = vocabulary
            .system_principals
            .get(selected.principal)
            .ok_or_else(|| invalid(entry, "principal is not declared"))?;
        let owned = declarations.iter().any(|declaration| {
            declaration.provenance.module == entry.provenance.module
                && declaration
                    .get::<VocabularyContribution>()
                    .is_ok_and(|vocabulary| {
                        vocabulary
                            .system_principals
                            .iter()
                            .any(|p| p.name == principal.name)
                    })
        });
        if !owned {
            return Err(invalid(
                entry,
                "principal must be declared by the definition's owner",
            ));
        }
        if !principal.capabilities.contains(&EXECUTE)
            || !principal.capabilities.contains(&TENANT_WORK)
        {
            return Err(invalid(
                entry,
                "principal must declare EXECUTE and TENANT_WORK",
            ));
        }
        // Scheduled owner authority always requires explicit durable grants.
        // Standing grants would install business authority without review.
        if vocabulary.standing.contains_key(principal.name) {
            return Err(invalid(
                entry,
                "scheduled owner authority cannot have standing grants",
            ));
        }
        result.insert(entry.id, *principal);
    }
    Ok(result)
}

/// Bind review to the selected identity and its complete declared bound. The
/// base fingerprint remains unchanged for definitions without this contribution.
pub(crate) fn fingerprint(base: &str, principal: &SystemPrincipalDefinition) -> String {
    let mut hash = Sha256::new();
    let mut field = |value: &str| {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    };
    field("bilibi-schedule-principal-v1");
    field(base);
    field(principal.name);
    let mut capabilities = principal.capabilities.to_vec();
    capabilities.sort_unstable();
    capabilities.dedup();
    for capability in capabilities {
        field(capability);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
