//! Stable software identity types, supplied by the executable host at startup.
//! Bilibi-specific version contract (docs/versions.md).
use crate::Layer;

/// Who reviewed the code carried by a composed Extension (ADR 28).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionCode {
    /// Platform or installation-operator reviewed code, admitted everywhere.
    Reviewed,
    /// Customer-supplied code, admitted only by an isolated installation.
    Customer,
}
impl ExtensionCode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "reviewed" => Some(Self::Reviewed),
            "customer" => Some(Self::Customer),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reviewed => "reviewed",
            Self::Customer => "customer",
        }
    }
}

/// Whether the mounted Domains and Extensions were pinned when this program was built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinState {
    /// Built from `.local/composition`: every version and commit is exact.
    Pinned,
    /// Built without `--pin`: a development build; only the Platform commit is known.
    Unpinned,
    /// A pin was present but describes another composition or other checkouts; its facts are not trusted.
    Stale(String),
}

/// A mounted Domain or Extension in this program.
#[derive(Clone, Debug)]
pub struct Container {
    pub layer: Layer,
    /// The container ID, `finance`.
    pub id: &'static str,
    /// Its module IDs, `finance/ledger`.
    pub modules: Vec<&'static str>,
    pub commit: Option<&'static str>,
    /// The release the container is at, `0.1.0`; `None` when unpinned or built from an untagged commit.
    pub version: Option<&'static str>,
    /// Extensions declare reviewed or customer code; Domains have no value.
    pub extension_code: Option<ExtensionCode>,
}

/// The whole program's make-up.
#[derive(Clone, Debug)]
pub struct Composition {
    /// The Platform version from `Cargo.toml`.
    pub version: &'static str,
    pub commit: Option<&'static str>,
    /// `Some("v0.1.0")` when the Platform commit is that release.
    pub tag: Option<&'static str>,
    pub pin: PinState,
    pub containers: Vec<Container>,
}
impl Composition {
    pub fn validate_installation_mode(&self, mode: &str) -> crate::Result<()> {
        validate_admission(mode, self.customer_extensions())
    }

    /// `true` when this build is exactly the release `v<version>`.
    pub fn released(&self) -> bool {
        self.tag.is_some()
    }
    /// The commit shortened to the usual twelve characters, if known.
    pub fn short_commit(&self) -> Option<&'static str> {
        self.commit.map(short)
    }
    /// Customer-code Extensions compiled into this program.
    pub fn customer_extensions(&self) -> Vec<&'static str> {
        self.containers
            .iter()
            .filter(|container| container.extension_code == Some(ExtensionCode::Customer))
            .map(|container| container.id)
            .collect()
    }
}
impl Container {
    pub fn short_commit(&self) -> Option<&'static str> {
        self.commit.map(short)
    }
}
fn short(commit: &'static str) -> &'static str {
    commit.get(..12).unwrap_or(commit)
}

pub(super) fn validate_admission<'a>(
    mode: &str,
    customers: impl IntoIterator<Item = &'a str>,
) -> crate::Result<()> {
    if !matches!(mode, "shared" | "isolated") {
        return Err(crate::RegistryError::Graph(
            "Invalid installation.mode".into(),
        ));
    }
    if mode == "shared" {
        if let Some(id) = customers.into_iter().next() {
            return Err(crate::RegistryError::Graph(format!(
                "Extension {id} requires an isolated installation; use an isolated installation, or prepare a code-free release"
            )));
        }
    }
    Ok(())
}
