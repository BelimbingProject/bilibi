//! Portable shared-view setup uses the same authorization, storage and locks as Grid.
use crate::{
    page_metadata::{metadata, PageMetadata},
    shared_views::{as_view, lock_page, SHARED_COLUMNS},
    Audience, PageViews, View, SHARE_VIEWS,
};
use bilibi_database::{Error, MutationTransaction, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::list_state::Filter;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tokio_postgres::Transaction;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSetup {
    pub page: String,
    pub name: String,
    pub view: Value,
    pub params: BTreeMap<String, String>,
    /// Empty for everyone. System roles use `code:`, custom roles use `name:`.
    pub roles: Vec<String>,
}

pub enum ViewTransfer {
    Same,
    Add(Audience),
    Missing(String),
    Collision(String),
}

pub struct ViewPreview<'a> {
    pub fields: &'a [crate::CustomTable],
    pub pending: i64,
    pub lock: bool,
}

async fn missing_column(
    tx: &Transaction<'_>,
    scope: &Scope,
    mut columns: crate::PageColumns,
    arrangement: &View,
    fields: &[crate::CustomTable],
) -> Result<Option<String>> {
    if arrangement.columns.is_empty() && arrangement.lenses.is_empty() {
        return Ok(None);
    }
    let catalog = crate::Catalog::for_setup_preview(tx, scope, fields)
        .await
        .map_err(Error::from)?
        .without_record_fields();
    let allowed = columns.preview_columns(&catalog, arrangement.clone());
    Ok(arrangement
        .columns
        .iter()
        .chain(arrangement.lenses.keys())
        .find(|spec| !allowed.iter().any(|(kept, _)| kept == *spec))
        .cloned())
}

impl PageViews {
    async fn transfer_page(
        self: &Arc<Self>,
        tx: &Transaction<'_>,
        scope: &Scope,
        page: &str,
    ) -> Result<Option<(&'static PageMetadata, crate::PageColumns)>> {
        bilibi_authz::authorize_key(tx, scope, SHARE_VIEWS).await?;
        let Some(page) = metadata(page).map_err(|_| Error::Internal)? else {
            return Ok(None);
        };
        bilibi_authz::authorize_key(tx, scope, page.capability).await?;
        let columns = (page.columns)(
            tokio_postgres::GenericClient::client(tx),
            scope,
            self.clone(),
        )
        .await?;
        Ok(Some((page, columns)))
    }

    pub async fn export_view(
        self: &Arc<Self>,
        tx: &Transaction<'_>,
        scope: &Scope,
        page: &str,
        id: i64,
    ) -> Result<ViewSetup> {
        let (metadata, columns) = self
            .transfer_page(tx, scope, page)
            .await?
            .ok_or(Error::NotFound)?;
        let shared = self
            .shared_on(tx, scope, page, Some(id))
            .await?
            .pop()
            .ok_or(Error::NotFound)?;
        let roles = match shared.audience {
            Audience::Everyone => Vec::new(),
            Audience::Roles(ids) if !ids.is_empty() => {
                let rows = tx.query(
                    "SELECT CASE WHEN code IS NULL THEN 'name:' || name ELSE 'code:' || code END FROM roles WHERE tenant_id=$1 AND id=ANY($2) ORDER BY 1",
                    &[&scope.tenant(), &ids],
                ).await?;
                let roles: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
                if roles.windows(2).any(|pair| pair[0] == pair[1]) {
                    return Err(Error::Refused(
                        "The view uses roles with the same name.".into(),
                    ));
                }
                roles
            }
            Audience::Roles(_) => {
                return Err(Error::Refused(
                    "The saved view no longer has a shared audience.".into(),
                ))
            }
        };
        portable_params(metadata, &shared.params)?;
        if let Some(column) = missing_column(tx, scope, columns, &shared.view, &[]).await? {
            return Err(Error::Refused(format!(
                "The column “{column}” is unavailable."
            )));
        }
        Ok(ViewSetup {
            page: page.into(),
            name: shared.name,
            view: shared.view.to_json(),
            params: shared.params,
            roles,
        })
    }

    pub async fn compare_view(
        self: &Arc<Self>,
        tx: &Transaction<'_>,
        scope: &Scope,
        setup: &ViewSetup,
        role_map: &BTreeMap<&str, i64>,
        preview: ViewPreview<'_>,
    ) -> Result<ViewTransfer> {
        let Some((page, columns)) = self.transfer_page(tx, scope, &setup.page).await? else {
            return Ok(ViewTransfer::Missing(format!(
                "List “{}” is unavailable.",
                setup.page
            )));
        };
        portable_params(page, &setup.params)?;
        let arrangement =
            crate::shared_views::canonical_arrangement(&View::from_json(&setup.view, &setup.page));
        if arrangement.to_json() != setup.view {
            return Err(Error::Refused(
                "Supply a canonical Grid arrangement.".into(),
            ));
        }
        if let Some(column) =
            missing_column(tx, scope, columns, &arrangement, preview.fields).await?
        {
            return Ok(ViewTransfer::Missing(format!(
                "Column “{column}” is unavailable on this list."
            )));
        }
        if setup.roles.len() > crate::shared_views::MAX_ROLES
            || setup.roles.iter().any(|key| {
                !key.strip_prefix("code:")
                    .or_else(|| key.strip_prefix("name:"))
                    .is_some_and(|key| !key.trim().is_empty())
            })
        {
            return Err(Error::Refused(
                "Choose everyone or up to 50 role keys.".into(),
            ));
        }
        let mut roles = Vec::new();
        for key in &setup.roles {
            let id = role_map.get(key.as_str()).copied();
            let code = key.strip_prefix("code:").filter(|code| !code.is_empty());
            let sql = "SELECT id FROM roles WHERE tenant_id=$1 AND ($2::bigint IS NOT NULL AND id=$2 OR $2 IS NULL AND code=$3)";
            let sql = if preview.lock {
                format!("{sql} FOR KEY SHARE")
            } else {
                sql.into()
            };
            let role = tx.query_opt(&sql, &[&scope.tenant(), &id, &code]).await?;
            let Some(role) = role else {
                return Ok(ViewTransfer::Missing(format!(
                    "Choose a destination role for “{key}”."
                )));
            };
            roles.push(role.get(0));
        }
        roles.sort_unstable();
        roles.dedup();
        let audience = if setup.roles.is_empty() {
            Audience::Everyone
        } else {
            Audience::Roles(roles)
        };
        crate::shared_views::prepare_shared(&arrangement, &setup.params, &setup.name, &audience)?;
        if preview.lock {
            lock_page(tx, scope, &setup.page).await?;
        }
        let row = tx
            .query_one(
                "SELECT count(*) FROM base_grid_shared_views WHERE tenant_id=$1 AND page=$2",
                &[&scope.tenant(), &setup.page],
            )
            .await?;
        let count: i64 = row.get(0);
        let current = tx.query_opt(
            &format!("SELECT {SHARED_COLUMNS} FROM base_grid_shared_views v WHERE v.tenant_id=$1 AND v.page=$2 AND lower(v.name)=lower($3)"),
            &[&scope.tenant(), &setup.page, &setup.name.trim()],
        ).await?;
        if let Some(row) = current {
            let current = as_view(&row, &setup.page, scope.user());
            let visible = current.owned
                || match &current.audience {
                    Audience::Everyone => true,
                    Audience::Roles(roles) => {
                        crate::shared_views::has_audience_role(tx, scope, roles).await?
                    }
                };
            return Ok(
                if visible
                    && current.view == arrangement
                    && current.params == setup.params
                    && current.audience == audience
                {
                    ViewTransfer::Same
                } else {
                    ViewTransfer::Collision(
                        "The destination list already has a different view with this name.".into(),
                    )
                },
            );
        }
        if count + preview.pending >= crate::shared_views::MAX_PER_PAGE {
            return Err(Error::Refused(
                "The list would exceed its saved-view limit.".into(),
            ));
        }
        Ok(ViewTransfer::Add(audience))
    }

    pub async fn import_view(
        self: &Arc<Self>,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        setup: &ViewSetup,
        audience: &Audience,
    ) -> Result<()> {
        self.transfer_page(tx.sql(), scope, &setup.page)
            .await?
            .ok_or(Error::NotFound)?;
        self.save_shared(
            tx,
            scope,
            &View::from_json(&setup.view, &setup.page),
            &setup.params,
            &setup.name,
            audience,
        )
        .await?;
        Ok(())
    }
}

/// Only declared setup parameters travel. Free text and dynamic filters can
/// contain account, company or record identifiers and search terms.
fn portable_params(page: &PageMetadata, params: &BTreeMap<String, String>) -> Result<()> {
    let spec = page.spec.map(|spec| spec());
    for (key, value) in params {
        let valid = spec.as_ref().is_some_and(|spec| {
            let Some(key) = key.strip_prefix(&spec.param_prefix) else {
                return false;
            };
            if key == spec.param_names.sort_by {
                spec.sortable.contains_key(value)
            } else if key == spec.param_names.sort_dir {
                matches!(value.as_str(), "asc" | "desc")
            } else if key == spec.page_size_param
                || spec.page_size_aliases.iter().any(|alias| alias == key)
            {
                value
                    .parse()
                    .is_ok_and(|size| spec.page_sizes.contains(&size))
            } else if key == spec.param_names.search {
                value.is_empty()
            } else {
                match spec.filters.get(key) {
                    Some(Filter::OneOf { allowed, .. }) => allowed.contains(value),
                    Some(filter) => value == filter.default_value() && value.is_empty(),
                    None => false,
                }
            }
        });
        if !valid {
            return Err(Error::Refused(format!("The view parameter “{key}” is unavailable or is a search or record filter. Clear it before exporting.")));
        }
    }
    Ok(())
}
