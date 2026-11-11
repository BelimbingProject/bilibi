//! Browser origins: the only form an allow-list entry or an `Origin` header
//! may take, `scheme://host[:port]` with nothing after it.
use bilibi_database::{Error, Result};
use url::Url;

/// The most origins one site may list.
pub const MAX_ORIGINS: usize = 50;

/// The serialized origin `text` names, or `None` when it is anything else: a
/// path, a query, a fragment, a user name, another scheme, an uppercase host,
/// a default port written out, or the opaque `null`.
pub fn parse(text: &str) -> Option<String> {
    if text.len() > 255 || !text.is_ascii() {
        return None;
    }
    let url = Url::parse(text).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let origin = url.origin();
    if !origin.is_tuple() {
        return None;
    }
    let canonical = origin.ascii_serialization();
    (canonical == text).then_some(canonical)
}

/// The allow-list a site names: at least one and at most [`MAX_ORIGINS`]
/// origins, each exactly as a browser sends it.
pub fn list(origins: Vec<String>) -> Result<Vec<String>> {
    if origins.is_empty() || origins.len() > MAX_ORIGINS {
        return Err(Error::Invalid);
    }
    let mut parsed = Vec::with_capacity(origins.len());
    for origin in &origins {
        let canonical = parse(origin).ok_or(Error::Invalid)?;
        if !parsed.contains(&canonical) {
            parsed.push(canonical);
        }
    }
    Ok(parsed)
}
