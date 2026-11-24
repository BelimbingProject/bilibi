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
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

mod budget;
mod contributions;
pub mod link;

pub use budget::{Counter, SenderBudget};
pub use contributions::{
    PublicLink, PublicPage, PUBLIC_LINKS, PUBLIC_PAGES, SENDER_PER_HOUR, SENDER_PER_MINUTE,
};
pub use link::{Cache, Link, LinkFuture, LinkRequest, LinkService, Local, Reply, Target, PIXEL};

/// The registry service ID of [`Public`].
pub const SERVICE: &str = "base/public.site";
/// The page service a [`PublicPage`] names.
pub type PageService = bilibi_ui::page::PageService<PublicScope, Error>;
/// Senders the listener budget counts at once on one server.
const LISTENER_SENDERS: usize = 262_144;
/// How long one reading of the listener budget is reused.
const POLICY_FRESH: Duration = Duration::from_secs(5);

/// The public site a request reached. The host seals it after it admitted the
/// request; it is not a staff `Scope`, converts to nothing that is, and has
/// no public constructor or `Deserialize`. It names a tenant and grants
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

/// The facade: the listener's budget and the scope of the site it serves.
pub struct Public {
    db: Db,
    settings: Arc<Settings>,
    listener: Counter,
    policy: Mutex<Option<(Instant, SenderBudget)>>,
}
impl Public {
    fn new(db: Db, settings: Arc<Settings>) -> Self {
        Self {
            db,
            settings,
            listener: Counter::with_capacity(LISTENER_SENDERS),
            policy: Mutex::default(),
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

    /// The scope of the public site the listener serves for `tenant`. The
    /// host calls it once it has admitted a request; an owner never needs to.
    pub fn site(&self, tenant: i64) -> PublicScope {
        PublicScope { tenant }
    }

    /// Whether `sender` may make one more request of the public listener on
    /// this server, under the operator's `public.sender_per_minute` and
    /// `public.sender_per_hour` (read again within seconds of a change).
    pub async fn admit(&self, sender: &str) -> Result<bool> {
        let budget = self.policy().await?;
        Ok(self.listener.allow(sender, budget))
    }

    async fn policy(&self) -> Result<SenderBudget> {
        if let Some((read, budget)) = *self.policy.lock().unwrap_or_else(|p| p.into_inner()) {
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
            per_minute: limit(
                self.settings
                    .get_global(&**client, SENDER_PER_MINUTE)
                    .await?,
            )?,
            per_hour: limit(self.settings.get_global(&**client, SENDER_PER_HOUR).await?)?,
        };
        *self.policy.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), budget));
        Ok(budget)
    }
}

inventory::submit! {
    Module {
        id: "base/public", layer: bilibi_module_registry::Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/settings", "base/ui"],
        migrations: &[],
        services: &[
            Service { id: SERVICE, factory: |services| Ok(Arc::new(Public::new(
                services.get::<Db>("base/database.pool")?.as_ref().clone(),
                services.get("base/settings.store")?,
            ))) },
        ],
    }
}
