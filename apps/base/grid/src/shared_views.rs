//! Named list views one person saves and shares with the tenant or with
//! roles. A view is [`View`] (columns, lenses, row height, change-since date)
//! plus the page's own query (`params`: its sort, search and filters), so
//! "the list we all look at on Monday" opens the same way for everyone.
//!
//! It stores no rows and grants no access. A reader opens a shared view as a
//! link to the page's own address (`href`), so the page applies the reader's
//! own grants, field restrictions and reach, and a column or filter the
//! reader may not use is pruned like in any shared address. Opening one never
//! changes the reader's own remembered arrangement ([`crate::page_views`]).
//!
//! Who reads a view is one SQL condition, [`VISIBLE`]: its owner, everyone in
//! the tenant when shared that way, or a holder of a role it is shared with
//! (Authz's `effective_principal_roles`, as Reports and Tiling read theirs).
//! Write authorization is enforced by [`PageViews::save_shared`] and
//! [`PageViews::delete_shared`].
//! Every write is audited through the mutation boundary.
use crate::page_views::PageViews;
use crate::view::View;
use bilibi_authz::{CapabilityDefinition, VocabularyContribution, VOCABULARY};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::{ReadClient, Scope};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Share a saved view with everyone or with roles.
pub const SHARE: &str = "grid.views.share";
pub(crate) const ROLE_AUDIENCE_LABEL: &str = "People with a role it is shared with";
/// The audit kind of a saved view.
pub const KIND: &str = "grid.shared_view";
/// Views one page keeps for a tenant, at most.
pub const MAX_PER_PAGE: i64 = 100;
/// Roles one view is shared with, at most.
pub const MAX_ROLES: usize = 50;
/// Characters of a view's name.
pub const MAX_NAME: usize = 80;
/// Maximum bytes of the serialized page query a view keeps.
pub(crate) const MAX_QUERY: usize = 16384;

inventory::submit! { Contribution::new(VOCABULARY, "grid.views.vocabulary",
Provenance { module: "base/grid", source: file!(), line: line!() },
&VocabularyContribution {
    domains: &["grid"],
    verbs: &["share"],
    capabilities: &[CapabilityDefinition::tenant(SHARE)],
    roles: &[],
    system_principals: &[],
}) }

/// `$1` tenant, `$2` reader, `$4` current company; the view row is `v`.
const VISIBLE: &str = "v.tenant_id=$1 AND (v.owner_id=$2 OR v.shared_with_everyone OR EXISTS (SELECT 1 FROM base_grid_shared_view_roles s JOIN effective_principal_roles p ON (p.company_id IS NULL OR p.company_id=$4) AND p.tenant_id=s.tenant_id AND p.role_id=s.role_id WHERE s.tenant_id=v.tenant_id AND s.view_id=v.id AND p.user_id=$2))";

/// Whom a saved view is for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Audience {
    /// Everyone in the tenant.
    Everyone,
    /// The holders of these roles.
    Roles(Vec<i64>),
}

/// A saved view as its reader sees it.
#[derive(Clone, Debug)]
pub struct SharedView {
    pub id: i64,
    /// The reader saved it, so may delete it.
    pub owned: bool,
    pub name: String,
    pub view: View,
    /// The page's own query keys the view keeps (sort, search, filters).
    pub params: BTreeMap<String, String>,
    pub audience: Audience,
}

impl SharedView {
    /// The query that opens the page the way the view was saved: its own
    /// params and the arrangement, with the row height always named so the
    /// address wins over the reader's memory even for a page's own columns.
    pub fn query(&self) -> String {
        let mut pairs: BTreeMap<String, String> = self.params.clone();
        pairs.extend(self.view.to_params());
        pairs.insert("z".into(), self.view.zoom.to_string());
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    }
}

/// The page query a view keeps from the address it was saved at: the page's
/// own keys only. The arrangement is taken from the live view, the page number
/// is dropped so a view opens on its first page. Invalid keys, control
/// characters and oversized queries are refused rather than truncated.
pub(crate) fn clean_params(params: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let kept: BTreeMap<_, _> = params
        .iter()
        .filter(|(key, _)| {
            !crate::view::KEYS.contains(&key.as_str()) && !matches!(key.as_str(), "page" | "after")
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if kept.iter().any(|(key, value)| {
        key.is_empty() || key.chars().any(char::is_control) || value.chars().any(char::is_control)
    }) || serde_json::to_vec(&kept).map_err(|_| Error::Invalid)?.len() > MAX_QUERY
    {
        return Err(refused("The view's filters are too large or invalid."));
    }
    Ok(kept)
}

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && key.len() <= 40
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
}

/// A page is named by the catalog table its rows come from.
fn valid_page(page: &str) -> bool {
    valid_key(page) && page.len() <= 90
}

fn refused(message: &str) -> Error {
    Error::Refused(message.into())
}

pub(crate) fn canonical_arrangement(view: &View) -> View {
    View::from_params(&view.to_params(), &view.table)
}

pub(crate) async fn has_audience_role(
    client: &dyn bilibi_tenancy::ProofClient,
    scope: &Scope,
    roles: &[i64],
) -> Result<bool> {
    if roles.is_empty() {
        return Ok(false);
    }
    Ok(client
        .query_opt(
            "SELECT 1 FROM effective_principal_roles WHERE tenant_id=$1 AND user_id=$2
         AND (company_id IS NULL OR company_id=$3) AND role_id=ANY($4) LIMIT 1",
            &[
                &scope.tenant(),
                &scope.user(),
                &scope.current_company().map(|company| company.get()),
                &roles,
            ],
        )
        .await?
        .is_some())
}

fn as_view(row: &tokio_postgres::Row, page: &str, reader: i64) -> SharedView {
    let stored: Value = row.get(3);
    let roles: Vec<i64> = row.get(5);
    let params = stored
        .get("params")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    SharedView {
        id: row.get(0),
        owned: row.get::<_, i64>(1) == reader,
        name: row.get(2),
        view: canonical_arrangement(&View::from_json(&stored, page)),
        params,
        audience: if row.get(4) {
            Audience::Everyone
        } else {
            Audience::Roles(roles)
        },
    }
}

const SHARED_COLUMNS: &str = "v.id, v.owner_id, v.name, v.view, v.shared_with_everyone, COALESCE((SELECT array_agg(s.role_id ORDER BY s.role_id) FROM base_grid_shared_view_roles s WHERE s.tenant_id=v.tenant_id AND s.view_id=v.id), '{}')";

impl PageViews {
    /// The views of `page` this reader may open: their own, those shared with
    /// everyone and those shared with a role they hold, by name.
    pub async fn shared(&self, scope: &Scope, page: &str) -> Result<Vec<SharedView>> {
        let client = self.db().get().await?;
        self.shared_on(&**client, scope, page, None).await
    }

    /// The same visibility rule on the caller's connection, including inside a write.
    pub(crate) async fn shared_on(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        page: &str,
        id: Option<i64>,
    ) -> Result<Vec<SharedView>> {
        let rows = client.query(
            &format!("SELECT {SHARED_COLUMNS} FROM base_grid_shared_views v WHERE {VISIBLE} AND v.page=$3 AND ($5::bigint IS NULL OR v.id=$5) ORDER BY lower(v.name), v.id LIMIT {MAX_PER_PAGE}"),
            &[&scope.tenant(), &scope.user(), &page, &scope.current_company().map(|company| company.get()), &id],
        ).await?;
        Ok(rows
            .iter()
            .map(|row| as_view(row, page, scope.user()))
            .collect())
    }

    pub(crate) async fn shared_for_pages(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        pages: &[&str],
    ) -> Result<Vec<(String, SharedView)>> {
        let rows = client.query(
            &format!("SELECT {SHARED_COLUMNS}, v.page FROM base_grid_shared_views v WHERE {VISIBLE} AND v.page = ANY($3) ORDER BY v.page, lower(v.name), v.id"),
            &[&scope.tenant(), &scope.user(), &pages, &scope.current_company().map(|company| company.get())],
        ).await?;
        Ok(rows
            .iter()
            .map(|row| {
                let page: String = row.get(6);
                let view = as_view(row, &page, scope.user());
                (page, view)
            })
            .collect())
    }

    pub(crate) async fn full_pages(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        pages: &[&str],
    ) -> Result<BTreeSet<String>> {
        let rows = client
            .query(
                "SELECT page FROM base_grid_shared_views WHERE tenant_id=$1 AND page=ANY($2) GROUP BY page HAVING count(*) >= $3",
                &[&scope.tenant(), &pages, &MAX_PER_PAGE],
            )
            .await?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }

    /// The tenant's roles by name, for choosing whom to share with; `None`
    /// when the reader may not share (they see no role names).
    pub async fn share_roles(&self, scope: &Scope) -> Result<Option<Vec<(i64, String)>>> {
        let client = self.db().get().await?;
        match bilibi_authz::authorize_key(&**client, scope, SHARE).await {
            Ok(()) => {}
            Err(Error::Forbidden) => return Ok(None),
            Err(error) => return Err(error),
        }
        let rows = client
            .query(
                "SELECT id, name FROM roles WHERE tenant_id=$1 ORDER BY lower(name), id LIMIT 500",
                &[&scope.tenant()],
            )
            .await?;
        Ok(Some(
            rows.iter().map(|row| (row.get(0), row.get(1))).collect(),
        ))
    }

    /// Saves `view` and the page's `params` under `name` for `audience`.
    /// Saving needs [`SHARE`]. A taken name, too
    /// many views or an unknown role is [`Error::Refused`] with its reason.
    pub async fn save_shared(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        view: &View,
        params: &BTreeMap<String, String>,
        name: &str,
        audience: &Audience,
    ) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control)
        {
            return Err(refused("Give the view a name of up to 80 characters."));
        }
        if !valid_page(&view.table) {
            return Err(Error::Invalid);
        }
        let mut roles = match audience {
            Audience::Roles(roles) => roles.clone(),
            _ => Vec::new(),
        };
        roles.sort_unstable();
        roles.dedup();
        if matches!(audience, Audience::Roles(_)) && roles.is_empty() {
            return Err(refused("Choose at least one role."));
        }
        if roles.len() > MAX_ROLES {
            return Err(refused("Share a view with at most 50 roles."));
        }
        let mut stored = canonical_arrangement(view).to_json();
        stored["params"] = json!(clean_params(params)?);
        if serde_json::to_vec(&stored)
            .map_err(|_| Error::Invalid)?
            .len()
            > 8000
        {
            return Err(refused(
                "The saved view is too large; use fewer columns or filters.",
            ));
        }
        let everyone = *audience == Audience::Everyone;
        let (tenant, owner, page) = (scope.tenant(), scope.user(), view.table.clone());
        bilibi_authz::authorize_key(tx.sql(), scope, SHARE).await?;
        tx.sql().execute(
            "SELECT pg_advisory_xact_lock(hashtextextended('base-grid-shared-views-' || $1::text || ':' || $2, 0))",
            &[&tenant.to_string(), &page],
        ).await?;
        let taken: i64 = tx
            .sql()
            .query_one(
                "SELECT count(*) FILTER (WHERE lower(name)=lower($3)) FROM base_grid_shared_views WHERE tenant_id=$1 AND page=$2",
                &[&tenant, &page, &name],
            )
            .await?
            .get(0);
        if taken > 0 {
            return Err(Error::Refused(format!(
                "A view called “{name}” already exists on this list."
            )));
        }
        if self
            .full_pages(tx.sql(), scope, &[&page])
            .await?
            .contains(&page)
        {
            return Err(refused(
                "This list already has 100 saved views; delete one first.",
            ));
        }
        let known: Vec<i64> = tx
            .sql()
            .query(
                "SELECT id FROM roles WHERE tenant_id=$1 AND id=ANY($2) ORDER BY id",
                &[&tenant, &roles],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        if known != roles {
            return Err(refused("Choose a role that exists."));
        }
        let name = name.to_owned();
        let id = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let id: i64 = sql
                        .query_one(
                            "INSERT INTO base_grid_shared_views(tenant_id,owner_id,page,name,view,shared_with_everyone) VALUES($1,$2,$3,$4,$5,$6) RETURNING id",
                            &[&tenant, &owner, &page, &name, &stored, &everyone],
                        )
                        .await?
                        .get(0);
                    sql.execute(
                        "INSERT INTO base_grid_shared_view_roles(tenant_id,view_id,role_id) SELECT $1,$2,role FROM unnest($3::bigint[]) role",
                        &[&tenant, &id, &roles],
                    )
                    .await?;
                    let new = json!({"owner_id": owner, "page": page, "name": name, "view": stored, "shared_with_everyone": everyone, "roles": roles});
                    Ok((id, vec![Change { kind: KIND.into(), id, old: Value::Null, new }]))
                })
            })
            .await?;
        Ok(id)
    }

    /// Deletes the reader's own view, or another owner's with [`SHARE`];
    /// the people it was shared with lose it.
    pub async fn delete_shared(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
    ) -> Result<()> {
        let (tenant, owner) = (scope.tenant(), scope.user());
        let row = tx
            .sql()
            .query_opt(
                "SELECT page, name, view, shared_with_everyone, owner_id FROM base_grid_shared_views WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
                &[&tenant, &id],
            )
            .await?
            .ok_or(Error::NotFound)?;
        if row.get::<_, i64>(4) != owner {
            bilibi_authz::authorize_key(tx.sql(), scope, SHARE).await?;
        }
        let roles: Vec<i64> = tx.sql().query_one(
            "SELECT ARRAY(SELECT role_id FROM base_grid_shared_view_roles WHERE tenant_id=$1 AND view_id=$2 ORDER BY role_id)",
            &[&tenant, &id],
        ).await?.get(0);
        let old = json!({"page": row.get::<_, String>(0), "name": row.get::<_, String>(1),
            "view": row.get::<_, Value>(2), "shared_with_everyone": row.get::<_, bool>(3),
            "owner_id": row.get::<_, i64>(4), "roles": roles});
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "DELETE FROM base_grid_shared_views WHERE tenant_id=$1 AND id=$2",
                    &[&tenant, &id],
                )
                .await?;
                Ok((
                    (),
                    vec![Change {
                        kind: KIND.into(),
                        id,
                        old,
                        new: Value::Null,
                    }],
                ))
            })
        })
        .await?;
        Ok(())
    }
}
