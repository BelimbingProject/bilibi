//! The public visitor edge (`base/visitor`): a website on another origin opens
//! a session for someone with no account, renews and ends it, and talks to the
//! Domain's channel over one socket. Nothing here reads a cookie, a staff or
//! customer session, or a CSRF secret, and nothing here can reach a staff
//! route: the token is a different credential in a different store, sent in a
//! body or a first frame, never a cookie. The Platform proves the browser
//! origin, the token, the limits and the site on every use; the Domain's
//! channel decides everything about the conversation.
//!
//! HTTP calls are "simple" cross-origin requests (`POST`, `text/plain`), so a
//! browser sends no preflight to an address nobody checked: the reply carries
//! `Access-Control-Allow-Origin` only when the origin is on the site's
//! allow-list, and every refusal is one fixed answer.
use crate::{api, App};
use axum::{
    body::Bytes,
    extract::{ws::Message, ws::WebSocket, ConnectInfo, State, WebSocketUpgrade},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension, Router,
};
use bilibi_agent::Throttle;
use bilibi_base::visitor::{
    Channel, ChannelService, Frame, Hello, Policy, Upload, VisitorScope, Visitors, CHANNELS,
};
use bilibi_database::Error;
use bilibi_module_registry::Services;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const SESSIONS: &str = "/visitor/v1/sessions";
pub const RENEW: &str = "/visitor/v1/session/renew";
pub const END: &str = "/visitor/v1/session/end";
pub const LIVE: &str = "/visitor/v1/live";
pub const CLIENT: &str = "/visitor/v1/client.js";
/// Whether `path` is the visitor edge's: no page or download may claim it.
pub fn reserved(path: &str) -> bool {
    path == "/visitor" || path.starts_with("/visitor/")
}

/// A frame the visitor sends is at most this many bytes of JSON; a message is
/// at most [`MESSAGE_LIMIT`] of them, a typing signal [`TYPING_LIMIT`].
const FRAME_LIMIT: usize = 32 * 1024;
const MESSAGE_LIMIT: usize = 16 * 1024;
const TYPING_LIMIT: usize = 1024;
const DETAILS_LIMIT: usize = 4 * 1024;
/// An upload chunk: the same size and eight-byte header (entry id, offset;
/// big-endian `u32`s) as the staff live socket's.
const CHUNK: usize = 64 * 1024;
const HEADER: usize = 8;
/// A socket that has not said who it is within this long is closed.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How often an open socket proves its session and site again.
const RECHECK: Duration = Duration::from_secs(10);
/// A sender may renew, end and reconnect this many times for each session it
/// may start: the budget of the calls that follow an opened session.
const RECONNECTS: u32 = 6;
/// Refused frames in a row after which a socket is closed.
const REFUSALS_BEFORE_CLOSE: u32 = 100;

/// The bound channels and the edge's own counters, apart from sign-in's and
/// the other public endpoints'.
pub struct Boundary {
    channels: HashMap<&'static str, Arc<ChannelService>>,
    visitors: Arc<Visitors>,
    senders: Throttle,
    sites: Throttle,
    frames: Throttle,
    sockets: Mutex<Sockets>,
}
#[derive(Default)]
struct Sockets {
    total: u32,
    per_session: HashMap<String, u32>,
}
impl Boundary {
    pub fn bind(services: &Services) -> bilibi_database::Result<Self> {
        let mut channels = HashMap::new();
        for entry in services.catalog()?.entries::<Channel>(CHANNELS)? {
            let channel = entry.get::<Channel>()?;
            channels.insert(channel.key, services.get(channel.service)?);
        }
        Ok(Self {
            channels,
            visitors: services.get(bilibi_base::visitor::SERVICE)?,
            senders: Throttle::default(),
            sites: Throttle::default(),
            frames: Throttle::default(),
            sockets: Mutex::default(),
        })
    }
    fn channel(&self, key: &str) -> Result<Arc<ChannelService>, Error> {
        self.channels.get(key).cloned().ok_or(Error::NotFound)
    }
}
/// One open socket's place; dropping it frees it.
struct Slot {
    app: App,
    session: Option<String>,
}
impl Slot {
    fn admit(app: &App, total: u32) -> Option<Self> {
        let mut sockets = app
            .visitor
            .sockets
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if sockets.total >= total {
            return None;
        }
        sockets.total += 1;
        Some(Self {
            app: app.clone(),
            session: None,
        })
    }
    /// Counts this socket against its session, or refuses at the limit.
    fn bind(&mut self, session: &str, per_session: u32) -> bool {
        let mut sockets = self
            .app
            .visitor
            .sockets
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let held = sockets.per_session.entry(session.to_owned()).or_default();
        if *held >= per_session {
            if *held == 0 {
                sockets.per_session.remove(session);
            }
            return false;
        }
        *held += 1;
        self.session = Some(session.to_owned());
        true
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        let mut sockets = self
            .app
            .visitor
            .sockets
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        sockets.total -= 1;
        if let Some(session) = &self.session {
            if let Some(held) = sockets.per_session.get_mut(session) {
                *held -= 1;
                if *held == 0 {
                    sockets.per_session.remove(session);
                }
            }
        }
    }
}

pub fn router() -> Router<App> {
    use axum::routing::{get, post};
    Router::new()
        .route(SESSIONS, post(open))
        .route(RENEW, post(renew))
        .route(END, post(end))
        .route(LIVE, get(upgrade))
        .route(CLIENT, get(client))
}

/// The embeddable script, versioned by its path (`v1`). A page on another
/// origin loads it with a script tag, so it is marked cross-origin readable.
async fn client() -> Response {
    let mut response = (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        bilibi_base::visitor::CLIENT_V1,
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    headers.insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("cross-origin"),
    );
    response
}

/// How a request is refused. Every refusal of a credential or a site is the
/// same `403 refused`, so a caller learns nothing about which part failed.
struct Refusal {
    status: StatusCode,
    code: &'static str,
}
impl Refusal {
    const LIMITED: Self = Self {
        status: StatusCode::TOO_MANY_REQUESTS,
        code: "rate_limited",
    };
}
impl From<Error> for Refusal {
    fn from(error: Error) -> Self {
        match error {
            Error::Unauthorized
            | Error::Forbidden
            | Error::NotFound
            | Error::StepUpRequired { .. } => Self {
                status: StatusCode::FORBIDDEN,
                code: "refused",
            },
            Error::Invalid | Error::Refused(_) | Error::Conflict => Self {
                status: StatusCode::BAD_REQUEST,
                code: "invalid",
            },
            error => {
                if crate::server_fault(&error) {
                    eprintln!("error: visitor edge failed: {error:?}");
                }
                Self {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    code: "unavailable",
                }
            }
        }
    }
}
impl From<tokio_postgres::Error> for Refusal {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}
/// A JSON reply that a browser on `origin` may read; `None` for a refusal.
fn reply(status: StatusCode, origin: Option<&str>, body: Value) -> Response {
    let mut response = (status, body.to_string()).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    if let Some(origin) = origin.and_then(|origin| HeaderValue::from_str(origin).ok()) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    response
}
fn refused(refusal: Refusal) -> Response {
    reply(refusal.status, None, json!({"error": refusal.code}))
}
fn origin_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
}
fn sender(peer: &Option<Extension<ConnectInfo<SocketAddr>>>, headers: &HeaderMap) -> String {
    api::client_address(peer.as_ref().map(|Extension(ConnectInfo(p))| *p), headers)
        .map_or_else(|| "unknown".into(), |ip| ip.to_string())
}
async fn policy(app: &App) -> Result<Policy, Refusal> {
    let client = app.db.get().await.map_err(Error::from)?;
    Ok(app.visitor.visitors.policy(&**client).await?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    channel: String,
    site: String,
    #[serde(default)]
    details: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenRequest {
    token: String,
}

async fn open(
    State(app): State<App>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: Bytes,
) -> Response {
    match open_session(&app, &headers, &peer, &body).await {
        Ok((body, origin)) => reply(StatusCode::OK, Some(&origin), body),
        Err(refusal) => refused(refusal),
    }
}
async fn open_session(
    app: &App,
    headers: &HeaderMap,
    peer: &Option<Extension<ConnectInfo<SocketAddr>>>,
    body: &[u8],
) -> Result<(Value, String), Refusal> {
    let boundary = &app.visitor;
    let sender = sender(peer, headers);
    let policy = policy(app).await?;
    if !boundary
        .senders
        .allow(&format!("open:{sender}"), policy.sessions_per_sender)
    {
        return Err(Refusal::LIMITED);
    }
    let request: OpenRequest = serde_json::from_slice(body).map_err(|_| Error::Invalid)?;
    if serde_json::to_vec(&request.details).map_or(true, |d| d.len() > DETAILS_LIMIT) {
        return Err(Error::Invalid.into());
    }
    let channel = boundary.channel(&request.channel)?;
    let site = channel
        .0
        .site(&request.site)
        .await?
        .ok_or(Error::Forbidden)?;
    if !boundary.sites.allow(
        &format!("open:{}:{}", request.channel, site.key()),
        policy.sessions_per_site,
    ) {
        return Err(Refusal::LIMITED);
    }
    let admission = site.admit(origin_of(headers))?;
    let hello = Hello {
        remote: api::client_address(peer.as_ref().map(|Extension(ConnectInfo(p))| *p), headers),
        details: request.details,
    };
    let subject = channel.0.admit(&site, &hello).await?;
    let client = app.db.get().await.map_err(Error::from)?;
    let opened = boundary
        .visitors
        .open(&**client, &admission, &request.channel, &subject)
        .await?;
    Ok((
        json!({"token": opened.token, "session": opened.session, "expires_in": opened.expires_in}),
        origin_of(headers).unwrap_or_default().to_owned(),
    ))
}

/// A token proved for the browser origin that presents it, then the site
/// proved again: the Domain still has it, for the same tenant, and the origin
/// is still on its allow-list.
async fn authenticate(
    app: &App,
    token: &str,
    origin: Option<&str>,
) -> Result<(VisitorScope, Arc<ChannelService>), Error> {
    let scope = {
        let client = app.db.get().await.map_err(Error::from)?;
        app.visitor.visitors.check(&**client, token, origin).await?
    };
    let channel = app.visitor.channel(scope.channel())?;
    let site = channel
        .0
        .site(scope.site())
        .await?
        .ok_or(Error::Unauthorized)?;
    if site.tenant() != scope.tenant() {
        return Err(Error::Unauthorized);
    }
    site.admit(origin)?;
    Ok((scope, channel))
}

async fn renew(
    State(app): State<App>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: Bytes,
) -> Response {
    match with_session(&app, &headers, &peer, &body, false).await {
        Ok((body, origin)) => reply(StatusCode::OK, Some(&origin), body),
        Err(refusal) => refused(refusal),
    }
}
async fn end(
    State(app): State<App>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: Bytes,
) -> Response {
    match with_session(&app, &headers, &peer, &body, true).await {
        Ok((body, origin)) => reply(StatusCode::OK, Some(&origin), body),
        Err(refusal) => refused(refusal),
    }
}
async fn with_session(
    app: &App,
    headers: &HeaderMap,
    peer: &Option<Extension<ConnectInfo<SocketAddr>>>,
    body: &[u8],
    ending: bool,
) -> Result<(Value, String), Refusal> {
    let boundary = &app.visitor;
    let policy = policy(app).await?;
    if !boundary.senders.allow(
        &format!("session:{}", sender(peer, headers)),
        policy.sessions_per_sender.saturating_mul(RECONNECTS),
    ) {
        return Err(Refusal::LIMITED);
    }
    let request: TokenRequest = serde_json::from_slice(body).map_err(|_| Error::Invalid)?;
    let origin = origin_of(headers);
    let (scope, _) = authenticate(app, &request.token, origin).await?;
    let client = app.db.get().await.map_err(Error::from)?;
    let body = if ending {
        boundary.visitors.end(&**client, &scope).await?;
        json!({"ended": true})
    } else {
        let renewed = boundary.visitors.renew(&**client, &scope).await?;
        json!({"expires_in": renewed.expires_in})
    };
    Ok((body, origin.unwrap_or_default().to_owned()))
}

async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    ws: WebSocketUpgrade,
) -> Response {
    // A browser always names the page's origin on a socket; a missing or
    // opaque one is not a browser a site's allow-list could have admitted.
    let Some(origin) = origin_of(&headers).and_then(bilibi_base::visitor::parse_origin) else {
        return refused(Refusal::from(Error::Forbidden));
    };
    let policy = match policy(&app).await {
        Ok(policy) => policy,
        Err(refusal) => return refused(refusal),
    };
    if !app.visitor.senders.allow(
        &format!("live:{}", sender(&peer, &headers)),
        policy.sessions_per_sender.saturating_mul(RECONNECTS),
    ) {
        return refused(Refusal::LIMITED);
    }
    let Some(slot) = Slot::admit(&app, policy.sockets_total) else {
        return refused(Refusal::LIMITED);
    };
    ws.max_message_size(FRAME_LIMIT + HEADER)
        .max_frame_size(FRAME_LIMIT + HEADER)
        .on_upgrade(move |socket| run(app, socket, origin, slot))
        .into_response()
}

/// What the browser sends as text. A binary frame is an upload chunk.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Inbound {
    Hello {
        token: String,
        #[serde(default)]
        cursor: Option<String>,
    },
    Message {
        #[serde(default)]
        id: Option<u64>,
        body: Value,
    },
    Typing {
        body: Value,
    },
    Upload {
        id: u32,
        name: String,
        size: u64,
    },
    UploadCancel {},
}
/// A file being received: bytes arrive in order and the channel is handed the
/// whole, so no half-file ever reaches a Domain.
struct Receiving {
    id: u32,
    name: String,
    size: usize,
    bytes: Vec<u8>,
}
async fn send(socket: &mut WebSocket, message: Value) -> bool {
    socket
        .send(Message::Text(message.to_string().into()))
        .await
        .is_ok()
}
async fn close(socket: &mut WebSocket, reason: &str) {
    send(socket, json!({"type": "closed", "reason": reason})).await;
    let _ = socket.send(Message::Close(None)).await;
}
fn code(refusal: &Refusal) -> &'static str {
    refusal.code
}
async fn wake(receiver: &mut Option<tokio::sync::broadcast::Receiver<()>>) {
    match receiver {
        Some(receiver) => {
            let _ = receiver.recv().await;
        }
        None => std::future::pending().await,
    }
}

async fn run(app: App, mut socket: WebSocket, origin: String, mut slot: Slot) {
    let boundary = &app.visitor;
    // Who is this? The first frame says, or the socket closes.
    let first = tokio::time::timeout(HELLO_WAIT, socket.recv()).await;
    let Ok(Some(Ok(Message::Text(text)))) = first else {
        return close(&mut socket, "refused").await;
    };
    let Ok(Inbound::Hello { token, cursor }) = serde_json::from_str(&text) else {
        return close(&mut socket, "refused").await;
    };
    if cursor.as_ref().is_some_and(|c| c.len() > 128) {
        return close(&mut socket, "refused").await;
    }
    let Ok((mut scope, channel)) = authenticate(&app, &token, Some(&origin)).await else {
        return close(&mut socket, "refused").await;
    };
    let Ok(policy) = policy(&app).await else {
        return close(&mut socket, "unavailable").await;
    };
    if !slot.bind(scope.session(), policy.sockets_per_session) {
        return close(&mut socket, "rate_limited").await;
    }
    let mut cursor = cursor;
    let mut watch = channel.0.watch(&scope);
    let mut revoked = boundary.visitors.subscribe_revocations();
    let mut recheck = tokio::time::interval(RECHECK);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    recheck.tick().await;
    let mut renewed_at = Instant::now();
    let mut receiving: Option<Receiving> = None;
    let mut refusals = 0u32;
    if !send(
        &mut socket,
        json!({"type": "ready", "session": scope.session()}),
    )
    .await
        || !deliver(&channel, &scope, &mut cursor, &mut socket).await
    {
        return;
    }
    loop {
        let message = tokio::select! {
            message = socket.recv() => message,
            _ = wake(&mut watch) => {
                if !deliver(&channel, &scope, &mut cursor, &mut socket).await { return; }
                continue;
            }
            _ = revoked.recv() => {
                match authenticate(&app, &token, Some(&origin)).await {
                    Ok(_) => continue,
                    Err(_) => return close(&mut socket, "ended").await,
                }
            }
            _ = recheck.tick() => {
                match authenticate(&app, &token, Some(&origin)).await {
                    Ok(_) => continue,
                    Err(_) => return close(&mut socket, "ended").await,
                }
            }
        };
        let reply = match message {
            Some(Ok(Message::Text(text))) => {
                // The session must still hold, and the site still admit it,
                // before any frame acts.
                match authenticate(&app, &token, Some(&origin)).await {
                    Ok((fresh, _)) => scope = fresh,
                    Err(_) => return close(&mut socket, "ended").await,
                }
                let Ok(policy) = self::policy(&app).await else {
                    return close(&mut socket, "unavailable").await;
                };
                if !boundary.frames.allow(
                    &format!("frame:{}", scope.session()),
                    policy.messages_per_minute,
                ) {
                    refusals += 1;
                    if refusals >= REFUSALS_BEFORE_CLOSE {
                        return close(&mut socket, "rate_limited").await;
                    }
                    Some(json!({"type": "error", "code": "rate_limited"}))
                } else {
                    refusals = 0;
                    if renewed_at.elapsed() >= Duration::from_secs(policy.session_seconds / 2) {
                        if let Ok(client) = app.db.get().await {
                            if let Ok(done) = boundary.visitors.renew(&**client, &scope).await {
                                renewed_at = Instant::now();
                                if !send(
                                    &mut socket,
                                    json!({"type": "renewed", "expires_in": done.expires_in}),
                                )
                                .await
                                {
                                    return;
                                }
                            }
                        }
                    }
                    frame(&app, &channel, &scope, &policy, &text, &mut receiving).await
                }
            }
            Some(Ok(Message::Binary(chunk))) => {
                let Ok(policy) = self::policy(&app).await else {
                    return close(&mut socket, "unavailable").await;
                };
                let chunks =
                    policy.uploads_per_minute as u64 * (policy.upload_max_bytes / CHUNK as u64 + 1);
                if !boundary.frames.allow(
                    &format!("chunk:{}", scope.session()),
                    u32::try_from(chunks).unwrap_or(u32::MAX),
                ) {
                    return close(&mut socket, "rate_limited").await;
                }
                match authenticate(&app, &token, Some(&origin)).await {
                    Ok((fresh, _)) => scope = fresh,
                    Err(_) => return close(&mut socket, "ended").await,
                }
                chunked(&channel, &scope, &chunk, &mut receiving).await
            }
            Some(Ok(Message::Ping(data))) => {
                if socket.send(Message::Pong(data)).await.is_err() {
                    return;
                }
                continue;
            }
            Some(Ok(Message::Pong(_))) => continue,
            _ => return,
        };
        if let Some(reply) = reply {
            if !send(&mut socket, reply).await {
                return;
            }
        }
        if !deliver(&channel, &scope, &mut cursor, &mut socket).await {
            return;
        }
    }
}

/// Sends what the channel has for the visitor since `cursor`, if anything.
/// `false` when the socket is gone.
async fn deliver(
    channel: &ChannelService,
    scope: &VisitorScope,
    cursor: &mut Option<String>,
    socket: &mut WebSocket,
) -> bool {
    let outbox = match channel.0.since(scope, cursor.as_deref()).await {
        Ok(outbox) => outbox,
        Err(error) => {
            let refusal = Refusal::from(error);
            return send(socket, json!({"type": "error", "code": code(&refusal)})).await;
        }
    };
    let next = outbox
        .cursor
        .filter(|next| next.len() <= bilibi_base::visitor::SUBJECT_MAX);
    if outbox.items.is_empty() && next == *cursor {
        return true;
    }
    if next.is_some() {
        *cursor = next;
    }
    send(
        socket,
        json!({"type": "items", "items": outbox.items, "cursor": cursor}),
    )
    .await
}

fn failure(id: Option<u64>, error: Error) -> Value {
    let refusal = Refusal::from(error);
    json!({"type": "error", "id": id, "code": code(&refusal)})
}
fn ack(id: Option<u64>, reply: Option<Value>) -> Value {
    json!({"type": "ack", "id": id, "reply": reply})
}
/// One text frame of an open session. A refusal never closes the socket; the
/// session check before it does that.
async fn frame(
    app: &App,
    channel: &ChannelService,
    scope: &VisitorScope,
    policy: &Policy,
    text: &str,
    receiving: &mut Option<Receiving>,
) -> Option<Value> {
    if text.len() > FRAME_LIMIT {
        return Some(failure(None, Error::Invalid));
    }
    let inbound: Inbound = match serde_json::from_str(text) {
        Ok(inbound) => inbound,
        Err(_) => return Some(failure(None, Error::Invalid)),
    };
    match inbound {
        Inbound::Hello { .. } => Some(failure(None, Error::Invalid)),
        Inbound::Message { id, body } => {
            if serde_json::to_vec(&body).map_or(true, |b| b.len() > MESSAGE_LIMIT) {
                return Some(failure(id, Error::Invalid));
            }
            Some(match channel.0.receive(scope, Frame::Message(body)).await {
                Ok(reply) => ack(id, reply),
                Err(error) => failure(id, error),
            })
        }
        Inbound::Typing { body } => {
            if serde_json::to_vec(&body).map_or(true, |b| b.len() > TYPING_LIMIT) {
                return Some(failure(None, Error::Invalid));
            }
            match channel.0.receive(scope, Frame::Typing(body)).await {
                Ok(_) => None,
                Err(error) => Some(failure(None, error)),
            }
        }
        Inbound::Upload { id, name, size } => {
            let size = usize::try_from(size).unwrap_or(usize::MAX);
            let name_ok =
                (1..=255).contains(&name.chars().count()) && !name.chars().any(char::is_control);
            if receiving.is_some()
                || !name_ok
                || size == 0
                || size as u64 > policy.upload_max_bytes
                || !app.visitor.frames.allow(
                    &format!("upload:{}", scope.session()),
                    policy.uploads_per_minute,
                )
            {
                return Some(failure(Some(id.into()), Error::Invalid));
            }
            *receiving = Some(Receiving {
                id,
                name,
                size,
                bytes: Vec::with_capacity(size.min(CHUNK)),
            });
            Some(json!({"type": "upload_ready", "id": id}))
        }
        Inbound::UploadCancel {} => {
            *receiving = None;
            None
        }
    }
}
/// One binary frame: `[entry id u32][offset u32][bytes]`, in order. The file
/// reaches the channel only when every byte is here.
async fn chunked(
    channel: &ChannelService,
    scope: &VisitorScope,
    frame: &[u8],
    receiving: &mut Option<Receiving>,
) -> Option<Value> {
    let Some(file) = receiving.as_mut() else {
        return Some(failure(None, Error::Invalid));
    };
    let id = file.id;
    let bytes = frame.get(HEADER..).unwrap_or_default();
    let header_id = frame
        .get(..4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let offset = frame
        .get(4..HEADER)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if header_id != Some(id)
        || offset != Some(file.bytes.len())
        || bytes.is_empty()
        || bytes.len() > CHUNK
        || file.bytes.len() + bytes.len() > file.size
    {
        *receiving = None;
        return Some(failure(Some(id.into()), Error::Invalid));
    }
    file.bytes.extend_from_slice(bytes);
    if file.bytes.len() < file.size {
        return None;
    }
    let done = receiving.take()?;
    let upload = Upload {
        name: done.name,
        bytes: done.bytes,
    };
    Some(
        match channel.0.receive(scope, Frame::Upload(upload)).await {
            Ok(reply) => ack(Some(id.into()), reply),
            Err(error) => failure(Some(id.into()), error),
        },
    )
}
