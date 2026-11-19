//! The shapes of `ai.providers` and `ai.provider_keys`, checked on every
//! write and again on every read.
use serde_json::Value;
use std::collections::BTreeMap;

/// The most providers one installation lists.
pub(crate) const PROVIDERS_MAX: usize = 20;
const KEY_MAX: usize = 4096;

/// One configured OpenAI-compatible provider. A hosted provider (the
/// default) is public HTTPS only; `local` is the operator's explicit choice
/// of a server on this machine or a private network.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Provider {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub local: bool,
}
impl Provider {
    /// The chat-completions endpoint under the base URL.
    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

/// A provider name: 1 to 64 lower-case letters, digits, `-`, `_` or `.`,
/// starting with a letter or digit.
pub(crate) fn name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_' | '.')
        })
}

/// An http or https URL with a host and no credentials, query or fragment;
/// https only for a hosted provider.
fn base_url(value: &str, local: bool) -> bool {
    value.len() <= 2048
        && url::Url::parse(value).is_ok_and(|url| {
            (url.scheme() == "https" || local && url.scheme() == "http")
                && url.host_str().is_some_and(|host| !host.is_empty())
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
}

fn model(value: &str) -> bool {
    (1..=255).contains(&value.chars().count())
        && !value.trim().is_empty()
        && !value.chars().any(char::is_control)
}

/// The providers in order, or `None` for a value of the wrong shape: not a
/// list, more than [`PROVIDERS_MAX`], an unknown or missing field, a bad
/// name, URL or model, a `local` that is not a boolean, an http URL on a
/// hosted provider, or a repeated name.
pub(crate) fn parse(value: &Value) -> Option<Vec<Provider>> {
    let items = value.as_array()?;
    if items.len() > PROVIDERS_MAX {
        return None;
    }
    let mut providers: Vec<Provider> = Vec::with_capacity(items.len());
    for item in items {
        let object = item.as_object()?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "base_url" | "model" | "local"))
        {
            return None;
        }
        let field = |key: &str| object.get(key)?.as_str();
        let local = match object.get("local") {
            None => false,
            Some(flag) => flag.as_bool()?,
        };
        let provider = Provider {
            name: field("name").filter(|value| name(value))?.to_owned(),
            base_url: field("base_url")
                .filter(|value| base_url(value, local))?
                .to_owned(),
            model: field("model").filter(|value| model(value))?.to_owned(),
            local,
        };
        if providers.iter().any(|seen| seen.name == provider.name) {
            return None;
        }
        providers.push(provider);
    }
    Some(providers)
}

/// The API keys by provider name (empty for null), or `None` for a value of
/// the wrong shape. A key is 1 to 4096 characters with no whitespace or
/// control character. A key whose provider is gone is kept and unused.
pub(crate) fn keys(value: &Value) -> Option<BTreeMap<String, String>> {
    if value.is_null() {
        return Some(BTreeMap::new());
    }
    let object = value.as_object()?;
    if object.len() > PROVIDERS_MAX {
        return None;
    }
    object
        .iter()
        .map(|(provider, key)| {
            let key = key.as_str().filter(|key| {
                (1..=KEY_MAX).contains(&key.len())
                    && !key
                        .chars()
                        .any(|character| character.is_whitespace() || character.is_control())
            })?;
            name(provider).then(|| (provider.clone(), key.to_owned()))
        })
        .collect()
}
