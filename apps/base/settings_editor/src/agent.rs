//! Tenant-setting operation contracts. A module opts one exact setting in
//! through `bilibi_settings::TENANT_ADMIN`; no scope, tenant or actor comes
//! from input. Reads stay masked and writes use Settings' audited mutation.
use crate::{form, TENANT_MANAGE, TENANT_SECRET_CHANGE, TENANT_VIEW};
use bilibi_agent_api::{
    json::Output, Channels, Definitions, Error as AgentError, Guide, Handler, Operation,
    OperationClient, OperationFuture, ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_authz::{authorize_proof, effective_capabilities};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, Settings, Target};
use bilibi_step_up::StepUp;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

struct Core {
    settings: Arc<Settings>,
    step_up: Arc<StepUp>,
}
static CORE: OnceLock<Core> = OnceLock::new();

pub(crate) fn bind(settings: Arc<Settings>, step_up: Arc<StepUp>) -> bool {
    CORE.set(Core { settings, step_up }).is_ok()
}

fn core() -> bilibi_agent_api::Result<&'static Core> {
    CORE.get().ok_or(Error::Internal.into())
}

struct Field {
    key: &'static str,
    module: &'static str,
    definition: &'static Definition,
    editor: &'static Editor,
}

fn field(key: &str) -> bilibi_agent_api::Result<Field> {
    let settings = &core()?.settings;
    if !settings.definitions().is_tenant_admin(key) {
        return Err(AgentError::NotFound);
    }
    let owned = settings
        .definitions()
        .lookup(key)
        .map_err(|_| AgentError::NotFound)?;
    let Setting::Definition(definition) = owned.setting else {
        return Err(AgentError::NotFound);
    };
    let editor = definition.editor.as_ref().ok_or(AgentError::NotFound)?;
    Ok(Field {
        key: owned.pattern,
        module: owned.owner,
        definition,
        editor,
    })
}

async fn authorize_field(
    client: &dyn bilibi_tenancy::ProofClient,
    scope: &Scope,
    field: &Field,
) -> bilibi_agent_api::Result<()> {
    if let Some(capability) = field.editor.capability {
        authorize_proof(client, scope, capability)
            .await
            .map_err(|error| match error {
                Error::Forbidden => AgentError::NotFound,
                error => error.into(),
            })?;
    }
    Ok(())
}

fn source(scope: Option<ScopeKind>) -> &'static str {
    match scope {
        Some(ScopeKind::Tenant) => "tenant",
        Some(ScopeKind::Global) => "global",
        Some(ScopeKind::Company) => "company",
        Some(ScopeKind::User) => "user",
        None => "default",
    }
}

fn shown(definition: &Definition, value: &Value) -> Value {
    if definition.encrypted {
        Value::Null
    } else {
        value.clone()
    }
}

fn present(value: &Value) -> bool {
    !(value.is_null() || value.as_str() == Some(""))
}

fn setting_json(field: &Field, resolved: &bilibi_settings::Resolved) -> Value {
    json!({
        "key": field.key,
        "module": field.module,
        "group": field.editor.group,
        "label": field.editor.label,
        "help": field.editor.help,
        "value": shown(field.definition, &resolved.value),
        "source": source(resolved.source.map(|scope| scope.kind)),
        "set_for_tenant": resolved.overridden,
        "secret": field.definition.encrypted,
        "secret_is_set": field.definition.encrypted && (resolved.source.is_some() || present(&resolved.value)),
    })
}

struct List;
impl ReadHandler for List {
    fn owner(&self) -> &'static str {
        "base/settings_editor"
    }

    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        _input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let core = core()?;
            let capabilities = effective_capabilities(client.sql_client().await?, scope).await?;
            let fields: Vec<Field> = core
                .settings
                .definitions()
                .tenant_admin()
                .filter_map(|owned| {
                    let Setting::Definition(definition) = owned.setting else {
                        return None;
                    };
                    let editor = definition.editor.as_ref()?;
                    if editor
                        .capability
                        .is_some_and(|capability| !capabilities.contains(capability))
                    {
                        return None;
                    }
                    Some(Field {
                        key: owned.pattern,
                        module: owned.owner,
                        definition,
                        editor,
                    })
                })
                .collect();
            let keys: Vec<&str> = fields.iter().map(|field| field.key).collect();
            let mut resolved = core
                .settings
                .resolve_masked(client, scope, Target::Tenant, &keys)
                .await?;
            let settings = fields
                .iter()
                .filter_map(|field| {
                    resolved
                        .remove(field.key)
                        .map(|value| setting_json(field, &value))
                })
                .collect::<Vec<_>>();
            Ok(Output::from(json!({"settings": settings})))
        })
    }
}

enum Change {
    Set,
    Restore,
}

impl Change {
    async fn review(
        &self,
        client: &dyn bilibi_tenancy::ReadClient,
        scope: &Scope,
        input: &Value,
    ) -> bilibi_agent_api::Result<Value> {
        let field = field(input["key"].as_str().unwrap_or_default())?;
        authorize_field(client, scope, &field).await?;
        let core = core()?;
        let mut current = core
            .settings
            .resolve_masked(client, scope, Target::Tenant, &[field.key])
            .await?;
        let current = current.remove(field.key).ok_or(Error::Internal)?;
        let (action, next, next_source, changes) = match self {
            Self::Set => {
                let value = form::cast(
                    input["value"].as_str().unwrap_or_default(),
                    field.definition,
                )
                .map_err(|message| bilibi_agent_api::invalid("value", message))?;
                let changes = field.definition.encrypted || current.value != value;
                ("set", value, "tenant", changes)
            }
            Self::Restore => {
                let mut inherited = core
                    .settings
                    .resolve_masked(client, scope, Target::Global, &[field.key])
                    .await?;
                let inherited = inherited.remove(field.key).ok_or(Error::Internal)?;
                let next_source = source(inherited.source.map(|scope| scope.kind));
                ("restore", inherited.value, next_source, current.overridden)
            }
        };
        let summary = if changes {
            match self {
                Self::Set => format!("Set {} for this tenant", field.editor.label),
                Self::Restore => format!(
                    "Restore {} to its inherited value from {next_source}",
                    field.editor.label
                ),
            }
        } else {
            format!("No change to {}", field.editor.label)
        };
        Ok(json!({
            "action": action,
            "key": field.key,
            "module": field.module,
            "setting": field.editor.label,
            "secret": field.definition.encrypted,
            "value_before": shown(field.definition, &current.value),
            "source_before": source(current.source.map(|scope| scope.kind)),
            "value_after": shown(field.definition, &next),
            "source_after": next_source,
            "will_change": changes,
            "summary": summary,
        }))
    }

    async fn protect(&self, scope: &Scope, field: &Field) -> bilibi_agent_api::Result<()> {
        let core = core()?;
        if field.definition.encrypted {
            core.step_up
                .require(
                    scope,
                    TENANT_SECRET_CHANGE,
                    &format!("tenant-setting:{}", field.key),
                )
                .await?;
        }
        if let Some(guard) = core.step_up.guarding_setting(field.key) {
            core.step_up
                .require(
                    scope,
                    guard.operation.key,
                    &format!("setting:{}", field.key),
                )
                .await?;
        }
        Ok(())
    }
}

impl WriteHandler for Change {
    fn owner(&self) -> &'static str {
        "base/settings_editor"
    }

    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move { Ok(Output::from(self.review(client, scope, input).await?)) })
    }

    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let field = field(input["key"].as_str().unwrap_or_default())?;
            let result = self.review(tx.sql(), scope, input).await?;
            self.protect(scope, &field).await?;
            let core = core()?;
            match self {
                Self::Set => {
                    let value = form::cast(
                        input["value"].as_str().unwrap_or_default(),
                        field.definition,
                    )
                    .map_err(|message| bilibi_agent_api::invalid("value", message))?;
                    let current = core
                        .settings
                        .get(tx.sql(), scope, Target::Tenant, field.key)
                        .await?;
                    if field.definition.encrypted || current.value != value {
                        core.settings
                            .put(tx, scope, Target::Tenant, field.key, value)
                            .await?;
                    }
                }
                Self::Restore => {
                    core.settings
                        .delete(tx, scope, Target::Tenant, field.key)
                        .await?;
                }
            }
            Ok(Output::from(result))
        })
    }
}

fn empty_input() -> Value {
    json!({"type":"object","properties":{}})
}

fn key_input() -> Value {
    json!({"type":"object","required":["key"],"properties":{
        "key":{"type":"string","maxLength":255,"description":"Exact key from base.settings_editor.tenant.list"}
    }})
}

fn set_input() -> Value {
    json!({"type":"object","required":["key","value"],"properties":{
        "key":{"type":"string","maxLength":255,"description":"Exact key from base.settings_editor.tenant.list"},
        "value":{"type":"string","maxLength":1048576,"writeOnly":true,"description":"The new value. Use true or false for yes/no, a decimal for a number, and JSON for an object or list. Plain text is used as written."}
    }})
}

fn list_output() -> Value {
    json!({"type":"object","description":"Delegated settings the caller may manage, with module, label, help, effective value, source and whether this tenant set an override. Secret values are null; only whether one is set is returned."})
}

fn change_output() -> Value {
    json!({"type":"object","description":"The setting, source before and after, whether it changes, and a plain review summary. Secret values are always null."})
}

static LIST: List = List;
static SET: Change = Change::Set;
static RESTORE: Change = Change::Restore;

inventory::submit! { Contribution::new(DEFINITIONS, "base/settings_editor", Provenance { module: "base/settings_editor", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key:"base.settings_editor.tenant.list", title:"List tenant settings", summary:"List settings their modules let tenant administrators manage, with the effective value and whether it comes from this tenant, the installation or the declared default. Secret values are never returned.", keywords:&["settings","configuration","customisation","tenant","inherited","default"], capability:TENANT_VIEW, input:empty_input, output:list_output, handler:Handler::Read(&LIST), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.settings_editor.tenant.set", title:"Set a tenant setting", summary:"Review and set one explicitly delegated setting for this tenant. The definition validates its type and bounds; a secret is redacted and always asks for 2FA.", keywords:&["settings","configuration","customisation","tenant","set","secret"], capability:TENANT_MANAGE, input:set_input, output:change_output, handler:Handler::SafeWrite(&SET), channels:Channels::Every, step_up:&[TENANT_SECRET_CHANGE] },
        Operation { key:"base.settings_editor.tenant.restore", title:"Restore an inherited tenant setting", summary:"Review and remove this tenant's override of one explicitly delegated setting, so it follows the installation value or declared default again. Secret values are never returned.", keywords:&["settings","configuration","customisation","tenant","restore","inherit","default"], capability:TENANT_MANAGE, input:key_input, output:change_output, handler:Handler::SafeWrite(&RESTORE), channels:Channels::Every, step_up:&[TENANT_SECRET_CHANGE] },
    ],
    guides: &[Guide { key:"base.settings_editor", title:"Tenant settings", summary:"Read, set and restore settings their modules delegate to tenant administrators.", keywords:&["settings","configuration","customisation","tenant","inherit","restore","secret"], capability:TENANT_VIEW,
        body:"Use base.settings_editor.tenant.list first. It returns only settings their owning modules explicitly delegated and this person may manage. `source` is tenant, global or default; `set_for_tenant` says whether restoring will remove a tenant override. Secret values are always null and only `secret_is_set` is shown. To change a setting, plan base.settings_editor.tenant.set with its exact key and a string value: true or false for yes/no, a decimal for a number, JSON for an object or list, and plain text for text. Show the returned before/after source and value, then wait for approval. Secret values remain redacted and the commit asks the account holder for a fresh 2FA code. To follow later installation changes again, plan base.settings_editor.tenant.restore; it removes only this tenant's override. Never invent a key or a tenant ID: the credential supplies the tenant and the list supplies the keys." }],
}) }
