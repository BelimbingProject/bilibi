//! The host-owned cover for a page that still owes 2FA.
//!
//! The protected owner is never rendered beneath this component: the quiet
//! shapes are placeholders, and only the factor provider's prompt is embedded.
use crate::Html;
use askama::Template;

/// A 2FA prompt floating over a protected page's placeholder.
#[derive(Template, Clone, Debug)]
#[template(path = "step_up_gate.html")]
pub struct StepUpGate {
    pub prompt: String,
    pub return_to: String,
    pub title: String,
    pub body: Html,
}

impl StepUpGate {
    pub fn action(title: &str, heading: &str, body: Html) -> askama::Result<Self> {
        Self::action_with_heading(title, Html::text(heading), body)
    }

    pub fn action_with_heading(title: &str, heading: Html, body: Html) -> askama::Result<Self> {
        let body = Html::render(&ActionCard { heading, body })?;
        Ok(Self::new("", "", title, body))
    }

    pub fn new(
        prompt: impl Into<String>,
        return_to: impl Into<String>,
        title: impl Into<String>,
        body: Html,
    ) -> Self {
        Self {
            prompt: prompt.into(),
            return_to: return_to.into(),
            title: title.into(),
            body,
        }
    }

    /// Covers a page body already rendered by a trusted server-owned page.
    ///
    /// Request data and stored user content must be rendered through their
    /// typed components before the host passes the resulting page body here.
    pub fn rendered_page(
        prompt: impl Into<String>,
        return_to: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self::new(prompt, return_to, title, Html::raw(body.into()))
    }
}

#[derive(Template)]
#[template(
    source = r#"{% import "components.html" as ui %}{% call ui::card(title=heading) %}{{ body|safe }}{% endcall %}"#,
    ext = "html"
)]
struct ActionCard {
    heading: Html,
    body: Html,
}
