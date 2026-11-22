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
}
