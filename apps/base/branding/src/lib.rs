//! The installation's own brand in place of Bilibi's: a display name, a logo
//! and an accent, kept as three installation settings. See README.md for the
//! contract.
mod operations;

use bilibi_authz::{CapabilityDefinition, VocabularyContribution, VOCABULARY};
use bilibi_database::Db;
use bilibi_media::Media;
use bilibi_module_registry::{Contribution, Module, Provenance, Service};
use bilibi_settings::{
    Definition, Editor, ScopeKind, Setting, Settings, Validator, ValueType, DEFINITIONS,
};
use bilibi_ui::{Accent, Brand};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::GenericClient;

pub const SERVICE: &str = "base/branding.brand";
/// Changing the installation's name, logo and accent: the platform operator's.
pub const MANAGE: &str = "admin.system.branding.manage";
pub const NAME_KEY: &str = "branding.name";
pub const LOGO_KEY: &str = "branding.logo";
pub const ACCENT_KEY: &str = "branding.accent";
/// The largest logo. Media's own limit still applies.
pub const LOGO_MAX_BYTES: u64 = 1_048_576;
/// The longest display name, in characters.
pub const NAME_MAX_CHARS: usize = 40;

/// A display name: short, one line, and never able to pose as an address,
/// because mail carries it as the sender name (`base/mail`).
pub fn valid_name(name: &str) -> bool {
    name.trim() == name
        && name.chars().count() <= NAME_MAX_CHARS
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '@' | '<' | '>' | '"'))
}
fn name_accepts(value: &Value) -> bool {
    value.as_str().is_some_and(valid_name)
}
fn logo_accepts(value: &Value) -> bool {
    value.is_null() || value.as_i64().is_some_and(i64::is_positive)
}
fn accent_accepts(value: &Value) -> bool {
    value.as_str().and_then(Accent::parse).is_some()
}

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/branding",
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! { Contribution::new(DEFINITIONS, NAME_KEY, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: &[ScopeKind::Global],
    default: || json!(""), nullable: false, encrypted: false,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: name_accepts, message: "Enter a name of up to 40 characters without @, quotes or angle brackets." }),
    editor: Some(Editor { group: "Branding", label: "Display name",
        help: "Shown in place of Bilibi on sign-in, in the top bar, in tab titles and as the email sender name. Blank keeps Bilibi.",
        capability: Some(MANAGE), reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, LOGO_KEY, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global],
    default: || Value::Null, nullable: true, encrypted: false,
    minimum: Some(1.0), maximum: None,
    validator: Some(Validator { accepts: logo_accepts, message: "must name a stored logo image" }),
    editor: Some(Editor { group: "Branding", label: "Logo",
        help: "Shown in place of the Bilibi mark on sign-in, in the top bar and as the tab icon.",
        capability: Some(MANAGE), reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(bilibi_settings_editor::IMAGE_SETTINGS, LOGO_KEY, provenance!(), &bilibi_settings_editor::ImageSetting {
    directory: "branding", max_bytes: LOGO_MAX_BYTES, noun: "Logo",
    current: "Current logo", unset: "The Bilibi mark is in use", placeholder: false,
}) }
inventory::submit! { Contribution::new(DEFINITIONS, ACCENT_KEY, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: &[ScopeKind::Global],
    default: || json!(Accent::Lime.as_str()), nullable: false, encrypted: false,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: accent_accepts, message: "Enter one of: lime, teal, cyan, indigo, violet, fuchsia." }),
    editor: Some(Editor { group: "Branding", label: "Accent colour",
        help: "The colour of primary buttons, the current menu item and selections: lime, teal, cyan, indigo, violet or fuchsia.",
        capability: Some(MANAGE), reveal_duration_ms: 5_000 }),
})) }

inventory::submit! { Contribution::new(VOCABULARY, "base/branding", provenance!(), &VocabularyContribution {
    domains: &[], verbs: &[], capabilities: &[CapabilityDefinition::platform(MANAGE)], roles: &[], system_principals: &[],
}) }

/// What the installation set, before any link is signed.
pub(crate) struct Stored {
    pub name: String,
    pub logo: Option<i64>,
    pub accent: Accent,
}

/// The brand facade: one read for every page, with or without a session.
pub struct Branding {
    db: Db,
    pub(crate) settings: Arc<Settings>,
    pub(crate) media: Arc<Media>,
}

impl Branding {
    pub(crate) async fn stored(
        &self,
        client: &(impl GenericClient + Sync),
    ) -> bilibi_database::Result<Stored> {
        let name = self.settings.get_global(client, NAME_KEY).await?;
        let logo = self.settings.get_global(client, LOGO_KEY).await?;
        let accent = self.settings.get_global(client, ACCENT_KEY).await?;
        Ok(Stored {
            name: name.as_str().unwrap_or_default().to_owned(),
            logo: logo.as_i64(),
            accent: accent.as_str().and_then(Accent::parse).unwrap_or_default(),
        })
    }

    /// The brand to draw on `client`'s connection. A logo that cannot be
    /// served (removed, quarantined, or no operator tenant yet) is left out:
    /// a page without its logo is still a page.
    pub async fn resolve(
        &self,
        client: &(impl GenericClient + Sync),
    ) -> bilibi_database::Result<Brand> {
        let stored = self.stored(client).await?;
        let logo = match stored.logo {
            Some(asset) => self.logo_url(client, asset).await?,
            None => None,
        };
        Ok(Brand {
            name: stored.name,
            logo,
            accent: stored.accent,
        })
    }

    /// [`Branding::resolve`] on a pooled connection, never failing a page: a
    /// read that fails is logged and the page carries Bilibi's own brand.
    pub async fn current(&self) -> Brand {
        let resolved = async {
            let client = self.db.get().await?;
            self.resolve(&**client).await
        };
        match resolved.await {
            Ok(brand) => brand,
            Err(error) => {
                eprintln!("warning: brand lookup failed: {error:?}");
                Brand::default()
            }
        }
    }

    /// A signed Media link to the logo. The image belongs to the platform
    /// operator's tenant, which is the only one that may set it.
    async fn logo_url(
        &self,
        client: &(impl GenericClient + Sync),
        asset: i64,
    ) -> bilibi_database::Result<Option<String>> {
        let Some(operator) = bilibi_tenancy::platform_operator(client).await? else {
            return Ok(None);
        };
        match self
            .media
            .tenant_stream_url(client, operator.id, asset)
            .await
        {
            Ok(url) => Ok(Some(url)),
            Err(bilibi_media::Error::Database(error)) => Err(error),
            Err(_) => Ok(None),
        }
    }
}

inventory::submit! { Module {
    id: "base/branding", layer: bilibi_module_registry::Layer::Base,
    dependencies: &["base/module_registry", "base/database", "base/tenancy", "base/authz", "base/agent_api", "base/settings", "base/settings_editor", "base/media", "base/labels", "base/ui"],
    migrations: &[],
    services: &[Service { id: SERVICE, factory: |services| {
        let branding = Arc::new(Branding {
            db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
            settings: services.get("base/settings.store")?,
            media: services.get(bilibi_media::SERVICE)?,
        });
        operations::bind(branding.clone())?;
        Ok(branding)
    }}],
} }
