//! The client address of a request that came through a reverse proxy on
//! another machine. The operator lists their proxies in
//! `session.trusted_proxies` (`bilibi_base::session::TrustedProxies`); a
//! request from one of them is given the client the proxy named before any
//! handler runs, so every sender limit, sign-in record and audit row sees the
//! person's address and not the proxy's. A proxy on this machine needs no
//! entry, and a peer that is not listed is never believed.
use crate::App;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use bilibi_base::session::{TrustedProxies, TRUSTED_PROXIES_KEY};
use bilibi_base::settings::{Changed, Settings};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::broadcast::{self, error::TryRecvError};

/// How long one reading of the setting is reused when nothing in this
/// process changed it; another server's change applies within this long.
const FRESH: Duration = Duration::from_secs(60);

/// The operator's list as the host last read it.
pub(crate) struct Proxies {
    trusted: RwLock<Arc<TrustedProxies>>,
    changes: Mutex<broadcast::Receiver<Changed>>,
    read: Mutex<Option<Instant>>,
}
impl Proxies {
    pub(crate) fn new(settings: &Settings) -> Self {
        Self {
            trusted: RwLock::default(),
            changes: Mutex::new(settings.subscribe()),
            read: Mutex::default(),
        }
    }
    /// Whether the list is due a reading: never read, read [`FRESH`] ago, or
    /// changed in this process since. Answers `true` to one caller only.
    fn due(&self) -> bool {
        let mut changed = false;
        {
            let mut changes = self.changes.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                match changes.try_recv() {
                    Ok(change) => changed |= change.key == TRUSTED_PROXIES_KEY,
                    Err(TryRecvError::Lagged(_)) => changed = true,
                    Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                }
            }
        }
        let mut read = self.read.lock().unwrap_or_else(|p| p.into_inner());
        if changed || read.is_none_or(|at| at.elapsed() >= FRESH) {
            *read = Some(Instant::now());
            return true;
        }
        false
    }
    /// The current list. A reading that fails keeps the last one.
    async fn current(&self, app: &App) -> Arc<TrustedProxies> {
        if self.due() {
            let value: bilibi_database::Result<serde_json::Value> = async {
                let client = app.db.get().await?;
                app.settings
                    .get_global(&**client, TRUSTED_PROXIES_KEY)
                    .await
            }
            .await;
            match value.map(|value| value.as_str().and_then(TrustedProxies::parse)) {
                Ok(Some(list)) => {
                    *self.trusted.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(list);
                }
                Ok(None) => eprintln!("warning: {TRUSTED_PROXIES_KEY} could not be understood"),
                Err(error) => {
                    eprintln!("warning: {TRUSTED_PROXIES_KEY} could not be read: {error}");
                }
            }
        }
        self.trusted
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// The `X-Forwarded-For` hops of a request, in the order they were sent.
pub(crate) fn hops(headers: &HeaderMap) -> impl DoubleEndedIterator<Item = &str> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
}

/// Router middleware: a connection from a listed proxy becomes the client's
/// (its address with the connection's port), so `api::client_address` and
/// every `ConnectInfo` reader below see the client.
pub(crate) async fn resolve(State(app): State<App>, mut request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    if let Some(peer) = peer {
        let trusted = app.proxies.current(&app).await;
        let client = trusted.client(peer.ip(), hops(request.headers()));
        if client != peer.ip() {
            request
                .extensions_mut()
                .insert(ConnectInfo(SocketAddr::new(client, peer.port())));
        }
    }
    next.run(request).await
}
