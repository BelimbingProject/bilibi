//! The public site's pages and links (`base/public`, `PUBLIC_PAGES` and
//! `PUBLIC_LINKS`): bound at boot in the public router's pattern space, and on
//! every request admitted by the listener budget, then the route's own
//! budget, before the owner gets the site's `PublicScope`. The owner never
//! sees the sender, a cookie or a header. A link's reply carries the headers
//! `Reply::headers` decides; every refusal of a link is one fixed answer.
use crate::{
    api,
    routes::{method_filter, validate_host_free, validate_pair, Downloads, Pages},
    App, HttpError,
};
use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, on},
    Extension, Router,
};
use bilibi_base::public::{
    ActionService, Counter, LinkRequest, LinkService, PageService, Public, PublicAction,
    PublicLink, PublicPage, Reached, Reply, Submission, PUBLIC_ACTIONS, PUBLIC_LINKS, PUBLIC_PAGES,
};
use bilibi_database::{Error, Result};
use bilibi_module_registry::{contributions::Method, Services};
use bilibi_ui::page::PageRequest;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

#[derive(Clone)]
struct BoundPage {
    page: &'static PublicPage,
    service: Arc<PageService>,
    counter: Arc<Counter>,
}
#[derive(Clone)]
struct BoundLink {
    link: &'static PublicLink,
    service: Arc<LinkService<Error>>,
    counter: Arc<Counter>,
}

#[derive(Clone)]
struct BoundAction {
    id: &'static str,
    action: &'static PublicAction,
    service: Arc<ActionService<Error>>,
    counter: Arc<Counter>,
}

/// Every public page, link and action of the composition, with its counters.
pub struct PublicRoutes {
    pages: Vec<BoundPage>,
    links: Vec<BoundLink>,
    actions: Vec<BoundAction>,
}
impl PublicRoutes {
    pub(crate) fn patterns(&self) -> impl Iterator<Item = (Method, &'static str)> + '_ {
        self.pages
            .iter()
            .map(|bound| (Method::Get, bound.page.path))
            .chain(
                self.links
                    .iter()
                    .map(|bound| (bound.link.method, bound.link.path)),
            )
            .chain(
                self.actions
                    .iter()
                    .map(|bound| (Method::Post, bound.action.path)),
            )
    }
    /// What serves `pattern` for `method`, for the debug report: the kind
    /// (`page`, `link` or `action`), the operation and the service.
    pub(crate) fn describe(
        &self,
        pattern: &str,
        method: &str,
    ) -> Option<(&'static str, &'static str, &'static str)> {
        let page = self
            .pages
            .iter()
            .find(|bound| bound.page.path == pattern && method.eq_ignore_ascii_case("GET"))
            .map(|bound| ("page", bound.page.operation, bound.page.service));
        page.or_else(|| {
            self.links
                .iter()
                .find(|bound| {
                    bound.link.path == pattern
                        && format!("{:?}", bound.link.method).eq_ignore_ascii_case(method)
                })
                .map(|bound| ("link", bound.link.operation, bound.link.service))
        })
        .or_else(|| {
            self.actions
                .iter()
                .find(|bound| bound.action.path == pattern && method.eq_ignore_ascii_case("POST"))
                .map(|bound| ("action", bound.action.operation, bound.action.service))
        })
    }
    /// Whether a GET page of the public site has exactly this pattern.
    pub(crate) fn serves(&self, path: &str) -> bool {
        self.pages.iter().any(|bound| bound.page.path == path)
    }
}

/// Bound after the portal's pages and downloads, which share the public
/// router: a public page or link claims no host path and intersects no
/// other route of the same method there.
pub fn bind(services: &Services, pages: &Pages, downloads: &Downloads) -> Result<PublicRoutes> {
    let catalog = services.catalog()?;
    // The credential rule first: a `{token}` path its module did not declare.
    let declared = Public::declared(catalog)?;
    services
        .get::<Public>(bilibi_base::public::SERVICE)?
        .adopt(&declared);
    let mut bound = PublicRoutes {
        pages: Vec::new(),
        links: Vec::new(),
        actions: Vec::new(),
    };
    let portal = || {
        pages
            .portal
            .iter()
            .map(|page| (page.route.method, page.route.path))
            .chain(
                downloads
                    .portal
                    .iter()
                    .map(|download| (Method::Get, download.download.path)),
            )
    };
    for entry in catalog.entries::<PublicPage>(PUBLIC_PAGES)? {
        let page = entry.get::<PublicPage>()?;
        validate_host_free(entry, page.path)?;
        for previous in portal().chain(bound.patterns()) {
            validate_pair(previous, (Method::Get, page.path))?;
        }
        bound.pages.push(BoundPage {
            page,
            service: services.get(page.service)?,
            counter: Arc::new(Counter::route()),
        });
    }
    for entry in catalog.entries::<PublicLink>(PUBLIC_LINKS)? {
        let link = entry.get::<PublicLink>()?;
        validate_host_free(entry, link.path)?;
        for previous in portal().chain(bound.patterns()) {
            validate_pair(previous, (link.method, link.path))?;
        }
        bound.links.push(BoundLink {
            link,
            service: services.get(link.service)?,
            counter: Arc::new(Counter::route()),
        });
    }
    for entry in catalog.entries::<PublicAction>(PUBLIC_ACTIONS)? {
        let action = entry.get::<PublicAction>()?;
        validate_host_free(entry, action.path)?;
        for previous in portal().chain(bound.patterns()) {
            validate_pair(previous, (Method::Post, action.path))?;
        }
        bound.actions.push(BoundAction {
            id: entry.id,
            action,
            service: services.get(action.service)?,
            counter: Arc::new(Counter::route()),
        });
    }
    Ok(bound)
}

pub fn install(mut router: Router<App>, routes: &PublicRoutes) -> Router<App> {
    for bound in &routes.pages {
        let bound = bound.clone();
        router = router.route(
            bound.page.path,
            get(
                move |State(app): State<App>,
                      headers: HeaderMap,
                      peer: Option<Extension<ConnectInfo<SocketAddr>>>,
                      Path(path): Path<HashMap<String, String>>,
                      Query(query): Query<HashMap<String, String>>| {
                    page(app, bound, headers, peer, path, query)
                },
            ),
        );
    }
    for bound in &routes.links {
        let bound = bound.clone();
        router = router.route(
            bound.link.path,
            on(
                method_filter(bound.link.method),
                move |State(app): State<App>,
                      headers: HeaderMap,
                      peer: Option<Extension<ConnectInfo<SocketAddr>>>,
                      Path(path): Path<HashMap<String, String>>,
                      body: Bytes| { link(app, bound, headers, peer, path, body) },
            ),
        );
    }
    for bound in &routes.actions {
        let bound = bound.clone();
        router = router.route(
            bound.action.path,
            axum::routing::post(
                move |State(app): State<App>,
                      uri: axum::http::Uri,
                      headers: HeaderMap,
                      peer: Option<Extension<ConnectInfo<SocketAddr>>>,
                      Path(path): Path<HashMap<String, String>>,
                      body: Bytes| {
                    act(app, bound, uri, headers, peer, path, body)
                },
            ),
        );
    }
    router
}

tokio::task_local! {
    /// The site of the public request being served, set by `public_host`.
    pub(crate) static PUBLIC_SITE: bilibi_base::public::PublicScope;
}

/// The public site the request being served reached; none on the staff
/// listener.
pub(crate) fn site() -> Result<bilibi_base::public::PublicScope> {
    PUBLIC_SITE
        .try_with(Clone::clone)
        .map_err(|_| Error::NotFound)
}

/// The public listener's door, first for every request on every path: the
/// `Host` must name a hostname of a site that is on, of a live tenant
/// (`Public::resolve_from`); anything else is a bare 404. A hostname whose
/// answer is not at hand costs its sender one of the lookup budget's
/// lookups, and past that budget the answer is 429 without a lookup, so a
/// flood of made-up hostnames is bounded before it reaches the database.
/// The request then runs with that site's scope.
pub(crate) async fn door(
    State(app): State<App>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let port = url::Url::parse(&app.origin).ok().and_then(|url| url.port());
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    match app
        .public
        .resolve_from(&host, port, &sender(peer, request.headers()))
        .await
    {
        Ok(Reached::Site(site)) => PUBLIC_SITE.scope(site, next.run(request)).await,
        Ok(Reached::Nothing) => StatusCode::NOT_FOUND.into_response(),
        Ok(Reached::Busy) => too_many(),
        Err(error) => HttpError::from(error).into_response(),
    }
}

/// The key a sender is budgeted under, from the connection the trusted-proxy
/// boundary already resolved.
fn sender(peer: Option<SocketAddr>, headers: &HeaderMap) -> String {
    api::sender_key(api::client_address(peer, headers))
}
fn too_many() -> Response {
    (StatusCode::TOO_MANY_REQUESTS, "Too many requests").into_response()
}
fn framed(headers: &HeaderMap) -> bool {
    matches!(
        headers.get("sec-fetch-dest").and_then(|h| h.to_str().ok()),
        Some("iframe" | "frame")
    )
}

/// The listener budget, after the door and before any route: one count per
/// sender for the whole public site. A delivery to a registered signed
/// webhook, on a site's own hostname, is a machine with its own limits
/// (`base/integration`), not a visitor, so it is not counted here; any
/// other path under `/webhooks/` is.
pub(crate) async fn listener(State(app): State<App>, request: Request, next: Next) -> Response {
    if request.method() == axum::http::Method::POST && app.webhooks.serves(request.uri().path()) {
        return next.run(request).await;
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    match app.public.admit(&sender(peer, request.headers())).await {
        Ok(true) => next.run(request).await,
        Ok(false) => too_many(),
        Err(error) => HttpError::from(error).into_response(),
    }
}

async fn page(
    app: App,
    bound: BoundPage,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    path: HashMap<String, String>,
    query: HashMap<String, String>,
) -> std::result::Result<Response, HttpError> {
    let peer = peer.map(|Extension(ConnectInfo(peer))| peer);
    if !bound
        .counter
        .allow(&sender(peer, &headers), bound.page.budget)
    {
        return Ok(too_many());
    }
    // The public site is never framed (`frame-ancestors 'none'`).
    if framed(&headers) {
        return Err(Error::Forbidden.into());
    }
    let scope = app.public_scope()?;
    let page = crate::render_fields(
        &app,
        None,
        bound.service.0.render(PageRequest {
            operation: bound.page.operation,
            scope: Some(&scope),
            path: &path,
            query: &query,
            body: &[],
            framed: false,
        }),
    )
    .await?;
    if let Some(destination) = bound.service.0.redirect(bound.page.operation) {
        return Ok(axum::response::Redirect::to(destination).into_response());
    }
    let brand = app.branding.current().await;
    let title = page
        .title
        .as_deref()
        .or_else(|| bound.service.0.title(bound.page.operation))
        .unwrap_or(brand.product());
    Ok(crate::document_themed(title, &page.html, "system", &brand)?.into_response())
}

/// The one answer to anything refused: an unknown token, a known one whose
/// stored target is unusable, a frame.
fn refused() -> Response {
    let mut response = (StatusCode::NOT_FOUND, "Not found").into_response();
    for (name, value) in Reply::NoContent.headers() {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(name), value);
        }
    }
    response
}

async fn link(
    app: App,
    bound: BoundLink,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    path: HashMap<String, String>,
    body: Bytes,
) -> std::result::Result<Response, HttpError> {
    let peer = peer.map(|Extension(ConnectInfo(peer))| peer);
    if !bound
        .counter
        .allow(&sender(peer, &headers), bound.link.budget)
    {
        return Ok(too_many());
    }
    if framed(&headers) {
        return Ok(refused());
    }
    let scope = app.public_scope()?;
    let body: &[u8] = match bound.link.method {
        Method::Get => &[],
        _ => &body,
    };
    let reply = bound
        .service
        .0
        .answer(LinkRequest {
            operation: bound.link.operation,
            scope: &scope,
            path: &path,
            content_type: headers
                .get(header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok()),
            body,
        })
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) if crate::server_fault(&error) => return Err(error.into()),
        Err(_) => return Ok(refused()),
    };
    let status = StatusCode::from_u16(reply.status()).map_err(|_| Error::Internal)?;
    let delivery = reply.headers();
    // Only bytes carry a body and a type; a 204 or 302 has neither.
    let mut response = Response::new(Body::from(reply.into_body()));
    *response.status_mut() = status;
    for (name, value) in delivery {
        // `Reply::headers` cleans every value; an unrepresentable one is a
        // host fault, not a reason to answer under default headers.
        let value = HeaderValue::from_str(&value).map_err(|_| Error::Internal)?;
        response
            .headers_mut()
            .insert(HeaderName::from_static(name), value);
    }
    Ok(response)
}

/// An action: the budgets, then the request must come from this site's own
/// page (its `Origin`), then `base/public` checks the ticket, trap field,
/// fill time and places and calls the owner. The answer is a 303 back to the
/// request's own path.
async fn act(
    app: App,
    bound: BoundAction,
    uri: axum::http::Uri,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    path: HashMap<String, String>,
    body: Bytes,
) -> std::result::Result<Response, HttpError> {
    let peer = peer.map(|Extension(ConnectInfo(peer))| peer);
    if !bound
        .counter
        .allow(&sender(peer, &headers), bound.action.budget)
    {
        return Ok(too_many());
    }
    crate::origin(&app, &headers)?;
    if framed(&headers) {
        return Err(Error::Forbidden.into());
    }
    let scope = app.public_scope()?;
    let posted: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
    let back = app
        .public
        .submit(Submission {
            scope: &scope,
            id: bound.id,
            action: bound.action,
            owner: &bound.service,
            page: uri.path(),
            path: &path,
            posted,
        })
        .await?;
    Ok(axum::response::Redirect::to(back.as_str()).into_response())
}
