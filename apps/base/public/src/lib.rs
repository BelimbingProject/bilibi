//! The public site's one owner: what modules serve there (pages and token
//! links), the per-sender budgets that guard it, and the sealed scope an
//! owner receives. A Domain declares a page or link and gets a request that
//! the host already admitted; it never sees a sender address, a cookie or a
//! header. Contract: `README.md`.
use bilibi_database::{Db, Error, Result};
use bilibi_module_registry::{
    contributions::{CredentialPath, CREDENTIAL_PATHS},
    Catalog, Module, RegistryError, Service,
};
use bilibi_settings::Settings;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

mod budget;
mod contributions;
pub mod link;
mod operations;
mod resolve;
mod sites;
mod web;

pub use budget::{Counter, SenderBudget};
pub use contributions::{
    PublicLink, PublicPage, LOOKUPS_PER_HOUR, LOOKUPS_PER_MINUTE, PUBLIC_LINKS, PUBLIC_PAGES,
    SENDER_PER_HOUR, SENDER_PER_MINUTE,
};
pub use link::{Cache, Link, LinkFuture, LinkRequest, LinkService, Local, Reply, Target, PIXEL};
pub use resolve::Reached;
pub use sites::{
    address_shaped, hostname_shaped, Checked, FieldError, Hostname, LogoChange, Site, SiteInput,
    MANAGE, TITLE_MAX, VIEW,
};

/// The registry service ID of [`Public`].
pub const SERVICE: &str = "base/public.site";
/// The Public site staff page's service.
pub const PAGES: &str = "base/public.pages";
/// The Public site staff page.
pub const PAGE_PATH: &str = "/public-site";
pub use operations::{HOSTNAME_ADD, HOSTNAME_REMOVE, SITE_GET, SITE_SET};
/// The page service a [`PublicPage`] names.
pub type PageService = bilibi_ui::page::PageService<PublicScope, Error>;
/// Senders the listener budget counts at once on one server.
const LISTENER_SENDERS: usize = 262_144;
/// How long one reading of the listener budget, or of a hostname's site, is
/// reused.
const POLICY_FRESH: Duration = Duration::from_secs(5);
/// Hostnames whose answer one server keeps at once.
const RESOLVED_MAX: usize = 10_000;

/// The public site a request reached, found by its hostname
/// ([`Public::resolve`]). It is not a staff `Scope`, converts to nothing that
/// is, and has no public constructor or `Deserialize`. It names a tenant and grants
/// nothing: an owner reads and writes as its own system principal in that
/// tenant, and finds a token's or address's record there only.
#[derive(Clone, Debug)]
pub struct PublicScope {
    tenant: i64,
}
impl PublicScope {
    /// The tenant whose public site this is.
    pub fn tenant(&self) -> i64 {
        self.tenant
    }
}

/// The public pages and links of this composition, checked against each
/// other's rules: a `{token}` path is declared under `CREDENTIAL_PATHS` by
/// the same module, so its token never reaches a log, debug report or
/// referrer.
pub struct Declared {
    pub pages: Vec<&'static PublicPage>,
    pub links: Vec<&'static PublicLink>,
}

/// The facade: the listener's budget, the tenants' sites and the hostnames
/// that name them.
pub struct Public {
    db: Db,
    settings: Arc<Settings>,
    media: Arc<bilibi_media::Media>,
    listener: Counter,
    policy: Mutex<Option<(Instant, SenderBudget)>>,
    /// Database lookups of a hostname per sender, before any other count.
    lookups: Counter,
    lookup_policy: Mutex<Option<(Instant, SenderBudget)>>,
    /// Hostname to its live site's tenant (`None`: no site), as last read.
    resolved: Mutex<HashMap<String, (Instant, Option<i64>)>>,
    /// The staff site's hostname, which no public site may take.
    staff_host: Option<String>,
    /// The public listener's scheme and its port when not the default one,
    /// for the addresses emailed links use; no scheme without a listener.
    public_scheme: Option<String>,
    public_port: Option<u16>,
}
impl Public {
    fn new(db: Db, settings: Arc<Settings>, media: Arc<bilibi_media::Media>) -> Self {
        let origin = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|origin| url::Url::parse(&origin).ok())
        };
        let public = origin("PUBLIC_ORIGIN");
        Self {
            db,
            settings,
            media,
            listener: Counter::with_capacity(LISTENER_SENDERS),
            policy: Mutex::default(),
            lookups: Counter::with_capacity(LISTENER_SENDERS),
            lookup_policy: Mutex::default(),
            resolved: Mutex::default(),
            staff_host: origin("APP_ORIGIN")
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase)),
            public_scheme: public.as_ref().map(|url| url.scheme().to_owned()),
            public_port: public.as_ref().and_then(url::Url::port),
        }
    }

    /// Every public page and link, or the boot refusal that names the first
    /// one whose `{token}` its own module did not declare a credential.
    pub fn declared(catalog: &Catalog) -> Result<Declared> {
        let credentials = catalog.entries::<CredentialPath>(CREDENTIAL_PATHS)?;
        let check = |entry: &bilibi_module_registry::Entry, path: &str| -> Result<()> {
            if !contributions::parameters(path).any(|name| name == "token") {
                return Ok(());
            }
            let declared = credentials.iter().any(|credential| {
                credential.provenance.module == entry.provenance.module
                    && credential
                        .get::<CredentialPath>()
                        .is_ok_and(|credential| credential.path == path)
            });
            if declared {
                return Ok(());
            }
            Err(RegistryError::Contribution(format!(
                "{} at {}:{} serves {path} on the public site without its own {CREDENTIAL_PATHS} declaration",
                entry.id, entry.provenance.source, entry.provenance.line
            ))
            .into())
        };
        let mut declared = Declared {
            pages: Vec::new(),
            links: Vec::new(),
        };
        for entry in catalog.entries::<PublicPage>(PUBLIC_PAGES)? {
            let page = entry.get::<PublicPage>()?;
            check(entry, page.path)?;
            declared.pages.push(page);
        }
        for entry in catalog.entries::<PublicLink>(PUBLIC_LINKS)? {
            let link = entry.get::<PublicLink>()?;
            check(entry, link.path)?;
            declared.links.push(link);
        }
        Ok(declared)
    }

    /// Whether `sender` may make one more request of the public listener on
    /// this server, under the operator's `public.sender_per_minute` and
    /// `public.sender_per_hour` (read again within seconds of a change).
    pub async fn admit(&self, sender: &str) -> Result<bool> {
        let budget = self.policy().await?;
        Ok(self.listener.allow(sender, budget))
    }

    async fn policy(&self) -> Result<SenderBudget> {
        self.budget(&self.policy, SENDER_PER_MINUTE, SENDER_PER_HOUR)
            .await
    }
    /// The lookup budget ([`Public::resolve_from`]).
    pub(crate) async fn lookup_policy(&self) -> Result<SenderBudget> {
        self.budget(&self.lookup_policy, LOOKUPS_PER_MINUTE, LOOKUPS_PER_HOUR)
            .await
    }
    pub(crate) fn charge_lookup(&self, sender: &str, budget: SenderBudget) -> bool {
        self.lookups.allow(sender, budget)
    }
    async fn budget(
        &self,
        cache: &Mutex<Option<(Instant, SenderBudget)>>,
        minute: &str,
        hour: &str,
    ) -> Result<SenderBudget> {
        if let Some((read, budget)) = *cache.lock().unwrap_or_else(|p| p.into_inner()) {
            if read.elapsed() < POLICY_FRESH {
                return Ok(budget);
            }
        }
        let client = self.db.get().await?;
        let limit = |value: serde_json::Value| {
            value
                .as_u64()
                .and_then(|limit| u32::try_from(limit).ok())
                .ok_or(Error::Internal)
        };
        let budget = SenderBudget {
            per_minute: limit(self.settings.get_global(&**client, minute).await?)?,
            per_hour: limit(self.settings.get_global(&**client, hour).await?)?,
        };
        *cache.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), budget));
        Ok(budget)
    }
}

inventory::submit! {
    Module {
        id: "base/public", layer: bilibi_module_registry::Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/settings", "base/ui", "base/authz", "base/media", "base/tenancy", "base/agent_api", "base/menu"],
        migrations: &[("base-public-001-sites", include_str!("../migrations/001-sites.sql"))],
        services: &[
            Service { id: SERVICE, factory: |services| {
                let public = Arc::new(Public::new(
                    services.get::<Db>("base/database.pool")?.as_ref().clone(),
                    services.get("base/settings.store")?,
                    services.get(bilibi_media::SERVICE)?,
                ));
                operations::bind(public.clone())?;
                Ok(public)
            } },
            Service { id: PAGES, factory: |services| Ok(Arc::new(bilibi_ui::page::PageService::<bilibi_tenancy::Scope, Error>(Arc::new(web::Pages {
                db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
                public: services.get(SERVICE)?,
                contracts: services.get(bilibi_agent_api::page::SERVICE)?,
            })))) },
        ],
    }
}

inventory::submit! { bilibi_module_registry::Contribution::new(bilibi_authz::VOCABULARY, "base/public", bilibi_module_registry::Provenance { module: "base/public", source: file!(), line: line!() }, &bilibi_authz::VocabularyContribution {
    domains: &["public-site"], verbs: &[],
    capabilities: &[bilibi_authz::CapabilityDefinition::tenant(VIEW), bilibi_authz::CapabilityDefinition::tenant(MANAGE)],
    roles: &[bilibi_authz::RoleDefinition { code: "tenant_owner", name: "Tenant Owner", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: &[VIEW, MANAGE] }],
    system_principals: &[],
}) }

macro_rules! provenance {
    () => {
        bilibi_module_registry::Provenance {
            module: "base/public",
            source: file!(),
            line: line!(),
        }
    };
}
use bilibi_module_registry::{
    contributions::{
        Access, CompanyScope, Method, PageWrites, Pending, Route, PAGE_WRITES, ROUTES,
    },
    Contribution,
};
inventory::submit! { Contribution::new(ROUTES, "base/public.site", provenance!(), &Route {
    company_scope: CompanyScope::Tenant, method: Method::Get, path: PAGE_PATH, service: PAGES,
    operation: "site", access: Access::Capability(VIEW), framed: true,
}) }
// Every staff write goes through the contracts agents call.
inventory::submit! { Contribution::new(PAGE_WRITES, PAGES, provenance!(), &PageWrites {
    service: PAGES,
    contracts: &[("save_site", SITE_SET), ("add", HOSTNAME_ADD), ("make_primary", HOSTNAME_ADD), ("confirm_remove", HOSTNAME_REMOVE)],
    pending: Pending::Events { events: &["store_logo"], reason: "Storing a logo image puts its bytes into Media outside a contract; the site save that uses it is the site.set contract." },
}) }
bilibi_menu::contribute!(
    "base/public",
    bilibi_menu::MenuItem::page("admin.public-site", "Public site", PAGE_PATH)
        .under("admin")
        .icon("globe-alt")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(56)
);
