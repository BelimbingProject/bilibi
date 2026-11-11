//! What a Domain contributes behind a public visitor session, and what it is
//! given. The Platform carries opaque values; it never reads a message.
use crate::origin;
use bilibi_database::{Error, Result};
use bilibi_module_registry::{Consumer, Entry, RegistryError};
use serde_json::Value;
use std::{collections::BTreeSet, future::Future, net::IpAddr, pin::Pin, sync::Arc};
use tokio::sync::broadcast;

/// The consumer a Domain contributes a [`Channel`] to.
pub const CHANNELS: &str = "base/visitor.channels";

/// A Domain's visitor channel: `key` is what a page names in its embed
/// (`data-channel`), `service` resolves to a [`ChannelService`].
pub struct Channel {
    pub key: &'static str,
    pub service: &'static str,
}
/// Registry service type: contribute its service ID in `Channel.service`.
pub struct ChannelService(pub Arc<dyn VisitorChannel>);

/// 1–64 lowercase letters, digits, `.`, `_`, `-` and `/`, starting with a
/// letter or digit: the same shape the session table checks.
pub fn valid_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && key.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-' | b'/')
        })
}
fn validate(entries: &[Entry]) -> bilibi_module_registry::Result<()> {
    let mut seen = BTreeSet::new();
    for entry in entries {
        let channel = entry.get::<Channel>()?;
        if !valid_key(channel.key) || channel.service.is_empty() || !seen.insert(channel.key) {
            return Err(RegistryError::Contribution(format!(
                "invalid or duplicate visitor channel {} ({}) at {}:{}",
                entry.id, channel.key, entry.provenance.source, entry.provenance.line
            )));
        }
    }
    Ok(())
}
inventory::submit! { Consumer::new::<Channel>(CHANNELS, "base/visitor", validate) }

/// A website a Domain lets visitors chat from: the tenant it belongs to, the
/// Domain's own key for it and the origins its pages are served from.
#[derive(Clone, Debug)]
pub struct Site {
    tenant: i64,
    key: String,
    origins: Vec<String>,
}
impl Site {
    /// `key` is what the page's embed names (1–128 printable characters);
    /// `origins` is the allow-list, each `scheme://host[:port]` exactly as a
    /// browser sends it. Read both from the Domain's own record of the site,
    /// never from the request.
    pub fn new(tenant: i64, key: &str, origins: Vec<String>) -> Result<Self> {
        if tenant <= 0 || !printable(key, 128) {
            return Err(Error::Invalid);
        }
        Ok(Self {
            tenant,
            key: key.to_owned(),
            origins: origin::list(origins)?,
        })
    }
    pub fn tenant(&self) -> i64 {
        self.tenant
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    /// The proof that `origin`, the `Origin` header the request carried, is
    /// on this site's allow-list. Anything else, including a missing header,
    /// is `Forbidden`.
    pub fn admit(&self, origin: Option<&str>) -> Result<Admission> {
        let origin = origin
            .and_then(origin::parse)
            .filter(|origin| self.origins.contains(origin))
            .ok_or(Error::Forbidden)?;
        Ok(Admission {
            tenant: self.tenant,
            site: self.key.clone(),
            origin,
        })
    }
}
/// A site's allow-list accepted an origin. Only [`Site::admit`] makes one, and
/// [`Visitors::open`](crate::Visitors::open) takes nothing else.
#[derive(Debug)]
pub struct Admission {
    pub(crate) tenant: i64,
    pub(crate) site: String,
    pub(crate) origin: String,
}

pub(crate) fn printable(text: &str, max: usize) -> bool {
    (1..=max).contains(&text.chars().count()) && !text.chars().any(char::is_control)
}

/// Who a visitor session acts for, sealed by the host after it proved the
/// token and the browser origin. It is not a staff `Scope`, converts to
/// nothing that is, and has no constructor or `Deserialize`: a channel acts
/// only on the records its own `subject` names.
#[derive(Clone, Debug)]
pub struct VisitorScope {
    pub(crate) tenant: i64,
    pub(crate) session: String,
    pub(crate) channel: String,
    pub(crate) site: String,
    pub(crate) subject: String,
}
impl VisitorScope {
    /// The tenant the site belongs to.
    pub fn tenant(&self) -> i64 {
        self.tenant
    }
    /// The session's public id, for a Domain to record and to revoke by.
    pub fn session(&self) -> &str {
        &self.session
    }
    pub fn channel(&self) -> &str {
        &self.channel
    }
    pub fn site(&self) -> &str {
        &self.site
    }
    /// What the channel chose when it admitted the visitor (the conversation
    /// the visitor may read and write). Bind every read and write to it.
    pub fn subject(&self) -> &str {
        &self.subject
    }
}

/// What the browser sent when it asked for a session. `details` is the JSON
/// the embed was configured to send (at most 4 KiB), untrusted.
#[derive(Debug)]
pub struct Hello {
    pub remote: Option<IpAddr>,
    pub details: Value,
}
/// One thing the visitor sent over an open session.
#[derive(Debug)]
pub enum Frame {
    /// A message (at most 16 KiB of JSON).
    Message(Value),
    /// The visitor is typing, or stopped (at most 1 KiB).
    Typing(Value),
    /// A file, complete. Store it through Base Media's attachment path; its
    /// type, size and scan decide whether anyone may read it.
    Upload(Upload),
}
#[derive(Debug)]
pub struct Upload {
    /// The name the browser gave the file: untrusted, at most 255 characters.
    pub name: String,
    pub bytes: Vec<u8>,
}
/// What the channel has for the visitor since the cursor it last gave.
#[derive(Debug, Default)]
pub struct Outbox {
    pub items: Vec<Value>,
    /// Handed back by the browser on its next `since`, so a dropped
    /// connection loses nothing. At most 128 characters.
    pub cursor: Option<String>,
}
pub type ChannelFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The conversation behind a visitor session. The host proves the origin, the
/// token and the limits; every method here is the Domain's own decision.
/// `Forbidden`, `Unauthorized` and `NotFound` reach the visitor as one
/// refusal, `Invalid` as a rejected value, anything else as unavailable.
pub trait VisitorChannel: Send + Sync {
    /// The site a page named, from the Domain's own records, or `None`. Asked
    /// before any session exists and again each time one is used, so a site
    /// the Domain closed or a removed origin stops its sessions at once.
    fn site<'a>(&'a self, key: &'a str) -> ChannelFuture<'a, Option<Site>>;
    /// A new visitor on an allow-listed origin: start (or find) the
    /// conversation and return its subject (at most 128 printable
    /// characters).
    fn admit<'a>(&'a self, site: &'a Site, hello: &'a Hello) -> ChannelFuture<'a, String>;
    /// One frame from the visitor. The reply, if any, goes to the visitor as
    /// the acknowledgement of that frame.
    fn receive<'a>(
        &'a self,
        scope: &'a VisitorScope,
        frame: Frame,
    ) -> ChannelFuture<'a, Option<Value>>;
    /// What the visitor has not seen yet. `cursor` is the last one this
    /// channel returned to this visitor, or `None` for everything.
    fn since<'a>(
        &'a self,
        scope: &'a VisitorScope,
        cursor: Option<&'a str>,
    ) -> ChannelFuture<'a, Outbox>;
    /// A wake-up for this visitor's open socket, sent whenever `since` has
    /// something new. Does no I/O.
    fn watch(&self, scope: &VisitorScope) -> Option<broadcast::Receiver<()>>;
}
