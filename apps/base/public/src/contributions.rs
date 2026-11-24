//! What a module serves on the public site, and the operator's listener
//! budget. Pages and links are the only public routes a module declares;
//! each names its sender budget, so no public route is unthrottled.
use crate::budget::SenderBudget;
use bilibi_module_registry::{
    contributions::{in_portal, Method},
    Consumer, Contribution, Entry, Provenance, RegistryError, Result,
};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, ValueType, DEFINITIONS};
use serde_json::json;
use std::collections::BTreeSet;

/// Anonymous pages: `Contribution::new(PUBLIC_PAGES, id, provenance, &PublicPage { .. })`.
pub const PUBLIC_PAGES: &str = "base/public.pages";
/// Token links: `Contribution::new(PUBLIC_LINKS, id, provenance, &PublicLink { .. })`.
pub const PUBLIC_LINKS: &str = "base/public.links";

/// A GET page anyone may open on the public site: a form, a survey, a
/// preference page. The service resolves to [`crate::PageService`]; the
/// owner's page receives the site's [`crate::PublicScope`] and nothing about
/// the sender. A POST from such a page goes to a [`PublicLink`] at the same
/// path when its token is the credential.
#[derive(Clone, Copy, Debug)]
pub struct PublicPage {
    pub path: &'static str,
    pub service: &'static str,
    pub operation: &'static str,
    /// What one sender may ask of this page on one server.
    pub budget: SenderBudget,
}

/// A token link a mail client, image proxy or mail provider calls with no
/// session, answered outside any layout with bytes, a checked redirect or an
/// empty 204 ([`crate::link`]). GET or POST; the path carries `{token}`,
/// which the owning module also declares under `CREDENTIAL_PATHS`, so it
/// never reaches a log, debug report or referrer. The host reads no cookie,
/// `Origin` or CSRF secret for it: a provider's one-click POST carries none.
#[derive(Clone, Copy, Debug)]
pub struct PublicLink {
    pub method: Method,
    pub path: &'static str,
    pub service: &'static str,
    pub operation: &'static str,
    /// What one sender may ask of this link on one server. Mail providers
    /// and image proxies fetch for many readers from few addresses: size it
    /// for them.
    pub budget: SenderBudget,
}

fn invalid(message: String) -> RegistryError {
    RegistryError::Contribution(message)
}
fn at(entry: &Entry) -> String {
    format!(
        "{} at {}:{}",
        entry.id, entry.provenance.source, entry.provenance.line
    )
}
pub(crate) fn parameters(path: &str) -> impl Iterator<Item = &str> {
    path.split('/')
        .filter_map(|part| part.strip_prefix('{').and_then(|p| p.strip_suffix('}')))
}
/// Parameter names do not distinguish route patterns.
fn normalized(path: &str) -> String {
    path.split('/')
        .map(|part| if part.starts_with('{') { "{}" } else { part })
        .collect::<Vec<_>>()
        .join("/")
}
/// The shape every public route shares: a path of its own outside the
/// portal, an operation and a usable budget.
fn shaped(entry: &Entry, path: &str, operation: &str, budget: SenderBudget) -> Result<()> {
    if !path.starts_with('/') || path.contains(['?', '#']) || operation.is_empty() {
        return Err(invalid(format!("public route {} is malformed", at(entry))));
    }
    if in_portal(path) {
        return Err(invalid(format!(
            "public route {} claims the portal path {path}",
            at(entry)
        )));
    }
    if !budget.valid() {
        return Err(invalid(format!(
            "public route {} needs a budget with positive limits and no more a minute than an hour",
            at(entry)
        )));
    }
    Ok(())
}

fn validate_pages(entries: &[Entry]) -> Result<()> {
    let mut paths = BTreeSet::new();
    for entry in entries {
        let page = entry.get::<PublicPage>()?;
        shaped(entry, page.path, page.operation, page.budget)?;
        if !paths.insert(normalized(page.path)) {
            return Err(invalid(format!("duplicate public page {}", page.path)));
        }
    }
    Ok(())
}
fn validate_links(entries: &[Entry]) -> Result<()> {
    let mut paths = BTreeSet::new();
    for entry in entries {
        let link = entry.get::<PublicLink>()?;
        shaped(entry, link.path, link.operation, link.budget)?;
        if !matches!(link.method, Method::Get | Method::Post)
            || !parameters(link.path).any(|name| name == "token")
        {
            return Err(invalid(format!(
                "public link {} must be a GET or POST whose path carries {{token}}",
                at(entry)
            )));
        }
        if !paths.insert((link.method, normalized(link.path))) {
            return Err(invalid(format!("duplicate public link {}", link.path)));
        }
    }
    Ok(())
}
inventory::submit! { Consumer::new::<PublicPage>(PUBLIC_PAGES, "base/public", validate_pages) }
inventory::submit! { Consumer::new::<PublicLink>(PUBLIC_LINKS, "base/public", validate_links) }

/// The listener budget: requests one sender may make of the whole public
/// site, every page, link, asset, portal page and webhook together, on one
/// server. The coarse guard before any route's own budget.
pub const SENDER_PER_MINUTE: &str = "public.sender_per_minute";
pub const SENDER_PER_HOUR: &str = "public.sender_per_hour";

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/public",
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! { Contribution::new(DEFINITIONS, SENDER_PER_MINUTE, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global], default: || json!(1_200), nullable: false, encrypted: false,
    minimum: Some(1.0), maximum: Some(1_000_000.0), validator: None,
    editor: Some(Editor { group: "Public site", label: "Public site requests per sender a minute",
        help: "Most requests one address may make of the whole public site in a minute, on each server: pages, links, images and scripts together. Past it the site answers 429. Mail services and office networks send many people's requests from one address, so keep it high.",
        capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, SENDER_PER_HOUR, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global], default: || json!(20_000), nullable: false, encrypted: false,
    minimum: Some(1.0), maximum: Some(100_000_000.0), validator: None,
    editor: Some(Editor { group: "Public site", label: "Public site requests per sender an hour",
        help: "Most requests one address may make of the whole public site in an hour, on each server. A limit a minute above this one never takes effect.",
        capability: None, reveal_duration_ms: 5_000 }),
})) }
