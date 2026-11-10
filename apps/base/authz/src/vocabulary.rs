//! Immutable contributed vocabulary. Grants never create capabilities implicitly.
use bilibi_module_registry::{Consumer, Contribution, Entry, Provenance, RegistryError, Result};
use std::collections::{BTreeMap, BTreeSet};

pub const VOCABULARY: &str = "base/authz.vocabulary";
/// The catalog a module grants its own system principal standing
/// capabilities in: `Contribution::new(STANDING_GRANTS, "<principal>",
/// provenance, &StandingGrant { principal, capabilities })`.
pub const STANDING_GRANTS: &str = "base/authz.standing_grants";
/// The catalog a module names, for its own system principal, capabilities of
/// a module it uses only while that module is mounted (`OptionalCapabilities`).
pub const OPTIONAL_CAPABILITIES: &str = "base/authz.optional_capabilities";
/// A role owner can require a named company engagement on assignments.
pub const ENGAGEMENT_ROLES: &str = "base/authz.engagement_roles";
#[derive(Clone, Copy, Debug)]
pub struct EngagementRole {
    pub role: &'static str,
}

/// Who may ever hold a capability (ADR 16). `decide` denies a capability
/// outside the actor's audience before any grant, grant-all included.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Audience {
    /// Staff of the tenant.
    Tenant,
    /// Staff of the operator tenant only (the `platform_operator` identity).
    Platform,
    /// Customer accounts on the tenant's portal; never staff.
    Portal,
}
#[derive(Clone, Copy, Debug)]
pub struct CapabilityDefinition {
    pub key: &'static str,
    pub audience: Audience,
    /// The owner proves an explicit company for every record access.
    pub company_scoped: bool,
}
impl CapabilityDefinition {
    pub const fn tenant(key: &'static str) -> Self {
        Self {
            key,
            audience: Audience::Tenant,
            company_scoped: false,
        }
    }
    pub const fn platform(key: &'static str) -> Self {
        Self {
            key,
            audience: Audience::Platform,
            company_scoped: false,
        }
    }
    /// A capability only a customer account may hold, on portal routes,
    /// downloads and embeds (`Access::Portal`).
    pub const fn portal(key: &'static str) -> Self {
        Self {
            key,
            audience: Audience::Portal,
            company_scoped: false,
        }
    }
    pub fn is_platform(&self) -> bool {
        self.audience == Audience::Platform
    }
    /// Opt in only after every owner path proves its target company through
    /// `authorize_company_proof` (Core Company's `authorize_work_target`).
    /// Tenant-wide assignments and direct overrides retain their normal reach.
    pub const fn company(key: &'static str) -> Self {
        Self {
            key,
            audience: Audience::Tenant,
            company_scoped: true,
        }
    }
    /// Whether an account of `kind` may ever hold this capability: a
    /// customer only portal ones, staff never a portal one.
    pub fn admits(&self, kind: bilibi_tenancy::AccountKind) -> bool {
        (self.audience == Audience::Portal) == (kind == bilibi_tenancy::AccountKind::Customer)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct RoleDefinition {
    pub code: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub grant_all: bool,
    pub capabilities: &'static [&'static str],
}
#[derive(Clone, Copy, Debug)]
pub struct SystemPrincipalDefinition {
    pub name: &'static str,
    /// This declaration bounds possible grants; it grants nothing by itself.
    pub capabilities: &'static [&'static str],
}
/// Capabilities a principal holds in every live tenant with no stored grant,
/// for Platform work no tenant should have to switch on (Media's malware
/// scans). A tenant's denying grant (`Grants::set_system(.., Some(false))`)
/// still wins. Boot refuses a grant from a module that does not declare the
/// principal, for a capability outside its declaration or a platform one,
/// and a second grant for one principal.
#[derive(Clone, Copy, Debug)]
pub struct StandingGrant {
    pub principal: &'static str,
    pub capabilities: &'static [&'static str],
}
/// Capabilities of `module`, a module the declaring module calls only while
/// it is mounted (a connector behind a port), added to the bounds of the
/// declaring module's own `principal` while `module` is installed. With
/// `module` absent the entry is ignored and grants nothing; with it present,
/// boot refuses a key `module` does not contribute, a portal key, an entry from
/// a module that does not declare the principal, a `module` that is the
/// declaring module or one of its dependencies (declare those keys in
/// `SystemPrincipalDefinition`), and a second entry for one principal and
/// module. The keys never enter a role. `standing` also holds them in every
/// live tenant with no stored grant, as a `StandingGrant` does (tenant
/// capabilities only); otherwise a tenant administrator grants them.
#[derive(Clone, Copy, Debug)]
pub struct OptionalCapabilities {
    pub principal: &'static str,
    pub module: &'static str,
    pub capabilities: &'static [&'static str],
    pub standing: bool,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct VocabularyContribution {
    pub domains: &'static [&'static str],
    pub verbs: &'static [&'static str],
    pub capabilities: &'static [CapabilityDefinition],
    pub roles: &'static [RoleDefinition],
    pub system_principals: &'static [SystemPrincipalDefinition],
}
#[derive(Clone, Debug)]
pub struct OwnedCapability {
    pub definition: CapabilityDefinition,
    pub provenance: Provenance,
}
#[derive(Clone, Debug)]
pub struct SystemRole {
    pub definition: RoleDefinition,
    /// Multiple modules may extend a named seed role explicitly.
    pub capabilities: BTreeSet<&'static str>,
    /// Each contribution that defined or extended the role, for boot errors.
    pub sources: Vec<Provenance>,
}
#[derive(Default, Debug)]
pub struct Vocabulary {
    pub domains: BTreeSet<&'static str>,
    pub verbs: BTreeSet<&'static str>,
    pub capabilities: BTreeMap<&'static str, OwnedCapability>,
    pub roles: BTreeMap<&'static str, SystemRole>,
    pub system_principals: BTreeMap<&'static str, SystemPrincipalDefinition>,
    /// Each principal's standing capabilities (`StandingGrant`).
    pub engagement_roles: BTreeSet<&'static str>,
    pub standing: BTreeMap<&'static str, BTreeSet<&'static str>>,
    /// The module that declared each principal.
    principal_modules: BTreeMap<&'static str, &'static str>,
}
pub fn segment(key: &str) -> bool {
    let mut chars = key.bytes();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
pub fn valid_key(key: &str) -> bool {
    key.len() <= 200 && key.split('.').count() >= 2 && key.split('.').all(segment)
}
fn invalid(reason: impl Into<String>) -> RegistryError {
    RegistryError::Contribution(reason.into())
}
impl Vocabulary {
    pub fn from_entries(entries: &[Entry]) -> Result<Self> {
        let mut result = Self::default();
        for entry in entries {
            let v = entry.get::<VocabularyContribution>()?;
            for (values, target) in [
                (v.domains, &mut result.domains),
                (v.verbs, &mut result.verbs),
            ] {
                for value in values {
                    if !segment(value) {
                        return Err(invalid(format!("invalid authz vocabulary {value}")));
                    }
                    target.insert(*value);
                }
            }
            for cap in v.capabilities {
                if !valid_key(cap.key)
                    || result
                        .capabilities
                        .insert(
                            cap.key,
                            OwnedCapability {
                                definition: *cap,
                                provenance: entry.provenance,
                            },
                        )
                        .is_some()
                {
                    return Err(invalid(format!("invalid/duplicate capability {}", cap.key)));
                }
            }
            for role in v.roles {
                if !segment(&role.code.replace('_', "-"))
                    || role.name.is_empty()
                    || role.name.len() > 200
                {
                    return Err(invalid(format!("invalid system role {}", role.code)));
                }
                let existing = result.roles.entry(role.code).or_insert_with(|| SystemRole {
                    definition: *role,
                    capabilities: BTreeSet::new(),
                    sources: Vec::new(),
                });
                existing.sources.push(entry.provenance);
                if existing.definition.name != role.name
                    || existing.definition.grant_all != role.grant_all
                {
                    return Err(invalid(format!("conflicting system role {}", role.code)));
                }
                // Other modules extend built-in roles with capabilities. Their
                // catalog order must not replace Authz's canonical metadata.
                if entry.provenance.module == "base/authz" && entry.id == "authz.builtin" {
                    existing.definition = *role;
                }
                existing
                    .capabilities
                    .extend(role.capabilities.iter().copied());
            }
            for principal in v.system_principals {
                if !segment(principal.name)
                    || principal.name.len() > 100
                    || result
                        .system_principals
                        .insert(principal.name, *principal)
                        .is_some()
                {
                    return Err(invalid(format!(
                        "invalid/duplicate system principal {}",
                        principal.name
                    )));
                }
                result
                    .principal_modules
                    .insert(principal.name, entry.provenance.module);
            }
        }
        for cap in result.capabilities.values() {
            let mut parts = cap.definition.key.split('.');
            if !result.domains.contains(parts.next().unwrap())
                || !result.verbs.contains(parts.next_back().unwrap())
            {
                return Err(invalid(format!(
                    "undeclared domain/verb in {}",
                    cap.definition.key
                )));
            }
        }
        for key in result
            .roles
            .values()
            .flat_map(|r| r.capabilities.iter())
            .chain(
                result
                    .system_principals
                    .values()
                    .flat_map(|p| p.capabilities.iter()),
            )
        {
            if !result.capabilities.contains_key(key) {
                return Err(invalid(format!("undeclared grant {key}")));
            }
        }
        result.check_audiences()?;
        Ok(result)
    }
    /// A role serves one audience (ADR 16): its keys are all portal or none
    /// are, and a portal role is never grant-all, so no assignment can hand a
    /// customer a staff capability or staff a portal one. A system principal
    /// is the tenant's own work and declares no portal capability.
    fn check_audiences(&self) -> Result<()> {
        let portal = |key: &str| self.capabilities[key].definition.audience == Audience::Portal;
        for role in self.roles.values() {
            let portal_keys = role.capabilities.iter().filter(|key| portal(key)).count();
            let mixed = portal_keys > 0 && portal_keys < role.capabilities.len();
            let grant_all = portal_keys > 0 && role.definition.grant_all;
            if mixed || grant_all {
                let at = role
                    .sources
                    .iter()
                    .map(|source| format!("{}:{}", source.source, source.line))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(invalid(format!(
                    "system role {} {} (contributed at {at})",
                    role.definition.code,
                    if mixed {
                        "mixes portal and staff audiences"
                    } else {
                        "is grant-all with portal capabilities"
                    }
                )));
            }
        }
        for principal in self.system_principals.values() {
            if principal.capabilities.iter().any(|key| portal(key)) {
                return Err(invalid(format!(
                    "system principal {} declares a portal capability",
                    principal.name
                )));
            }
        }
        Ok(())
    }
    pub fn from_catalog(catalog: &bilibi_module_registry::Catalog) -> Result<Self> {
        let mut vocabulary =
            Self::from_entries(catalog.entries::<VocabularyContribution>(VOCABULARY)?)?;
        vocabulary.add_standing(catalog.entries::<StandingGrant>(STANDING_GRANTS)?)?;
        vocabulary.add_optional(
            catalog,
            catalog.entries::<OptionalCapabilities>(OPTIONAL_CAPABILITIES)?,
        )?;
        for entry in catalog.entries::<EngagementRole>(ENGAGEMENT_ROLES)? {
            let rule = entry.get::<EngagementRole>()?;
            let role = vocabulary
                .roles
                .get(rule.role)
                .ok_or_else(|| invalid("engagement rule names an unknown role"))?;
            if role.definition.grant_all
                || !role
                    .sources
                    .iter()
                    .any(|source| source.module == entry.provenance.module)
                || role
                    .capabilities
                    .iter()
                    .any(|key| !vocabulary.capabilities[key].definition.company_scoped)
                || !vocabulary.engagement_roles.insert(rule.role)
            {
                return Err(invalid("invalid or duplicate engagement role rule"));
            }
        }
        Ok(vocabulary)
    }
    fn add_standing(&mut self, entries: &[Entry]) -> Result<()> {
        for entry in entries {
            let grant = entry.get::<StandingGrant>()?;
            let declared = self.system_principals.get(grant.principal);
            let valid = entry.id == grant.principal
                && self.principal_modules.get(grant.principal) == Some(&entry.provenance.module)
                && !grant.capabilities.is_empty()
                && declared.is_some_and(|declared| {
                    grant.capabilities.iter().all(|key| {
                        declared.capabilities.contains(key)
                            && self.capabilities.get(key).is_some_and(|capability| {
                                capability.definition.audience == Audience::Tenant
                            })
                    })
                });
            if !valid
                || self
                    .standing
                    .insert(
                        grant.principal,
                        grant.capabilities.iter().copied().collect(),
                    )
                    .is_some()
            {
                return Err(invalid(format!(
                    "invalid standing grant {} at {}:{}",
                    entry.id, entry.provenance.source, entry.provenance.line
                )));
            }
        }
        Ok(())
    }
    fn add_optional(
        &mut self,
        catalog: &bilibi_module_registry::Catalog,
        entries: &[Entry],
    ) -> Result<()> {
        let mut seen = BTreeSet::new();
        for entry in entries {
            let grant = entry.get::<OptionalCapabilities>()?;
            let refuse = |why: &str| {
                invalid(format!(
                    "invalid optional capabilities {} at {}:{}: {why}",
                    entry.id, entry.provenance.source, entry.provenance.line
                ))
            };
            let owner = entry.provenance.module;
            if self.principal_modules.get(grant.principal) != Some(&owner) {
                return Err(refuse("the principal is not declared by this module"));
            }
            if grant.module == owner
                || catalog
                    .dependencies(owner)
                    .is_some_and(|dependencies| dependencies.contains(&grant.module))
            {
                return Err(refuse(
                    "the module is a dependency; declare its keys on the principal",
                ));
            }
            if !seen.insert((grant.principal, grant.module)) {
                return Err(refuse("a second entry for this principal and module"));
            }
            let mut keys = BTreeSet::new();
            if grant.capabilities.is_empty()
                || !grant
                    .capabilities
                    .iter()
                    .all(|key| valid_key(key) && keys.insert(*key))
            {
                return Err(refuse("empty, invalid or repeated keys"));
            }
            if !catalog.installed(grant.module) {
                continue;
            }
            let declared = self.system_principals[grant.principal];
            for key in grant.capabilities {
                let Some(capability) = self.capabilities.get(key) else {
                    return Err(refuse(&format!("{key} is not declared")));
                };
                if capability.provenance.module != grant.module
                    || capability.definition.audience == Audience::Portal
                    || (grant.standing && capability.definition.audience != Audience::Tenant)
                    || declared.capabilities.contains(key)
                {
                    return Err(refuse(&format!(
                        "{key} is not a {} capability of {} the principal lacks",
                        if grant.standing { "tenant" } else { "staff" },
                        grant.module
                    )));
                }
            }
            // The snapshot is built once per process (`vocabulary()`) and
            // lives as long as the contributions it joins, so the merged
            // bounds are `'static` like every other declaration: each
            // reader sees one list, whichever modules are mounted.
            let merged: Vec<&'static str> = declared
                .capabilities
                .iter()
                .chain(grant.capabilities)
                .copied()
                .collect();
            self.system_principals
                .get_mut(grant.principal)
                .expect("declared principal")
                .capabilities = Vec::leak(merged);
            if grant.standing {
                self.standing
                    .entry(grant.principal)
                    .or_default()
                    .extend(grant.capabilities.iter().copied());
            }
        }
        Ok(())
    }
    /// The module that declared `principal`, so a contribution naming a
    /// principal can be held to its own module's.
    pub fn principal_module(&self, principal: &str) -> Option<&'static str> {
        self.principal_modules.get(principal).copied()
    }
    /// Whether `principal` holds `key` in every tenant that has not denied it.
    pub fn standing(&self, principal: &str, key: &str) -> bool {
        self.standing
            .get(principal)
            .is_some_and(|keys| keys.contains(key))
    }
    pub fn from_inventory() -> Result<Self> {
        let catalog =
            bilibi_module_registry::Catalog::discover(&bilibi_module_registry::discover()?)?;
        let vocabulary = Self::from_catalog(&catalog)?;
        vocabulary.validate_references(&catalog)?;
        Ok(vocabulary)
    }
    /// Validate capability references without importing their owners. A typo in
    /// navigation/route/settings metadata must fail at boot, not hide a page.
    pub fn validate_references(&self, catalog: &bilibi_module_registry::Catalog) -> Result<()> {
        use bilibi_module_registry::contributions::{
            Access, Download, Embed, Route, DOWNLOADS, EMBEDS, ROUTES,
        };
        let check = |key: &str| {
            if self.capabilities.contains_key(key) {
                Ok(())
            } else {
                Err(invalid(format!("undeclared capability reference {key}")))
            }
        };
        // A portal access names a portal capability and a staff access never
        // does (ADR 16), so a typo cannot open a portal page to staff or a
        // staff page to customers.
        let audience = |entry: &Entry, key: &str, portal: bool| {
            check(key)?;
            if (self.capabilities[key].definition.audience == Audience::Portal) == portal {
                Ok(())
            } else {
                Err(invalid(format!(
                    "{} at {}:{} names {key}, which is {} capability",
                    entry.id,
                    entry.provenance.source,
                    entry.provenance.line,
                    if portal { "not a portal" } else { "a portal" }
                )))
            }
        };
        let access = |entry: &Entry, value: Access| match value.base() {
            Access::Capability(key) | Access::OperatorCapability(key) => {
                audience(entry, key, false)
            }
            Access::AnyOf(keys) => keys.iter().try_for_each(|key| audience(entry, key, false)),
            Access::Portal(key) => audience(entry, key, true),
            _ => Ok(()),
        };
        for entry in catalog.entries::<Route>(ROUTES)? {
            access(entry, entry.get::<Route>()?.access)?;
        }
        for entry in catalog.entries::<Embed>(EMBEDS)? {
            access(entry, entry.get::<Embed>()?.access)?;
        }
        for entry in catalog.entries::<Download>(DOWNLOADS)? {
            access(entry, entry.get::<Download>()?.access)?;
        }
        let menu = bilibi_menu::Menu::from_catalog(catalog)?;
        for entry in catalog.entries::<bilibi_menu::MenuItem>(bilibi_menu::ITEMS)? {
            let item = entry.get::<bilibi_menu::MenuItem>()?;
            let portal = menu.in_portal(item.id);
            for key in item.requirement.keys() {
                audience(entry, key, portal)?;
            }
        }
        for entry in catalog.entries::<bilibi_settings::Setting>(bilibi_settings::DEFINITIONS)? {
            if let bilibi_settings::Setting::Definition(definition) =
                entry.get::<bilibi_settings::Setting>()?
            {
                if let Some(key) = definition
                    .editor
                    .as_ref()
                    .and_then(|editor| editor.capability)
                {
                    check(key)?;
                }
            }
        }
        Ok(())
    }
}
inventory::submit! { Consumer::new::<VocabularyContribution>(VOCABULARY,"base/authz",|entries| Vocabulary::from_entries(entries).map(|_|())) }
// The principal and capability checks need the whole vocabulary; `from_catalog` runs them.
inventory::submit! { Consumer::new::<StandingGrant>(STANDING_GRANTS,"base/authz",|entries| entries.iter().try_for_each(|entry| entry.get::<StandingGrant>().map(|_| ()))) }
inventory::submit! { Consumer::new::<OptionalCapabilities>(OPTIONAL_CAPABILITIES,"base/authz",|entries| entries.iter().try_for_each(|entry| entry.get::<OptionalCapabilities>().map(|_| ()))) }

use CapabilityDefinition as C;
const fn role(
    code: &'static str,
    name: &'static str,
    grant_all: bool,
    capabilities: &'static [&'static str],
) -> RoleDefinition {
    RoleDefinition {
        code,
        name,
        description: "Contributed system role; reconciled explicitly",
        grant_all,
        capabilities,
    }
}
inventory::submit! { Contribution::new(VOCABULARY,"authz.builtin",Provenance {module:"base/authz",source:file!(),line:line!()}, &VocabularyContribution {
    domains:&["admin"],
    verbs:&["view","list","create","update","delete","grant","revoke","manage","impersonate"],
    capabilities:&[
        C::tenant("admin.authz.role.list"), C::tenant("admin.authz.role.view"), C::tenant("admin.authz.role.create"), C::tenant("admin.authz.role.update"), C::tenant("admin.authz.role.delete"),
        C::tenant("admin.authz.capability.list"), C::tenant("admin.authz.decision-log.list"),
        C::tenant("admin.authz.system-principal.list"), C::tenant("admin.authz.system-principal.grant"), C::tenant("admin.authz.system-principal.revoke"),
        C::tenant("admin.authz.field.manage"), C::tenant("admin.authz.step-up.manage"), C::tenant("admin.user.impersonate"),
    ],
    roles:&[
        RoleDefinition {
            description: "System role with all capabilities. New capabilities are automatically granted.",
            ..role("core_admin", "Core Administrator", true, &[])
        },
        RoleDefinition {
            description: "Full control within a single tenant: commerce, AI, messaging, company, employees, and addresses. No platform administration.",
            ..role("tenant_owner", "Tenant Owner", false, &["admin.authz.field.manage", "admin.authz.step-up.manage"])
        },
        RoleDefinition {
            description: "Read-only access to decision logs, system logs, and sessions for compliance.",
            ..role("auditor", "Auditor", false, &["admin.authz.decision-log.list"])
        },
        RoleDefinition {
            description: "Read-only access to system infrastructure: tables, jobs, cache, schedule, and sessions.",
            ..role("system_viewer", "System Viewer", false, &[])
        }],
    system_principals:&[],
}) }

inventory::submit! { Consumer::new::<EngagementRole>(ENGAGEMENT_ROLES,"base/authz",|entries| entries.iter().try_for_each(|entry| entry.get::<EngagementRole>().map(|_| ()))) }
