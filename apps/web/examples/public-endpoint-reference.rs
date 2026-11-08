//! Running specimen of public endpoints (`PUBLIC_ENDPOINTS`), separate from
//! product routes. Run with a migrated, disposable DATABASE_URL, APP_ORIGIN
//! and BIND_ADDR; no one signs in.
//!
//! A reference mail module owns three token links, as an email Domain would:
//! - `GET /reference/mail/o/{token}`: the open pixel, the same 1×1 GIF for
//!   any token (an unknown one records nothing);
//! - `GET /reference/mail/c/{token}`: the click link, a 302 to the address the
//!   token's message stored, once `Target::parse` accepts it;
//! - `POST /reference/mail/u/{token}`: RFC 8058 one-click unsubscribe, sent by
//!   a mail provider with no `Origin`; 204 for any token whose body says
//!   `List-Unsubscribe=One-Click` (an unknown one changes nothing).
//!
//! Tokens are held as SHA-256 digests and found by digest, never compared raw.
//! `click-1` stores a valid address, `click-js` a `javascript:` URL and
//! `click-proto` a protocol-relative one: both of those and an unknown token
//! get the same refusal. The owner never sees a query string, so `?to=` on a
//! click link changes nothing. The owner prints one line per action it took.
//!
//! `--refusals` adds each wrong shape to the real catalog and prints its
//! refusal; it needs no database.
use bilibi_database::{Error, Result};
use bilibi_module_registry::{
    contributions::{
        Catalog, CredentialPath, Method, PublicEndpoint, CREDENTIAL_PATHS, PUBLIC_ENDPOINTS,
    },
    Consumer, Contribution, Layer, Module, Provenance, Service,
};
use bilibi_ui::endpoint::{
    Endpoint, EndpointFuture, EndpointRequest, EndpointService, Reply, Target,
};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};

const MODULE: &str = "extensions/endpoint_reference";
const LINKS: &str = "extensions/endpoint_reference.links";
const OPEN: &str = "/reference/mail/o/{token}";
const CLICK: &str = "/reference/mail/c/{token}";
const UNSUBSCRIBE: &str = "/reference/mail/u/{token}";

macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

/// What each token's message stored: the recipient and, for a click link, the
/// address it was written with.
const MESSAGES: [(&str, &str, &str); 5] = [
    ("open-1", "reader@example.test", ""),
    (
        "click-1",
        "reader@example.test",
        "https://example.com/offer?from=mail",
    ),
    ("click-js", "reader@example.test", "javascript:alert(1)"),
    (
        "click-proto",
        "reader@example.test",
        "//evil.example/landing",
    ),
    ("unsubscribe-1", "reader@example.test", ""),
];

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The owner: links keyed by the digest of their token, so a lookup never
/// compares the secret itself.
struct Links {
    by_digest: HashMap<[u8; 32], (&'static str, &'static str, &'static str)>,
}
impl Links {
    fn new() -> Self {
        Self {
            by_digest: MESSAGES
                .iter()
                .map(|message| (digest(message.0), *message))
                .collect(),
        }
    }
    fn find(&self, token: &str) -> Option<&(&'static str, &'static str, &'static str)> {
        self.by_digest.get(&digest(token))
    }
}
impl Endpoint<Error> for Links {
    fn answer<'a>(&'a self, request: EndpointRequest<'a>) -> EndpointFuture<'a, Error> {
        Box::pin(async move {
            let found = self.find(request.token());
            match request.operation {
                "open" => {
                    if let Some((id, recipient, _)) = found {
                        println!("endpoint: open of {id} by {recipient}");
                    }
                    Ok(Reply::pixel())
                }
                "click" => {
                    let (id, _, stored) = found.ok_or(Error::NotFound)?;
                    let target = Target::parse(stored).ok_or(Error::NotFound)?;
                    println!("endpoint: click of {id} to {}", target.as_str());
                    Ok(Reply::Redirect(target))
                }
                "unsubscribe" => {
                    let one_click = form_fields(request.body)
                        .any(|(key, value)| key == "List-Unsubscribe" && value == "One-Click");
                    if !one_click {
                        return Err(Error::Invalid);
                    }
                    if let Some((id, recipient, _)) = found {
                        println!("endpoint: {recipient} unsubscribed through {id}");
                    }
                    Ok(Reply::NoContent)
                }
                _ => Err(Error::NotFound),
            }
        })
    }
}

/// `key=value` pairs of an urlencoded body (the RFC 8058 body is one pair).
fn form_fields(body: &[u8]) -> impl Iterator<Item = (&str, &str)> {
    std::str::from_utf8(body)
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| pair.split_once('='))
}

inventory::submit! {
    Module {
        id: MODULE,
        layer: Layer::Extensions,
        dependencies: &["base/ui"],
        migrations: &[],
        services: &[Service {
            id: LINKS,
            factory: |_| Ok(Arc::new(EndpointService::<Error>(Arc::new(Links::new())))),
        }],
    }
}
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "endpoint_reference.open", provenance!(), &CredentialPath { path: OPEN }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "endpoint_reference.click", provenance!(), &CredentialPath { path: CLICK }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "endpoint_reference.unsubscribe", provenance!(), &CredentialPath { path: UNSUBSCRIBE }) }
inventory::submit! { Contribution::new(PUBLIC_ENDPOINTS, "endpoint_reference.open", provenance!(), &PublicEndpoint {
    method: Method::Get, path: OPEN, service: LINKS, operation: "open",
}) }
inventory::submit! { Contribution::new(PUBLIC_ENDPOINTS, "endpoint_reference.click", provenance!(), &PublicEndpoint {
    method: Method::Get, path: CLICK, service: LINKS, operation: "click",
}) }
inventory::submit! { Contribution::new(PUBLIC_ENDPOINTS, "endpoint_reference.unsubscribe", provenance!(), &PublicEndpoint {
    method: Method::Post, path: UNSUBSCRIBE, service: LINKS, operation: "unsubscribe",
}) }

/// Wrong shapes, each added alone to the real catalog.
static NO_CREDENTIAL: Contribution = Contribution::new(
    PUBLIC_ENDPOINTS,
    "endpoint_reference.undeclared",
    provenance!(),
    &PublicEndpoint {
        method: Method::Get,
        path: "/reference/mail/x/{token}",
        service: LINKS,
        operation: "open",
    },
);
static NO_TOKEN: Contribution = Contribution::new(
    PUBLIC_ENDPOINTS,
    "endpoint_reference.tokenless",
    provenance!(),
    &PublicEndpoint {
        method: Method::Post,
        path: "/reference/mail/everyone",
        service: LINKS,
        operation: "unsubscribe",
    },
);
static WRONG_METHOD: Contribution = Contribution::new(
    PUBLIC_ENDPOINTS,
    "endpoint_reference.delete",
    provenance!(),
    &PublicEndpoint {
        method: Method::Delete,
        path: UNSUBSCRIBE,
        service: LINKS,
        operation: "unsubscribe",
    },
);
static PORTAL: Contribution = Contribution::new(
    PUBLIC_ENDPOINTS,
    "endpoint_reference.portal",
    provenance!(),
    &PublicEndpoint {
        method: Method::Get,
        path: "/portal/mail/{token}",
        service: LINKS,
        operation: "open",
    },
);
static PORTAL_CREDENTIAL: Contribution = Contribution::new(
    CREDENTIAL_PATHS,
    "endpoint_reference.portal",
    provenance!(),
    &CredentialPath {
        path: "/portal/mail/{token}",
    },
);

fn refusals() -> std::process::ExitCode {
    let modules = match bilibi_web::modules() {
        Ok(modules) => modules,
        Err(error) => {
            eprintln!("UNEXPECTED refusal of valid catalog: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let cases: [(&str, &[&Contribution]); 4] = [
        ("endpoint without a credential path", &[&NO_CREDENTIAL]),
        ("endpoint without {token}", &[&NO_TOKEN]),
        ("endpoint with DELETE", &[&WRONG_METHOD]),
        ("endpoint under /portal", &[&PORTAL, &PORTAL_CREDENTIAL]),
    ];
    let mut failed = false;
    for (case, extra) in cases {
        let consumers = inventory::iter::<Consumer>.into_iter().collect();
        let mut contributions: Vec<&Contribution> =
            inventory::iter::<Contribution>.into_iter().collect();
        contributions.extend(extra.iter().copied());
        match Catalog::build(&modules, consumers, contributions) {
            Err(error) => println!("refused {case}: {error}"),
            Ok(_) => {
                eprintln!("UNEXPECTEDLY accepted {case}");
                failed = true;
            }
        }
    }
    if failed {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    if std::env::args().any(|arg| arg == "--refusals") {
        return Ok(refusals());
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let bind = std::env::var("BIND_ADDR").map_err(|_| Error::Invalid)?;
    let app = bilibi_web::App::new(db, &origin)?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| Error::Internal)?;
    println!("Public endpoint reference listening at {origin}");
    axum::serve(
        listener,
        bilibi_web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|_| Error::Internal)?;
    Ok(std::process::ExitCode::SUCCESS)
}
