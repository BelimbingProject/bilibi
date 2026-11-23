//! Whose name the staff application carries (`DESIGN.md`, Character, Adopter
//! brand): the installation's own name, logo and accent, or Bilibi's when the
//! installation set none. Display only: `base/branding` resolves it from the
//! installation settings and the host hands it to the shell, the credential
//! layout and the document. The `wordmark` macro draws it.

/// The product's own name, shown wherever the installation set none.
pub const PRODUCT: &str = "Bilibi";

/// The accent an installation draws its orientation marks in: the top edge of
/// a card, the current menu item, a selection, the primary button. A fixed
/// set, because the host's Content Security Policy allows no inline style and
/// each one is checked in both themes (`assets/css/app.css`, Adopter accent).
/// None is a status colour (`DESIGN.md`, Tokens).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Accent {
    #[default]
    Lime,
    Teal,
    Cyan,
    Indigo,
    Violet,
    Fuchsia,
}

impl Accent {
    pub const ALL: [Self; 6] = [
        Self::Lime,
        Self::Teal,
        Self::Cyan,
        Self::Indigo,
        Self::Violet,
        Self::Fuchsia,
    ];

    /// The stored value and the `data-accent` attribute.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lime => "lime",
            Self::Teal => "teal",
            Self::Cyan => "cyan",
            Self::Indigo => "indigo",
            Self::Violet => "violet",
            Self::Fuchsia => "fuchsia",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Lime => "Lime",
            Self::Teal => "Teal",
            Self::Cyan => "Cyan",
            Self::Indigo => "Indigo",
            Self::Violet => "Violet",
            Self::Fuchsia => "Fuchsia",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|accent| accent.as_str() == value)
    }
}

/// The brand of one render. `Brand::default()` is Bilibi's own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Brand {
    /// The installation's display name; empty keeps "Bilibi".
    pub name: String,
    /// A link to the installation's logo, already authorized by its owner
    /// (a signed Media link); `None` draws the name alone.
    pub logo: Option<String>,
    pub accent: Accent,
}

impl Brand {
    /// The name sentences, tab titles and labels use.
    pub fn product(&self) -> &str {
        if self.name.is_empty() {
            PRODUCT
        } else {
            &self.name
        }
    }

    /// Whether the installation shows its own name or logo in place of the
    /// Bilibi wordmark. An accent alone keeps the wordmark.
    pub fn custom(&self) -> bool {
        !self.name.is_empty() || self.logo.is_some()
    }

    pub fn favicon(&self) -> String {
        if let Some(logo) = &self.logo {
            logo.clone()
        } else if self.custom() {
            crate::Assets::url(&format!("/assets/brand-icon-{}.svg", self.accent.as_str()))
                .to_string()
        } else {
            crate::Assets::url("/favicon.svg").to_string()
        }
    }

    /// The `data-accent` value for `<html>`; empty for the default accent,
    /// which needs no attribute.
    pub fn accent_attr(&self) -> &'static str {
        match self.accent {
            Accent::Lime => "",
            accent => accent.as_str(),
        }
    }
}
