use crate::{
    page_metadata::{metadata, pages, PageMetadata},
    shared_views::{canonical_arrangement, ROLE_AUDIENCE_LABEL},
    Audience, Catalog, PageViews, View, SHARE_VIEWS,
};
use bilibi_agent_api::{
    AudiencePreview, Channels, Definitions, Guide, Handler, Operation, OperationClient,
    OperationFuture, ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_tenancy::{Channel, Scope};
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
    Ok(canonical_arrangement(&View::from_json(
        &stored,
        input["page"].as_str().unwrap_or_default(),
    )))
}

fn shared_json(view: &crate::SharedView, can_share: bool) -> Value {
    let audience = match &view.audience {
        Audience::Everyone => json!({"everyone": true}),
        Audience::Roles(roles) if can_share => json!({"roles": roles}),
        Audience::Roles(_) => json!({"name": ROLE_AUDIENCE_LABEL}),
    };
    json!({"id": view.id, "owned": view.owned, "name": view.name,
        "view": view.view.to_json(), "params": view.params, "audience": audience})
}

async fn page_columns(
    client: &tokio_postgres::Client,
    scope: &Scope,
    metadata: &PageMetadata,
) -> bilibi_database::Result<crate::PageColumns> {
    let views = BOUND.get().cloned().ok_or(Error::Internal)?;
    (metadata.columns)(client, scope, views).await
}

async fn authorized_page(
    client: &dyn OperationClient,
    scope: &Scope,
    page: &str,
) -> bilibi_agent_api::Result<Option<&'static PageMetadata>> {
    let metadata = metadata(page).map_err(|_| Error::Internal)?;
    if let Some(metadata) = metadata {
        bilibi_authz::authorize_proof(client, scope, metadata.capability).await?;
        page_columns(client.sql_client().await?, scope, metadata).await?;
    }
    Ok(metadata)
}

async fn save_page(
    client: &dyn OperationClient,
    scope: &Scope,
    page: &str,
) -> bilibi_agent_api::Result<Option<&'static PageMetadata>> {
    let metadata = authorized_page(client, scope, page).await?;
    if metadata.is_none() && scope.channel() != Channel::Web {
        return Err(bilibi_agent_api::invalid(
            "page",
            "choose a registered list page",
        ));
    }
    Ok(metadata)
}

enum Read {
    Pages,
    List,
    Get,
    Fields,
}
/// Every registered list page the reader holds the grant for, with the
/// shared views they may open there after the page owner checks access.
async fn readable_pages(
    client: &dyn OperationClient,
    scope: &Scope,
) -> bilibi_agent_api::Result<Value> {
    let sql = client.sql_client().await?;
    let held = bilibi_authz::effective_capabilities(sql, scope).await?;
    let mut readable = Vec::new();
    for page in pages().map_err(|_| Error::Internal)? {
        if !held.contains(page.capability) {
            continue;
        }
        match page_columns(sql, scope, page).await {
            Ok(_) => readable.push(*page),
            Err(Error::Forbidden | Error::NotFound) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let names: Vec<&str> = readable.iter().map(|page| page.page).collect();
    let shared = bound()?.shared_for_pages(client, scope, &names).await?;
    let can_share = held.contains(SHARE_VIEWS);
    let installed = crate::installed().map_err(|_| Error::Internal)?;
    let rows: Vec<Value> = readable
        .iter()
        .map(|page| {
            let table = installed.table(page.page);
            let record_kinds: Vec<&str> =
                table.map_or_else(|| vec![page.page], |table| table.record_kinds().collect());
            let visible: Vec<_> = shared
                .iter()
                .filter(|(on, _)| on == page.page)
                .map(|(_, view)| view)
                .collect();
            let views: Vec<_> = visible.iter().map(|view| view.name.as_str()).collect();
            let saved_views: Vec<_> = visible
                .iter()
                .map(|view| shared_json(view, can_share))
                .collect();
            json!({"page": page.page, "path": page.path, "label": table.map(|table| table.label()),
                   "record_kinds": record_kinds, "views": views, "saved_views": saved_views,
                   "can_share": can_share})
        })
        .collect();
    Ok(json!({"pages": rows}))
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
            if matches!(self, Self::Pages) {
                return Ok(readable_pages(client, scope).await?.into());
            }
            let page = input["page"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(Error::Invalid)?;
            let metadata = authorized_page(client, scope, page)
                .await?
                .ok_or(Error::NotFound)?;
            if matches!(self, Self::Fields) {
                let spec = metadata.spec.map(|spec| spec());
                let mut filters: Vec<Value> = spec.iter().flat_map(|spec| &spec.filters).map(|(key, filter)| match filter {
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
                let mut page_columns =
                    page_columns(client.sql_client().await?, scope, metadata).await?;
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
                let mut result = json!({"page":page,"columns":columns,"filters":filters,"next_offset":if more { Some(offset + 50) } else { None },"notice":"Page-owned columns appear on the first page. Walked columns are paged in groups of 50 and use your access."});
                if let Some(spec) = spec {
                    result["search_key"] = json!(spec.param_names.search);
                }
                return Ok(result.into());
            }
            let id = match self {
                Self::List => None,
                Self::Pages | Self::Fields => unreachable!(),
                Self::Get => Some(
                    input["id"]
                        .as_i64()
                        .filter(|id| *id > 0)
                        .ok_or(Error::Invalid)?,
                ),
            };
            let rows = bound()?.shared_on(client, scope, page, id).await?;
            let can_share = bilibi_authz::holds_proof(client, scope, SHARE_VIEWS).await?;
            let mut views = rows.iter().map(|row| shared_json(row, can_share));
            match self {
                Self::List => Ok(json!({"views": views.collect::<Vec<_>>()}).into()),
                Self::Get => Ok(views.next().ok_or(Error::NotFound)?.into()),
                Self::Pages | Self::Fields => unreachable!(),
            }
        })
    }
}
fn pages_schema() -> Value {
    json!({"type":"object","properties":{}})
}
fn pages_output() -> Value {
    json!({"type":"object","description":"pages: each list page you may open, with its page name (the page input of the other view operations), label, record_kinds (its Grid table and record types) the address when registered, and the names and details of the shared views you may open there."})
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
                    params(input)?;
                    let metadata = save_page(client, scope, &view.table).await?;
                    let summary = format!(
                        "Share list view “{}” on {}",
                        input["name"].as_str().unwrap_or_default(),
                        view.table
                    );
                    let audience = match audience {
                        Audience::Everyone => AudiencePreview::everyone(),
                        Audience::Roles(roles) => {
                            AudiencePreview::roles(client, scope, roles).await?
                        }
                    };
                    let columns = if let Some(metadata) = metadata {
                        let sql = client.sql_client().await?;
                        let catalog = Catalog::for_scope(sql, scope)
                            .await
                            .map_err(Error::from)?
                            .without_custom_references();
                        let mut page = page_columns(sql, scope, metadata).await?;
                        Some(
                            page.preview_columns(&catalog, view)
                                .into_iter()
                                .map(|(spec, label)| json!({"spec":spec,"label":label}))
                                .collect::<Vec<_>>(),
                        )
                    } else {
                        None
                    };
                    return Ok(json!({
                        "summary": summary,
                        "audience_preview": audience.review(json!(columns)),
                        "input": input,
                    })
                    .into());
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
                    let arrangement = view(input)?;
                    let metadata = save_page(tx.sql(), scope, &arrangement.table).await?;
                    let id = views
                        .save_shared(
                            tx,
                            scope,
                            &arrangement,
                            &params(input)?,
                            input["name"].as_str().unwrap_or_default(),
                            &audience(&input["audience"])?,
                        )
                        .await?;
                    if metadata.is_none() {
                        return Ok(json!({"id": id}).into());
                    }
                    let saved = views
                        .shared_on(tx.sql(), scope, &arrangement.table, Some(id))
                        .await?
                        .into_iter()
                        .next()
                        .ok_or(Error::Internal)?;
                    json!({"id": id, "view": shared_json(&saved, true)})
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
        "params":{"type":"object"},"audience":{"type":"object","properties":{"everyone":{"type":"boolean"},"roles":{"type":"array","items":{"type":"integer"}},"name":{"type":"string"}}}}})
}
fn list_output() -> Value {
    json!({"type":"object","properties":{"views":{"type":"array","items":view_output()}}})
}
fn fields_output() -> Value {
    json!({"type":"object","description":"Page query keys and scoped Grid column suggestions; the notice explains their bounds.","properties":{
        "page":{"type":"string"},
        "columns":{"type":"array","items":{"type":"object","properties":{"spec":{"type":"string"},"label":{"type":"string"},"type":{"type":"string"}}}},
        "filters":{"type":"array","items":{"type":"object","properties":{"key":{"type":"string"},"type":{"type":"string"},"label":{"type":"string"},"field":{"type":"string"},"description":{"type":"string"},"default":{"type":"string"},"operator":{"type":"string"},"operators":{"type":"array","items":{"type":"string"}},"choices":{"type":"array","items":{"type":"string"}}}}},
        "search_key":{"type":"string"},"next_offset":{"type":"integer","nullable":true},"notice":{"type":"string"}}})
}
fn save_output() -> Value {
    json!({"type":"object","required":["id"],"properties":{"id":{"type":"integer"},"view":view_output()}})
}
fn output() -> Value {
    json!({"type":"object"})
}
inventory::submit! { Contribution::new(DEFINITIONS, "base/grid.views", Provenance { module: "base/grid", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "base.grid.view.pages", title: "List pages that take shared views", summary: "The list pages you may open, each with the names of the shared views you can open there.", keywords: &["list", "view", "page", "share"], capability: bilibi_agent_api::SIGNED_IN, input: pages_schema, output: pages_output, handler: Handler::Read(&Read::Pages), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.list", title: "Shared list views", summary: "List the views you can open on a page.", keywords: &["list", "view", "share"], capability: bilibi_agent_api::SIGNED_IN, input: list_schema, output: list_output, handler: Handler::Read(&Read::List), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.get", title: "Shared list view", summary: "Read a shared view you can open.", keywords: &["list", "view", "get"], capability: bilibi_agent_api::SIGNED_IN, input: get_schema, output: view_output, handler: Handler::Read(&Read::Get), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.fields", title: "List view fields and filters", summary: "Find permitted Grid columns and the page's query filters.", keywords: &["list", "view", "fields", "filters"], capability: bilibi_agent_api::SIGNED_IN, input: fields_schema, output: fields_output, handler: Handler::Read(&Read::Fields), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.save", title: "Share a list view", summary: "Save columns, filters and sort for everyone or roles. Readers retain their own access checks.", keywords: &["list", "view", "share"], capability: SHARE_VIEWS, input: save_schema, output: save_output, handler: Handler::SafeWrite(&Write::Save), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.grid.view.delete", title: "Delete a shared list view", summary: "Delete your own view, or another owner’s with sharing permission.", keywords: &["list", "view", "delete"], capability: bilibi_agent_api::SIGNED_IN, input: delete_schema, output, handler: Handler::SafeWrite(&Write::Delete), channels: Channels::Every, step_up: &[] },
    ], guides: &[Guide { key:"base.grid.views", title:"Shared list views", summary:"Discover list fields and filters, share a view, and read it back.", keywords:&["grid","list","view","share"], capability:SHARE_VIEWS, body:"Use base.grid.view.fields with a page key to discover the page's columns and filter keys. Search and follow next_offset for walked columns. Use base.grid.view.list and base.grid.view.get to read views you can open. Save with base.grid.view.save using a registered page key. View and params are JSON strings, and audience is everyone or role IDs. Review the named audience and columns as you see them; each recipient retains their own access, so their result can differ. The save result includes the saved view. Delete your own view with base.grid.view.delete, or another owner's if you have sharing permission." }],
}) }

/// What may be changed: every list page the reader may open, with or
/// without a shared view.
fn customisation_choices(reads: &[Value]) -> Vec<bilibi_labels::Choice> {
    let pages = reads[0]["pages"].as_array().into_iter().flatten();
    let mut choices = Vec::new();
    for page in pages {
        let (Some(name), Some(views)) = (page["page"].as_str(), page["views"].as_array()) else {
            continue;
        };
        let title = page["label"].as_str().unwrap_or(name);
        let address = page["path"].as_str().map(str::to_owned);
        let record_kinds: Vec<_> = page["record_kinds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|kind| kind.as_str().map(str::to_owned))
            .collect();
        choices.push(bilibi_labels::Choice {
            subject: name.to_owned(),
            title: format!("{title}: shared list views"),
            page: address.clone(),
            record_kinds: record_kinds.clone(),
            value: Some(json!(views)),
            default: Some(json!([])),
            source: Some(
                if views.is_empty() {
                    "default"
                } else {
                    "tenant"
                }
                .into(),
            ),
            customised: !views.is_empty(),
            input: json!({"page": name}),
            verify_input: json!({"page": name}),
            writes: Some(vec!["base.grid.view.save"]),
            ..Default::default()
        });
        choices.extend(
            page["saved_views"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|view| {
                    let id = view["id"].as_i64()?;
                    let can_delete = view["owned"].as_bool()? || page["can_share"].as_bool()?;
                    Some(bilibi_labels::Choice {
                        subject: format!("{name}:{id}"),
                        title: format!("{title}: {}", view["name"].as_str()?),
                        page: address.clone(),
                        record_kinds: record_kinds.clone(),
                        value: Some(view.clone()),
                        source: Some("tenant".into()),
                        customised: true,
                        input: json!({"id": id}),
                        verify_input: json!({"page": name}),
                        writes: Some(if can_delete {
                            vec!["base.grid.view.delete"]
                        } else {
                            Vec::new()
                        }),
                        ..Default::default()
                    })
                }),
        );
    }
    choices
}

inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/grid.customisations", Provenance { module: "base/grid", source: file!(), line: line!() }, &bilibi_labels::CustomisationSection {
    title: "Shared list views",
    undo: None,
    changes: Some(bilibi_labels::Changes {
        purpose: "Save a list's columns, filters and sort as a named view for everyone in this tenant or for chosen roles.",
        keywords: &["list", "view", "columns", "column", "filter", "sort", "layout", "share", "arrange"],
        limits: "Up to 100 shared views on a list, each with a name of at most 80 characters, for everyone or 1 to 50 roles. A view never widens what a reader may see: their own grants and field restrictions still apply.",
        guide: "base.grid.views",
        reads: &[bilibi_labels::Read { operation: "base.grid.view.pages", input: pages_input }],
        choices: customisation_choices,
        change: &["base.grid.view.save"],
        preview: None,
        verify: "base.grid.view.list",
        restore: Some("base.grid.view.delete"),
    }),
}) }
fn pages_input() -> Value {
    json!({})
}
