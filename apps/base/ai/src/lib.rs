//! One-shot model calls through configured providers (ADR 34). A module
//! sends instructions plus text to a named provider, or the default one,
//! through one facade, [`Ai`], and gets text back or a plain
//! [`Answer::Unavailable`]. Providers are settings, not code: any
//! OpenAI-compatible server, called through `base/integration`. AI is off by
//! default at installation and company level, every call keeps a deadline,
//! and each call leaves one metadata row, never any text. There are no agent
//! loops, tools or streaming here.
//!
//! `web` is the operator's AI Interactions list and record pages
//! (`/system/ai-interactions`).
mod contributions;
mod gateway;
mod providers;
mod retention;
mod store;
mod web;

pub use contributions::{
    COMPANY_ENABLED, ENABLED, LIST, PROVIDERS, PROVIDER_KEYS, RETENTION_DAYS, TIMEOUT_SECONDS,
};
pub use gateway::{
    ANSWER_CHARACTERS_MAX, CUT_OFF_GRACE, INSTRUCTIONS_CHARACTERS_MAX, TEXT_CHARACTERS_MAX,
};
pub use retention::WORKER as RETENTION_WORKER;
pub use store::{Filter, Interaction, Outcome, Page, LIST_CAP, PAGE_SIZES};

use bilibi_database::{Db, Error, Result};
use bilibi_integration::Integration;
use bilibi_module_registry::{
    contributions::{
        Access, CompanyScope, Method, PageWrites, Pending, Route, PAGE_WRITES, ROUTES,
    },
    Contribution, Module, Provenance, Service,
};
use bilibi_settings::Settings;
use bilibi_tenancy::Scope;
use std::{sync::Arc, time::Duration};
use tokio_postgres::GenericClient;

pub const MODULE: &str = "base/ai";
pub const SERVICE: &str = "base/ai.gateway";
const PAGES: &str = "base/ai.pages";

/// One call. `module` is the calling module's registered id, kept on the
/// row; `provider` names a configured provider, or `None` for the default
/// (the first listed). The text travels as the user message and the
/// instructions as the system message; neither is stored.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub module: &'a str,
    pub provider: Option<&'a str>,
    pub instructions: &'a str,
    pub text: &'a str,
    pub deadline: Duration,
}

/// Why no text came back. `Disabled` and `NotConfigured` are known before
/// any call; `Timeout` and `ProviderError` only after one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    /// AI is off for the installation or the scope's company, or the scope
    /// has no company.
    Disabled,
    /// No provider is configured, or none by the requested name.
    NotConfigured,
    Timeout,
    /// The provider answered with an HTTP error, a malformed or empty reply,
    /// or could not be reached.
    ProviderError,
}
impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NotConfigured => "not_configured",
            Self::Timeout => "timeout",
            Self::ProviderError => "provider_error",
        }
    }
}

/// Untrusted text from a model. Render it escaped, and never act on it
/// without a person's reviewed write through the owner's contract.
#[derive(Clone, Debug)]
pub struct Generated {
    pub text: String,
    pub provider: String,
    pub model: String,
    pub interaction_id: String,
}

#[derive(Clone, Debug)]
pub enum Answer {
    Generated(Generated),
    Unavailable { reason: Reason },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Availability {
    Available { provider: String, model: String },
    Unavailable(Reason),
}

/// The service (`base/ai.gateway`).
pub struct Ai {
    db: Db,
    settings: Arc<Settings>,
    integration: Arc<Integration>,
}
impl Ai {
    /// The newest interactions this scope may see, behind [`LIST`]: its own
    /// tenant's, or every tenant's for the platform operator.
    pub async fn interactions(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        filter: &Filter,
        page: i64,
        page_size: i64,
    ) -> Result<Page> {
        store::list(client, scope, filter, page, page_size).await
    }

    /// One interaction this scope may see, behind [`LIST`]. Another tenant's
    /// row is `NotFound`.
    pub async fn interaction(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        id: &str,
    ) -> Result<Interaction> {
        store::get(client, scope, id).await
    }
}

inventory::submit! {
    Module { id: MODULE, layer: bilibi_module_registry::Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/tenancy", "base/authz", "base/settings", "base/integration", "base/queue", "base/schedule", "base/ui", "base/datetime", "base/locale", "base/menu", "base/grid", "base/agent_api"],
        migrations: &[("base-ai-001-interactions", include_str!("../migrations/001-interactions.sql"))],
        services: &[Service { id: SERVICE, factory: |services| {
            Ok(Arc::new(Ai {
                db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
                settings: services.get::<Settings>("base/settings.store")?,
                integration: services.get::<Integration>(bilibi_integration::SERVICE)?,
            }))
        } }, Service { id: PAGES, factory: |services| {
            Ok(Arc::new(bilibi_ui::page::PageService::<Scope, Error>(Arc::new(web::Pages {
                db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
                ai: services.get(SERVICE)?,
                display: services.get("base/datetime.display")?,
                locales: services.get("base/locale.locales")?,
                views: services.get::<bilibi_grid::PageViews>(bilibi_grid::PAGE_VIEWS)?,
                contracts: services.get(bilibi_agent_api::page::SERVICE)?,
            }))))
        } }, Service { id: retention::SERVICE, factory: |services| {
            Ok(Arc::new(bilibi_queue::HandlerService(Arc::new(retention::Retention {
                schedule: services.get("base/schedule.store")?,
                settings: services.get("base/settings.store")?,
                db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
            }))))
        } }],
    }
}

macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}
// The list and record pages, both behind the list capability; the record
// page reads the row through the facade, which scopes it to the actor's
// tenant.
inventory::submit! { Contribution::new(ROUTES, "base/ai.interactions", provenance!(), &Route {
    company_scope: CompanyScope::Tenant, method: Method::Get, path: web::INDEX_PATH,
    service: PAGES, operation: "interactions", access: Access::Capability(LIST), framed: true,
}) }
inventory::submit! { Contribution::new(ROUTES, "base/ai.interaction", provenance!(), &Route {
    company_scope: CompanyScope::Tenant, method: Method::Get, path: "/system/ai-interactions/{id}",
    service: PAGES, operation: "interaction", access: Access::Capability(LIST), framed: true,
}) }
// Menu item under System, after API Exchanges.
bilibi_menu::contribute!(
    "base/ai",
    bilibi_menu::MenuItem::page("admin.system.ai", "AI Interactions", web::INDEX_PATH)
        .under("admin.system")
        .requires(bilibi_menu::Requirement::Capability(LIST))
        .order(46)
);
// The pages only read; their one write is a person's own list arrangement.
inventory::submit! { Contribution::new(PAGE_WRITES, PAGES, provenance!(), &PageWrites {
    service: PAGES,
    contracts: &[("grid", "base.grid.view.save"), ("grid", "base.grid.view.delete")],
    pending: Pending::None,
}) }
