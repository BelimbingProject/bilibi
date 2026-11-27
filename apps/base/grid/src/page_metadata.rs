//! Page-owned query contracts for agent discovery. ListSpec remains the source
//! used by the page to parse its query; this contribution makes it discoverable.
use bilibi_module_registry::{Consumer, Entry, RegistryError};
use bilibi_tenancy::Scope;
use bilibi_ui::ListSpec;
use serde_json::Value;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
};
use tokio_postgres::Client;

pub const PAGE_METADATA: &str = "base/grid.page_metadata";

pub type PageDetailsFuture<'a> =
    Pin<Box<dyn Future<Output = bilibi_database::Result<Vec<Value>>> + Send + 'a>>;
pub type PageColumnsFuture<'a> =
    Pin<Box<dyn Future<Output = bilibi_database::Result<crate::PageColumns>> + Send + 'a>>;
pub type PageColumnsFactory =
    for<'a> fn(&'a Client, &'a Scope, Arc<crate::PageViews>) -> PageColumnsFuture<'a>;
pub type PageDetails = for<'a> fn(&'a Client, &'a Scope) -> PageDetailsFuture<'a>;

pub struct PageMetadata {
    pub page: &'static str,
    pub path: Option<&'static str>,
    pub capability: &'static str,
    pub spec: Option<fn() -> Arc<ListSpec>>,
    /// Additional query keys owned outside ListState, with an explanation.
    pub extra_filters: &'static [(&'static str, &'static str)],
    pub details: Option<PageDetails>,
    pub columns: PageColumnsFactory,
}

fn validate(entries: &[Entry]) -> bilibi_module_registry::Result<()> {
    let mut pages = std::collections::BTreeSet::new();
    for entry in entries {
        let page = entry.get::<PageMetadata>()?;
        if page.page.is_empty() || !pages.insert(page.page) {
            return Err(RegistryError::Contribution(format!(
                "duplicate or empty grid page {}",
                page.page
            )));
        }
        if let Some(spec) = page.spec {
            spec()
                .validate()
                .map_err(|e| RegistryError::Contribution(e.to_string()))?;
        }
    }
    Ok(())
}
inventory::submit! { Consumer::new::<PageMetadata>(PAGE_METADATA, "base/grid", validate) }

/// Every registered list page.
pub(crate) fn pages() -> bilibi_module_registry::Result<&'static [&'static PageMetadata]> {
    static PAGES: OnceLock<bilibi_module_registry::Result<Vec<&'static PageMetadata>>> =
        OnceLock::new();
    PAGES
        .get_or_init(|| {
            let catalog =
                bilibi_module_registry::Catalog::discover(&bilibi_module_registry::discover()?)?;
            catalog
                .entries::<PageMetadata>(PAGE_METADATA)?
                .iter()
                .map(|entry| entry.get())
                .collect()
        })
        .as_deref()
        .map_err(|e| RegistryError::Contribution(e.to_string()))
}

pub(crate) fn metadata(
    page: &str,
) -> bilibi_module_registry::Result<Option<&'static PageMetadata>> {
    Ok(pages()?.iter().copied().find(|entry| entry.page == page))
}

#[macro_export]
macro_rules! contribute_page_metadata {
    ($module:literal, $id:literal, $page:expr, $path:expr, $cap:expr, $spec:expr, $extra:expr, $details:expr, $columns:expr) => {
        $crate::inventory::submit! {
            $crate::Contribution::new($crate::PAGE_METADATA, $id,
                $crate::Provenance { module: $module, source: file!(), line: line!() },
                &$crate::PageMetadata { page: $page, path: $path, capability: $cap, spec: $spec, extra_filters: $extra, details: $details, columns: $columns })
        }
    };
}
