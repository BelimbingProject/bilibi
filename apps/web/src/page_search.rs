//! Page navigation results shared by the top bar and agent discovery.
use crate::App;
use bilibi_base::{authz, menu, tenancy};
use bilibi_database::{Error, Result};
use bilibi_module_registry::contributions::{Access, Method, PageSection, Route, Viewer};
use bilibi_tenancy::Scope;
use bilibi_ui::SearchPage;
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub(crate) fn visible_menu(
    app: &App,
    capabilities: &BTreeSet<&str>,
    owner: bool,
) -> Vec<menu::Node> {
    app.menu.visible_tree(&|key| {
        if key == menu::OWNER_STANDING {
            owner
        } else {
            capabilities.contains(key)
        }
    })
}

/// The address of Company's operator setup page, which the status bar links to
/// while the operator stands on no primary company (`shell.html`).
const OPERATOR_SETUP: &str = "/setup/platform-operator";

/// The pages the shell's search lists: each page route the person `may` open,
/// named by its title, else its menu row, and placed where it lives: at its
/// menu row, else under the deepest row its address sits beneath. A personal
/// settings page takes its tab's label under `SETTINGS_LOCATION`. A route with
/// a path parameter and a page with no name are left out. A route that only
/// redirects is listed under its title when the person may also open where it
/// leads (Create Contact opens the Contacts list's dialog), so an old address
/// that redirects returns no title.
pub(crate) fn pages(
    app: &App,
    tree: &[menu::Node],
    viewer: Viewer<'_>,
    manage_instance: bool,
    operator_company_missing: bool,
) -> Vec<SearchPage> {
    let may = |route: &Route| {
        if route.path == OPERATOR_SETUP && !operator_company_missing {
            return false;
        }
        match route.access.base() {
            Access::Public => false,
            Access::OperatorCapability(bilibi_base::system::INSTANCE_MANAGE) => manage_instance,
            access => access.admits(&viewer),
        }
    };
    let mut rows = Vec::new();
    menu_rows(tree, &[], &mut rows);
    let settings = bilibi_ui::SettingsTab::pages();
    let mut pages = Vec::new();
    let mut account = Vec::new();
    for bound in app.routes.staff.iter() {
        let route = bound.route;
        if route.method != Method::Get || route.path.contains('{') || !may(route) {
            continue;
        }
        if let Some(target) = bound.page.0.redirect(route.operation) {
            let path = target.split('?').next().unwrap_or(target);
            match crate::routes::resolve(app, path) {
                Ok((to, _)) if may(to.route) => {}
                _ => continue,
            }
        }
        if let Some(at) = settings.iter().position(|(_, href)| *href == route.path) {
            let label = settings[at].0;
            let page = SearchPage {
                label: label.to_owned(),
                location: bilibi_ui::SETTINGS_LOCATION
                    .iter()
                    .chain([&label])
                    .map(|part| (*part).to_owned())
                    .collect(),
                href: route.path.to_owned(),
                account: true,
                section_keywords: None,
            };
            account.push((at, search_rows(page, &bound.sections)));
            continue;
        }
        let row = rows.iter().find(|(path, _)| *path == route.path);
        let Some(label) = bound
            .page
            .0
            .title(route.operation)
            .or(row.and_then(|(_, labels)| labels.last().copied()))
        else {
            continue;
        };
        let location: Vec<&str> = match row {
            Some((_, labels)) => labels.clone(),
            None => rows
                .iter()
                .filter(|(path, _)| {
                    *path != "/"
                        && route
                            .path
                            .strip_prefix(path)
                            .is_some_and(|rest| rest.starts_with('/'))
                })
                .max_by_key(|(path, _)| path.len())
                .map(|(_, labels)| labels.iter().copied().chain([label]).collect())
                .unwrap_or_default(),
        };
        pages.extend(search_rows(
            SearchPage {
                label: label.to_owned(),
                location: location.into_iter().map(str::to_owned).collect(),
                href: route.path.to_owned(),
                account: false,
                section_keywords: None,
            },
            &bound.sections,
        ));
    }
    // Sections read alphabetically, as the menu does; settings in tab order.
    pages.sort_by_cached_key(|page| {
        let path = if page.location.is_empty() {
            page.label.to_lowercase()
        } else {
            page.path().to_lowercase()
        };
        (
            path,
            page.section_keywords.is_some(),
            page.label.to_lowercase(),
        )
    });
    account.sort_by_key(|(at, _)| *at);
    pages.extend(account.into_iter().flat_map(|(_, rows)| rows));
    pages
}

fn search_rows(page: SearchPage, sections: &[&PageSection]) -> Vec<SearchPage> {
    let mut rows = Vec::with_capacity(sections.len() + 1);
    for section in sections {
        let query = section
            .query
            .map_or_else(String::new, |query| format!("?{query}"));
        let location = if page.location.is_empty() {
            vec![page.label.clone()]
        } else {
            page.location.clone()
        };
        rows.push(SearchPage {
            label: section.title.to_owned(),
            location,
            href: format!("{}{query}#{}", page.href, section.anchor),
            account: page.account,
            section_keywords: Some(section.keywords),
        });
    }
    rows.sort_by_cached_key(|row| row.label.to_lowercase());
    rows.insert(0, page);
    rows
}

/// Each visible menu row that opens a page, with the labels from its section
/// down to it.
fn menu_rows(
    nodes: &[menu::Node],
    above: &[&'static str],
    rows: &mut Vec<(&'static str, Vec<&'static str>)>,
) {
    for node in nodes {
        let item = node.item.item;
        let mut labels = above.to_vec();
        labels.push(item.label);
        if let Some(route) = item.route {
            rows.push((route, labels.clone()));
        }
        menu_rows(&node.children, &labels, rows);
    }
}

/// Agent discovery reads the same authorized page list with a bounded result set.
pub(crate) async fn hits(
    app: &App,
    scope: &Scope,
    words: &str,
    limit: usize,
) -> Result<Vec<Value>> {
    let query = words.trim().to_lowercase();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let client = app.db.get().await?;
    let capabilities = authz::effective_capabilities(&**client, scope).await?;
    let owner = match tenancy::prove_owner(&**client, scope).await {
        Ok(_) => true,
        Err(Error::Forbidden) => false,
        Err(error) => return Err(error),
    };
    let operator = match tenancy::prove_operator(&**client, scope).await {
        Ok(_) => true,
        Err(Error::Forbidden) => false,
        Err(error) => return Err(error),
    };
    let operator_company_missing = operator
        && matches!(
            bilibi_company::platform_operator_company(&**client).await,
            Ok(None)
        );
    drop(client);
    let manage_instance = operator
        && capabilities.contains(bilibi_base::system::INSTANCE_MANAGE)
        && app
            .services
            .get::<bilibi_base::system::Instances>(bilibi_base::system::SERVICE)?
            .is_core_administrator(scope)
            .await?;
    let tree = visible_menu(app, &capabilities, owner);
    let viewer = Viewer {
        capabilities: &capabilities,
        operator,
        owner,
    };
    let origin = crate::api::origin(app);
    Ok(pages(
        app,
        &tree,
        viewer,
        manage_instance,
        operator_company_missing,
    )
    .into_iter()
    .filter(|page| {
        page.labels()
            .to_lowercase()
            .lines()
            .any(|label| label.contains(&query))
    })
    .take(limit)
    .map(|page| {
        json!({
            "title": page.label,
            "page_path": page.path(),
            "section": page.section_keywords.is_some(),
            "path": page.href,
            "url": format!("{origin}{}", page.href),
        })
    })
    .collect())
}
