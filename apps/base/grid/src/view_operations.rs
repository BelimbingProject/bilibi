use crate::{Audience, PageViews, View, SHARE_VIEWS};
use bilibi_agent_api::{
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, WriteHandler,
    DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

static BOUND: OnceLock<Arc<PageViews>> = OnceLock::new();
pub(crate) fn bind(views: Arc<PageViews>) -> bilibi_module_registry::Result<()> {
    BOUND
        .set(views)
        .map_err(|_| RegistryError::Service("duplicate grid views binding".into()))
}
fn bound() -> bilibi_agent_api::Result<&'static PageViews> {
    BOUND.get().map(AsRef::as_ref).ok_or(Error::Internal.into())
}
fn audience(input: &Value) -> bilibi_agent_api::Result<Audience> {
    if input["everyone"].as_bool() == Some(true) && input.get("roles").is_none() {
        return Ok(Audience::Everyone);
    }
    let roles: Vec<i64> =
        serde_json::from_value(input["roles"].clone()).map_err(|_| Error::Invalid)?;
    if input.get("everyone").is_some()
        || roles.is_empty()
        || roles.len() > crate::shared_views::MAX_ROLES
        || roles.iter().any(|id| *id <= 0)
    {
        return Err(bilibi_agent_api::invalid(
            "audience",
            "choose everyone or one to 50 roles",
        ));
    }
    Ok(Audience::Roles(roles))
}
fn params(input: &Value) -> bilibi_agent_api::Result<BTreeMap<String, String>> {
    let params = serde_json::from_str(input["params"].as_str().ok_or(Error::Invalid)?)
        .map_err(|_| Error::Invalid)?;
    Ok(crate::shared_views::clean_params(&params)?)
}
fn view(input: &Value) -> bilibi_agent_api::Result<View> {
    let stored: Value = serde_json::from_str(input["view"].as_str().ok_or(Error::Invalid)?)
        .map_err(|_| Error::Invalid)?;
    if !stored.is_object() {
        return Err(Error::Invalid.into());
    }
    Ok(View::from_json(
        &stored,
        input["page"].as_str().unwrap_or_default(),
    ))
}
enum Write {
    Save,
    Delete,
}
impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        "base/grid"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_proof(client, scope, SHARE_VIEWS).await?;
            let summary = match self {
                Self::Save => {
                    audience(&input["audience"])?;
                    params(input)?;
                    view(input)?;
                    format!(
                        "Share list view “{}” on {}",
                        input["name"].as_str().unwrap_or_default(),
                        input["page"].as_str().unwrap_or_default()
                    )
                }
                Self::Delete => {
                    let row = client.query_opt("SELECT name FROM base_grid_shared_views WHERE tenant_id=$1 AND owner_id=$2 AND id=$3", &[&scope.tenant(), &scope.user(), &input["id"].as_i64().ok_or(Error::Invalid)?]).await?.ok_or(Error::NotFound)?;
                    format!("Delete shared view “{}”", row.get::<_, String>(0))
                }
            };
            Ok(json!({"summary": summary, "input": input}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let views = bound()?;
            let output = match self {
                Self::Save => {
                    let id = views
                        .save_shared(
                            tx,
                            scope,
                            &view(input)?,
                            &params(input)?,
                            input["name"].as_str().unwrap_or_default(),
                            &audience(&input["audience"])?,
                        )
                        .await?;
                    json!({"id": id})
                }
                Self::Delete => {
                    views
                        .delete_shared(tx, scope, input["id"].as_i64().ok_or(Error::Invalid)?)
                        .await?;
                    json!({"deleted": true})
                }
            };
            Ok(output.into())
        })
    }
}
fn save_schema() -> Value {
    json!({"type":"object","required":["page","view","params","name","audience"],"properties":{
        "page":{"type":"string","maxLength":90},
        "view":{"type":"string","maxLength":16384,"description":"JSON arrangement with columns (ordered strings), lenses (spec to lens), zoom (integer) and since (date or null)."}, "params":{"type":"string","maxLength":16384,"description":"JSON object of the page query keys and their decoded string values, without Grid or pagination keys."},
        "name":{"type":"string","maxLength":80}, "audience":{"type":"object","properties":{"everyone":{"type":"boolean"},"roles":{"type":"array","items":{"type":"integer","minimum":1}}}}}})
}
fn delete_schema() -> Value {
    json!({"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1}}})
}
fn output() -> Value {
    json!({"type":"object"})
}
inventory::submit! { Contribution::new(DEFINITIONS, "base/grid.views", Provenance { module: "base/grid", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "base.grid.view.save", title: "Share a list view", summary: "Save columns, filters and sort for everyone or roles. Readers retain their own access checks.", keywords: &["list", "view", "share"], capability: SHARE_VIEWS, input: save_schema, output, handler: Handler::SafeWrite(&Write::Save), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.delete", title: "Delete a shared list view", summary: "Delete a view you saved, removing it for everyone it was shared with.", keywords: &["list", "view", "delete"], capability: SHARE_VIEWS, input: delete_schema, output, handler: Handler::SafeWrite(&Write::Delete), channels: Channels::Every, step_up: &[] },
    ], guides: &[],
}) }
