//! HTML printing: an owner supplies trusted HTML and Base prints it to a private
//! PDF through an isolated Chromium (`engine`). Two entry points:
//!
//! * `render_inline` prints the HTML in an offline browser.
//! * `render_view` has the browser fetch it from this app's single-use signed
//!   `/pdf/render/{token}` route, so the template renders under the same
//!   process, scope and live authority checks as everything else. Claims never
//!   leave this process: the token only names a waiting `render_view` call, and
//!   the route hands the browser whatever that call produces.
use crate::{
    engine, Artifacts, Error, Metadata, Operation, Owned, Owner, PdfArtifact, PdfOptions,
    Reference, Result,
};
use base64::Engine as _;
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::Scope;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_postgres::Client;

/// Mounted by the Web host; the token is the only credential it takes.
pub const RENDER_ROUTE: &str = "/pdf/render/{token}";
const PREFIX: &str = "/pdf/render/";
/// Headers the host must send with every response from the render route. The
/// policy lets a template carry only embedded styles, images and fonts.
pub const RENDER_HEADERS: [(&str, &str); 6] = [
    ("cache-control", "private, no-store"),
    ("x-content-type-options", "nosniff"),
    ("x-robots-tag", "noindex, nofollow"),
    ("referrer-policy", "no-referrer"),
    ("content-type", "text/html; charset=utf-8"),
    (
        "content-security-policy",
        "default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
    ),
];

/// Physical sizes available to a business print layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfPaper {
    #[default]
    A4,
    Letter,
}

impl PdfPaper {
    fn format(self) -> &'static str {
        match self {
            Self::A4 => "A4",
            Self::Letter => "Letter",
        }
    }
}

/// Complete HTML and its physical paper size, prepared by a trusted server-side renderer.
pub struct InlineDocument {
    html: String,
    paper: PdfPaper,
}

impl InlineDocument {
    pub fn new(html: String, paper: PdfPaper) -> Self {
        Self { html, paper }
    }

    pub fn paper(&self) -> PdfPaper {
        self.paper
    }

    pub fn html(&self) -> &str {
        &self.html
    }
}

pub type HtmlFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
/// Implement on an owner that prints HTML. Base authorizes `Create` before
/// every call to `render_html`, which selects only the data this scope's actor
/// may see and returns a complete document: styles, images and fonts must be
/// embedded (`data:` URLs), because scripts and every other resource are
/// blocked. Never print arbitrary user-supplied HTML or URLs, and never log it.
pub trait HtmlOwner: Owner {
    fn render_html<'a>(
        &'a self,
        scope: &'a Scope,
        company: OrganisationId,
        reference: &'a Reference,
        data: &'a Value,
    ) -> HtmlFuture<'a>;
}

/// What the waiting `render_view` call tells the route to answer.
pub(crate) enum Reply {
    Html(String),
    Refused,
    Failed,
}
/// The route's answer: `Page` is 200 with `RENDER_HEADERS`, `NotFound` is an
/// empty 404 (unknown, forged, expired, replayed or refused alike), `Failed` an
/// empty 500. Neither failure says why, and none echoes document contents.
pub enum Served {
    Page(String),
    NotFound,
    Failed,
}

struct Pending {
    expiry: Instant,
    wait: Duration,
    request: oneshot::Sender<oneshot::Sender<Reply>>,
}
/// Signed, random, single-use capabilities held only in this process. A fresh
/// HMAC key per boot means a token is checked before any claim is touched, and
/// removing the entry under the lock is what makes replay impossible.
pub(crate) struct Tokens {
    key: [u8; 32],
    pending: Mutex<HashMap<String, Pending>>,
}
type Mac256 = Hmac<Sha256>;
const URL_SAFE: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
impl Tokens {
    pub(crate) fn new() -> Result<Self> {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|_| Error::PdfFailed)?;
        Ok(Self {
            key,
            pending: Mutex::new(HashMap::new()),
        })
    }
    fn mac(&self, id: &str) -> Mac256 {
        let mut mac = Mac256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(id.as_bytes());
        mac
    }
    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    fn issue(&self, ttl: Duration, wait: Duration, limit: usize) -> Result<Issued<'_>> {
        let mut raw = [0u8; 24];
        getrandom::fill(&mut raw).map_err(|_| Error::PdfFailed)?;
        let id = URL_SAFE.encode(raw);
        let token = format!(
            "{id}.{}",
            URL_SAFE.encode(self.mac(&id).finalize().into_bytes())
        );
        let (request, receiver) = oneshot::channel();
        let now = Instant::now();
        let mut pending = self.locked();
        pending.retain(|_, claim| claim.expiry > now && !claim.request.is_closed());
        if pending.len() >= limit {
            return Err(Error::RenderTokensFull);
        }
        pending.insert(
            token.clone(),
            Pending {
                expiry: now + ttl,
                wait,
                request,
            },
        );
        Ok(Issued {
            tokens: self,
            token,
            receiver,
        })
    }
    /// Verify the signature first, so a forged or altered token touches nothing.
    fn consume(&self, token: &str) -> Option<Pending> {
        let (id, signature) = token.split_once('.')?;
        if token.len() > 128 {
            return None;
        }
        let signature = URL_SAFE.decode(signature).ok()?;
        self.mac(id).verify_slice(&signature).ok()?;
        let claim = self.locked().remove(token)?;
        (claim.expiry > Instant::now()).then_some(claim)
    }
}
/// Removes its claim when the `render_view` call ends, however it ends.
struct Issued<'a> {
    tokens: &'a Tokens,
    token: String,
    receiver: oneshot::Receiver<oneshot::Sender<Reply>>,
}
impl Drop for Issued<'_> {
    fn drop(&mut self) {
        self.tokens.locked().remove(&self.token);
    }
}
struct Abort<T>(JoinHandle<T>);
impl<T> Drop for Abort<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Artifacts {
    /// Answer `GET /pdf/render/{token}`. Consumes the token before doing
    /// anything else; the waiting `render_view` call reauthorizes the live
    /// actor and runs the owner's template, so this takes no session, actor,
    /// template or data from the request.
    pub async fn serve_render(&self, token: &str) -> Served {
        let Some(claim) = self.tokens.consume(token) else {
            return Served::NotFound;
        };
        let (reply, answer) = oneshot::channel();
        if claim.request.send(reply).is_err() {
            return Served::NotFound;
        }
        match tokio::time::timeout(claim.wait, answer).await {
            Ok(Ok(Reply::Html(html))) => Served::Page(html),
            Ok(Ok(Reply::Failed)) => Served::Failed,
            _ => Served::NotFound,
        }
    }
    async fn pdf_config(&self, client: &Client, paper: Option<PdfPaper>) -> Result<engine::Config> {
        let get = |key: &'static str| async move {
            self.settings
                .get_global(client, key)
                .await
                .map_err(Error::from)
        };
        let format = match paper {
            Some(paper) => paper.format().to_owned(),
            None => get("pdf.paper.format")
                .await?
                .as_str()
                .ok_or(Error::Policy)?
                .to_owned(),
        };
        let seconds = get("pdf.render_timeout_seconds").await?.as_u64();
        let launch = get("pdf.launch_timeout_seconds").await?.as_u64();
        let max = get("artifacts.max_bytes").await?.as_u64();
        Ok(engine::Config {
            binary: get("pdf.chromium.binary")
                .await?
                .as_str()
                .ok_or(Error::PdfEngineUnavailable)?
                .to_owned(),
            format,
            background: get("pdf.paper.print_background")
                .await?
                .as_bool()
                .unwrap_or(true),
            timeout: Duration::from_secs(
                seconds
                    .filter(|s| (1..=600).contains(s))
                    .ok_or(Error::Policy)?,
            ),
            launch_timeout: Duration::from_secs(
                launch
                    .filter(|s| (1..=600).contains(s))
                    .ok_or(Error::Policy)?,
            ),
            max_bytes: max.filter(|max| *max > 0).ok_or(Error::Policy)? as usize,
        })
    }
    /// Render an authorized inline document to PDF bytes without storing it.
    pub async fn print_document(
        &self,
        client: &Client,
        document: InlineDocument,
    ) -> Result<Vec<u8>> {
        let config = self.pdf_config(client, Some(document.paper)).await?;
        print_html(&config, document.html).await
    }
}

impl<O: Owner> Owned<'_, O> {
    /// Print a complete business layout through the same offline renderer as
    /// `render_inline`. The owner authorizes its source before building the
    /// document. Artifacts rechecks create authority before Chromium and publication.
    pub async fn render_inline_document(
        &self,
        client: &mut Client,
        reference: Reference,
        document: InlineDocument,
        options: &PdfOptions,
    ) -> Result<PdfArtifact> {
        options.validate()?;
        self.authorize(&*client, Operation::Create, Some(&reference))
            .await?;
        let bytes = self.store.print_document(client, document).await?;
        self.persist_pdf(client, reference, &bytes, options).await
    }
}

async fn print_html(config: &engine::Config, html: String) -> Result<Vec<u8>> {
    if html.is_empty() || html.len() > config.max_bytes {
        return Err(Error::Invalid);
    }
    engine::print(config, engine::Source::Html(html)).await
}

impl<O: HtmlOwner> Owned<'_, O> {
    async fn html(&self, reference: &Reference, data: &Value, max: usize) -> Result<String> {
        let html = self
            .owner
            .render_html(self.scope, self.company, reference, data)
            .await?;
        if html.is_empty() || html.len() > max {
            return Err(Error::Invalid);
        }
        Ok(html)
    }
    /// Authorize, print the owner's HTML in an offline browser and store the
    /// result as a private `application/pdf`. Needs no origin or HTTP endpoint.
    /// Same call rules as `put`; publication reauthorizes.
    pub async fn render_inline(
        &self,
        client: &mut Client,
        reference: Reference,
        data: &Value,
    ) -> Result<Metadata> {
        Ok(self
            .render_inline_with(client, reference, data, &PdfOptions::default())
            .await?
            .document)
    }
    /// `render_inline` with provenance versions and optional AES-256 protection.
    pub async fn render_inline_with(
        &self,
        client: &mut Client,
        reference: Reference,
        data: &Value,
        options: &PdfOptions,
    ) -> Result<PdfArtifact> {
        options.validate()?;
        if !data.is_object() {
            return Err(Error::Invalid);
        }
        self.authorize(&*client, Operation::Create, Some(&reference))
            .await?;
        let config = self.store.pdf_config(&*client, None).await?;
        let html = self
            .owner
            .render_html(self.scope, self.company, &reference, data)
            .await?;
        let bytes = print_html(&config, html).await?;
        self.persist_pdf(client, reference, &bytes, options).await
    }
    /// Like `render_inline`, but the browser fetches the page from this app's
    /// single-use signed `/pdf/render/{token}` URL, which needs
    /// `pdf.render_origin` to reach this process (the issuing node). The route
    /// answers only once, only for this call, and the template runs after a
    /// fresh authorization of the live actor.
    pub async fn render_view(
        &self,
        client: &mut Client,
        reference: Reference,
        data: &Value,
    ) -> Result<Metadata> {
        Ok(self
            .render_view_with(client, reference, data, &PdfOptions::default())
            .await?
            .document)
    }
    /// `render_view` with provenance versions and optional AES-256 protection.
    pub async fn render_view_with(
        &self,
        client: &mut Client,
        reference: Reference,
        data: &Value,
        options: &PdfOptions,
    ) -> Result<PdfArtifact> {
        options.validate()?;
        if !data.is_object() {
            return Err(Error::Invalid);
        }
        self.authorize(&*client, Operation::Create, Some(&reference))
            .await?;
        let config = self.store.pdf_config(&*client, None).await?;
        let setting = |key: &'static str| {
            let settings = &self.store.settings;
            let client = &*client;
            async move { settings.get_global(client, key).await.map_err(Error::from) }
        };
        let origin = setting("pdf.render_origin").await?;
        let origin = origin
            .as_str()
            .map(|o| o.trim_end_matches('/'))
            .unwrap_or_default();
        if origin.is_empty() {
            return Err(Error::RenderOrigin);
        }
        let ttl = setting("pdf.signed_url_ttl_seconds")
            .await?
            .as_u64()
            .ok_or(Error::Policy)?;
        let limit = setting("pdf.max_render_tokens")
            .await?
            .as_u64()
            .ok_or(Error::Policy)?;
        let mut issued =
            self.store
                .tokens
                .issue(Duration::from_secs(ttl), config.timeout, limit as usize)?;
        let url = format!("{origin}{PREFIX}{}", issued.token);
        let max = config.max_bytes;
        let mut printing = Abort(tokio::spawn(async move {
            engine::print(&config, engine::Source::Url(url)).await
        }));
        let mut served = false;
        let bytes = loop {
            tokio::select! {
                printed = &mut printing.0 => break printed.map_err(|_| Error::PdfFailed)??,
                request = &mut issued.receiver, if !served => {
                    served = true;
                    let Ok(reply) = request else { continue };
                    let answer = match self.authorize(&*client, Operation::Create, Some(&reference)).await {
                        Err(_) => Reply::Refused,
                        Ok(()) => match self.html(&reference, data, max).await {
                            Ok(html) => Reply::Html(html),
                            Err(_) => Reply::Failed,
                        },
                    };
                    let _ = reply.send(answer);
                }
            }
        };
        drop(issued);
        self.persist_pdf(client, reference, &bytes, options).await
    }
}
