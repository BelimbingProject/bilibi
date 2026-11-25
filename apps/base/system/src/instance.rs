//! Instance identity, shared by the shell, banners, Software and agents.//! `instance.role` is initialized by the explicit production seed from
//! `BILIBI_ENV`; later environment changes cannot override the stored role.
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{
    Definition, Editor, ScopeKind, Setting, Settings, Validator, ValueType, DEFINITIONS,
};
use bilibi_ui::{Html, InstanceBadge, InstanceBanner, InstanceStatus, StatusKind};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_postgres::GenericClient;

pub const INSTANCE_ROLE_KEY: &str = "instance.role";
pub const INSTANCE_LABEL_KEY: &str = "instance.label";
pub const INSTANCE_COLOUR_KEY: &str = "instance.colour";
pub const INSTALLATION_MODE_KEY: &str = "installation.mode";
/// Changing how the instance announces itself.
pub const INSTANCE_MANAGE: &str = "admin.system.instance.manage";

/// What this server is for. Software is built locally; servers install tested releases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Dev,
    Staging,
    Production,
}

/// Whether this process serves unrelated customers or one isolated customer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallationMode {
    Shared,
    Isolated,
}
impl InstallationMode {
    pub const ALL: [Self; 2] = [Self::Shared, Self::Isolated];
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "shared" => Some(Self::Shared),
            "isolated" => Some(Self::Isolated),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Isolated => "isolated",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Shared => "Shared service",
            Self::Isolated => "Isolated installation",
        }
    }
    pub async fn resolve(
        settings: &Settings,
        client: &(impl GenericClient + Sync),
    ) -> bilibi_database::Result<Self> {
        if !client
            .query_one("SELECT to_regclass('base_settings') IS NOT NULL", &[])
            .await?
            .get::<_, bool>(0)
        {
            return Ok(Self::Shared);
        }
        settings
            .get_global(client, INSTALLATION_MODE_KEY)
            .await?
            .as_str()
            .and_then(Self::parse)
            .ok_or(bilibi_database::Error::Invalid)
    }
}
impl Role {
    /// The install seed's reading of `BILIBI_ENV`: `development`,
    /// `staging`, and `production` (an empty value means production).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "" | "production" => Some(Self::Production),
            "staging" => Some(Self::Staging),
            "development" => Some(Self::Dev),
            _ => None,
        }
    }
    /// Installation input only, used by the explicit production seed.
    pub fn from_env() -> bilibi_database::Result<Self> {
        match std::env::var("BILIBI_ENV") {
            Ok(value) => Self::parse(&value).ok_or(bilibi_database::Error::Invalid),
            Err(std::env::VarError::NotPresent) => Ok(Self::Production),
            Err(_) => Err(bilibi_database::Error::Invalid),
        }
    }
    pub fn env_value(self) -> &'static str {
        match self {
            Self::Dev => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }
    /// The default label: what the banner says when the operator set none.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dev => "Dev",
            Self::Staging => "Staging",
            Self::Production => "Production",
        }
    }
    /// What this copy is, to follow "is": "a development copy".
    pub fn kind(self) -> &'static str {
        match self {
            Self::Dev => "a development copy",
            Self::Staging => "a test copy",
            Self::Production => "the live system",
        }
    }
    /// What a change here does, as one sentence.
    pub fn effect(self) -> &'static str {
        match self {
            Self::Dev | Self::Staging => "Changes here do not affect production.",
            Self::Production => "",
        }
    }
    /// What a change here does, in one or two short sentences.
    pub fn consequence(self) -> &'static str {
        match self {
            Self::Dev => "This is a development copy. Changes here do not affect production.",
            Self::Staging => "This is a test copy. Changes here do not affect production.",
            Self::Production => "",
        }
    }
    /// The default colour, one of `DESIGN.md`'s status roles.
    pub fn colour(self) -> Colour {
        match self {
            Self::Dev => Colour::Info,
            Self::Staging => Colour::Warning,
            Self::Production => Colour::Danger,
        }
    }
}

/// A `DESIGN.md` status role the banner is drawn in. Never an arbitrary colour.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Colour {
    Info,
    Success,
    Warning,
    Danger,
}

pub(super) fn colour_input(field: bilibi_ui::Field, role: &str) -> bilibi_ui::ColourInput {
    use bilibi_ui::{ColourChoice, ColourInput, ColourRole};
    let swatch = |colour| match colour {
        Colour::Info => ColourRole::Info,
        Colour::Success => ColourRole::Success,
        Colour::Warning => ColourRole::Warning,
        Colour::Danger => ColourRole::Danger,
    };
    let inherited = swatch(Role::parse(role).unwrap_or(Role::Production).colour());
    let mut options = vec![ColourChoice::new("Use role colour", "", inherited)];
    options.extend(Colour::ALL.map(|colour| {
        let role = swatch(colour);
        ColourChoice::new(role.label(), colour.as_str(), role)
    }));
    ColourInput::new(field, options)
}

impl Colour {
    pub const ALL: [Colour; 4] = [Self::Info, Self::Success, Self::Warning, Self::Danger];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Danger => "danger",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|colour| colour.as_str() == value)
    }
    fn status(self) -> StatusKind {
        match self {
            Self::Info => StatusKind::Info,
            Self::Success => StatusKind::Success,
            Self::Warning => StatusKind::Warning,
            Self::Danger => StatusKind::Error,
        }
    }
}

/// The same rule `scripts/install-server.sh` applies to an instance name.
pub fn valid_instance_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
}

/// The longest [`Instance::server_name`].
const SERVER_NAME_LIMIT: usize = 32;

/// This instance as it should be announced.
#[derive(Clone, Debug)]
pub struct Instance {
    /// Canonical public address from APP_ORIGIN, never the request Host.
    pub origin: String,
    pub role: Role,
    pub label: String,
    pub colour: Colour,
}
impl Instance {
    /// Resolve identity from global settings. Unset label and colour follow the stored role.
    pub async fn resolve(
        settings: &Settings,
        client: &(impl GenericClient + Sync),
    ) -> bilibi_database::Result<Self> {
        let role = settings
            .get_global(client, INSTANCE_ROLE_KEY)
            .await?
            .as_str()
            .and_then(Role::parse)
            .ok_or(bilibi_database::Error::Invalid)?;
        let label = settings
            .get_global(client, INSTANCE_LABEL_KEY)
            .await?
            .as_str()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map_or_else(|| role.label().to_owned(), str::to_owned);
        let colour = settings
            .get_global(client, INSTANCE_COLOUR_KEY)
            .await?
            .as_str()
            .and_then(Colour::parse)
            .unwrap_or_else(|| role.colour());
        Ok(Self {
            origin: std::env::var("APP_ORIGIN")
                .unwrap_or_else(|_| "http://localhost:3000".into())
                .trim_end_matches('/')
                .to_owned(),
            role,
            label,
            colour,
        })
    }
    /// The name an AI app saves this Bilibi under: the host of the canonical
    /// address with dots as hyphens and existing hyphens doubled
    /// (`try.bilibi.app` is `try-bilibi-app`, `bilibi-app.example.com` is
    /// `bilibi--app-example-com`). The MCP server name, the connection prompt
    /// and the plugin package all use it.
    ///
    /// The address is unique per deployment; a label is not (two Productions
    /// would both be `production`, and one saved sign-in would then serve
    /// both). It changes only when the address does, which ends a saved
    /// sign-in anyway, so renaming the label never orphans one. Dots are not
    /// safe in every app's server-name grammar; lower-case letters, digits
    /// and hyphens are.
    ///
    /// Two different addresses do not share a name. An HTTPS host name of up
    /// to 32 characters maps one to one: a label never starts or ends with a
    /// hyphen, so a dot's single hyphen never meets a doubled one. Any other
    /// address (a port, an IPv6 literal, plain HTTP, or a name past 32
    /// characters, since apps name a tool `mcp__<server>__<tool>` within 64)
    /// keeps the start of that name and ends in three hyphens and the first
    /// 64 bits of the SHA-256 of the whole origin, scheme and port included.
    /// Three hyphens in a row never come from a host name, so the two kinds
    /// cannot meet either.
    pub fn server_name(&self) -> String {
        let (scheme, rest) = self
            .origin
            .split_once("://")
            .unwrap_or(("", self.origin.as_str()));
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let mut name = String::new();
        for c in authority.chars() {
            if c == '-' {
                name.push_str("--");
            } else if c.is_ascii_alphanumeric() {
                name.push(c.to_ascii_lowercase());
            } else if !name.is_empty() && !name.ends_with('-') {
                name.push('-');
            }
        }
        let name = name.trim_end_matches('-');
        if name.is_empty() {
            return "bilibi".to_owned();
        }
        // A host name whose every label starts and ends with a letter or digit.
        let host_name = authority.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
        if host_name && scheme.eq_ignore_ascii_case("https") && name.len() <= SERVER_NAME_LIMIT {
            return name.to_owned();
        }
        let origin = format!("{scheme}://{authority}").to_ascii_lowercase();
        let digest = Sha256::digest(origin.as_bytes());
        let tail: String = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let kept = name[..name.len().min(SERVER_NAME_LIMIT - 19)].trim_end_matches('-');
        format!("{kept}---{tail}")
    }
    /// Whether an AI app on the internet (a chat website, a bot) can reach
    /// this Bilibi at its configured address. Decided from what the server
    /// knows, never from the browser: a Dev copy is never public, and an
    /// address that is `localhost`, a loopback, private, carrier-grade NAT
    /// (`100.64.0.0/10`) or link-local IP (an IPv4-mapped IPv6 literal counts
    /// as its IPv4 address), a bare host name, or a name under a local-only
    /// domain (`.localhost`, `.local`, `.internal`, `.lan`, `.home.arpa`,
    /// `.test`, `.ts.net`) is reachable only from the machine or network it
    /// runs on. A false `false` costs one
    /// extra sentence in the connect panel; a false `true` would send a
    /// person to a bot that cannot connect, so the doubt resolves to `false`.
    pub fn public_address(&self) -> bool {
        if self.role == Role::Dev {
            return false;
        }
        let Some(host) = self
            .origin
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .map(|authority| authority.rsplit('@').next().unwrap_or(authority))
        else {
            return false;
        };
        // An IPv6 literal keeps its brackets; a port follows the host.
        let host = if let Some(end) = host.strip_prefix('[').and_then(|h| h.find(']')) {
            &host[1..=end]
        } else {
            host.rsplit_once(':').map_or(host, |(name, _)| name)
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            let ip = match ip {
                std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, std::net::IpAddr::V4),
                v4 => v4,
            };
            return match ip {
                std::net::IpAddr::V4(ip) => {
                    !(ip.is_loopback()
                        || ip.is_private()
                        || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
                        || ip.is_link_local()
                        || ip.is_unspecified()
                        || ip.is_broadcast()
                        || ip.is_documentation())
                }
                std::net::IpAddr::V6(ip) => {
                    !(ip.is_loopback()
                        || ip.is_unspecified()
                        || ip.is_unique_local()
                        || ip.is_unicast_link_local())
                }
            };
        }
        const LOCAL_ONLY: [&str; 7] = [
            ".localhost",
            ".local",
            ".internal",
            ".lan",
            ".home.arpa",
            ".test",
            ".ts.net",
        ];
        host != "localhost"
            && host.contains('.')
            && !LOCAL_ONLY.iter().any(|suffix| host.ends_with(suffix))
    }
    /// The sentence an agent or a hover shows: "You are on Staging. This is a
    /// test copy. Changes here do not affect production."
    pub fn notice(&self) -> String {
        format!("You are on {}. {}", self.label, self.role.consequence())
            .trim_end()
            .to_owned()
    }
    /// Agent connection identity includes the role even when the operator's
    /// label is ambiguous, and the canonical address rather than request data.
    pub fn agent_notice(&self) -> String {
        format!(
            "You are on {} ({}), at {}. {}",
            self.label,
            self.role.label(),
            self.origin,
            self.role.consequence()
        )
        .trim_end()
        .to_owned()
    }
    /// What the Agent API and MCP send, so an agent can tell its person
    /// "this will create the order on Staging".
    pub fn agent_json(&self) -> Value {
        json!({
            "environment": self.role.env_value(),
            "role": self.role.label(),
            "origin": self.origin,
            "label": self.label,
            "production": self.role == Role::Production,
            "notice": self.agent_notice(),
        })
    }
    /// The shared badge for sign-in and the shell. The workspace shell shows
    /// only Staging because its status bar already names the other roles.
    pub fn badge(&self) -> askama::Result<Html> {
        let title = self.notice();
        Html::render(&InstanceBadge {
            tone: self.colour.status(),
            role: self.role.env_value(),
            label: &self.label,
            title: &title,
        })
    }
    /// The status bar always names the role, including Production.
    pub fn status(&self, editable: bool) -> askama::Result<Html> {
        Html::render(&InstanceStatus {
            id: "app-env",
            tone: self.colour.status(),
            role: self.role.env_value(),
            role_label: self.role.label(),
            label: &self.label,
            title: &self.notice(),
            editable,
        })
    }
    /// The banner at the top of a review or confirmation, in every role,
    /// production included. `class` places it (`mb-4`).
    pub fn banner(&self, id: &str, class: &str) -> askama::Result<Html> {
        Html::render(&InstanceBanner {
            id,
            tone: self.colour.status(),
            role: self.role.env_value(),
            label: &self.label,
            message: self.role.consequence(),
            class,
        })
    }
}

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/system",
            source: file!(),
            line: line!(),
        }
    };
}
const GLOBAL: &[ScopeKind] = &[ScopeKind::Global];
inventory::submit! { Contribution::new(DEFINITIONS, INSTALLATION_MODE_KEY, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: GLOBAL, default: || json!("shared"), nullable: false, encrypted: false,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: |value: &Value| value.as_str().is_some_and(|mode| InstallationMode::parse(mode).is_some()), message: "Choose Shared service or Isolated installation." }),
    // The owner-only Instance settings operation owns this protected setting.
    editor: None,
})) }
inventory::submit! { Contribution::new(DEFINITIONS, INSTANCE_LABEL_KEY, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: GLOBAL, default: || json!(null), nullable: true, encrypted: false,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: |value: &Value| value.is_null() || value.as_str().is_some_and(|label| { let label = label.trim(); !label.is_empty() && label.len() <= 40 }), message: "The instance name must be 1 to 40 characters, like Production or Dev (laptop)." }),
    editor: Some(Editor { group: "Instance", label: "Instance name shown on every page",
        help: "Shown in the banner and in every confirm page so people know which server they are on. The default follows the server's role: Dev, Staging or Production (server owner only).",
        capability: Some(INSTANCE_MANAGE), reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, INSTANCE_COLOUR_KEY, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: GLOBAL, default: || json!(null), nullable: true, encrypted: false,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: |value: &Value| value.is_null() || value.as_str().is_some_and(|colour| Colour::parse(colour).is_some()), message: "The colour must be info (blue), success (green), warning (amber) or danger (red)." }),
    editor: Some(Editor { group: "Instance", label: "Dev and Staging banner colour",
        help: "info (blue), success (green), warning (amber) or danger (red). Shared Dev badges and banners default to blue and Staging defaults to amber. Shared Production badges are always neutral.",
        capability: Some(INSTANCE_MANAGE), reveal_duration_ms: 5_000 }),
})) }
// The dedicated Core Administrator page guards this setting; the generic editor cannot change it.
inventory::submit! { Contribution::new(DEFINITIONS, INSTANCE_ROLE_KEY, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: GLOBAL, default: || json!("production"), nullable: false, encrypted: false,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: |value: &Value| matches!(value.as_str(), Some("development" | "staging" | "production")), message: "Choose Dev, Staging or Production." }),
    editor: None,
})) }

pub(super) struct InstallRole;
impl bilibi_database::seeds::SeedExecutor for InstallRole {
    fn execute<'a>(
        &'a self,
        tx: &'a tokio_postgres::Transaction<'_>,
        operation: &'a str,
    ) -> bilibi_database::seeds::SeedFuture<'a> {
        Box::pin(async move {
            if operation != "initialize" {
                return Err(bilibi_database::Error::Invalid);
            }
            // Explicit install seed, on the seed runner's transaction. Never on boot.
            // Repeated installation preserves an operator's saved role.
            if tx.query_opt("SELECT id FROM base_settings WHERE key=$1 AND tenant_id=0 AND scope_type='global' AND scope_id=0", &[&INSTANCE_ROLE_KEY]).await?.is_none() {
                let role = Role::from_env()?;
                tx.execute("INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES($1,0,'global',0,$2,false) ON CONFLICT DO NOTHING", &[&INSTANCE_ROLE_KEY, &json!(role.env_value())]).await?;
            }
            Ok(())
        })
    }
}
inventory::submit! { Contribution::new(bilibi_module_registry::contributions::SEEDS, "base/system.instance-role", provenance!(), &bilibi_module_registry::contributions::Seed {
    version: "system-instance-role-v1", mode: bilibi_module_registry::contributions::SeedMode::Production,
    action: bilibi_module_registry::contributions::SeedAction::Service { service: "base/system.install-role", operation: "initialize" },
}) }
