//! The wordmark (`DESIGN.md`, Character): the installation's own logo and
//! name when its `Brand` has them, otherwise the Bilibi mark, then "bilibi" in
//! lowercase in its own face, on one line. The `wordmark` macro in
//! `templates/components.html` owns the DOM and `.ui-wordmark` in
//! `assets/css/app.css` the look.

/// Where the wordmark is drawn: in the workspace top bar, or as the brand
/// line of a credential page.
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
