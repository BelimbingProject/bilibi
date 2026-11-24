//! Running proof of public actions (`base/public` `PUBLIC_ACTIONS`): a POST
//! from the public site's own page that makes a record with no session.
//! Run with a migrated, disposable DATABASE_URL and APP_ORIGIN; no one signs
//! in. The walkthrough fixture's tenants 1 and 2 each get a public site on
//! its own hostname.
//!
//! A reference contact page (`/reference/contact`) shows a ticket and posts
//! to its action at the same path. The action's owner counts every call it
//! receives and refuses a blank email. Through the real public router the
//! walkthrough proves, in order:
//!
//! - a POST without the page's `Origin` is refused (403);
//! - a POST with no ticket, or a forged one, is sent back with `?expired=1`
//!   (its UTM parameter carried) and never reaches the owner;
//! - a ticket sent back within the fill time is spam: a receipt that reads
//!   as accepted, and the owner is not called;
//! - a filled trap field is spam the same way;
//! - a good submission reaches the owner once, and its replay answers with
//!   the same receipt without calling the owner again;
//! - the owner's refusal shows its words on the page's receipt;
//! - past the tenant's places for the hour the answer is `?busy=1` and the
//!   owner is not called;
//! - tenant 1's ticket posted to tenant 2's hostname is not accepted there;
//! - the abuse counts (trap, too fast, busy, expired) were written.
use bilibi_base::public::{
    Acted, Action, ActionFuture, ActionRequest, ActionService, Checks, Public, PublicAction,
    PublicPage, PublicScope, Receipt, SenderBudget, PUBLIC_ACTIONS, PUBLIC_PAGES,
};
use bilibi_base::settings::{Definition, ScopeKind, Setting, ValueType, DEFINITIONS};
use bilibi_database::{Error, Result};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance, Service};
use bilibi_ui::{
    page::{Page, PageBody, PageFuture, PageRequest},
    Field, FlashMap, Html, PortalCard, PortalForm,
};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const MODULE: &str = "extensions/actions_reference";
const PAGES: &str = "extensions/actions_reference.pages";
const OWNER: &str = "extensions/actions_reference.contact";
const ACTION: &str = "actions_reference.contact";
const PATH: &str = "/reference/contact";
const PLACES: &str = "actions_reference.contact_per_hour";
const ONE: &str = "one.actions.localhost";
const TWO: &str = "two.actions.localhost";
/// Calls the owner received, by every path.
static CALLS: AtomicUsize = AtomicUsize::new(0);

macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

struct ContactPage(Arc<Public>);
impl Page<PublicScope, Error> for ContactPage {
    fn title(&self, _: &str) -> Option<&'static str> {
        Some("Contact us")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, PublicScope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::NotFound)?;
            let (heading, success, error) = match request.query.get("receipt") {
                Some(token) => match self.0.receipt(scope, ACTION, token).await? {
                    Some(Receipt::Accepted) => ("Thank you", "We will be in touch.", ""),
                    Some(Receipt::Refused(_)) => ("Contact us", "", "REFUSED"),
                    None => ("Contact us", "", "UNKNOWN RECEIPT"),
                },
                None if request.query.contains_key("busy") => ("Contact us", "", "BUSY"),
                None if request.query.contains_key("expired") => ("Contact us", "", "EXPIRED"),
                None => ("Contact us", "", ""),
            };
            // The refusal's own words, when there are any.
            let words = match request.query.get("receipt") {
                Some(token) => match self.0.receipt(scope, ACTION, token).await? {
                    Some(Receipt::Refused(Some(words))) => words,
                    _ => String::new(),
                },
                None => String::new(),
            };
            let error = if words.is_empty() {
                error.to_owned()
            } else {
                words
            };
            let mut fields = self.0.ticket(scope, ACTION, "contact:1").await?.fields()?;
            fields.push(
                Html::render(
                    &Field::in_form("contact", "email")
                        .id("contact-email")
                        .label("Email"),
                )
                .map_err(|_| Error::Internal)?,
            );
            let body = Html::render(&PortalForm {
                action: "",
                id: "contact",
                heading,
                intro: "",
                success,
                error: &error,
                fields,
                submit: "Send",
                working: "Sending…",
                links: &[],
            })
            .map_err(|_| Error::Internal)?;
            let html = Html::render(&PortalCard {
                title: "Reference",
                logo: None,
                body,
                flash: FlashMap::default(),
            })
            .map_err(|_| Error::Internal)?;
            Ok(PageBody {
                html: html.to_string(),
                live: false,
                title: None,
            })
        })
    }
}

struct Contact;
impl Action<Error> for Contact {
    fn act<'a>(&'a self, request: ActionRequest<'a>) -> ActionFuture<'a, Error> {
        Box::pin(async move {
            CALLS.fetch_add(1, Ordering::SeqCst);
            let email = request
                .fields
                .get("contact[email]")
                .map_or("", String::as_str);
            println!(
                "owner: {} in tenant {} about {} from {email:?}",
                request.operation,
                request.scope.tenant(),
                request.subject
            );
            Ok(if email.trim().is_empty() {
                Acted::Refused("Email: can't be blank".into())
            } else {
                Acted::Accepted
            })
        })
    }
}

inventory::submit! {
    Module {
        id: MODULE,
        layer: Layer::Extensions,
        dependencies: &["base/ui", "base/public", "base/settings"],
        migrations: &[],
        services: &[
            Service {
                id: PAGES,
                factory: |services| Ok(Arc::new(bilibi_ui::page::PageService::<PublicScope, Error>(Arc::new(ContactPage(services.get(bilibi_base::public::SERVICE)?))))),
            },
            Service {
                id: OWNER,
                factory: |_| Ok(Arc::new(ActionService::<Error>(Arc::new(Contact)))),
            },
        ],
    }
}
inventory::submit! { Contribution::new(DEFINITIONS, PLACES, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Tenant], default: || json!(2), nullable: false, encrypted: false,
    minimum: Some(1.0), maximum: Some(1000.0), validator: None, editor: None,
})) }
inventory::submit! { Contribution::new(PUBLIC_PAGES, "actions_reference.contact_page", provenance!(), &PublicPage {
    path: PATH, service: PAGES, operation: "contact", budget: SenderBudget { per_minute: 100, per_hour: 1000 },
}) }
inventory::submit! { Contribution::new(PUBLIC_ACTIONS, ACTION, provenance!(), &PublicAction {
    path: PATH, service: OWNER, operation: "contact",
    budget: SenderBudget { per_minute: 100, per_hour: 1000 },
    checks: Checks { trap_field: true, min_fill_seconds: 2, places_per_hour: Some(PLACES) },
    carry: &["utm_source"],
}) }

struct Reply {
    status: u16,
    location: String,
    body: String,
}

/// One request to the public listener at `address` for the site `host`.
async fn request(
    address: &str,
    host: &str,
    method: &str,
    path: &str,
    origin: Option<&str>,
    body: &str,
) -> Result<Reply> {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .map_err(|_| Error::Internal)?;
    let port = address.rsplit_once(':').map_or("", |(_, port)| port);
    let mut wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(origin) = origin {
        wire.push_str(&format!("Origin: {origin}\r\n"));
    }
    wire.push_str("\r\n");
    wire.push_str(body);
    stream
        .write_all(wire.as_bytes())
        .await
        .map_err(|_| Error::Internal)?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|_| Error::Internal)?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .ok_or(Error::Internal)?;
    let location = head
        .lines()
        .find_map(|line| {
            line.strip_prefix("location: ")
                .or_else(|| line.strip_prefix("Location: "))
        })
        .unwrap_or_default()
        .to_owned();
    Ok(Reply {
        status,
        location,
        body: rest.to_owned(),
    })
}

/// The ticket value a fresh page shows.
fn ticket_of(html: &str) -> Result<String> {
    let at = html
        .find("name=\"public[ticket]\"")
        .ok_or(Error::Internal)?;
    let tag_start = html[..at].rfind('<').ok_or(Error::Internal)?;
    let tag = &html[tag_start..at + html[at..].find('>').ok_or(Error::Internal)?];
    let value = tag.split("value=\"").nth(1).ok_or(Error::Internal)?;
    Ok(value[..value.find('"').ok_or(Error::Internal)?].to_owned())
}

fn check(label: &str, ok: bool) -> Result<()> {
    println!("{} {label}", if ok { "ok  " } else { "FAIL" });
    if ok {
        Ok(())
    } else {
        Err(Error::Internal)
    }
}

fn form(ticket: &str, email: &str, trap: &str) -> String {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    body.append_pair("public[ticket]", ticket);
    body.append_pair("public[check]", trap);
    body.append_pair("contact[email]", email);
    body.append_pair("utm_source", "walkthrough");
    body.finish()
}

#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| Error::Internal)?;
    let address = listener
        .local_addr()
        .map_err(|_| Error::Internal)?
        .to_string();
    let port = address
        .rsplit_once(':')
        .map_or("", |(_, port)| port)
        .to_owned();
    let staff = std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:3999".into());
    let app = bilibi_web::App::new(db.clone(), &staff)?;
    let public = app.services.get::<Public>(bilibi_base::public::SERVICE)?;
    for (tenant, host) in [(1, ONE), (2, TWO)] {
        public
            .bind_hostname(&app.mutations, tenant, host, true)
            .await?
            .map_err(|_| Error::Invalid)?;
    }
    let one = format!("http://{ONE}:{port}");
    let app = app.at_public_site(&one);
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
    let page = || async { request(&address, ONE, "GET", PATH, None, "").await };
    let post = |body: String, origin: Option<String>| {
        let address = address.clone();
        async move { request(&address, ONE, "POST", PATH, origin.as_deref(), &body).await }
    };
    let origin = Some(one.clone());

    let shown = page().await?;
    check(
        "the page shows a ticket",
        shown.status == 200 && ticket_of(&shown.body).is_ok(),
    )?;
    let ticket = ticket_of(&shown.body)?;
    check(
        "a POST without the page's Origin is refused",
        post(form(&ticket, "a@example.test", ""), None)
            .await?
            .status
            == 403,
    )?;
    let forged = post(form("v1~contact:1~1~00000000000000000000000000000000~0000000000000000000000000000000000000000000000000000000000000000", "a@example.test", ""), origin.clone()).await?;
    check(
        "a forged ticket goes back to the form, expired, with its UTM parameter",
        forged.status == 303
            && forged.location == format!("{PATH}?expired=1&utm_source=walkthrough"),
    )?;
    let missing = post(form("", "a@example.test", ""), origin.clone()).await?;
    check(
        "no ticket goes back to the form, expired",
        missing.location.starts_with(&format!("{PATH}?expired=1")),
    )?;
    check(
        "neither reached the owner",
        CALLS.load(Ordering::SeqCst) == 0,
    )?;

    // Sent back at once: faster than a person fills the form.
    let fast = post(form(&ticket, "a@example.test", ""), origin.clone()).await?;
    check(
        "a ticket sent back within the fill time gets a receipt",
        fast.status == 303 && fast.location.contains("?receipt="),
    )?;
    let landed = request(&address, ONE, "GET", &fast.location, None, "").await?;
    check(
        "its page reads as accepted, and the owner was not called",
        landed.body.contains("Thank you") && CALLS.load(Ordering::SeqCst) == 0,
    )?;

    let ticket = ticket_of(&page().await?.body)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let trapped = post(
        form(&ticket, "a@example.test", "I am a robot"),
        origin.clone(),
    )
    .await?;
    let landed = request(&address, ONE, "GET", &trapped.location, None, "").await?;
    check(
        "a filled trap field is spam: accepted on the page, never the owner",
        landed.body.contains("Thank you") && CALLS.load(Ordering::SeqCst) == 0,
    )?;

    let ticket = ticket_of(&page().await?.body)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let good = post(form(&ticket, "ana@example.test", ""), origin.clone()).await?;
    check(
        "a good submission reaches the owner once",
        good.status == 303 && CALLS.load(Ordering::SeqCst) == 1,
    )?;
    let replay = post(form(&ticket, "ana@example.test", ""), origin.clone()).await?;
    check(
        "its replay answers with the same receipt and does not call the owner",
        replay.location == good.location && CALLS.load(Ordering::SeqCst) == 1,
    )?;

    let ticket = ticket_of(&page().await?.body)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let refused = post(form(&ticket, "", ""), origin.clone()).await?;
    let landed = request(&address, ONE, "GET", &refused.location, None, "").await?;
    check(
        "the owner's refusal shows its words on the receipt's page",
        landed.body.contains("Email: can") && landed.body.contains("t be blank"),
    )?;

    // The tenant's places: 2 an hour, both taken (the accepted and the
    // refused submission). A server writes a subject's abuse counts at most
    // once a minute: the busy request a minute on writes them all.
    let ticket = ticket_of(&page().await?.body)?;
    tokio::time::sleep(Duration::from_secs(61)).await;
    let calls = CALLS.load(Ordering::SeqCst);
    let busy = post(form(&ticket, "ben@example.test", ""), origin.clone()).await?;
    check(
        "past the hour's places the form comes back busy, and the owner is not called",
        busy.location == format!("{PATH}?busy=1&utm_source=walkthrough")
            && CALLS.load(Ordering::SeqCst) == calls,
    )?;

    // Tenant 1's ticket is signed with tenant 1's key: tenant 2's site does
    // not accept it.
    let ticket = ticket_of(&page().await?.body)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let two = format!("http://{TWO}:{port}");
    let elsewhere = request(
        &address,
        TWO,
        "POST",
        PATH,
        Some(&two),
        &form(&ticket, "eve@example.test", ""),
    )
    .await?;
    check(
        "tenant 1's ticket on tenant 2's hostname goes back expired",
        elsewhere.location.starts_with(&format!("{PATH}?expired=1"))
            && CALLS.load(Ordering::SeqCst) == calls,
    )?;

    let client = db.get().await?;
    let tally = public.tally(&**client, 1, ACTION, "contact:1", 2).await?;
    check(
        "the abuse counts were written: a trap, a too-fast and a busy request",
        tally.trap == 1 && tally.too_fast == 1 && tally.busy == 1,
    )?;
    let receipts: i64 = client
        .query_one(
            "SELECT count(*) FROM public_receipts WHERE tenant_id=1 AND action=$1",
            &[&ACTION],
        )
        .await?
        .get(0);
    check(
        "four receipts: two spam, one accepted, one refused",
        receipts == 4,
    )?;

    let _ = stop.send(());
    server
        .await
        .map_err(|_| Error::Internal)?
        .map_err(|_| Error::Internal)?;
    println!("public actions: ticket, Origin, fill time, trap field, replay, refusal, places, cross-tenant and abuse counts passed");
    Ok(std::process::ExitCode::SUCCESS)
}
