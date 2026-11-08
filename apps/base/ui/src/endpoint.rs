//! Public endpoints: the owner half of a `PublicEndpoint` contribution, for an
//! address a mail client, image proxy or mail provider calls with no session:
//! an open-tracking pixel, a click-tracking redirect, an RFC 8058 one-click
//! unsubscribe. The `{token}` in the path is the only credential, so the owner
//! answers from the record that token names and nothing else.
//!
//! The delivery rules live here, in [`Reply::headers`], so no owner composes
//! its own cache, framing or referrer policy: a reply is never framed, never
//! sniffed, never scripted, and a redirect sends no referrer. A redirect goes
//! only to a [`Target`], which an owner builds from the address its record
//! stored: the request carries no query string to build one from.
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

pub type EndpointFuture<'a, Error> =
    Pin<Box<dyn Future<Output = Result<Reply, Error>> + Send + 'a>>;

pub struct EndpointRequest<'a> {
    pub operation: &'static str,
    /// The path parameters; `token` is always present. The query string is
    /// never passed on, so no request can choose where a redirect goes.
    pub path: &'a HashMap<String, String>,
    /// The request's declared media type (a one-click POST is form data).
    pub content_type: Option<&'a str>,
    /// The bounded request body; empty for a GET.
    pub body: &'a [u8],
}
impl EndpointRequest<'_> {
    /// The bearer token from the path. Look it up by its digest, never by
    /// comparing the raw value in SQL or with `==`.
    pub fn token(&self) -> &str {
        self.path.get("token").map_or("", String::as_str)
    }
}

/// How long a client or proxy may reuse a reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cache {
    /// Every request reaches the owner (a tracking pixel counts each open).
    NoStore,
    /// The requesting client alone may reuse it this long; shared caches never.
    Private { max_age_seconds: u32 },
}

/// The owner's answer. Anything the owner refuses is an `Err`, which Web turns
/// into one fixed refusal whatever the cause, so an unknown token and a known
/// one whose stored target is unusable look the same.
#[derive(Debug)]
pub enum Reply {
    /// `200` with these bytes, as the media type the owner knows them to be.
    Bytes {
        bytes: Vec<u8>,
        content_type: String,
        cache: Cache,
    },
    /// `302` to an address the owner stored and validated.
    Redirect(Target),
    /// `204` with no body: the action was taken (or, to say nothing about
    /// whether the token exists, would have been).
    NoContent,
}

/// A transparent 1×1 GIF, the open-tracking pixel.
pub const PIXEL: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// Every reply's policy: no document, no script, no framing by anyone.
const POLICY: &str = "default-src 'none'; frame-ancestors 'none'; sandbox";

impl Reply {
    /// The transparent pixel, never cached so each open reaches the owner.
    pub fn pixel() -> Self {
        Self::Bytes {
            bytes: PIXEL.to_vec(),
            content_type: "image/gif".into(),
            cache: Cache::NoStore,
        }
    }
    pub fn status(&self) -> u16 {
        match self {
            Self::Bytes { .. } => 200,
            Self::Redirect(_) => 302,
            Self::NoContent => 204,
        }
    }
    /// The response headers, in one place. Every value is header-safe.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let cache = match self {
            Self::Bytes {
                cache: Cache::Private { max_age_seconds },
                ..
            } => format!("private, max-age={max_age_seconds}"),
            _ => "no-store".into(),
        };
        let mut headers = vec![
            ("cache-control", cache),
            ("x-content-type-options", "nosniff".into()),
            ("x-frame-options", "DENY".into()),
            ("content-security-policy", POLICY.into()),
        ];
        match self {
            Self::Bytes { content_type, .. } => {
                headers.push(("content-type", crate::download::content_type(content_type)))
            }
            // The destination learns nothing of the token link it came from.
            Self::Redirect(target) => {
                headers.push(("location", target.0.clone()));
                headers.push(("referrer-policy", "no-referrer".into()));
            }
            Self::NoContent => {}
        }
        headers
    }
    pub fn into_body(self) -> Vec<u8> {
        match self {
            Self::Bytes { bytes, .. } => bytes,
            _ => Vec::new(),
        }
    }
}

/// Longest redirect address accepted, as browsers and mail clients keep it.
const TARGET_LIMIT: usize = 2048;

/// A redirect destination the owner validated: an absolute `http` or `https`
/// address with a host and no user name or password, at most 2048 characters
/// once normalized. Build it from the address your record stored: an
/// endpoint sees no query string, so a link cannot name its own destination
/// (an open redirect).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target(String);
impl Target {
    /// `None` for anything else: a relative or protocol-relative address, a
    /// `javascript:`/`data:` URL, a `user@host` disguise, a backslash or
    /// control character.
    pub fn parse(stored: &str) -> Option<Self> {
        let stored = stored.trim();
        if stored.len() > TARGET_LIMIT
            || stored.contains('\\')
            || stored.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return None;
        }
        let url = url::Url::parse(stored).ok()?;
        let normalized: String = url.as_str().into();
        (matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|host| !host.is_empty())
            && url.username().is_empty()
            && url.password().is_none()
            && normalized.len() <= TARGET_LIMIT)
            .then_some(Self(normalized))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The owner's endpoint adapter. Web calls `answer` with no scope: the owner
/// finds the tenant and record from the token, answers the same for an
/// unknown token wherever a different answer would reveal which tokens exist,
/// and writes through its facade like any other unattended work.
pub trait Endpoint<Error>: Send + Sync {
    fn answer<'a>(&'a self, request: EndpointRequest<'a>) -> EndpointFuture<'a, Error>;
}
/// Registry service type (Web uses Database Error): contribute its service ID
/// in `PublicEndpoint.service`.
pub struct EndpointService<Error>(pub Arc<dyn Endpoint<Error>>);
