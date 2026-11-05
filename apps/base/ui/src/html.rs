//! A rendered HTML fragment that other templates may embed unescaped.
//!
//! Only shared fragments bypass escaping: an `Html` value comes from a
//! template's own output ([`Html::render`]) or from text escaped here
//! ([`Html::text`]) or from the filtered parser output in [`crate::Markdown`]. [`Html::raw`] exists for shared components that assemble
//! markup; page code never builds an `Html` from request data with it.
use std::fmt;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Html(String);

impl Html {
    pub const EMPTY: Html = Html(String::new());

    /// Escapes `text` for an HTML text or attribute position.
    pub fn text(text: &str) -> Self {
        let mut escaped = String::with_capacity(text.len());
        for character in text.chars() {
            match character {
                '&' => escaped.push_str("&amp;"),
                '<' => escaped.push_str("&lt;"),
                '>' => escaped.push_str("&gt;"),
                '"' => escaped.push_str("&quot;"),
                '\'' => escaped.push_str("&#x27;"),
                other => escaped.push(other),
            }
        }
        Html(escaped)
    }

    /// Wraps markup a shared component already rendered. Never pass request
    /// data here; escape it with [`Html::text`] or render it through a template.
    pub fn raw(markup: impl Into<String>) -> Self {
        Html(markup.into())
    }

    /// Renders a template into a fragment.
    pub fn render<T: askama::Template>(template: &T) -> askama::Result<Self> {
        template.render().map(Html)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Appends another fragment, for an actions row with several controls.
    pub fn push(&mut self, fragment: &Html) {
        self.0.push_str(&fragment.0);
    }
}

impl fmt::Display for Html {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl askama::filters::HtmlSafe for Html {}

impl FromIterator<Html> for Html {
    fn from_iter<I: IntoIterator<Item = Html>>(fragments: I) -> Self {
        let mut joined = Html::EMPTY;
        for fragment in fragments {
            joined.push(&fragment);
        }
        joined
    }
}
