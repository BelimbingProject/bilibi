//! Running specimen of the public site's pages and links (`base/public`'s
//! `PUBLIC_PAGES` and `PUBLIC_LINKS`), separate from product routes. Run with
//! a migrated, disposable DATABASE_URL, APP_ORIGIN and BIND_ADDR; no one
//! signs in. The walkthrough fixture's tenants 1 and 2 each get a public site
//! on its own hostname (`one.` and `two.reference.localhost`).
//!
//! A reference mail module owns three token links, as an email Domain would:
//! - `GET /reference/mail/o/{token}`: the open pixel, the same 1×1 GIF for
//!   any token (an unknown one records nothing);
//! - `GET /reference/mail/c/{token}`: the click link, a 302 to the address the
//!   token's message stored, once `Target::parse` accepts it, and only for a
//!   message of the site's own tenant;
//! - `POST /reference/mail/u/{token}`: RFC 8058 one-click unsubscribe, sent by
//!   a mail provider with no `Origin`; 204 for any token whose body says
//!   `List-Unsubscribe=One-Click` (an unknown one changes nothing).
//! - a public page and its POST link at `/reference/form/{token}`, with a
//!   one-request-per-minute sender budget, and a second pair at
//!   `/reference/hourly-form/{token}` whose hourly limit still refuses a
//!   sender after the minute resets.
//!
//! Tokens are held as SHA-256 digests and found by digest, never compared raw.
//! `click-1` stores a valid address, `click-js` a `javascript:` URL and
//! `click-proto` a protocol-relative one: both of those and an unknown token
//! get the same refusal, and so does `click-other`, a message of tenant 2.
//! The owner never sees a query string, so `?to=` on a click link changes
//! nothing. The owner prints one line per action it took.
//!
//! `--sender-admission` proves the route budgets (refusal, sender isolation,
//! minute and hour limits, minute reset), hostname resolution (each hostname
//! names its tenant; an unknown hostname and a site that is off answer a bare
//! 404), the cross-tenant refusal and the listener budget through the real
//! HTTP host. `--refusals` adds each wrong
//! shape to the real catalog and prints its refusal; it needs no database.
use bilibi_base::public::{
    Link, LinkFuture, LinkRequest, LinkService, Local, Public, PublicLink, PublicPage, PublicScope,
    Reply, SenderBudget, Target, PUBLIC_LINKS, PUBLIC_PAGES, SENDER_PER_MINUTE,
};
use bilibi_database::{Error, Result};
use bilibi_module_registry::{
    contributions::{Catalog, CredentialPath, Method, CREDENTIAL_PATHS},
    Consumer, Contribution, Layer, Module, Provenance, Service,
};
use bilibi_ui::{
    page::{Page, PageBody, PageFuture, PageRequest},
    Header, Html, Page as PageLayout, PageVariant,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MODULE: &str = "extensions/public_reference";
const LINKS: &str = "extensions/public_reference.links";
const PAGES: &str = "extensions/public_reference.pages";
const OPEN: &str = "/reference/mail/o/{token}";
const CLICK: &str = "/reference/mail/c/{token}";
const UNSUBSCRIBE: &str = "/reference/mail/u/{token}";
const FORM: &str = "/reference/form/{token}";
const HOURLY_FORM: &str = "/reference/hourly-form/{token}";
/// Mail providers fetch for many readers from few addresses.
const MAIL_BUDGET: SenderBudget = SenderBudget {
    per_minute: 600,
    per_hour: 10_000,
};
const FORM_BUDGET: SenderBudget = SenderBudget {
    per_minute: 1,
    per_hour: 4,
};
const HOUR_BUDGET: SenderBudget = SenderBudget {
    per_minute: 2,
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

/// What each token's message stored: its tenant, the recipient and, for a
/// click link, the address it was written with.
const MESSAGES: [(&str, i64, &str, &str); 6] = [
    ("open-1", 1, "reader@example.test", ""),
    (
        "click-1",
        1,
        "reader@example.test",
        "https://example.com/offer?from=mail",
    ),
    ("click-js", 1, "reader@example.test", "javascript:alert(1)"),
    (
        "click-proto",
        1,
        "reader@example.test",
        "//evil.example/landing",
    ),
    (
        "click-other",
        2,
        "someone@example.test",
        "https://example.com/other-tenant",
    ),
    ("unsubscribe-1", 1, "reader@example.test", ""),
];
type Message = (&'static str, i64, &'static str, &'static str);

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The owner: links keyed by the digest of their token, so a lookup never
/// compares the secret itself, and found in the site's tenant only.
struct Links {
    by_digest: HashMap<[u8; 32], Message>,
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
    fn find(&self, scope: &PublicScope, token: &str) -> Option<&Message> {
        self.by_digest
            .get(&digest(token))
            .filter(|message| message.1 == scope.tenant())
    }
}
impl Link<Error> for Links {
    fn answer<'a>(&'a self, request: LinkRequest<'a>) -> LinkFuture<'a, Error> {
        Box::pin(async move {
            let found = self.find(request.scope, request.token());
            match request.operation {
                "open" => {
                    if let Some((id, _, recipient, _)) = found {
                        println!("link: open of {id} by {recipient}");
                    }
                    Ok(Reply::pixel())
                }
                "click" => {
                    let (id, _, _, stored) = found.ok_or(Error::NotFound)?;
                    let target = Target::parse(stored).ok_or(Error::NotFound)?;
                    println!("link: click of {id} to {}", target.as_str());
                    Ok(Reply::Redirect(target))
                }
                "unsubscribe" => {
                    let one_click = form_fields(request.body)
                        .any(|(key, value)| key == "List-Unsubscribe" && value == "One-Click");
                    if !one_click {
                        return Err(Error::Invalid);
                    }
                    if let Some((id, _, recipient, _)) = found {
                        println!("link: {recipient} unsubscribed through {id}");
                    }
                    Ok(Reply::NoContent)
                }
                // Back to the page on this same site, as a form's save does.
                "submit" => Local::parse(&format!("/reference/form/{}?sent=1", request.token()))
                    .map(Reply::SeeOther)
                    .ok_or(Error::NotFound),
                _ => Err(Error::NotFound),
            }
        })
    }
}

struct FormPage;
impl Page<PublicScope, Error> for FormPage {
    fn render<'a>(&'a self, _request: PageRequest<'a, PublicScope>) -> PageFuture<'a, Error> {
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
        dependencies: &["base/ui", "base/public"],
        migrations: &[],
        services: &[
            Service {
                id: LINKS,
                factory: |_| Ok(Arc::new(LinkService::<Error>(Arc::new(Links::new())))),
            },
            Service {
                id: PAGES,
                factory: |_| Ok(Arc::new(bilibi_ui::page::PageService::<PublicScope, Error>(Arc::new(FormPage)))),
            },
        ],
    }
}
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "public_reference.open", provenance!(), &CredentialPath { path: OPEN }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "public_reference.click", provenance!(), &CredentialPath { path: CLICK }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "public_reference.unsubscribe", provenance!(), &CredentialPath { path: UNSUBSCRIBE }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "public_reference.form", provenance!(), &CredentialPath { path: FORM }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "public_reference.hourly_form", provenance!(), &CredentialPath { path: HOURLY_FORM }) }
inventory::submit! { Contribution::new(PUBLIC_PAGES, "public_reference.form", provenance!(), &PublicPage {
    path: FORM, service: PAGES, operation: "form", budget: FORM_BUDGET,
}) }
inventory::submit! { Contribution::new(PUBLIC_PAGES, "public_reference.hourly_form", provenance!(), &PublicPage {
    path: HOURLY_FORM, service: PAGES, operation: "form", budget: HOUR_BUDGET,
}) }
inventory::submit! { Contribution::new(PUBLIC_LINKS, "public_reference.open", provenance!(), &PublicLink {
    method: Method::Get, path: OPEN, service: LINKS, operation: "open", budget: MAIL_BUDGET,
}) }
inventory::submit! { Contribution::new(PUBLIC_LINKS, "public_reference.click", provenance!(), &PublicLink {
    method: Method::Get, path: CLICK, service: LINKS, operation: "click", budget: MAIL_BUDGET,
}) }
inventory::submit! { Contribution::new(PUBLIC_LINKS, "public_reference.unsubscribe", provenance!(), &PublicLink {
    method: Method::Post, path: UNSUBSCRIBE, service: LINKS, operation: "unsubscribe", budget: MAIL_BUDGET,
}) }
inventory::submit! { Contribution::new(PUBLIC_LINKS, "public_reference.form", provenance!(), &PublicLink {
    method: Method::Post, path: FORM, service: LINKS, operation: "submit", budget: FORM_BUDGET,
}) }
inventory::submit! { Contribution::new(PUBLIC_LINKS, "public_reference.hourly_form", provenance!(), &PublicLink {
    method: Method::Post, path: HOURLY_FORM, service: LINKS, operation: "submit", budget: HOUR_BUDGET,
}) }

/// Wrong shapes, each added alone to the real catalog.
macro_rules! link {
    ($name:ident, $method:expr, $path:expr, $budget:expr) => {
        static $name: Contribution = Contribution::new(
            PUBLIC_LINKS,
            concat!("public_reference.", stringify!($name)),
            provenance!(),
            &PublicLink {
                method: $method,
                path: $path,
                service: LINKS,
                operation: "open",
                budget: $budget,
            },
        );
    };
}
link!(
    NO_TOKEN,
    Method::Post,
    "/reference/mail/everyone",
    MAIL_BUDGET
);
link!(WRONG_METHOD, Method::Delete, UNSUBSCRIBE, MAIL_BUDGET);
link!(PORTAL, Method::Get, "/portal/mail/{token}", MAIL_BUDGET);
link!(
    NO_BUDGET,
    Method::Get,
    "/reference/mail/free/{token}",
    SenderBudget {
        per_minute: 0,
        per_hour: 0
    }
);
link!(
    MINUTE_OVER_HOUR,
    Method::Get,
    "/reference/mail/odd/{token}",
    SenderBudget {
        per_minute: 10,
        per_hour: 5
    }
);
link!(
    NO_CREDENTIAL,
    Method::Get,
    "/reference/mail/x/{token}",
    MAIL_BUDGET
);
static PORTAL_CREDENTIAL: Contribution = Contribution::new(
    CREDENTIAL_PATHS,
    "public_reference.portal",
    provenance!(),
    &CredentialPath {
        path: "/portal/mail/{token}",
    },
);
static PAGE_WITHOUT_BUDGET: Contribution = Contribution::new(
    PUBLIC_PAGES,
    "public_reference.free_page",
    provenance!(),
    &PublicPage {
        path: "/reference/free",
        service: PAGES,
        operation: "form",
        budget: SenderBudget {
            per_minute: 0,
            per_hour: 10,
        },
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
    let cases: [(&str, &[&Contribution]); 7] = [
        ("link without {token}", &[&NO_TOKEN]),
        ("link with DELETE", &[&WRONG_METHOD]),
        ("link under /portal", &[&PORTAL, &PORTAL_CREDENTIAL]),
        ("link without a budget", &[&NO_BUDGET]),
        (
            "link whose minute limit is above its hour's",
            &[&MINUTE_OVER_HOUR],
        ),
        ("page without a budget", &[&PAGE_WITHOUT_BUDGET]),
        ("link without a credential path", &[&NO_CREDENTIAL]),
    ];
    let mut failed = false;
    for (case, extra) in cases {
        let consumers = inventory::iter::<Consumer>.into_iter().collect();
        let mut contributions: Vec<&Contribution> =
            inventory::iter::<Contribution>.into_iter().collect();
        contributions.extend(extra.iter().copied());
        // Shapes fail discovery; the credential rule fails when the host
        // binds the public site (`Public::declared`).
        let refused = Catalog::build(&modules, consumers, contributions)
            .map_err(|error| error.to_string())
            .and_then(|catalog| {
                Public::declared(&catalog)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
        match refused {
            Err(error) => println!("refused {case}: {error}"),
            Ok(()) => {
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

/// One request to the public listener at `address`, for the site named by
/// `host`, as `sender` behind the local proxy: its status.
async fn request_to(
    address: &str,
    host: &str,
    method: &str,
    path: &str,
    sender: &str,
) -> Result<u16> {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .map_err(|_| Error::Internal)?;
    // A site's hostname at the listener's own port, as a browser sends it.
    let host = match (host.contains(':'), address.rsplit_once(':')) {
        (false, Some((_, port))) => format!("{host}:{port}"),
        _ => host.to_owned(),
    };
    let body = if method == "POST" {
        "List-Unsubscribe=One-Click"
    } else {
        ""
    };
    let wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nX-Forwarded-For: {sender}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    // The status line only: a pixel's body is not text.
    let response = String::from_utf8_lossy(&response);
    let line = response.lines().next().ok_or(Error::Internal)?;
    line.split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or(Error::Internal)
}

/// Tenant 1's public site; tenant 2's is [`TWO`].
const ONE: &str = "one.reference.localhost";
const TWO: &str = "two.reference.localhost";
async fn request(address: &str, method: &str, path: &str, sender: &str) -> Result<u16> {
    request_to(address, ONE, method, path, sender).await
}

fn expect(label: &str, actual: u16, expected: u16) -> Result<()> {
    if actual != expected {
        eprintln!("{label}: expected {expected}, got {actual}");
        return Err(Error::Internal);
    }
    println!("{label}: {actual}");
    Ok(())
}

fn second_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Internal)?
        .as_secs())
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
    // Two workspaces (the walkthrough fixture's tenants 1 and 2), each with
    // a public site on its own hostname; the hostname alone names the tenant.
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    let public = app.services.get::<Public>(bilibi_base::public::SERVICE)?;
    for (tenant, host) in [(1, ONE), (2, TWO)] {
        public
            .bind_hostname(&app.mutations, tenant, host, true)
            .await?
            .map_err(|_| Error::Invalid)?;
    }
    let app = app.at_public_site(&origin);
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

    let second_in_hour = second_now()? % 3600;
    let second = second_in_hour % 60;
    if second_in_hour > 58 * 60 + 50 {
        tokio::time::sleep(Duration::from_secs(3601 - second_in_hour)).await;
    } else if second > 50 {
        tokio::time::sleep(Duration::from_secs(61 - second)).await;
    }

    let sender = "198.51.100.10";
    expect(
        "GET page: first sender admitted",
        request(&address, "GET", "/reference/form/form-1", sender).await?,
        200,
    )?;
    expect(
        "GET page: first sender refused past its budget",
        request(&address, "GET", "/reference/form/form-1", sender).await?,
        429,
    )?;
    expect(
        "GET page: second sender admitted",
        request(&address, "GET", "/reference/form/form-1", "198.51.100.11").await?,
        200,
    )?;
    expect(
        "POST link: first sender admitted, sent back to the page (303)",
        request(&address, "POST", "/reference/form/form-1", sender).await?,
        303,
    )?;
    expect(
        "POST link: first sender refused past its budget",
        request(&address, "POST", "/reference/form/form-1", sender).await?,
        429,
    )?;
    expect(
        "POST link: second sender admitted",
        request(&address, "POST", "/reference/form/form-1", "198.51.100.11").await?,
        303,
    )?;
    for (method, admitted) in [("GET", 200), ("POST", 303)] {
        for n in ["first", "second"] {
            expect(
                &format!("{method} hourly pair: {n} request admitted"),
                request(&address, method, "/reference/hourly-form/form-1", sender).await?,
                admitted,
            )?;
        }
        expect(
            &format!("{method} hourly pair: refused past its budget"),
            request(&address, method, "/reference/hourly-form/form-1", sender).await?,
            429,
        )?;
    }

    // The hostname names the tenant, and a tenant's link answers only on
    // that tenant's site.
    let reader = "198.51.100.20";
    expect(
        "click of tenant 1's message on tenant 1's hostname",
        request(&address, "GET", "/reference/mail/c/click-1", reader).await?,
        302,
    )?;
    expect(
        "click of tenant 2's message on tenant 1's hostname: refused like an unknown token",
        request(&address, "GET", "/reference/mail/c/click-other", reader).await?,
        404,
    )?;
    expect(
        "click of tenant 2's message on tenant 2's hostname",
        request_to(
            &address,
            TWO,
            "GET",
            "/reference/mail/c/click-other",
            reader,
        )
        .await?,
        302,
    )?;
    expect(
        "click of tenant 1's message on tenant 2's hostname: refused",
        request_to(&address, TWO, "GET", "/reference/mail/c/click-1", reader).await?,
        404,
    )?;
    expect(
        "a hostname no site has: a bare 404 for every path",
        request_to(
            &address,
            "nobody.reference.localhost",
            "GET",
            "/favicon.svg",
            reader,
        )
        .await?,
        404,
    )?;
    expect(
        "the listener's own address as the hostname: a bare 404",
        request_to(&address, &address, "GET", "/favicon.svg", reader).await?,
        404,
    )?;
    expect(
        "click of an unknown token",
        request(&address, "GET", "/reference/mail/c/nothing", reader).await?,
        404,
    )?;
    expect(
        "click whose stored address is javascript:",
        request(&address, "GET", "/reference/mail/c/click-js", reader).await?,
        404,
    )?;

    tokio::time::sleep(Duration::from_secs(61 - second_now()? % 60)).await;
    for (method, admitted) in [("GET", 200), ("POST", 303)] {
        expect(
            &format!("{method} form pair: admitted after the minute reset"),
            request(&address, method, "/reference/form/form-1", sender).await?,
            admitted,
        )?;
        expect(
            &format!("{method} hourly pair: still refused by its hour after the minute reset"),
            request(&address, method, "/reference/hourly-form/form-1", sender).await?,
            429,
        )?;
    }

    // A site that is off answers nothing at all, like an unknown hostname.
    {
        let client = db.get().await?;
        client
            .execute(
                "UPDATE public_sites SET enabled=false WHERE tenant_id=2",
                &[],
            )
            .await?;
    }
    // A hostname's answer is reused for a few seconds.
    tokio::time::sleep(Duration::from_secs(6)).await;
    expect(
        "tenant 2's site switched off: its hostname answers 404",
        request_to(&address, TWO, "GET", "/favicon.svg", reader).await?,
        404,
    )?;
    expect(
        "tenant 1's site still answers",
        request(&address, "GET", "/favicon.svg", reader).await?,
        200,
    )?;

    // The listener budget counts every request of one sender to the whole
    // site, assets included, before any route's own budget.
    {
        let client = db.get().await?;
        client
            .execute(
                "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES($1,0,'global',0,'3'::jsonb,false) ON CONFLICT(key,tenant_id,scope_type,scope_id) DO UPDATE SET value=excluded.value",
                &[&SENDER_PER_MINUTE],
            )
            .await?;
    }
    // A reading of the setting is reused for a few seconds.
    tokio::time::sleep(Duration::from_secs(6)).await;
    if second_now()? % 60 > 50 {
        tokio::time::sleep(Duration::from_secs(61 - second_now()? % 60)).await;
    }
    let flood = "203.0.113.7";
    for path in [
        "/favicon.svg",
        "/reference/mail/o/open-1",
        "/reference/mail/o/open-1",
    ] {
        expect(
            &format!("listener budget of 3: {path} admitted"),
            request(&address, "GET", path, flood).await?,
            200,
        )?;
    }
    expect(
        "listener budget of 3: a fourth request, to a link with room in its own budget, refused",
        request(&address, "GET", "/reference/mail/o/open-1", flood).await?,
        429,
    )?;
    expect(
        "listener budget of 3: another sender admitted",
        request(&address, "GET", "/reference/mail/o/open-1", "203.0.113.8").await?,
        200,
    )?;

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
    // Bind a hostname to a tenant's site first (`bilibi-web public-hostname`).
    let app = bilibi_web::App::new(db, &origin)?.at_public_site(&origin);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| Error::Internal)?;
    println!("Public site reference listening at {origin}");
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
