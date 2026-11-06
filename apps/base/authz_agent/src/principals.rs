//! Reviewed administration of the installed vocabulary's named principals.
use crate::{Bound, SYSTEM_LIST};
use bilibi_authz::{authorize_key, effective_capabilities_in_company, effective_system, Effective};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::GenericClient;

pub const LIST: &str = "base.authz_agent.system_principal.list";
pub const SET: &str = "base.authz_agent.system_principal.set";
const GRANT: &str = "admin.authz.system-principal.grant";
const REVOKE: &str = "admin.authz.system-principal.revoke";

fn rule(current: &Effective, key: &str) -> &'static str {
    if current.direct_denied.contains(key) {
        "deny"
    } else if current.direct_allowed.contains(key) {
        "allow"
    } else {
        "clear"
    }
}

pub(crate) async fn list(
    bound: &Bound,
    client: &(impl GenericClient + Sync),
    scope: &Scope,
) -> Result<Value> {
    authorize_key(client, scope, SYSTEM_LIST).await?;
    let actor = effective_capabilities_in_company(client, scope, scope.user(), None).await?;
    let vocabulary = bound.grants.vocabulary();
    let mut entries = Vec::new();
    for principal in vocabulary.system_principals.values() {
        let current = effective_system(client, scope.tenant(), principal.name).await?;
        for key in principal.capabilities {
            entries.push(json!({
                "principal": principal.name, "capability": key,
                "rule": rule(&current, key), "granted": current.allowed.contains(key),
                "standing": vocabulary.standing(principal.name, key),
                "can_allow": actor.allowed.contains(GRANT) && actor.allowed.contains(key),
                "can_deny": actor.allowed.contains(GRANT),
                "can_clear": actor.allowed.contains(REVOKE),
            }));
        }
    }
    Ok(json!({"entries": entries}))
}

pub(crate) async fn review(
    bound: &Bound,
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    input: &Value,
) -> bilibi_agent_api::Result<Value> {
    authorize_key(client, scope, SYSTEM_LIST).await?;
    bound
        .grants
        .verify_principal(client, scope, scope.user())
        .await?;
    let principal = input["principal"].as_str().unwrap_or_default();
    let key = input["capability"].as_str().unwrap_or_default();
    let declaration = bound
        .grants
        .vocabulary()
        .system_principals
        .get(principal)
        .ok_or_else(|| {
            bilibi_agent_api::invalid("principal", "is not an installed system principal")
        })?;
    if !declaration.capabilities.contains(&key) {
        return Err(bilibi_agent_api::invalid(
            "capability",
            "is outside this principal's declared bounds",
        ));
    }
    let next = input["rule"].as_str().unwrap_or_default();
    let expected = input["expected_rule"].as_str().unwrap_or_default();
    if !["allow", "deny", "clear"].contains(&next) {
        return Err(bilibi_agent_api::invalid(
            "rule",
            "must be allow, deny or clear",
        ));
    }
    if !["allow", "deny", "clear"].contains(&expected) {
        return Err(bilibi_agent_api::invalid(
            "expected_rule",
            "must be the stored rule from the latest read",
        ));
    }
    // These are exactly the owner's gates: a stored deny is a grant rule;
    // removing either stored rule requires revoke authority.
    authorize_key(client, scope, if next == "clear" { REVOKE } else { GRANT }).await?;
    if next == "allow"
        && !effective_capabilities_in_company(client, scope, scope.user(), None)
            .await?
            .allowed
            .contains(key)
    {
        return Err(Error::Forbidden.into());
    }
    let current = effective_system(client, scope.tenant(), principal).await?;
    if rule(&current, key) != expected {
        return Err(Error::Conflict.into());
    }
    Ok(json!({
        "summary": format!("Set {key} on system principal {principal} to {next} in this tenant"),
        "principal": principal, "capability": key, "rule": next,
        "current_rule": expected, "currently_granted": current.allowed.contains(key),
        "standing": bound.grants.vocabulary().standing(principal, key),
        "step_up": [crate::STEP_UP_SYSTEM],
    }))
}

pub(crate) fn input_schema() -> Value {
    let rule = json!({"type":"string","enum":["allow","deny","clear"]});
    json!({"type":"object","required":["principal","capability","rule","expected_rule"],"properties":{
        "principal":{"type":"string","maxLength":200},
        "capability":{"type":"string","maxLength":255},
        "rule":rule,"expected_rule":rule,
    }})
}

pub(crate) fn output_schema() -> Value {
    json!({"type":"object","description":"Installed principal/capability pairs with stored rule (allow, deny or clear), effective access, standing grant and this administrator's allowed actions. Use the stored rule as expected_rule when planning a change."})
}
