//! Reading an operation's input and refusing it. The registry has already
//! checked the input against the operation's schema, so these readers only
//! settle absent, blank and padded text; a handler never re-checks types.
use crate::{schema::FieldErrors, Error};
use serde_json::Value;

/// Refuse one input field with a message the caller can show.
///
/// Shared helper: `return Err(bilibi_agent_api::invalid("code", "is already taken"));`
pub fn invalid(field: &str, message: impl Into<String>) -> Error {
    Error::InvalidInput(FieldErrors::from([(
        field.to_owned(),
        vec![message.into()],
    )]))
}

/// Read a text input, trimmed; empty when the field is absent or null.
///
/// Shared helper: `let code = bilibi_agent_api::text_field(input, "code");`
pub fn text_field(input: &Value, field: &str) -> String {
    input[field].as_str().unwrap_or_default().trim().to_owned()
}

/// Read an optional text input, trimmed; `None` when absent, null or blank.
///
/// Shared helper: `let note = bilibi_agent_api::optional_text_field(input, "note");`
pub fn optional_text_field(input: &Value, field: &str) -> Option<String> {
    let text = input[field].as_str()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
