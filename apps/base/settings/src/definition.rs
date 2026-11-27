//! Immutable owner contributions. Exact keys win over longest wildcard patterns.
use bilibi_module_registry::{Catalog, Consumer, Entry, RegistryError};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const DEFINITIONS: &str = "base/settings.definitions";
/// Settings their owning modules explicitly let tenant administrators manage
/// through the tenant-setting operation contracts. The contribution id is the
/// exact setting key; wildcard definitions cannot be delegated.
pub const TENANT_ADMIN: &str = "base/settings.tenant-admin";
/// The catalog of rules over several settings:
/// `Contribution::new(CONSTRAINTS, "<module>.<name>", provenance, &Constraint { .. })`.
pub const CONSTRAINTS: &str = "base/settings.constraints";
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ScopeKind {
    Global,
    Tenant,
    Company,
    User,
}
impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Tenant => "tenant",
            Self::Company => "company",
            Self::User => "user",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueType {
    Array,
    Boolean,
    Float,
    Integer,
    Mixed,
    String,
}
pub struct Validator {
    pub accepts: fn(&Value) -> bool,
    pub message: &'static str,
}
pub struct Editor {
    pub group: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub capability: Option<&'static str>,
    /// How long a revealed stored secret stays shown, 1,000 to 60,000 ms; or
    /// [`NEVER_REVEALED`] for an encrypted value no one may see again.
    pub reveal_duration_ms: u32,
}
/// Marker payload for [`TENANT_ADMIN`]. The definition remains the source of
/// the label, type, validation, capability and secret policy.
pub struct TenantAdmin;
/// `reveal_duration_ms` for an encrypted setting that is never shown back,
/// not even through the stored-secret reveal: a credential whose holder
/// should re-enter it rather than read it (such as an API key sent to an
/// operator-chosen server).
pub const NEVER_REVEALED: u32 = 0;
impl Editor {
    /// Whether a stored value may be revealed at all.
    pub fn revealable(&self) -> bool {
        self.reveal_duration_ms != NEVER_REVEALED
    }
}
pub struct Definition {
    pub value_type: ValueType,
    pub scopes: &'static [ScopeKind],
    pub default: fn() -> Value,
    pub nullable: bool,
    pub encrypted: bool,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub validator: Option<Validator>,
    pub editor: Option<Editor>,
}
impl Definition {
    /// Give named system principals a capability dedicated to decrypting this
    /// setting. Human editing and reveal paths continue to use only the
    /// editor capability.
    pub const fn principal_read(self, capability: &'static str) -> Setting {
        Setting::PrincipalReadable {
            definition: self,
            capability,
        }
    }

    pub fn validate(&self, value: &Value) -> std::result::Result<(), &'static str> {
        let typed = if value.is_null() {
            self.nullable
        } else {
            match self.value_type {
                ValueType::Array => value.is_array() || value.is_object(),
                ValueType::Boolean => value.is_boolean(),
                ValueType::Float => value.is_f64(),
                ValueType::Integer => value.is_i64() || value.is_u64(),
                ValueType::Mixed => true,
                ValueType::String => value.is_string(),
            }
        };
        if !typed {
            return Err("Value has the wrong type");
        }
        if let Some(number) = value.as_f64() {
            if self.minimum.is_some_and(|min| number < min)
                || self.maximum.is_some_and(|max| number > max)
            {
                return Err("Value is outside the allowed range");
            }
        }
        if let Some(validator) = &self.validator {
            if !(validator.accepts)(value) {
                return Err(validator.message);
            }
        }
        Ok(())
    }
}
/// A rule over 2–8 defined settings, as one scope resolves them (its own
/// values, else inherited, else defaults). Writing any of them at that scope
/// locks all of them, then refuses the write unless `accepts` holds for the
/// values in `keys` order: for example a period never below its minimum.
pub struct Constraint {
    pub keys: &'static [&'static str],
    pub accepts: fn(&[Value]) -> bool,
    pub message: &'static str,
}
/// Runtime claims have no default, reject null writes and store JSON without
/// encryption. Use a Definition for secrets or user-editable configuration.
pub enum Setting {
    Definition(Definition),
    /// Construct through [`Definition::principal_read`].
    PrincipalReadable {
        definition: Definition,
        capability: &'static str,
    },
    RuntimeClaim,
}
impl Setting {
    /// The declared definition, when this is not a runtime claim.
    pub fn definition(&self) -> Option<&Definition> {
        match self {
            Self::Definition(definition) | Self::PrincipalReadable { definition, .. } => {
                Some(definition)
            }
            Self::RuntimeClaim => None,
        }
    }

    /// Capability a named system principal needs to decrypt the value.
    /// Definitions without an explicit principal capability retain the editor
    /// capability rule for compatibility.
    pub fn principal_read_capability(&self) -> Option<&'static str> {
        match self {
            Self::PrincipalReadable { capability, .. } => Some(capability),
            Self::Definition(definition) => definition
                .editor
                .as_ref()
                .and_then(|editor| editor.capability),
            Self::RuntimeClaim => None,
        }
    }
}
#[derive(Clone)]
pub struct OwnedSetting {
    pub pattern: &'static str,
    pub owner: &'static str,
    pub setting: &'static Setting,
}
#[derive(Clone, Default)]
pub struct Definitions {
    entries: BTreeMap<&'static str, OwnedSetting>,
    constraints: Vec<&'static Constraint>,
    tenant_admin: BTreeSet<&'static str>,
}
impl Definitions {
    pub fn from_catalog(catalog: &Catalog) -> bilibi_module_registry::Result<Self> {
        let mut definitions = Self::from_entries(catalog.entries::<Setting>(DEFINITIONS)?)?;
        definitions.constraints =
            definitions.constraints_from(catalog.entries::<Constraint>(CONSTRAINTS)?)?;
        definitions.tenant_admin =
            definitions.tenant_admin_from(catalog.entries::<TenantAdmin>(TENANT_ADMIN)?)?;
        Ok(definitions)
    }
    fn tenant_admin_from(
        &self,
        entries: &[Entry],
    ) -> bilibi_module_registry::Result<BTreeSet<&'static str>> {
        let mut delegated = BTreeSet::new();
        for entry in entries {
            let Some(owned) = self.entries.get(entry.id) else {
                return Err(RegistryError::Contribution(format!(
                    "tenant setting delegation {} at {}:{} names no exact setting",
                    entry.id, entry.provenance.source, entry.provenance.line
                )));
            };
            let valid = owned.owner == entry.provenance.module
                && owned
                    .setting
                    .definition()
                    .is_some_and(|d| d.scopes.contains(&ScopeKind::Tenant) && d.editor.is_some());
            if !valid || !delegated.insert(entry.id) {
                return Err(RegistryError::Contribution(format!(
                    "tenant setting delegation {} at {}:{} must be unique, owned by the contributing module, tenant-scoped and editable",
                    entry.id, entry.provenance.source, entry.provenance.line
                )));
            }
        }
        Ok(delegated)
    }
    fn constraints_from(
        &self,
        entries: &[Entry],
    ) -> bilibi_module_registry::Result<Vec<&'static Constraint>> {
        let mut constraints = Vec::with_capacity(entries.len());
        for entry in entries {
            let constraint = entry.get::<Constraint>()?;
            let unique: BTreeSet<_> = constraint.keys.iter().collect();
            // Each key is defined at one scope, the same for all: no value of
            // the rule is inherited from another scope, so a write elsewhere
            // cannot break it, and every writer of its keys takes one lock set.
            let scopes: Option<BTreeSet<&str>> = constraint
                .keys
                .iter()
                .map(|key| {
                    match self
                        .entries
                        .get(key)
                        .and_then(|owned| owned.setting.definition())
                    {
                        Some(d) if !d.encrypted && d.scopes.len() == 1 => {
                            Some(d.scopes[0].as_str())
                        }
                        _ => None,
                    }
                })
                .collect();
            if !(2..=8).contains(&constraint.keys.len())
                || unique.len() != constraint.keys.len()
                || scopes.is_none_or(|scopes| scopes.len() != 1)
                || constraint.message.is_empty()
            {
                return Err(RegistryError::Contribution(format!(
                    "settings constraint {} declared by {} at {}:{} must name 2-8 distinct, unencrypted, defined settings of one and the same single scope, and a message",
                    entry.id, entry.provenance.module, entry.provenance.source, entry.provenance.line
                )));
            }
            constraints.push(constraint);
        }
        Ok(constraints)
    }
    /// The rules that name `key`.
    pub fn constraints_for<'a>(
        &'a self,
        key: &'a str,
    ) -> impl Iterator<Item = &'static Constraint> + 'a {
        self.constraints
            .iter()
            .copied()
            .filter(move |constraint| constraint.keys.contains(&key))
    }
    fn from_entries(entries: &[Entry]) -> bilibi_module_registry::Result<Self> {
        let mut result = Self::default();
        for entry in entries {
            let fail = |reason: &str| {
                RegistryError::Contribution(format!(
                    "setting {} declared by {} at {}:{}: {reason}",
                    entry.id,
                    entry.provenance.module,
                    entry.provenance.source,
                    entry.provenance.line
                ))
            };
            if !valid_key(entry.id) || result.entries.contains_key(entry.id) {
                return Err(fail("invalid or duplicate key"));
            }
            let setting = entry.get::<Setting>()?;
            if let Some(d) = setting.definition() {
                let scopes: BTreeSet<_> = d.scopes.iter().collect();
                if scopes.is_empty() || scopes.len() != d.scopes.len() {
                    return Err(fail("scopes must be nonempty and unique"));
                }
                if (d.minimum.is_some() || d.maximum.is_some())
                    && !matches!(d.value_type, ValueType::Integer | ValueType::Float)
                    || d.minimum.is_some_and(|v| !v.is_finite())
                    || d.maximum.is_some_and(|v| !v.is_finite())
                    || matches!((d.minimum,d.maximum),(Some(a),Some(b)) if a>b)
                {
                    return Err(fail("invalid numeric bounds"));
                }
                if let Some(editor) = &d.editor {
                    if editor.group.is_empty()
                        || editor.label.is_empty()
                        || editor.help.is_empty()
                        || !((1_000..=60_000).contains(&editor.reveal_duration_ms)
                            || d.encrypted && editor.reveal_duration_ms == NEVER_REVEALED)
                        || editor.capability.is_some_and(str::is_empty)
                    {
                        return Err(fail("invalid editor metadata"));
                    }
                }
                if d.validator.as_ref().is_some_and(|v| v.message.is_empty()) {
                    return Err(fail("validator must have a message"));
                }
                if matches!(setting, Setting::PrincipalReadable { capability, .. }
                    if !d.encrypted || capability.is_empty())
                {
                    return Err(fail(
                        "principal read capability must be nonempty and belongs only on an encrypted definition",
                    ));
                }
                d.validate(&(d.default)()).map_err(fail)?;
            }
            result.entries.insert(
                entry.id,
                OwnedSetting {
                    pattern: entry.id,
                    owner: entry.provenance.module,
                    setting,
                },
            );
        }
        Ok(result)
    }
    pub fn all(&self) -> impl Iterator<Item = &OwnedSetting> {
        self.entries.values()
    }
    /// Exact settings their owners delegated to tenant administrators.
    pub fn tenant_admin(&self) -> impl Iterator<Item = &OwnedSetting> {
        self.tenant_admin
            .iter()
            .filter_map(|key| self.entries.get(key))
    }
    pub fn is_tenant_admin(&self, key: &str) -> bool {
        self.tenant_admin.contains(key)
    }
    pub fn lookup(&self, key: &str) -> bilibi_database::Result<&OwnedSetting> {
        if !valid_key(key) || key.contains('*') {
            return Err(bilibi_database::Error::Invalid);
        }
        self.entries
            .get(key)
            .or_else(|| {
                self.entries
                    .values()
                    .filter(|entry| entry.pattern.contains('*') && matches(entry.pattern, key))
                    .max_by_key(|entry| (entry.pattern.len(), entry.pattern))
            })
            .ok_or(bilibi_database::Error::Invalid)
    }
}
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.chars().count() <= 255 && !key.chars().any(char::is_control)
}
fn matches(pattern: &str, key: &str) -> bool {
    let parts: Vec<_> = pattern.split('*').collect();
    let Some(mut rest) = key.strip_prefix(parts[0]) else {
        return false;
    };
    for (index, part) in parts.iter().enumerate().skip(1) {
        if index == parts.len() - 1 {
            return rest.ends_with(part);
        }
        let Some(position) = rest.find(part) else {
            return false;
        };
        rest = &rest[position + part.len()..];
    }
    rest.is_empty()
}
inventory::submit! {
    Consumer::new::<Setting>(DEFINITIONS, "base/settings", |entries| Definitions::from_entries(entries).map(|_| ()))
}
inventory::submit! {
    Consumer::new::<TenantAdmin>(TENANT_ADMIN, "base/settings", |entries| {
        let mut keys = BTreeSet::new();
        if entries.iter().all(|entry| valid_key(entry.id) && !entry.id.contains('*') && keys.insert(entry.id)) {
            Ok(())
        } else {
            Err(RegistryError::Contribution("tenant setting delegations must have unique exact setting keys".into()))
        }
    })
}
// Checked against the definitions when the store is built.
inventory::submit! {
    Consumer::new::<Constraint>(CONSTRAINTS, "base/settings", |_| Ok(()))
}
