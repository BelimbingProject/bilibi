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
//! - `GET` and `POST /reference/form/{token}`: a public form pair with a
//!   one-request-per-minute sender budget. `--sender-admission` proves refusal,
//!   sender isolation and reset through the real HTTP host.
//! - `GET` and `POST /reference/hourly-form/{token}`: a second pair whose
//!   hourly limit still refuses a sender after the minute resets.
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
        Access, Catalog, CompanyScope, CredentialPath, Method, PublicEndpoint, Route, SenderBudget,
        CREDENTIAL_PATHS, PUBLIC_ENDPOINTS, ROUTES,
    },
    Consumer, Contribution, Layer, Module, Provenance, Service,
};
use bilibi_security::Scope;
use bilibi_ui::endpoint::{
    Endpoint, EndpointFuture, EndpointRequest, EndpointService, Reply, Target,
};
use bilibi_ui::{
    page::{Page, PageBody, PageFuture, PageRequest, PageService},
    Header, Html, Page as PageLayout, PageVariant,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MODULE: &str = "extensions/endpoint_reference";
const LINKS: &str = "extensions/endpoint_reference.links";
const PAGES: &str = "extensions/endpoint_reference.pages";
const OPEN: &str = "/reference/mail/o/{token}";
const CLICK: &str = "/reference/mail/c/{token}";
const UNSUBSCRIBE: &str = "/reference/mail/u/{token}";
const FORM: &str = "/reference/form/{token}";
const HOURLY_FORM: &str = "/reference/hourly-form/{token}";
const FORM_BUDGET: SenderBudget = SenderBudget {
    per_minute: 1,
    per_hour: 4,
};
const HOUR_BUDGET: SenderBudget = SenderBudget {
    per_minute: 3,
    per_hour: 2,
};

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
                "submit" => Ok(Reply::NoContent),
                _ => Err(Error::NotFound),
            }
        })
    }
}

struct FormPage;
impl Page<Scope, Error> for FormPage {
    fn render<'a>(&'a self, _request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let header = Html::render(&Header {
                title: Html::text("Sender admission reference"),
                subtitle: "This disposable page proves the public form boundary.",
                ..Header::default()
            })
            .map_err(|_| Error::Internal)?;
            let html = Html::render(&PageLayout {
                id: "sender-admission-reference",
                variant: PageVariant::Form,
                body: header,
                ..PageLayout::default()
            })
            .map_err(|_| Error::Internal)?
            .to_string();
            Ok(PageBody {
                html,
                live: false,
                title: None,
            })
        })
    }

    fn title(&self, _operation: &str) -> Option<&'static str> {
        Some("Sender admission reference")
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
        dependencies: &["base/ui", "base/security"],
        migrations: &[],
        services: &[
            Service {
                id: LINKS,
                factory: |_| Ok(Arc::new(EndpointService::<Error>(Arc::new(Links::new())))),
            },
            Service {
                id: PAGES,
                factory: |_| Ok(Arc::new(PageService::<Scope, Error>(Arc::new(FormPage)))),
            },
        ],
    }
}
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "endpoint_reference.open", provenance!(), &CredentialPath { path: OPEN }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "endpoint_reference.click", provenance!(), &CredentialPath { path: CLICK }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "endpoint_reference.unsubscribe", provenance!(), &CredentialPath { path: UNSUBSCRIBE }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "endpoint_reference.form", provenance!(), &CredentialPath { path: FORM }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "endpoint_reference.hourly_form", provenance!(), &CredentialPath { path: HOURLY_FORM }) }
inventory::submit! { Contribution::new(ROUTES, "endpoint_reference.form", provenance!(), &Route {
    company_scope: CompanyScope::Tenant,
    method: Method::Get,
    path: FORM,
    service: PAGES,
    operation: "form",
    access: Access::Public { sender_budget: Some(FORM_BUDGET) },
    framed: false,
}) }
inventory::submit! { Contribution::new(ROUTES, "endpoint_reference.hourly_form", provenance!(), &Route {
    company_scope: CompanyScope::Tenant,
    method: Method::Get,
    path: HOURLY_FORM,
    service: PAGES,
    operation: "form",
    access: Access::Public { sender_budget: Some(HOUR_BUDGET) },
    framed: false,
}) }
inventory::submit! { Contribution::new(PUBLIC_ENDPOINTS, "endpoint_reference.open", provenance!(), &PublicEndpoint {
    method: Method::Get, path: OPEN, service: LINKS, operation: "open",
    post_sender_budget: None,
}) }
inventory::submit! { Contribution::new(PUBLIC_ENDPOINTS, "endpoint_reference.click", provenance!(), &PublicEndpoint {
    method: Method::Get, path: CLICK, service: LINKS, operation: "click",
    post_sender_budget: None,
}) }
inventory::submit! { Contribution::new(PUBLIC_ENDPOINTS, "endpoint_reference.unsubscribe", provenance!(), &PublicEndpoint {
    method: Method::Post, path: UNSUBSCRIBE, service: LINKS, operation: "unsubscribe",
    post_sender_budget: None,
}) }
inventory::submit! { Contribution::new(PUBLIC_ENDPOINTS, "endpoint_reference.form", provenance!(), &PublicEndpoint {
    method: Method::Post, path: FORM, service: LINKS, operation: "submit",
    post_sender_budget: Some(FORM_BUDGET),
}) }
inventory::submit! { Contribution::new(PUBLIC_ENDPOINTS, "endpoint_reference.hourly_form", provenance!(), &PublicEndpoint {
    method: Method::Post, path: HOURLY_FORM, service: LINKS, operation: "submit",
    post_sender_budget: Some(HOUR_BUDGET),
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
        post_sender_budget: None,
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
        post_sender_budget: None,
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
        post_sender_budget: None,
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
        post_sender_budget: None,
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
const BUDGETED_GET: &str = "/reference/mail/budgeted/{token}";
static GET_WITH_POST_BUDGET: Contribution = Contribution::new(
    PUBLIC_ENDPOINTS,
    "endpoint_reference.budgeted_get",
    provenance!(),
    &PublicEndpoint {
        method: Method::Get,
        path: BUDGETED_GET,
        service: LINKS,
        operation: "open",
        post_sender_budget: Some(FORM_BUDGET),
    },
);
static BUDGETED_GET_CREDENTIAL: Contribution = Contribution::new(
    CREDENTIAL_PATHS,
    "endpoint_reference.budgeted_get",
    provenance!(),
    &CredentialPath { path: BUDGETED_GET },
);

fn refusals() -> std::process::ExitCode {
    let modules = match bilibi_web::modules() {
        Ok(modules) => modules,
        Err(error) => {
            eprintln!("UNEXPECTED refusal of valid catalog: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let cases: [(&str, &[&Contribution]); 5] = [
        ("endpoint without a credential path", &[&NO_CREDENTIAL]),
        ("endpoint without {token}", &[&NO_TOKEN]),
        ("endpoint with DELETE", &[&WRONG_METHOD]),
        ("endpoint under /portal", &[&PORTAL, &PORTAL_CREDENTIAL]),
        (
            "GET endpoint with a POST sender budget",
            &[&GET_WITH_POST_BUDGET, &BUDGETED_GET_CREDENTIAL],
        ),
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

async fn request(address: &str, method: &str, path: &str, sender: &str) -> Result<u16> {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .map_err(|_| Error::Internal)?;
    let body = if method == "POST" {
        "List-Unsubscribe=One-Click"
    } else {
        ""
    };
    let wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nX-Forwarded-For: {sender}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(wire.as_bytes())
        .await
        .map_err(|_| Error::Internal)?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|_| Error::Internal)?;
    let line = std::str::from_utf8(&response)
        .map_err(|_| Error::Internal)?
        .lines()
        .next()
        .ok_or(Error::Internal)?;
    line.split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or(Error::Internal)
}

fn expect(label: &str, actual: u16, expected: u16) -> Result<()> {
    if actual != expected {
        eprintln!("{label}: expected {expected}, got {actual}");
        return Err(Error::Internal);
    }
    println!("{label}: {actual}");
    Ok(())
}

async fn sender_admission() -> Result<std::process::ExitCode> {
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let parsed = url::Url::parse(&origin).map_err(|_| Error::Invalid)?;
    let address = format!(
        "{}:{}",
        parsed.host_str().ok_or(Error::Invalid)?,
        parsed.port_or_known_default().ok_or(Error::Invalid)?
    );
    let app = bilibi_web::App::new(db, &origin)?.at_public_site(&origin, 1);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|_| Error::Internal)?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            bilibi_web::public_router(app)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
    });

    let second_in_hour = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Internal)?
        .as_secs()
        % 3600;
    let second = second_in_hour % 60;
    if second_in_hour > 58 * 60 + 50 {
        tokio::time::sleep(Duration::from_secs(3601 - second_in_hour)).await;
    } else if second > 50 {
        tokio::time::sleep(Duration::from_secs(61 - second)).await;
    }

    let sender = "198.51.100.10";
    expect(
        "GET first sender admitted",
        request(&address, "GET", "/reference/form/form-1", sender).await?,
        200,
    )?;
    expect(
        "GET first sender refused past budget",
        request(&address, "GET", "/reference/form/form-1", sender).await?,
        429,
    )?;
    expect(
        "GET second sender admitted",
        request(&address, "GET", "/reference/form/form-1", "198.51.100.11").await?,
        200,
    )?;
    expect(
        "POST first sender admitted",
        request(&address, "POST", "/reference/form/form-1", sender).await?,
        204,
    )?;
    expect(
        "POST first sender refused past budget",
        request(&address, "POST", "/reference/form/form-1", sender).await?,
        429,
    )?;
    expect(
        "POST second sender admitted",
        request(&address, "POST", "/reference/form/form-1", "198.51.100.11").await?,
        204,
    )?;

    for (method, admitted) in [("GET", 200), ("POST", 204)] {
        expect(
            &format!("{method} hourly budget first request admitted"),
            request(&address, method, "/reference/hourly-form/form-1", sender).await?,
            admitted,
        )?;
        expect(
            &format!("{method} hourly budget second request admitted"),
            request(&address, method, "/reference/hourly-form/form-1", sender).await?,
            admitted,
        )?;
        expect(
            &format!("{method} refused by hourly budget before minute limit"),
            request(&address, method, "/reference/hourly-form/form-1", sender).await?,
            429,
        )?;
    }

    let second = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Internal)?
        .as_secs()
        % 60;
    tokio::time::sleep(Duration::from_secs(61 - second)).await;
    for (method, admitted) in [("GET", 200), ("POST", 204)] {
        expect(
            &format!("{method} admitted after minute reset"),
            request(&address, method, "/reference/form/form-1", sender).await?,
            admitted,
        )?;
        expect(
            &format!("{method} still refused by hourly budget after minute reset"),
            request(&address, method, "/reference/hourly-form/form-1", sender).await?,
            429,
        )?;
    }

    let _ = stop.send(());
    server
        .await
        .map_err(|_| Error::Internal)?
        .map_err(|_| Error::Internal)?;
    Ok(std::process::ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    if std::env::args().any(|arg| arg == "--refusals") {
        return Ok(refusals());
    }
    if std::env::args().any(|arg| arg == "--sender-admission") {
        return sender_admission().await;
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let bind = std::env::var("BIND_ADDR").map_err(|_| Error::Invalid)?;
    let app = bilibi_web::App::new(db, &origin)?.at_public_site(&origin, 1);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| Error::Internal)?;
    println!("Public endpoint reference listening at {origin}");
    axum::serve(
        listener,
        bilibi_web::public_router(app)
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|_| Error::Internal)?;
    Ok(std::process::ExitCode::SUCCESS)
}
