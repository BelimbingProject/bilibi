//! The Bilibi wordmark (`DESIGN.md`, Character): the mark, then "bilibi" in
//! lowercase in its own face, on one line. The `wordmark` macro in
//! `templates/components.html` owns the DOM and `.ui-wordmark` in
//! `assets/css/app.css` the look; this model is how a crate outside
//! `base/ui` draws it.
use askama::Template;

/// Where the Bilibi wordmark is drawn: in the workspace top bar, or as the
/// brand line of a credential page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WordmarkSize {
    #[default]
    Bar,
    Credential,
}

impl WordmarkSize {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Bar => "ui-wordmark-bar",
            Self::Credential => "ui-wordmark-credential",
        }
    }

    /// The mark's box in CSS pixels, reserved before the stylesheet loads.
    pub fn mark_pixels(&self) -> u8 {
        match self {
            Self::Bar => 19,
            Self::Credential => 35,
        }
    }
}

/// The Bilibi wordmark: the mark, then "bilibi" in its own face, on one
/// line. `href` makes it a link named `label`. Use it for Bilibi's own name
/// only; a tenant's portal title keeps the interface face.
#[derive(Template, Debug)]
#[template(
    source = r#"{% import "components.html" as ui %}{% call ui::wordmark(size=size, href=href, id=id, label=label) %}{% endcall %}"#,
    ext = "html"
)]
pub struct Wordmark<'a> {
    pub size: WordmarkSize,
    pub href: &'a str,
    pub id: &'a str,
    pub label: &'a str,
}

impl Default for Wordmark<'_> {
    fn default() -> Self {
        Self {
            size: WordmarkSize::Bar,
            href: "",
            id: "",
            label: "Bilibi home",
        }
    }
}
