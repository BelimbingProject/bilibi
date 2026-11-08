//! Public endpoints (`PUBLIC_ENDPOINTS`): an address a mail client, image
//! proxy or mail provider calls with no session, whose `{token}` is the only
//! credential. Web reads no cookie, `Origin` or CSRF secret here, limits each
//! sender, refuses a frame, never renders the shell, and sends the owner's
//! typed reply with the headers `bilibi_ui::endpoint::Reply` decides. The
//! owner sees the path and body, never the query string, so no request names
//! where a redirect goes. Every refusal the owner makes is one fixed answer,
//! so a caller cannot tell an unknown token from an unusable stored target.
use crate::{
    api,
    routes::{validate_host_free, validate_pair, Downloads, Pages},
    App, HttpError,
};
use axum::{
    body::Body,
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::on,
    Extension, Router,
};
use bilibi_agent::Throttle;
use bilibi_database::{Error, Result};
use bilibi_module_registry::{
    contributions::{Method, PublicEndpoint, PUBLIC_ENDPOINTS},
    Services,
};
use bilibi_ui::endpoint::{EndpointRequest, Reply};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

type EndpointService = bilibi_ui::endpoint::EndpointService<Error>;

#[derive(Clone)]
pub struct BoundEndpoint {
    pub endpoint: &'static PublicEndpoint,
    service: Arc<EndpointService>,
}

/// The bound endpoints and their own per-sender throttle, apart from the
/// sign-in one, so a flood of pixel fetches never crowds out sign-in keys.
pub struct Endpoints {
    pub bound: Vec<BoundEndpoint>,
    throttle: Throttle,
}
impl Endpoints {
    pub(crate) fn patterns(&self) -> impl Iterator<Item = (Method, &'static str)> + '_ {
        self.bound
            .iter()
            .map(|bound| (bound.endpoint.method, bound.endpoint.path))
    }
}

/// Bound after pages and downloads, in their pattern space: an endpoint claims
/// no host path and intersects no page or download of the same method.
pub fn bind(services: &Services, pages: &Pages, downloads: &Downloads) -> Result<Endpoints> {
    let mut bound: Vec<BoundEndpoint> = Vec::new();
    for entry in services
        .catalog()?
        .entries::<PublicEndpoint>(PUBLIC_ENDPOINTS)?
    {
        let endpoint = entry.get::<PublicEndpoint>()?;
        validate_host_free(entry, endpoint.path)?;
        let taken = pages
            .patterns()
            .chain(downloads.patterns())
            .chain(bound.iter().map(|b| (b.endpoint.method, b.endpoint.path)));
        for previous in taken {
            validate_pair(previous, (endpoint.method, endpoint.path))?;
        }
        bound.push(BoundEndpoint {
            endpoint,
            service: services.get(endpoint.service)?,
        });
    }
    Ok(Endpoints {
        bound,
        throttle: Throttle::default(),
    })
}

pub fn install(mut router: Router<App>, endpoints: &Endpoints) -> Router<App> {
    for bound in &endpoints.bound {
        let bound = bound.clone();
        router = router.route(
            bound.endpoint.path,
            on(
                crate::routes::method_filter(bound.endpoint.method),
                move |State(app): State<App>,
                      headers: HeaderMap,
                      peer: Option<Extension<ConnectInfo<SocketAddr>>>,
                      Path(path): Path<HashMap<String, String>>,
                      body: Bytes| {
                    answer(app, bound, headers, peer, path, body)
                },
            ),
        );
    }
    router
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

async fn answer(
    app: App,
    bound: BoundEndpoint,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    path: HashMap<String, String>,
    body: Bytes,
) -> std::result::Result<Response, HttpError> {
    let sender = api::client_address(
        peer.as_ref().map(|Extension(ConnectInfo(peer))| *peer),
        &headers,
    )
    .map_or_else(|| "unknown".into(), |ip| ip.to_string());
    let per_minute = {
        let client = app.db.get().await.map_err(Error::from)?;
        app.settings
            .get_global(
                &**client,
                bilibi_base::integration::PUBLIC_ENDPOINT_SENDER_LIMIT,
            )
            .await?
            .as_u64()
            .and_then(|limit| u32::try_from(limit).ok())
            .ok_or(Error::Internal)?
    };
    let key = format!(
        "{:?} {}:{sender}",
        bound.endpoint.method, bound.endpoint.path
    );
    if !app.endpoints.throttle.allow(&key, per_minute) {
        return Ok((StatusCode::TOO_MANY_REQUESTS, "Too many requests").into_response());
    }
    if matches!(
        headers.get("sec-fetch-dest").and_then(|h| h.to_str().ok()),
        Some("iframe" | "frame")
    ) {
        return Ok(refused());
    }
    let body: &[u8] = match bound.endpoint.method {
        Method::Get => &[],
        _ => &body,
    };
    let reply = bound
        .service
        .0
        .answer(EndpointRequest {
            operation: bound.endpoint.operation,
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
