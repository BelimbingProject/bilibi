//! Running proof that a live subscription wakes only the sockets it is for
//! (issue 546), against an explicitly supplied disposable, migrated
//! DATABASE_URL.
//!
//! A fixture page (`/wake-reference`, which lives only in this executable)
//! subscribes to one process-wide channel, as the bell and the AI status tag
//! do, and counts every refresh it is asked for, by tenant and account. The
//! real host serves `/live` on a free local port. Three accounts each open a
//! socket on the page: two in tenant 1 and one in tenant 2. The run then
//! announces changes and reads the counts:
//!
//! - one person's change refreshes that person's page and nobody else's;
//! - a tenant-wide change refreshes that tenant's pages and no other tenant's;
//! - 100 changes for one person at once refresh that person's page a few
//!   times, not 100: the wakes waiting behind a refresh are merged;
//! - 4,000 more leave every other socket untouched (when each one cost a
//!   session guard their queues filled, the subscription fell behind and
//!   every page on the server was refreshed to catch up);
//! - a change still passes the session guard: after the person's session has
//!   expired, their next change sends the page back through sign-in.
//!
//! Each line says what was announced and what was counted; a line starting
//! `FAIL` makes the run exit non-zero.
//!
//! Environment: BILIBI_DISPOSABLE_LIVE_WAKE_DEMO=yes and DATABASE_URL.
use bilibi_database::{pool, Db, Error, Result};
use bilibi_module_registry::{
    contributions::{Access, CompanyScope, Contribution, Method, Route, ROUTES},
    Layer, Module, Provenance, Service,
};
use bilibi_security::Scope;
use bilibi_ui::page::{
    Change, LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
    PageFuture, PageRequest, PageService, PanelSpec, Subscription,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::{net::TcpStream, sync::broadcast};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const MODULE: &str = "extensions/wake_reference";
const SERVICE: &str = "extensions/wake_reference.pages";
const PATH: &str = "/wake-reference";
/// The same buffer as the notification and agent-activity channels'.
const BUFFER: usize = 128;
/// Fewer than [`BUFFER`], so no subscriber can miss one: each is a wake.
const VOLLEY: u64 = 100;
const BURST: u64 = 4_000;

/// The fixture's one channel and what its pages were asked to do.
struct Wakes {
    changes: broadcast::Sender<Change>,
    /// Refreshes asked of a page, by tenant and account.
    refreshes: Mutex<HashMap<(i64, i64), u64>>,
}
fn wakes() -> Arc<Wakes> {
    static WAKES: OnceLock<Arc<Wakes>> = OnceLock::new();
    WAKES
        .get_or_init(|| {
            Arc::new(Wakes {
                changes: broadcast::channel(BUFFER).0,
                refreshes: Mutex::default(),
            })
        })
        .clone()
}
impl Wakes {
    fn announce(&self, tenant: i64, user: Option<i64>) {
        let _ = self.changes.send(Change { tenant, user });
    }
    fn refreshes(&self, tenant: i64, user: i64) -> u64 {
        self.refreshes
            .lock()
            .unwrap()
            .get(&(tenant, user))
            .copied()
            .unwrap_or(0)
    }
}
struct Reference;
struct ReferenceState;
fn body(count: u64) -> String {
    format!("<section id=wake-reference><h1>Wake reference</h1><p data-count>{count}</p></section>")
}
impl Page<Scope, Error> for Reference {
    fn render<'a>(&'a self, _request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            Ok(PageBody {
                html: body(0),
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, _request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            let state: Box<dyn LiveState<Scope, Error>> = Box::new(ReferenceState);
            Ok(Some(state))
        })
    }
}
impl LiveState<Scope, Error> for ReferenceState {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if !matches!(request.input, LiveInput::Refresh) {
                return Err(Error::Invalid);
            }
            let key = (request.scope.tenant(), request.scope.user());
            let count = {
                let wakes = wakes();
                let mut refreshes = wakes.refreshes.lock().unwrap();
                let count = refreshes.entry(key).or_default();
                *count += 1;
                *count
            };
            Ok(LiveBody {
                html: body(count),
                ..LiveBody::default()
            })
        })
    }
    fn panels(&self) -> Vec<PanelSpec> {
        Vec::new()
    }
    fn subscriptions(&mut self) -> Vec<Subscription> {
        vec![Subscription {
            receiver: wakes().changes.subscribe(),
        }]
    }
}
inventory::submit! {
    Module {id: MODULE, layer: Layer::Extensions,
        dependencies: &["base/ui"], migrations: &[], services: &[Service {
            id: SERVICE, factory: |_| Ok(Arc::new(PageService::<Scope, Error>(Arc::new(Reference)))),
        }],
    }
}
inventory::submit! {
    Contribution::new(ROUTES, "extensions/wake_reference.page", Provenance {module: MODULE, source: file!(), line: line!()}, &Route { company_scope: CompanyScope::Tenant,
        method: Method::Get, path: PATH, service: SERVICE, operation: "page", access: Access::SignedIn, framed: true,
    })
}

struct Run {
    failures: u32,
}
impl Run {
    fn expect(&mut self, label: &str, ok: bool, saw: impl std::fmt::Display) {
        if ok {
            println!("ok    {label}: {saw}");
        } else {
            self.failures += 1;
            println!("FAIL  {label}: {saw}");
        }
    }
}
fn internal<E: std::fmt::Debug>(error: E) -> Error {
    eprintln!("walkthrough: {error:?}");
    Error::Internal
}
type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;
/// One account's open page: its session, its socket and where it is counted.
struct Tab {
    name: &'static str,
    tenant: i64,
    user: i64,
    token: String,
    socket: Socket,
    /// The count when the last check ended.
    seen: u64,
}
impl Tab {
    /// Refreshes asked of this page since the last check.
    fn fresh(&mut self) -> u64 {
        let now = wakes().refreshes(self.tenant, self.user);
        let fresh = now - self.seen;
        self.seen = now;
        fresh
    }
}
/// A browser session for the account, as sign-in stores it, and a framed
/// socket (no shell panels) with the fixture page mounted.
async fn open(
    db: &Db,
    addr: &str,
    host: &str,
    name: &'static str,
    tenant: i64,
    user: i64,
) -> Result<Tab> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let csrf = uuid::Uuid::new_v4().simple().to_string();
    db.get().await?.execute(
        "INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at) VALUES($1,$2,$3,$4,now()+interval '24 hours')",
        &[&token, &tenant, &user, &csrf],
    ).await?;
    let mut request = format!("ws://{addr}/live?framed=true")
        .into_client_request()
        .map_err(internal)?;
    let headers = request.headers_mut();
    headers.insert("Origin", host.parse().map_err(internal)?);
    headers.insert(
        "Cookie",
        format!("bilibi_session={token}")
            .parse()
            .map_err(internal)?,
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(internal)?;
    let mount = json!({"type": "mount", "path": PATH, "params": {}, "request_id": 1});
    socket
        .send(Message::text(mount.to_string()))
        .await
        .map_err(internal)?;
    let mounted = next(&mut socket, 10).await;
    if !mounted
        .as_ref()
        .is_some_and(|frame| frame["type"] == "page")
    {
        eprintln!("walkthrough: {name} could not mount {PATH}: {mounted:?}");
        return Err(Error::Internal);
    }
    Ok(Tab {
        name,
        tenant,
        user,
        token,
        socket,
        seen: wakes().refreshes(tenant, user),
    })
}
/// The next JSON frame within `seconds`, `None` when none came or the socket
/// closed.
async fn next(socket: &mut Socket, seconds: u64) -> Option<Value> {
    loop {
        match tokio::time::timeout(Duration::from_secs(seconds), socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => return serde_json::from_str(&text).ok(),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => return None,
        }
    }
}
/// Waits until no page has been refreshed for half a second.
async fn settle() {
    let total = || wakes().refreshes.lock().unwrap().values().sum::<u64>();
    let mut last = total();
    let mut quiet = Instant::now();
    let started = Instant::now();
    while quiet.elapsed() < Duration::from_millis(500) && started.elapsed() < Duration::from_secs(8)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = total();
        if now != last {
            last = now;
            quiet = Instant::now();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_LIVE_WAKE_DEMO").as_deref() != Ok("yes") {
        eprintln!(
            "Set BILIBI_DISPOSABLE_LIVE_WAKE_DEMO=yes: this installs the walkthrough fixture."
        );
        return Err(Error::Invalid);
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_web::modules()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(internal)?;
    let addr = listener.local_addr().map_err(internal)?.to_string();
    let host = format!("http://{addr}");
    let app = bilibi_web::App::new(db.clone(), &host)?;
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            bilibi_web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let mut run = Run { failures: 0 };
    let channel = wakes();

    // A socket refreshes its page by itself every 15 seconds; the counts
    // below are read before the first of those.
    let opened = Instant::now();
    let mut ada = open(&db, &addr, &host, "Ada (tenant 1)", 1, 1).await?;
    let mut lee = open(&db, &addr, &host, "Lee (tenant 1)", 1, 2).await?;
    let mut other = open(&db, &addr, &host, "Other (tenant 2)", 2, 5).await?;
    println!(
        "== three pages are open: {}, {}, {}",
        ada.name, lee.name, other.name
    );

    println!("== who a change wakes");
    channel.announce(1, Some(1));
    settle().await;
    let saw = (ada.fresh(), lee.fresh(), other.fresh());
    run.expect(
        "a change for Ada refreshes Ada's page only",
        saw == (1, 0, 0),
        format!("refreshes Ada {}, Lee {}, Other {}", saw.0, saw.1, saw.2),
    );
    channel.announce(1, None);
    settle().await;
    let saw = (ada.fresh(), lee.fresh(), other.fresh());
    run.expect(
        "a tenant-wide change refreshes tenant 1's pages, not tenant 2's",
        saw == (1, 1, 0),
        format!("refreshes Ada {}, Lee {}, Other {}", saw.0, saw.1, saw.2),
    );
    channel.announce(2, Some(1));
    settle().await;
    let saw = (ada.fresh(), lee.fresh(), other.fresh());
    run.expect(
        "another tenant's change for the same account number refreshes nobody",
        saw == (0, 0, 0),
        format!("refreshes Ada {}, Lee {}, Other {}", saw.0, saw.1, saw.2),
    );

    println!("== {VOLLEY} changes for one person at once");
    for _ in 0..VOLLEY {
        channel.announce(1, Some(1));
    }
    settle().await;
    let saw = (ada.fresh(), lee.fresh(), other.fresh());
    run.expect(
        "waiting wakes are merged: Ada's page refreshed a few times, not once per change",
        (1..=10).contains(&saw.0) && saw.1 == 0 && saw.2 == 0,
        format!(
            "refreshes Ada {} for {VOLLEY} changes, Lee {}, Other {}",
            saw.0, saw.1, saw.2
        ),
    );

    println!("== {BURST} more, fifty a millisecond");
    let burst = Instant::now();
    for sent in 0..BURST {
        channel.announce(1, Some(1));
        // Far more than a socket could guard one by one, and slow enough
        // that a subscriber which only drops them never falls behind.
        if sent % 50 == 49 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    let sending = burst.elapsed();
    settle().await;
    let saw = (ada.fresh(), lee.fresh(), other.fresh());
    run.expect(
        "the other sockets did nothing: no refresh, so no missed-event catch-up either",
        saw.1 == 0 && saw.2 == 0,
        format!(
            "refreshes Lee {}, Other {} ({BURST} sent in {} ms)",
            saw.1,
            saw.2,
            sending.as_millis()
        ),
    );
    run.expect(
        "Ada's page kept up with a fraction of the refreshes",
        saw.0 >= 1 && saw.0 < BURST / 10,
        format!("refreshes Ada {} for {BURST} changes", saw.0),
    );
    run.expect(
        "every count above was read before the page's own 15-second refresh",
        opened.elapsed() < Duration::from_secs(13),
        format!(
            "{} s since the first socket opened",
            opened.elapsed().as_secs()
        ),
    );

    println!("== a change that passes is still guarded");
    db.get()
        .await?
        .execute(
            "UPDATE sessions SET expires_at = now() - interval '1 second' WHERE id = $1",
            &[&ada.token],
        )
        .await?;
    channel.announce(1, Some(1));
    let asked = Instant::now();
    let mut sent_back = None;
    while asked.elapsed() < Duration::from_secs(5) {
        let Some(frame) = next(&mut ada.socket, 5).await else {
            break;
        };
        if frame["type"] == "page" && frame["html"] == "" && frame["redirect"] == PATH {
            sent_back = Some(frame);
            break;
        }
    }
    let refreshed = ada.fresh();
    run.expect(
        "with Ada's session expired, her next change refreshes nothing and sends the page back through sign-in",
        sent_back.is_some() && refreshed == 0,
        format!(
            "redirect {:?} after {} ms, refreshes {refreshed}",
            sent_back.as_ref().map(|frame| frame["redirect"].clone()),
            asked.elapsed().as_millis()
        ),
    );
    channel.announce(1, None);
    settle().await;
    let saw = (lee.fresh(), other.fresh());
    run.expect(
        "Lee's page, in the same tenant, still hears a tenant-wide change",
        saw == (1, 0),
        format!("refreshes Lee {}, Other {}", saw.0, saw.1),
    );

    println!();
    if run.failures == 0 {
        println!("every case behaved as documented");
        Ok(())
    } else {
        println!("{} case(s) failed", run.failures);
        Err(Error::Internal)
    }
}
