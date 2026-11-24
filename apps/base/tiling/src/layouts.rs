//! Company workspaces.
//!
//! Published trees live in company settings and are filtered by role. Every
//! tile still checks the viewer's own access to its page.
use crate::layout::Layout;
use bilibi_database::{Error, Result};
use bilibi_module_registry::contributions::{Method, Route};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{
    Definition, ScopeKind, Setting, Settings, Target, Validator, ValueType, DEFINITIONS,
};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};
use tokio_postgres::GenericClient;

pub const SHARED: &str = "ui.workspace.shared_layouts";
/// The capability that publishes, deletes and manages a company's shared
/// layouts; the route of the shared-workspaces screen requires it too.
pub const PUBLISH: &str = "ui.workspace.publish";
pub const MAX_LABEL: usize = 60;
const MAX_ENTRIES: usize = 100;
const MAX_TREE: usize = 2048;
/// Slugs the workspace's own routes take, so a shared layout never shadows them.
const RESERVED_SLUGS: &[&str] = &["shared-layouts", "shared"];

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/tiling",
            source: file!(),
            line: line!(),
        }
    };
}

/// One shared layout as stored.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Entry {
    pub slug: String,
    pub label: String,
    pub tree: String,
    /// Role codes a shared layout is limited to; empty is the whole company.
    pub roles: Vec<String>,
}
impl Entry {
    fn read(value: &Value) -> Option<Self> {
        let slug = value.get("slug")?.as_str()?;
        let label = value.get("label")?.as_str()?;
        let tree = clean_tree(value.get("tree")?.as_str()?)?;
        if !valid_slug(slug) || clean_label(label).is_none() {
            return None;
        }
        let roles = match value.get("roles") {
            None => Vec::new(),
            Some(roles) => roles
                .as_array()?
                .iter()
                .map(|role| role.as_str().filter(|code| valid_role(code)))
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .map(str::to_owned)
                .collect(),
        };
        Some(Self {
            slug: slug.to_owned(),
            label: label.to_owned(),
            tree,
            roles,
        })
    }
}

/// A role an audience can be limited to: its display name and the code
/// stored in a shared layout.
#[derive(Clone, Debug, PartialEq)]
pub struct RoleOption {
    pub name: String,
    pub code: String,
}

pub struct Layouts {
    pub(crate) settings: Arc<Settings>,
    pub(crate) routes: Vec<(&'static str, &'static Route)>,
}

impl Layouts {
    pub fn new(settings: Arc<Settings>, routes: Vec<(&'static str, &'static Route)>) -> Self {
        Self { settings, routes }
    }

    /// The discovered GET page serving `path`, literal segments before
    /// parameters, as Web resolves it.
    pub(crate) fn route(&self, path: &str) -> Option<&'static Route> {
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let segments: Vec<_> = path.split('/').collect();
        self.routes
            .iter()
            .map(|(_, route)| *route)
            .filter(|route| route.method == Method::Get && !route.access.portal())
            .filter_map(|route| {
                let pattern: Vec<_> = route.path.split('/').collect();
                if pattern.len() != segments.len() {
                    return None;
                }
                let mut literal = 0;
                for (pattern, value) in pattern.iter().zip(&segments) {
                    if pattern.starts_with('{') {
                        if value.is_empty()
                            || matches!(
                                value.to_ascii_lowercase().as_str(),
                                "." | ".." | "%2e" | ".%2e" | "%2e." | "%2e%2e"
                            )
                        {
                            return None;
                        }
                    } else if pattern == value {
                        literal += 1;
                    } else {
                        return None;
                    }
                }
                Some((literal, route))
            })
            .max_by_key(|(literal, _)| *literal)
            .map(|(_, route)| route)
    }

    // ---- The company's shared layouts ---------------------------------

    /// Every layout published for the actor's company.
    pub async fn shared(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Vec<Entry>> {
        let value = self
            .settings
            .get(client, scope, shared_target(scope)?, SHARED)
            .await?
            .value;
        Ok(entries(&value))
    }

    /// The shared layouts the actor may see: those for the whole company and
    /// those limited to a role the actor holds.
    pub async fn shared_visible(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Vec<Entry>> {
        let held = role_codes(client, scope).await?;
        Ok(self
            .shared(client, scope)
            .await?
            .into_iter()
            .filter(|entry| {
                entry.roles.is_empty() || entry.roles.iter().any(|code| held.contains(code))
            })
            .collect())
    }
}

/// Where the current company's shared layouts live.
pub(crate) fn shared_target(scope: &Scope) -> Result<Target> {
    scope
        .current_company()
        .map(Target::Company)
        .ok_or(Error::Forbidden)
}

/// The role codes the actor holds, which decide the shared layouts they see.
/// A role without a code (a tenant-defined role) is known by its name when that is a
/// valid code.
pub async fn role_codes(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
) -> Result<Vec<String>> {
    Ok(client
        .query(
            "SELECT DISTINCT COALESCE(r.code,r.name) FROM authz_roles_for($1,$2,$3) p JOIN roles r ON r.tenant_id=p.tenant_id AND r.id=p.role_id WHERE p.tenant_id=$1 AND p.user_id=$2",
            &[&scope.tenant(), &scope.user(), &scope.current_company().map(|id|id.get())],
        )
        .await?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .filter(|code| valid_role(code))
        .collect())
}

/// The roles a published layout may be limited to, by name.
pub async fn role_options(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
) -> Result<Vec<RoleOption>> {
    Ok(client
        .query(
            "SELECT r.name,COALESCE(r.code,r.name) FROM roles r WHERE r.tenant_id=$1 ORDER BY lower(r.name),r.id",
            &[&scope.tenant()],
        )
        .await?
        .iter()
        .map(|row| RoleOption {
            name: row.get(0),
            code: row.get(1),
        })
        .filter(|role| valid_role(&role.code))
        .collect())
}

fn entries(value: &Value) -> Vec<Entry> {
    value
        .as_array()
        .map(|entries| entries.iter().filter_map(Entry::read).collect())
        .unwrap_or_default()
}

/// The URL slug for a label: lower-case words joined by hyphens.
fn slug(label: &str) -> String {
    let mut slug = String::new();
    for character in label.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "layout".to_owned()
    } else {
        slug.to_owned()
    }
}

pub(crate) fn publish_slug(label: &str, current: Option<&Entry>, entries: &[Entry]) -> String {
    current.map_or_else(
        || unique_slug(&slug(label), entries),
        |entry| entry.slug.clone(),
    )
}

/// The first of `base`, `base-2`, `base-3`, ... that no entry and no workspace
/// route uses.
fn unique_slug(base: &str, entries: &[Entry]) -> String {
    let taken = |candidate: &str| {
        RESERVED_SLUGS.contains(&candidate) || entries.iter().any(|entry| entry.slug == candidate)
    };
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .expect("an unused numbered slug")
}

pub(crate) fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 90
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A label with whitespace collapsed, when it is one to 60 bytes.
pub fn clean_label(label: &str) -> Option<String> {
    let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() || label.len() > MAX_LABEL {
        return None;
    }
    Some(label)
}

/// The encoded tree, when it decodes to at least one tile.
pub fn clean_tree(tree: &str) -> Option<String> {
    if tree.len() > MAX_TREE {
        return None;
    }
    Layout::decode(tree)
        .filter(|layout| !layout.is_empty())
        .map(|layout| layout.encode())
        .filter(|encoded| encoded.len() <= MAX_TREE)
}

pub fn valid_role(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 100
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

// ---- Contributions -----------------------------------------------------

/// A stored list is a bounded array of complete entries, each decoding to at
/// least one tile; an entry Settings would refuse never reaches storage.
pub(crate) fn valid_entries(value: &Value) -> bool {
    value.as_array().is_some_and(|entries| {
        entries.len() <= MAX_ENTRIES
            && entries.iter().all(|entry| Entry::read(entry).is_some())
            && entries
                .iter()
                .filter_map(|entry| entry.get("slug").and_then(Value::as_str))
                .collect::<BTreeSet<_>>()
                .len()
                == entries.len()
    })
}

inventory::submit! { Contribution::new(DEFINITIONS, SHARED, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Array, scopes: &[ScopeKind::Company], default: || json!([]),
    nullable: false, encrypted: false, minimum: None, maximum: None,
    validator: Some(Validator { accepts: valid_entries, message: "must be a bounded list of shared workspace layouts with unique slugs and role codes" }),
    editor: None,
})) }
inventory::submit! { Contribution::new(bilibi_authz::VOCABULARY, "base/tiling", provenance!(), &bilibi_authz::VocabularyContribution {
    domains: &["ui"],
    verbs: &["publish"],
    capabilities: &[bilibi_authz::CapabilityDefinition::tenant(PUBLISH)],
    roles: &[],
    system_principals: &[],
}) }
// The workspace itself has no menu entry; the sidebar's tile control opens it.
// Only the shared-layouts screen is listed, under System.
bilibi_menu::contribute!(
    "base/tiling",
    bilibi_menu::MenuItem::page(
        "admin.system.workspace.shared",
        "Shared workspaces",
        "/workspace/shared-layouts"
    )
    .company_scoped()
    .under("admin.system")
    .icon("squares-2x2")
    .requires(bilibi_menu::Requirement::Capability(PUBLISH))
    .order(50)
);
