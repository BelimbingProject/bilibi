use crate::{Audience, Catalog, PageViews, View, SHARE_VIEWS};
use bilibi_agent_api::{
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
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

fn shared_json(view: &crate::SharedView) -> Value {
    let audience = match &view.audience {
        Audience::Everyone => json!({"everyone": true}),
        Audience::Roles(roles) => json!({"roles": roles}),
    };
    json!({"id": view.id, "owned": view.owned, "name": view.name,
        "view": view.view.to_json(), "params": view.params, "audience": audience,
        "query": view.query()})
}

enum Read {
    List,
    Get,
    Fields,
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "base/grid"
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let page = input["page"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(Error::Invalid)?;
            if matches!(self, Self::Fields) {
                let metadata = crate::page_metadata::metadata(page)
                    .map_err(|_| Error::Internal)?
                    .ok_or(Error::NotFound)?;
                bilibi_authz::authorize_proof(client, scope, metadata.capability).await?;
                let spec = (metadata.spec)();
                let mut filters: Vec<Value> = spec.filters.iter().map(|(key, filter)| match filter {
                    bilibi_ui::Filter::OneOf { allowed, default } => json!({"key": key, "type":"choice", "choices":allowed, "default":default}),
                    bilibi_ui::Filter::String { default, .. } => json!({"key":key, "type":"string", "default":default}),
                }).collect();
                filters.extend(metadata.extra_filters.iter().map(|(key, description)| json!({"key":key, "type":"string", "description":description})));
                if let Some(details) = metadata.details {
                    filters.extend(details(client.sql_client().await?, scope).await?);
                }
                let catalog = Catalog::for_scope(client.sql_client().await?, scope)
                    .await
                    .map_err(Error::from)?
                    .without_custom_references();
                let search = input["search"].as_str().unwrap_or_default();
                let offset = input["offset"].as_u64().unwrap_or(0).min(4096) as usize;
                let views = BOUND.get().cloned().ok_or(Error::Internal)?;
                let mut page_columns =
                    (metadata.columns)(client.sql_client().await?, scope, views).await?;
                let builtins = page_columns.available_builtins(&catalog);
                let mut columns: Vec<Value> = if offset == 0 {
                    builtins
                        .iter()
                        .filter(|(id, label)| {
                            search.is_empty()
                                || id.to_lowercase().contains(&search.to_lowercase())
                                || label.to_lowercase().contains(&search.to_lowercase())
                        })
                        .map(|(id, label)| json!({"spec":id,"label":label,"type":"page"}))
                        .collect()
                } else {
                    Vec::new()
                };
                let exclude: Vec<&str> = builtins.iter().map(|(id, _)| id.as_str()).collect();
                let walked: Vec<Value> = catalog.table(page).map(|root| catalog.suggest_page(root, search, &exclude, offset, 50).into_iter().map(|column| json!({"spec":column.spec,"label":column.label,"type":format!("{:?}", column.ty).to_lowercase()})).collect()).unwrap_or_default();
                let more = walked.len() == 50;
                columns.extend(walked);
                return Ok(json!({"page":page,"columns":columns,"filters":filters,"search_key":spec.param_names.search,"sort_key":spec.param_names.sort_by,"direction_key":spec.param_names.sort_dir,"sorts":spec.sortable.keys().collect::<Vec<_>>(),"next_offset":if more { Some(offset + 50) } else { None },"notice":"Page-owned columns appear on the first page. Walked columns are paged in groups of 50 and use your access."}).into());
            }
            let id = match self {
                Self::List => None,
                Self::Fields => unreachable!(),
                Self::Get => Some(
                    input["id"]
                        .as_i64()
                        .filter(|id| *id > 0)
                        .ok_or(Error::Invalid)?,
                ),
            };
            let rows = bound()?.shared_on(client, scope, page, id).await?;
            let mut views = rows.iter().map(shared_json);
            match self {
                Self::List => Ok(json!({"views": views.collect::<Vec<_>>()}).into()),
                Self::Get => Ok(views.next().ok_or(Error::NotFound)?.into()),
                Self::Fields => unreachable!(),
            }
        })
    }
}
fn list_schema() -> Value {
    json!({"type":"object","required":["page"],"properties":{"page":{"type":"string","maxLength":90}}})
}
fn fields_schema() -> Value {
    json!({"type":"object","required":["page"],"properties":{"page":{"type":"string","maxLength":90},"search":{"type":"string","maxLength":100},"offset":{"type":"integer","minimum":0,"maximum":4096}}})
}
fn get_schema() -> Value {
    json!({"type":"object","required":["page","id"],"properties":{"page":{"type":"string","maxLength":90},"id":{"type":"integer","minimum":1}}})
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
            let summary = match self {
                Self::Save => {
                    bilibi_authz::authorize_proof(client, scope, SHARE_VIEWS).await?;
                    let audience = audience(&input["audience"])?;
                    let view = view(input)?;
                    let params = params(input)?;
                    let audience = match audience {
                        Audience::Everyone => json!({"everyone": true, "name": "Everyone"}),
                        Audience::Roles(mut roles) => {
                            roles.sort_unstable();
                            roles.dedup();
                            let rows = client.query("SELECT id, name FROM roles WHERE tenant_id=$1 AND id=ANY($2) ORDER BY id", &[&scope.tenant(), &roles]).await?;
                            if rows.len() != roles.len() {
                                return Err(bilibi_agent_api::invalid(
                                    "audience",
                                    "choose roles that exist in this tenant",
                                ));
                            }
                            json!({"roles": rows.iter().map(|row| json!({"id":row.get::<_, i64>(0), "name":row.get::<_, String>(1)})).collect::<Vec<_>>()})
                        }
                    };
                    let metadata =
                        crate::page_metadata::metadata(&view.table).map_err(|_| Error::Internal)?;
                    let columns = if let Some(metadata) = metadata {
                        match bilibi_authz::authorize_proof(client, scope, metadata.capability)
                            .await
                        {
                            Ok(()) => {
                                let sql = client.sql_client().await?;
                                let catalog = Catalog::for_scope(sql, scope)
                                    .await
                                    .map_err(Error::from)?
                                    .without_custom_references();
                                let views = BOUND.get().cloned().ok_or(Error::Internal)?;
                                let mut page = (metadata.columns)(sql, scope, views).await?;
                                Some(
                                    page.preview_columns(&catalog, view.clone())
                                        .into_iter()
                                        .map(|(spec, label)| json!({"spec":spec,"label":label}))
                                        .collect::<Vec<_>>(),
                                )
                            }
                            Err(Error::Forbidden) => None,
                            Err(error) => return Err(error.into()),
                        }
                    } else {
                        None
                    };
                    return Ok(json!({"summary":format!("Share list view “{}” on {}", input["name"].as_str().unwrap_or_default(), view.table), "audience":audience, "columns_for_current_reader":columns, "params":params, "input":input, "notice":"The column preview uses your access. Each recipient's own permissions are checked when the view opens. A null preview means this page has not provided a preview or you cannot open it."}).into());
                }
                Self::Delete => {
                    let row = client.query_opt("SELECT name, owner_id FROM base_grid_shared_views WHERE tenant_id=$1 AND id=$2", &[&scope.tenant(), &input["id"].as_i64().ok_or(Error::Invalid)?]).await?.ok_or(Error::NotFound)?;
                    if row.get::<_, i64>(1) != scope.user() {
                        bilibi_authz::authorize_proof(client, scope, SHARE_VIEWS).await?;
                    }
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
                    let page = input["page"].as_str().ok_or(Error::Invalid)?;
                    let saved = views
                        .shared_on(tx.sql(), scope, page, Some(id))
                        .await?
                        .into_iter()
                        .next()
                        .ok_or(Error::Internal)?;
                    json!({"id": id, "view": shared_json(&saved)})
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
fn view_output() -> Value {
    json!({"type":"object","description":"A shared view visible to the reader. The arrangement and page parameters are reapplied with the reader's own access.","properties":{
        "id":{"type":"integer"},"owned":{"type":"boolean"},"name":{"type":"string"},
        "view":{"type":"object","properties":{"columns":{"type":"array","items":{"type":"string"}},"lenses":{"type":"object"},"zoom":{"type":"integer"},"since":{"type":"string","nullable":true}}},
        "params":{"type":"object"},"audience":{"type":"object","properties":{"everyone":{"type":"boolean"},"roles":{"type":"array","items":{"type":"integer"}}}},"query":{"type":"string"}}})
}
fn list_output() -> Value {
    json!({"type":"object","properties":{"views":{"type":"array","items":view_output()}}})
}
fn fields_output() -> Value {
    json!({"type":"object","description":"Page query keys and scoped Grid column suggestions; the notice explains their bounds.","properties":{
        "page":{"type":"string"},
        "columns":{"type":"array","items":{"type":"object","properties":{"spec":{"type":"string"},"label":{"type":"string"},"type":{"type":"string"}}}},
        "filters":{"type":"array","items":{"type":"object","properties":{"key":{"type":"string"},"type":{"type":"string"},"label":{"type":"string"},"field":{"type":"string"},"description":{"type":"string"},"default":{"type":"string"},"operator":{"type":"string"},"operators":{"type":"array","items":{"type":"string"}},"choices":{"type":"array","items":{"type":"string"}}}}},
        "search_key":{"type":"string"},"sort_key":{"type":"string"},"direction_key":{"type":"string"},"sorts":{"type":"array","items":{"type":"string"}},"next_offset":{"type":"integer","nullable":true},"notice":{"type":"string"}}})
}
fn output() -> Value {
    json!({"type":"object"})
}
inventory::submit! { Contribution::new(DEFINITIONS, "base/grid.views", Provenance { module: "base/grid", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "base.grid.view.list", title: "Shared list views", summary: "List the views you can open on a page.", keywords: &["list", "view", "share"], capability: bilibi_agent_api::SIGNED_IN, input: list_schema, output: list_output, handler: Handler::Read(&Read::List), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.get", title: "Shared list view", summary: "Read a shared view you can open.", keywords: &["list", "view", "get"], capability: bilibi_agent_api::SIGNED_IN, input: get_schema, output: view_output, handler: Handler::Read(&Read::Get), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.fields", title: "List view fields and filters", summary: "Find permitted Grid columns and the page's query filters.", keywords: &["list", "view", "fields", "filters"], capability: bilibi_agent_api::SIGNED_IN, input: fields_schema, output: fields_output, handler: Handler::Read(&Read::Fields), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.save", title: "Share a list view", summary: "Save columns, filters and sort for everyone or roles. Readers retain their own access checks.", keywords: &["list", "view", "share"], capability: SHARE_VIEWS, input: save_schema, output, handler: Handler::SafeWrite(&Write::Save), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.delete", title: "Delete a shared list view", summary: "Delete your own view, or another owner’s with sharing permission.", keywords: &["list", "view", "delete"], capability: bilibi_agent_api::SIGNED_IN, input: delete_schema, output, handler: Handler::SafeWrite(&Write::Delete), channels: Channels::Every, step_up: &[] },
    ], guides: &[Guide { key:"base.grid.views", title:"Shared list views", summary:"Discover list fields and filters, share a view, and read it back.", keywords:&["grid","list","view","share"], capability:SHARE_VIEWS, body:"Use base.grid.view.fields with a page key to discover the page's columns, filter keys and sort keys. Search and follow next_offset for walked columns. Use base.grid.view.list and base.grid.view.get to read views you can open. Save with base.grid.view.save: view and params are JSON strings, audience is everyone or role IDs. Review the named audience and columns as you see them; each recipient retains their own access, so their result can differ. The save result includes the saved view. Delete your own view with base.grid.view.delete, or another owner's if you have sharing permission." }],
}) }
