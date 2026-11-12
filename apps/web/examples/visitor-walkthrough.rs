//! Manual walkthrough of public visitor sessions (issue 513, ADR 27) against an
//! explicitly supplied disposable, migrated DATABASE_URL.
//!
//! A fixture channel (`walkthrough/chat`, which lives only in this executable)
//! keeps conversations in memory, takes its sites from a small table, and
//! stores a visitor's files through Base Media's attachment path as the
//! declared system principal `walkthrough-visitor`, so the scan decides who
//! may read them. The real host serves `/visitor/v1`.
//!
//! `prepare` (default) serves the host on a free local port and plays a
//! browser by hand: it opens sessions from allowed and refused origins, chats
//! over the socket (messages, typing, a clean and an infected file, an
//! oversize file, a broken chunk, a file of several chunks, the in-transit
//! budget and a stalled file), sees a staff reply arrive, hits each rate
//! limit, expires, renews, ends and revokes a session, proves another
//! tenant's visitor sees none of this tenant's conversations, and sends a
//! visitor token to every kind of staff route. Each line says what was sent
//! and what answered; a line starting `FAIL` makes the run exit non-zero.
//!
//! `serve` runs the host for a real browser on BIND_ADDR (APP_ORIGIN must be
//! its address), a customer's page on another origin on PAGE_ADDR (default
//! 127.0.0.1:3992, which embeds the client under a strict Content-Security-
//! Policy: `/?site=other-desk` embeds the other tenant's site) and a staff
//! control address on CONTROL_ADDR (default 127.0.0.1:3991): `/reply?subject=…&text=…`, `/revoke?subject=…`,
//! `/origins?site=…&list=…` (comma-separated; empty closes the site) and
//! `/state`. VISITOR_ORIGINS (comma-separated) lists the origins the sites
//! allow; the sites are `front-desk` (tenant 1) and `other-desk` (tenant 2).
//!
//! Environment: BILIBI_DISPOSABLE_VISITOR_DEMO=yes, DATABASE_URL and
//! BILIBI_MEDIA_DEMO_ROOT (an empty directory the media disk may use).
use bilibi_authz::{
    SystemAuthentication, SystemPrincipalDefinition, SystemScope, VocabularyContribution,
    VOCABULARY,
};
use bilibi_base::media::{
    AttachmentSubject, Media, Rules, ScanFacts, ScanFuture, Scanner, Subject, SubjectAction,
    SubjectOwner, Upload as MediaUpload, Verdict, SUBJECTS,
};
use bilibi_base::settings::{Settings, Target};
use bilibi_base::visitor::{
    Channel, ChannelFuture, ChannelService, Frame, Hello, Outbox, Site, Upload, VisitorChannel,
    VisitorScope, Visitors, CHANNELS,
};
use bilibi_database::{pool, Db, Error, MutationBoundary, Result};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance, Service, Services};
use bilibi_queue::{Config, Executor};
use bilibi_tenancy::{Authentication, ProofClient, ProofFuture, Scope};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::TcpStream, sync::broadcast};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const MODULE: &str = "extensions/visitor_walkthrough";
const CHANNEL: &str = "walkthrough/chat";
const SERVICE: &str = "extensions/visitor_walkthrough.channel";
const SUBJECT_KIND: &str = "walkthrough.chat";
const PRINCIPAL: &str = "walkthrough-visitor";
macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! { Contribution::new(VOCABULARY, "visitor_walkthrough.principals", provenance!(), &VocabularyContribution {
    domains: &[], verbs: &[], capabilities: &[], roles: &[],
    system_principals: &[SystemPrincipalDefinition { name: PRINCIPAL, capabilities: &[] }],
}) }
inventory::submit! { Contribution::new(CHANNELS, CHANNEL, provenance!(), &Channel { key: CHANNEL, service: SERVICE }) }
inventory::submit! { Module {
    id: MODULE, layer: Layer::Extensions,
    dependencies: &["base/visitor", "base/media", "base/tenancy", "base/authz"],
    migrations: &[],
    services: &[Service { id: SERVICE, factory: |_| Ok(Arc::new(ChannelService(conversations()))) }],
} }

/// The fixture channel's data: sites, conversations and what staff did.
struct Conversations {
    sites: Mutex<HashMap<String, (i64, Vec<String>)>>,
    chats: Mutex<HashMap<(i64, String), Chat>>,
    next: Mutex<u32>,
    /// The subject the latest visitor was admitted under.
    last: Mutex<String>,
    services: OnceLock<Arc<Services>>,
}
struct Chat {
    items: Vec<Value>,
    wake: broadcast::Sender<()>,
}
fn conversations() -> Arc<Conversations> {
    static CHANNEL: OnceLock<Arc<Conversations>> = OnceLock::new();
    CHANNEL
        .get_or_init(|| {
            Arc::new(Conversations {
                sites: Mutex::default(),
                chats: Mutex::default(),
                next: Mutex::default(),
                last: Mutex::default(),
                services: OnceLock::new(),
            })
        })
        .clone()
}
impl Conversations {
    fn push(&self, tenant: i64, subject: &str, mut item: Value) -> Option<u64> {
        let mut chats = self.chats.lock().unwrap();
        let chat = chats.get_mut(&(tenant, subject.to_owned()))?;
        let seq = chat.items.len() as u64 + 1;
        item["seq"] = json!(seq);
        chat.items.push(item);
        let _ = chat.wake.send(());
        Some(seq)
    }
    fn exists(&self, tenant: i64, subject: &str) -> bool {
        self.chats
            .lock()
            .unwrap()
            .contains_key(&(tenant, subject.to_owned()))
    }
    /// A staff member's reply into the conversation.
    fn reply(&self, tenant: i64, subject: &str, text: &str) -> bool {
        self.push(tenant, subject, json!({"from": "agent", "text": text}))
            .is_some()
    }
    /// A file the visitor sent, stored as a draft attachment of the
    /// conversation by the declared principal: Media decides its type, the
    /// scanner its verdict.
    async fn store(&self, scope: &VisitorScope, file: Upload) -> Result<Value> {
        let services = self.services.get().ok_or(Error::Internal)?;
        let db: Arc<Db> = services.get("base/database.pool")?;
        let mut client = db.get().await?;
        let system = SystemScope::rehydrate(&**client, scope.tenant(), Some(PRINCIPAL)).await?;
        let sealed = services
            .get::<Authentication>("base/tenancy.authentication")?
            .for_system(&**client, &system)
            .await?;
        let attachment = services
            .get::<Media>("base/media.store")?
            .upload_attachment(
                &mut client,
                &sealed,
                AttachmentSubject {
                    kind: SUBJECT_KIND,
                    id: scope.subject(),
                },
                MediaUpload {
                    filename: file.name.clone(),
                    bytes: file.bytes,
                },
                Rules::default(),
            )
            .await
            .map_err(|_| Error::Invalid)?;
        Ok(json!({"attachment": attachment.public_id, "name": file.name}))
    }
}
impl VisitorChannel for Conversations {
    fn site<'a>(&'a self, key: &'a str) -> ChannelFuture<'a, Option<Site>> {
        Box::pin(async move {
            let sites = self.sites.lock().unwrap();
            match sites.get(key) {
                Some((tenant, origins)) if !origins.is_empty() => {
                    Ok(Some(Site::new(*tenant, key, origins.clone())?))
                }
                _ => Ok(None),
            }
        })
    }
    fn admit<'a>(&'a self, site: &'a Site, _hello: &'a Hello) -> ChannelFuture<'a, String> {
        Box::pin(async move {
            let subject = {
                let mut next = self.next.lock().unwrap();
                *next += 1;
                format!("chat-{}", *next)
            };
            *self.last.lock().unwrap() = subject.clone();
            self.chats.lock().unwrap().insert(
                (site.tenant(), subject.clone()),
                Chat {
                    items: Vec::new(),
                    wake: broadcast::channel(16).0,
                },
            );
            self.push(
                site.tenant(),
                &subject,
                json!({"from": "agent", "text": "Hello! How can we help?"}),
            );
            Ok(subject)
        })
    }
    fn receive<'a>(
        &'a self,
        scope: &'a VisitorScope,
        frame: Frame,
    ) -> ChannelFuture<'a, Option<Value>> {
        Box::pin(async move {
            match frame {
                Frame::Message(body) => {
                    let text = body
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or(Error::Invalid)?;
                    let seq = self
                        .push(
                            scope.tenant(),
                            scope.subject(),
                            json!({"from": "visitor", "text": text}),
                        )
                        .ok_or(Error::NotFound)?;
                    Ok(Some(json!({"seq": seq})))
                }
                Frame::Typing(_) => Ok(None),
                Frame::Upload(file) => {
                    let stored = self.store(scope, file).await?;
                    let mut item = stored.clone();
                    item["from"] = json!("visitor");
                    self.push(scope.tenant(), scope.subject(), item);
                    Ok(Some(stored))
                }
            }
        })
    }
    fn since<'a>(
        &'a self,
        scope: &'a VisitorScope,
        cursor: Option<&'a str>,
    ) -> ChannelFuture<'a, Outbox> {
        Box::pin(async move {
            let after: u64 = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
            let chats = self.chats.lock().unwrap();
            let chat = chats
                .get(&(scope.tenant(), scope.subject().to_owned()))
                .ok_or(Error::NotFound)?;
            let items: Vec<Value> = chat
                .items
                .iter()
                .filter(|item| item["seq"].as_u64().unwrap_or(0) > after)
                .cloned()
                .collect();
            Ok(Outbox {
                cursor: Some(chat.items.len().to_string()),
                items,
            })
        })
    }
    fn watch(&self, scope: &VisitorScope) -> Option<broadcast::Receiver<()>> {
        let chats = self.chats.lock().unwrap();
        Some(
            chats
                .get(&(scope.tenant(), scope.subject().to_owned()))?
                .wake
                .subscribe(),
        )
    }
}
/// The conversation's own rule for who may file under it: the declared
/// principal, for a conversation of the scope's tenant that exists.
struct ChatSubjects;
impl SubjectOwner for ChatSubjects {
    fn authorize<'a>(
        &'a self,
        _client: &'a dyn ProofClient,
        scope: &'a Scope,
        action: SubjectAction,
        subject_id: &'a str,
    ) -> ProofFuture<'a, ()> {
        Box::pin(async move {
            let allowed = scope.principal() == Some(PRINCIPAL)
                && matches!(action, SubjectAction::Upload)
                && conversations().exists(scope.tenant(), subject_id);
            allowed.then_some(()).ok_or(Error::Forbidden)
        })
    }
}
static SUBJECT_OWNER: ChatSubjects = ChatSubjects;
inventory::submit! { Contribution::new(SUBJECTS, SUBJECT_KIND, Provenance { module: "base/media", source: file!(), line: line!() }, &Subject { owner: &SUBJECT_OWNER }) }

/// Fixture scanner: the EICAR test string is infected, everything else clean.
struct FixtureScanner;
impl Scanner for FixtureScanner {
    fn name(&self) -> &str {
        "fixture-scanner"
    }
    fn version(&self) -> String {
        "1.0 (walkthrough)".into()
    }
    fn scan<'a>(&'a self, bytes: &'a [u8], _facts: ScanFacts<'a>) -> ScanFuture<'a> {
        Box::pin(async move {
            let test = eicar();
            if bytes.windows(test.len()).any(|window| window == test) {
                Verdict::Infected {
                    signature: "Eicar-Test-Signature".into(),
                }
            } else {
                Verdict::Clean
            }
        })
    }
}
/// The EICAR anti-virus test string, in two halves so this source file is not
/// itself flagged.
fn eicar() -> Vec<u8> {
    [
        r"X5O!P%@AP[4\PZX54(P^)7CC)7}$",
        "EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*",
    ]
    .concat()
    .into_bytes()
}

fn internal<E: std::fmt::Debug>(error: E) -> Error {
    eprintln!("walkthrough: {error:?}");
    Error::Internal
}
struct Setup {
    services: Arc<Services>,
    root: String,
}
async fn actor(
    setup: &Setup,
    client: &tokio_postgres::Client,
    tenant: i64,
    user: i64,
) -> Result<Scope> {
    setup
        .services
        .get::<Authentication>("base/tenancy.authentication")?
        .authenticate(client, tenant, user, "walkthrough".into())
        .await
}
async fn setting(
    setup: &Setup,
    client: &mut tokio_postgres::Client,
    key: &str,
    value: Value,
) -> Result<()> {
    let operator = actor(setup, client, 1, 6).await?;
    let boundary = setup
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = setup.services.get::<Settings>("base/settings.store")?;
    let mut tx = boundary
        .begin(
            client,
            operator.mutation_context("visitor.walkthrough.configure"),
        )
        .await?;
    settings
        .put(&mut tx, &operator, Target::Global, key, value)
        .await?;
    tx.commit().await?;
    if key.starts_with("visitor.") {
        // The host reuses a reading of the limits for a moment.
        tokio::time::sleep(bilibi_base::visitor::POLICY_FRESH + Duration::from_millis(200)).await;
    }
    Ok(())
}
async fn prepare_host(db: &Db, services: Arc<Services>) -> Result<Setup> {
    let root = std::env::var("BILIBI_MEDIA_DEMO_ROOT").map_err(|_| Error::Invalid)?;
    bilibi_walkthrough_fixture::install(db).await?;
    let _ = conversations().services.set(services.clone());
    let mut client = db.get().await?;
    client
        .execute(
            "INSERT INTO platform_operator(singleton,tenant_id) VALUES(true,1) ON CONFLICT DO NOTHING",
            &[],
        )
        .await?;
    std::fs::create_dir_all(&root).map_err(internal)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).map_err(internal)?;
    let setup = Setup { services, root };
    let disks = json!({"local": setup.root});
    setting(&setup, &mut client, "media.disks", disks).await?;
    setting(&setup, &mut client, "media.attachment_disk", json!("local")).await?;
    setup
        .services
        .get::<Media>("base/media.store")?
        .bind_scanner(Arc::new(FixtureScanner))?;
    Ok(setup)
}
fn open_sites(front: &[String], other: &[String]) {
    let channel = conversations();
    let mut sites = channel.sites.lock().unwrap();
    sites.insert("front-desk".into(), (1, front.to_vec()));
    sites.insert("other-desk".into(), (2, other.to_vec()));
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_VISITOR_DEMO").as_deref() != Ok("yes") {
        eprintln!("Set BILIBI_DISPOSABLE_VISITOR_DEMO=yes: this installs the walkthrough fixture.");
        return Err(Error::Invalid);
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_web::modules()?;
    bilibi_database::migrate(&db, &modules).await?;
    match std::env::args().nth(1).as_deref() {
        Some("serve") => serve(db).await,
        None | Some("prepare") => prepare(db).await,
        Some(_) => Err(Error::Invalid),
    }
}

async fn serve(db: Db) -> Result<()> {
    let origin = std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let control = std::env::var("CONTROL_ADDR").unwrap_or_else(|_| "127.0.0.1:3991".into());
    let pages: Vec<String> = std::env::var("VISITOR_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .filter(|origin| !origin.is_empty())
        .map(str::to_owned)
        .collect();
    let page = std::env::var("PAGE_ADDR").unwrap_or_else(|_| "127.0.0.1:3992".into());
    open_sites(&pages, &pages);
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    let setup = prepare_host(&db, app.services.clone()).await?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let running = tokio::spawn(executor(&setup.services)?.run(stopped));
    tokio::spawn(control_server(
        setup.services.clone(),
        db.clone(),
        control.clone(),
    ));
    tokio::spawn(page_server(page.clone(), origin.clone()));
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(internal)?;
    println!("Bilibi at {origin}; customer page at http://{page}; staff control at http://{control}; sites allow {pages:?}");
    let served = axum::serve(
        listener,
        bilibi_web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await;
    let _ = stop.send(true);
    let _ = running.await;
    served.map_err(internal)
}
fn executor(services: &Services) -> Result<Arc<Executor>> {
    Executor::from_services(
        services,
        Config {
            concurrency: 4,
            poll_interval: Duration::from_millis(200),
            ..Config::default()
        },
    )
}
/// A customer's page on its own origin: nothing but the embed, under a
/// policy that allows scripts and connections to the Bilibi host only (no
/// inline script, no inline style, no frames, no images).
async fn page_server(address: String, host: String) {
    use axum::{extract::Query, http::header, response::IntoResponse, routing::get, Router};
    let socket = host.replacen("http", "ws", 1);
    let policy = format!(
        "default-src 'none'; script-src {host}; connect-src {host} {socket}; style-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    let router = Router::new().route(
        "/",
        get(move |Query(q): Query<HashMap<String, String>>| {
            let policy = policy.clone();
            let host = host.clone();
            async move {
                let site = q.get("site").map_or("front-desk", String::as_str);
                let site: String = site.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
                let body = format!(
                    "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>A customer's website</title></head><body><h1>A customer's website</h1><p>This page is served from another origin than Bilibi. It embeds the visitor chat with one script tag.</p><script src=\"{host}/visitor/v1/client.js\" data-channel=\"{CHANNEL}\" data-site=\"{site}\" data-title=\"Chat with us\" async></script></body></html>"
                );
                ([(header::CONTENT_SECURITY_POLICY, policy), (header::CONTENT_TYPE, "text/html; charset=utf-8".into())], body).into_response()
            }
        }),
    );
    if let Ok(listener) = tokio::net::TcpListener::bind(&address).await {
        let _ = axum::serve(listener, router).await;
    }
}
/// The staff side for a browser proof, on its own local address.
async fn control_server(services: Arc<Services>, db: Db, address: String) {
    use axum::{extract::Query, routing::get, Router};
    let channel = conversations();
    let reply = {
        let channel = channel.clone();
        get(move |Query(q): Query<HashMap<String, String>>| {
            let channel = channel.clone();
            async move {
                let tenant = q.get("tenant").and_then(|t| t.parse().ok()).unwrap_or(1);
                let ok = channel.reply(
                    tenant,
                    q.get("subject").map_or("", String::as_str),
                    q.get("text").map_or("", String::as_str),
                );
                format!("replied: {ok}")
            }
        })
    };
    let revoke = {
        let services = services.clone();
        let db = db.clone();
        get(move |Query(q): Query<HashMap<String, String>>| {
            let services = services.clone();
            let db = db.clone();
            async move {
                let subject = q.get("subject").cloned().unwrap_or_default();
                let client = db.get().await.map_err(|_| "database")?;
                let staff = services
                    .get::<Authentication>("base/tenancy.authentication")
                    .map_err(|_| "services")?
                    .authenticate(&**client, 1, 1, "walkthrough".into())
                    .await
                    .map_err(|_| "staff")?;
                let ended = services
                    .get::<Visitors>(bilibi_base::visitor::SERVICE)
                    .map_err(|_| "services")?
                    .revoke_subject(&**client, &staff, CHANNEL, &subject)
                    .await
                    .map_err(|_| "revoke")?;
                Ok::<_, &str>(format!("revoked: {ended}"))
            }
        })
    };
    let origins = {
        let channel = channel.clone();
        get(move |Query(q): Query<HashMap<String, String>>| {
            let channel = channel.clone();
            async move {
                let site = q.get("site").cloned().unwrap_or_default();
                let list: Vec<String> = q
                    .get("list")
                    .map(|l| {
                        l.split(',')
                            .filter(|o| !o.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut sites = channel.sites.lock().unwrap();
                match sites.get_mut(&site) {
                    Some(entry) => {
                        entry.1 = list.clone();
                        format!("{site} allows {list:?}")
                    }
                    None => "no such site".into(),
                }
            }
        })
    };
    let state = {
        let channel = channel.clone();
        get(move || {
            let channel = channel.clone();
            async move {
                let chats = channel.chats.lock().unwrap();
                let mut keys: Vec<_> = chats
                    .iter()
                    .map(|((tenant, subject), chat)| json!({"tenant": tenant, "subject": subject, "items": chat.items}))
                    .collect();
                keys.sort_by_key(|v| v["subject"].to_string());
                json!(keys).to_string()
            }
        })
    };
    let router = Router::new()
        .route("/reply", reply)
        .route("/revoke", revoke)
        .route("/origins", origins)
        .route("/state", state);
    if let Ok(listener) = tokio::net::TcpListener::bind(&address).await {
        let _ = axum::serve(listener, router).await;
    }
}

// ---- prepare: a browser played by hand ----

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
struct Reply {
    status: u16,
    headers: String,
    body: String,
}
impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}: ")))
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}
/// One HTTP/1.1 request on a fresh connection.
async fn http(addr: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers: String = lines
        .map(|l| {
            let (name, value) = l.split_once(": ").unwrap_or((l, ""));
            format!("{}: {value}\n", name.to_ascii_lowercase())
        })
        .collect();
    let body = if headers.contains("transfer-encoding: chunked") {
        let mut out = String::new();
        let mut rest = rest;
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            out.push_str(&tail[..n.min(tail.len())]);
            rest = tail.get(n + 2..).unwrap_or("");
        }
        out
    } else {
        rest.to_owned()
    };
    Reply {
        status,
        headers,
        body,
    }
}
async fn post(addr: &str, path: &str, origin: Option<&str>, body: Value) -> Reply {
    let mut headers = vec![("Content-Type", "text/plain;charset=UTF-8")];
    if let Some(origin) = origin {
        headers.push(("Origin", origin));
    }
    http(addr, "POST", path, &headers, &body.to_string()).await
}
/// A staff route to try: what it is, its path and the headers to send.
type StaffCase<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>);
type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;
async fn socket(addr: &str, origin: Option<&str>) -> std::result::Result<Socket, String> {
    let mut request = format!("ws://{addr}/visitor/v1/live")
        .into_client_request()
        .map_err(|e| e.to_string())?;
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", origin.parse().map_err(|_| "origin")?);
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(socket, _)| socket)
        .map_err(|e| e.to_string())
}
async fn say(ws: &mut Socket, value: Value) {
    ws.send(Message::text(value.to_string()))
        .await
        .expect("send");
}
/// The next JSON frame within `seconds`, `None` when none came or the socket
/// closed.
async fn next(ws: &mut Socket, seconds: u64) -> Option<Value> {
    loop {
        match tokio::time::timeout(Duration::from_secs(seconds), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => return serde_json::from_str(&text).ok(),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            other => {
                eprintln!("  (socket gave nothing more: {other:?})");
                return None;
            }
        }
    }
}
/// Frames until one of type `kind`; others are skipped.
async fn until(ws: &mut Socket, kind: &str, seconds: u64) -> Option<Value> {
    while let Some(frame) = next(ws, seconds).await {
        if frame["type"] == kind {
            return Some(frame);
        }
    }
    None
}
async fn hello(ws: &mut Socket, token: &str, cursor: Option<&str>) {
    say(
        ws,
        json!({"type": "hello", "token": token, "cursor": cursor}),
    )
    .await;
}
fn shorten(token: &str) -> String {
    format!("{}…", &token[..8.min(token.len())])
}
async fn open(addr: &str, site: &str, origin: Option<&str>) -> Reply {
    post(
        addr,
        "/visitor/v1/sessions",
        origin,
        json!({"channel": CHANNEL, "site": site}),
    )
    .await
}
async fn count(client: &tokio_postgres::Client, sql: &str) -> i64 {
    client
        .query_one(sql, &[])
        .await
        .map(|r| r.get(0))
        .unwrap_or(-1)
}

async fn prepare(db: Db) -> Result<()> {
    const PAGE: &str = "https://shop.example.test";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(internal)?;
    let addr = listener.local_addr().map_err(internal)?.to_string();
    let host = format!("http://{addr}");
    open_sites(&[PAGE.into()], &[PAGE.into()]);
    let app = bilibi_web::App::new(db.clone(), &host)?;
    let setup = prepare_host(&db, app.services.clone()).await?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let running = tokio::spawn(executor(&setup.services)?.run(stopped));
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            bilibi_web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let mut run = Run { failures: 0 };
    let mut client = db.get().await?;
    let visitors = setup
        .services
        .get::<Visitors>(bilibi_base::visitor::SERVICE)?;
    let staff = actor(&setup, &client, 1, 1).await?;

    // The cases below open many sessions from one address; the rate-limit
    // cases tighten this again.
    setting(
        &setup,
        &mut client,
        "visitor.sessions_per_sender_per_minute",
        json!(10_000),
    )
    .await?;
    println!("== opening a session: the origin allow-list");
    let opened = open(&addr, "front-desk", Some(PAGE)).await;
    let first_subject = conversations().last.lock().unwrap().clone();
    let token = opened.json()["token"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    run.expect(
        "listed origin",
        opened.status == 200
            && opened.header("access-control-allow-origin") == Some(PAGE)
            && token.starts_with("vst_"),
        format!(
            "{} token {} allow-origin {:?} vary {:?}",
            opened.status,
            shorten(&token),
            opened.header("access-control-allow-origin"),
            opened.header("vary")
        ),
    );
    run.expect(
        "the session is stored as a digest, never the token",
        count(&client, &format!("SELECT count(*) FROM visitor_sessions WHERE position('{token}' in encode(token_digest,'hex')) > 0 OR encode(token_digest,'hex') = '{token}'")).await == 0
            && count(&client, "SELECT count(*) FROM visitor_sessions").await == 1,
        "one row, 32-byte digest",
    );
    let refused_origins: [(&str, Option<&str>); 7] = [
        ("unlisted origin", Some("https://evil.example.test")),
        ("no Origin header", None),
        ("opaque origin `null`", Some("null")),
        (
            "listed host on another scheme",
            Some("http://shop.example.test"),
        ),
        (
            "listed host on another port",
            Some("https://shop.example.test:8443"),
        ),
        (
            "a suffix of a listed host",
            Some("https://shop.example.test.evil.test"),
        ),
        ("an origin with a path", Some("https://shop.example.test/")),
    ];
    for (label, origin) in refused_origins {
        let reply = open(&addr, "front-desk", origin).await;
        run.expect(
            label,
            reply.status == 403
                && reply.header("access-control-allow-origin").is_none()
                && reply.json()["error"] == "refused",
            format!("{} {} (no allow-origin header)", reply.status, reply.body),
        );
    }
    let unknown = open(&addr, "no-such-site", Some(PAGE)).await;
    let closed = {
        conversations()
            .sites
            .lock()
            .unwrap()
            .insert("closed".into(), (1, vec![]));
        open(&addr, "closed", Some(PAGE)).await
    };
    let channel = post(
        &addr,
        "/visitor/v1/sessions",
        Some(PAGE),
        json!({"channel": "nobody/chat", "site": "front-desk"}),
    )
    .await;
    run.expect(
        "unknown site, closed site and unknown channel answer alike",
        [&unknown, &closed, &channel]
            .iter()
            .all(|r| r.status == 403 && r.body == opened_refusal()),
        format!("{} / {} / {}", unknown.body, closed.body, channel.body),
    );
    let malformed = post(
        &addr,
        "/visitor/v1/sessions",
        Some(PAGE),
        json!({"channel": CHANNEL, "site": "front-desk", "surprise": 1}),
    )
    .await;
    run.expect(
        "an unknown field is invalid",
        malformed.status == 400,
        malformed.body.clone(),
    );
    let huge = post(
        &addr,
        "/visitor/v1/sessions",
        Some(PAGE),
        json!({"channel": CHANNEL, "site": "front-desk", "details": "x".repeat(5000)}),
    )
    .await;
    run.expect(
        "oversize details are invalid",
        huge.status == 400,
        huge.body.clone(),
    );
    let preflight = http(
        &addr,
        "OPTIONS",
        "/visitor/v1/sessions",
        &[("Origin", PAGE), ("Access-Control-Request-Method", "POST")],
        "",
    )
    .await;
    run.expect(
        "a preflight is never answered with permission",
        preflight.header("access-control-allow-origin").is_none(),
        format!("{}", preflight.status),
    );

    let script = http(&addr, "GET", "/visitor/v1/client.js", &[], "").await;
    run.expect(
        "the embeddable script is served as JavaScript any page may load, with no cookie",
        script.status == 200
            && script
                .header("content-type")
                .is_some_and(|t| t.starts_with("text/javascript"))
            && script.header("cross-origin-resource-policy") == Some("cross-origin")
            && script.header("set-cookie").is_none()
            && script.body.contains("Bilibi visitor chat client v1"),
        format!(
            "{} {:?} {} bytes",
            script.status,
            script.header("content-type"),
            script.body.len()
        ),
    );

    println!("== chatting over the socket");
    let mut ws = socket(&addr, Some(PAGE)).await.expect("socket");
    hello(&mut ws, &token, None).await;
    let ready = next(&mut ws, 5).await;
    let greeting = until(&mut ws, "items", 5).await;
    run.expect(
        "ready, then the conversation so far",
        ready
            .as_ref()
            .is_some_and(|f| f["type"] == "ready" && f["upload_max_bytes"] == 5_242_880)
            && greeting
                .as_ref()
                .is_some_and(|f| f["items"][0]["text"] == "Hello! How can we help?"),
        format!("{ready:?} {greeting:?}"),
    );
    let subject = first_subject.clone();
    say(
        &mut ws,
        json!({"type": "message", "id": 1, "body": {"text": "Is the red one in stock?"}}),
    )
    .await;
    let ack = until(&mut ws, "ack", 5).await;
    let echoed = until(&mut ws, "items", 5).await;
    run.expect(
        "a message is acknowledged and appears in the conversation",
        ack.as_ref()
            .is_some_and(|f| f["id"] == 1 && f["reply"]["seq"] == 2)
            && echoed
                .as_ref()
                .is_some_and(|f| f["items"][0]["text"] == "Is the red one in stock?"),
        format!("{ack:?}"),
    );
    say(&mut ws, json!({"type": "typing", "body": {"typing": true}})).await;
    run.expect(
        "typing is accepted without an answer",
        next(&mut ws, 1).await.is_none(),
        "silence",
    );
    conversations().reply(1, &subject, "Yes, two left.");
    let pushed = until(&mut ws, "items", 5).await;
    run.expect(
        "a staff reply is pushed to the open socket",
        pushed
            .as_ref()
            .is_some_and(|f| f["items"][0]["text"] == "Yes, two left."),
        format!("{pushed:?}"),
    );
    say(
        &mut ws,
        json!({"type": "message", "id": 2, "body": "not an object"}),
    )
    .await;
    let bad = until(&mut ws, "error", 5).await;
    run.expect(
        "a message the channel rejects is invalid, the socket stays open",
        bad.as_ref().is_some_and(|f| f["code"] == "invalid"),
        format!("{bad:?}"),
    );
    say(
        &mut ws,
        json!({"type": "message", "id": 3, "body": {"text": "x".repeat(17_000)}}),
    )
    .await;
    let big = until(&mut ws, "error", 5).await;
    run.expect(
        "a message over 16 KiB is invalid",
        big.as_ref().is_some_and(|f| f["code"] == "invalid"),
        format!("{big:?}"),
    );
    say(&mut ws, json!({"type": "hello", "token": token})).await;
    let again = until(&mut ws, "error", 5).await;
    run.expect(
        "a second hello is invalid",
        again.as_ref().is_some_and(|f| f["code"] == "invalid"),
        format!("{again:?}"),
    );

    println!("== sending files (Base Media and its scan)");
    let clean = send_file(&mut ws, 7, "note.txt", b"A clean note.\n").await;
    run.expect(
        "a clean file is acknowledged",
        clean
            .as_ref()
            .is_some_and(|f| f["type"] == "ack" && f["reply"]["attachment"].is_string()),
        format!("{clean:?}"),
    );
    let infected = send_file(&mut ws, 8, "invoice.txt", &eicar()).await;
    run.expect(
        "a file holding the test virus is acknowledged too (the scan decides later)",
        infected.as_ref().is_some_and(|f| f["type"] == "ack"),
        format!("{infected:?}"),
    );
    let mut states = Vec::new();
    for _ in 0..300 {
        states = client
            .query("SELECT s.original_filename, s.scan_state, a.uploaded_by_principal FROM base_media_assets s JOIN base_media_attachments a ON a.media_asset_id = s.id WHERE a.subject_type = $1 ORDER BY a.id", &[&SUBJECT_KIND])
            .await?
            .iter()
            .map(|r| format!("{}={} by {}", r.get::<_, Option<String>>(0).unwrap_or_default(), r.get::<_, String>(1), r.get::<_, Option<String>>(2).unwrap_or_default()))
            .collect();
        if states.len() == 2 && !states.iter().any(|s| s.contains("=pending")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    run.expect(
        "the queue's scan settles each file; the virus is quarantined",
        states
            .iter()
            .any(|s| s.starts_with("note.txt=clean by walkthrough-visitor"))
            && states.iter().any(|s| s.starts_with("invoice.txt=infected")),
        states.join("; "),
    );
    let too_big = {
        say(
            &mut ws,
            json!({"type": "upload", "id": 9, "name": "huge.bin", "size": 6_000_000}),
        )
        .await;
        until(&mut ws, "error", 5).await
    };
    run.expect(
        "a file over the operator's limit is refused before any byte",
        too_big.as_ref().is_some_and(|f| f["code"] == "invalid"),
        format!("{too_big:?}"),
    );
    say(
        &mut ws,
        json!({"type": "upload", "id": 10, "name": "a.txt", "size": 10}),
    )
    .await;
    let ready_upload = until(&mut ws, "upload_ready", 5).await;
    let mut frame = 10u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&5u32.to_be_bytes());
    frame.extend_from_slice(b"hello");
    ws.send(Message::Binary(frame.into())).await.expect("send");
    let skipped = until(&mut ws, "error", 5).await;
    run.expect(
        "a chunk that skips ahead drops the file",
        ready_upload.is_some() && skipped.as_ref().is_some_and(|f| f["code"] == "invalid"),
        format!("{skipped:?}"),
    );

    println!("== files of several chunks, and the memory files in transit may hold");
    setting(
        &setup,
        &mut client,
        "visitor.uploads_per_minute",
        json!(100),
    )
    .await?;
    say(
        &mut ws,
        json!({"type": "message", "id": 4, "body": {"text": "x".repeat(40_000)}}),
    )
    .await;
    let wide = until(&mut ws, "error", 5).await;
    run.expect(
        "a text frame over 32 KiB is invalid, the socket stays open",
        wide.as_ref().is_some_and(|f| f["code"] == "invalid"),
        format!("{wide:?}"),
    );
    // Four chunks: three of 64 KiB and a short last one.
    let long: Vec<u8> = (0..)
        .flat_map(|line: u32| format!("line {line} of a long note\n").into_bytes())
        .take(200_000)
        .collect();
    let several = send_file(&mut ws, 11, "long-note.txt", &long).await;
    let stored: Option<i64> = client
        .query_opt(
            "SELECT s.file_size FROM base_media_assets s JOIN base_media_attachments a ON a.media_asset_id = s.id WHERE a.subject_type = $1 AND s.original_filename = 'long-note.txt'",
            &[&SUBJECT_KIND],
        )
        .await?
        .and_then(|row| row.get(0));
    run.expect(
        "a 200,000-byte file sent as four chunks is acknowledged and stored whole",
        several.as_ref().is_some_and(|f| f["type"] == "ack") && stored == Some(200_000),
        format!("{several:?}; Media holds {stored:?} bytes"),
    );
    setting(
        &setup,
        &mut client,
        "visitor.upload_buffer_bytes",
        json!(2_048),
    )
    .await?;
    say(
        &mut ws,
        json!({"type": "upload", "id": 12, "name": "held.txt", "size": 1_500}),
    )
    .await;
    let held = until(&mut ws, "upload_ready", 5).await;
    let mut second = socket(&addr, Some(PAGE)).await.expect("socket");
    hello(&mut second, &token, None).await;
    let _ = until(&mut second, "ready", 5).await;
    say(
        &mut second,
        json!({"type": "upload", "id": 13, "name": "waiting.txt", "size": 1_500}),
    )
    .await;
    let over = until(&mut second, "error", 5).await;
    run.expect(
        "a file announced beyond the server's in-transit budget is refused",
        held.is_some()
            && over
                .as_ref()
                .is_some_and(|f| f["id"] == 13 && f["code"] == "rate_limited"),
        format!("{over:?}"),
    );
    let waited = std::time::Instant::now();
    let stalled = until(&mut ws, "error", 40).await;
    run.expect(
        "a file with no chunk for 30 seconds is dropped",
        stalled
            .as_ref()
            .is_some_and(|f| f["id"] == 12 && f["code"] == "invalid")
            && waited.elapsed() >= Duration::from_secs(25),
        format!("{stalled:?} after {} s", waited.elapsed().as_secs()),
    );
    let freed = send_file(&mut second, 14, "waiting.txt", &[b'w'; 1_500]).await;
    run.expect(
        "the dropped file gave its bytes back: the refused one now goes through",
        freed.as_ref().is_some_and(|f| f["type"] == "ack"),
        format!("{freed:?}"),
    );
    drop(second);
    setting(
        &setup,
        &mut client,
        "visitor.upload_buffer_bytes",
        json!(268_435_456u64),
    )
    .await?;

    println!("== renewal, expiry, ending");
    let renewed = post(
        &addr,
        "/visitor/v1/session/renew",
        Some(PAGE),
        json!({"token": token}),
    )
    .await;
    run.expect(
        "renew from the right origin",
        renewed.status == 200 && renewed.json()["expires_in"].as_u64().is_some_and(|s| s > 0),
        renewed.body.clone(),
    );
    let foreign = post(
        &addr,
        "/visitor/v1/session/renew",
        Some("https://evil.example.test"),
        json!({"token": token}),
    )
    .await;
    run.expect(
        "the same token from another origin is refused",
        foreign.status == 403 && foreign.header("access-control-allow-origin").is_none(),
        foreign.body.clone(),
    );
    let forged = post(
        &addr,
        "/visitor/v1/session/renew",
        Some(PAGE),
        json!({"token": format!("vst_{}", "0".repeat(64))}),
    )
    .await;
    run.expect(
        "an unknown token is refused",
        forged.status == 403,
        forged.body.clone(),
    );
    let wrong_shape = post(
        &addr,
        "/visitor/v1/session/renew",
        Some(PAGE),
        json!({"token": "abc"}),
    )
    .await;
    run.expect(
        "a malformed token is refused without a lookup",
        wrong_shape.status == 403,
        wrong_shape.body.clone(),
    );
    let socket_before = socket(&addr, Some("https://evil.example.test")).await;
    let mut evil = socket_before.expect("upgrade");
    hello(&mut evil, &token, None).await;
    let evil_closed = until(&mut evil, "closed", 5).await;
    run.expect(
        "a socket from another origin with the right token is closed",
        evil_closed
            .as_ref()
            .is_some_and(|f| f["reason"] == "refused"),
        format!("{evil_closed:?}"),
    );
    let no_origin = socket(&addr, None).await;
    run.expect(
        "a socket with no Origin is refused at the upgrade",
        no_origin.is_err(),
        format!("{:?}", no_origin.as_ref().err()),
    );
    let silent = socket(&addr, Some(PAGE)).await.expect("upgrade");
    drop(silent);
    client
        .execute("UPDATE visitor_sessions SET expires_at = now() - interval '1 second' WHERE token_digest = sha256(convert_to($1, 'UTF8'))", &[&token])
        .await?;
    say(
        &mut ws,
        json!({"type": "message", "id": 20, "body": {"text": "still there?"}}),
    )
    .await;
    let expired = until(&mut ws, "closed", 5).await;
    run.expect(
        "an expired session closes its open socket before acting",
        expired.as_ref().is_some_and(|f| f["reason"] == "ended"),
        format!("{expired:?}"),
    );
    let after = post(
        &addr,
        "/visitor/v1/session/renew",
        Some(PAGE),
        json!({"token": token}),
    )
    .await;
    run.expect(
        "an expired session cannot be renewed",
        after.status == 403,
        after.body.clone(),
    );

    println!("== revoking a session");
    let second = open(&addr, "front-desk", Some(PAGE)).await.json();
    let token2 = second["token"].as_str().unwrap_or_default().to_owned();
    let subject2 = conversations().last.lock().unwrap().clone();
    let third = open(&addr, "front-desk", Some(PAGE)).await.json();
    let token3 = third["token"].as_str().unwrap_or_default().to_owned();
    let mut live = socket(&addr, Some(PAGE)).await.expect("upgrade");
    hello(&mut live, &token2, None).await;
    run.expect(
        "a second visitor connects",
        until(&mut live, "ready", 5).await.is_some(),
        "ready",
    );
    let ended = visitors
        .revoke_subject(&**client, &staff, CHANNEL, &subject2)
        .await?;
    let closed_by_staff = until(&mut live, "closed", 5).await;
    run.expect(
        "staff revoke one conversation: its open socket closes at once",
        ended == 1
            && closed_by_staff
                .as_ref()
                .is_some_and(|f| f["reason"] == "ended"),
        format!("ended {ended}, {closed_by_staff:?}"),
    );
    let renew_revoked = post(
        &addr,
        "/visitor/v1/session/renew",
        Some(PAGE),
        json!({"token": token2}),
    )
    .await;
    let mut reconnect = socket(&addr, Some(PAGE)).await.expect("upgrade");
    hello(&mut reconnect, &token2, None).await;
    let reconnect_closed = until(&mut reconnect, "closed", 5).await;
    run.expect(
        "a revoked session is refused on renew and on reconnect",
        renew_revoked.status == 403 && reconnect_closed.is_some_and(|f| f["reason"] == "refused"),
        renew_revoked.body.clone(),
    );
    let neighbour = post(
        &addr,
        "/visitor/v1/session/renew",
        Some(PAGE),
        json!({"token": token3}),
    )
    .await;
    run.expect(
        "another visitor's session is untouched",
        neighbour.status == 200,
        neighbour.body.clone(),
    );
    let ended_by_visitor = post(
        &addr,
        "/visitor/v1/session/end",
        Some(PAGE),
        json!({"token": token3}),
    )
    .await;
    let renew_ended = post(
        &addr,
        "/visitor/v1/session/renew",
        Some(PAGE),
        json!({"token": token3}),
    )
    .await;
    run.expect(
        "a visitor ends their own session",
        ended_by_visitor.status == 200 && renew_ended.status == 403,
        ended_by_visitor.body.clone(),
    );
    let origin_removed = {
        let fourth = open(&addr, "front-desk", Some(PAGE)).await.json();
        let token4 = fourth["token"].as_str().unwrap_or_default().to_owned();
        conversations()
            .sites
            .lock()
            .unwrap()
            .get_mut("front-desk")
            .unwrap()
            .1 = vec!["https://other.example.test".into()];
        let reply = post(
            &addr,
            "/visitor/v1/session/renew",
            Some(PAGE),
            json!({"token": token4}),
        )
        .await;
        conversations()
            .sites
            .lock()
            .unwrap()
            .get_mut("front-desk")
            .unwrap()
            .1 = vec![PAGE.into()];
        reply
    };
    run.expect(
        "removing an origin from the site stops its sessions at once",
        origin_removed.status == 403,
        origin_removed.body.clone(),
    );
    run.expect(
        "opening, ending and revoking are audited; no token or digest is",
        count(&client, "SELECT count(*) FROM base_audit_actions WHERE event = 'visitor.session.opened'").await >= 4
            && count(&client, "SELECT count(*) FROM base_audit_actions WHERE event = 'visitor.session.revoked'").await == 1
            && count(&client, "SELECT count(*) FROM base_audit_actions WHERE event LIKE 'visitor.%' AND payload::text LIKE '%vst_%'").await == 0,
        "guest actions for the tenant",
    );

    println!("== another tenant's visitor");
    let other = open(&addr, "other-desk", Some(PAGE)).await.json();
    let other_token = other["token"].as_str().unwrap_or_default().to_owned();
    let mut ows = socket(&addr, Some(PAGE)).await.expect("upgrade");
    hello(&mut ows, &other_token, None).await;
    let _ = until(&mut ows, "ready", 5).await;
    let items = until(&mut ows, "items", 5).await;
    let texts: Vec<String> = items
        .as_ref()
        .map(|f| {
            f["items"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|i| i["text"].to_string())
                .collect()
        })
        .unwrap_or_default();
    run.expect(
        "it sees only its own greeting, none of tenant 1's conversations",
        texts == vec!["\"Hello! How can we help?\"".to_string()],
        format!("{texts:?}"),
    );
    let tenant_one_before = conversations()
        .chats
        .lock()
        .unwrap()
        .get(&(1, subject.clone()))
        .map(|c| c.items.len())
        .unwrap_or(0);
    say(&mut ows, json!({"type": "message", "id": 1, "body": {"text": "hi", "subject": subject, "tenant": 1}})).await;
    let own = until(&mut ows, "ack", 5).await;
    let tenant_one_items = conversations()
        .chats
        .lock()
        .unwrap()
        .get(&(1, subject.clone()))
        .map(|c| c.items.len())
        .unwrap_or(0);
    let tenant_two_items: usize = conversations()
        .chats
        .lock()
        .unwrap()
        .iter()
        .filter(|(k, _)| k.0 == 2)
        .map(|(_, c)| c.items.len())
        .sum();
    run.expect(
        "a subject or tenant named in a frame is ignored: the message landed in its own conversation",
        own.is_some()
            && tenant_one_before > 0
            && tenant_one_items == tenant_one_before
            && tenant_two_items == 2,
        format!("tenant 1 conversation {tenant_one_before} items before and {tenant_one_items} after, tenant 2 {tenant_two_items}"),
    );
    let swapped = post(
        &addr,
        "/visitor/v1/sessions",
        Some(PAGE),
        json!({"channel": CHANNEL, "site": "front-desk"}),
    )
    .await;
    run.expect(
        "a site belongs to its own tenant",
        swapped.json()["session"].is_string(),
        "tenant 1 site opens a tenant 1 session",
    );

    println!("== rate limits");
    setting(&setup, &mut client, "visitor.messages_per_minute", json!(5)).await?;
    let mut limited = None;
    let mut rate = socket(&addr, Some(PAGE)).await.expect("upgrade");
    let burst = open(&addr, "front-desk", Some(PAGE)).await.json();
    hello(&mut rate, burst["token"].as_str().unwrap_or_default(), None).await;
    let _ = until(&mut rate, "ready", 5).await;
    for n in 0..8 {
        say(&mut rate, json!({"type": "typing", "body": {"n": n}})).await;
        if let Some(frame) = until(&mut rate, "error", 1).await {
            limited = Some(frame);
            break;
        }
    }
    run.expect(
        "past the per-session budget a frame is refused without acting",
        limited
            .as_ref()
            .is_some_and(|f| f["code"] == "rate_limited"),
        format!("{limited:?}"),
    );
    say(
        &mut rate,
        json!({"type": "message", "id": 77, "body": {"text": "one too many"}}),
    )
    .await;
    let named = until(&mut rate, "error", 2).await;
    run.expect(
        "a message refused by the budget is named by its id, so the browser can give it back",
        named
            .as_ref()
            .is_some_and(|f| f["id"] == 77 && f["code"] == "rate_limited"),
        format!("{named:?}"),
    );
    setting(
        &setup,
        &mut client,
        "visitor.messages_per_minute",
        json!(60),
    )
    .await?;
    setting(
        &setup,
        &mut client,
        "visitor.sessions_per_sender_per_minute",
        json!(2),
    )
    .await?;
    let before = count(&client, "SELECT count(*) FROM visitor_sessions").await;
    let mut statuses = Vec::new();
    for _ in 0..4 {
        statuses.push(open(&addr, "front-desk", Some(PAGE)).await.status);
    }
    let after_count = count(&client, "SELECT count(*) FROM visitor_sessions").await;
    run.expect(
        "new sessions per sender are limited; the refused ones stored nothing",
        statuses.contains(&429) && after_count - before <= 2,
        format!("{statuses:?}, {} stored", after_count - before),
    );
    setting(
        &setup,
        &mut client,
        "visitor.sessions_per_sender_per_minute",
        json!(10_000),
    )
    .await?;
    setting(&setup, &mut client, "visitor.sockets_per_session", json!(1)).await?;
    let single = open(&addr, "front-desk", Some(PAGE)).await.json();
    let single_token = single["token"].as_str().unwrap_or_default().to_owned();
    let mut first = socket(&addr, Some(PAGE)).await.expect("upgrade");
    hello(&mut first, &single_token, None).await;
    let _ = until(&mut first, "ready", 5).await;
    let mut second_socket = socket(&addr, Some(PAGE)).await.expect("upgrade");
    hello(&mut second_socket, &single_token, None).await;
    let over = until(&mut second_socket, "closed", 5).await;
    run.expect(
        "a session's second socket is refused at the per-session limit",
        over.is_some_and(|f| f["reason"] == "rate_limited"),
        "closed rate_limited",
    );
    setting(&setup, &mut client, "visitor.sockets_per_session", json!(3)).await?;

    println!("== a visitor token against every staff route");
    let t = token.as_str();
    let bearer = format!("Bearer {t}");
    let cookie_staff = format!("bilibi_session={t}");
    let thirty_two = format!("bilibi_session={}", &t[4..36]);
    let cookie_portal = format!("bilibi_portal={t}");
    let cases: Vec<StaffCase> = vec![
        (
            "staff page, token as the session cookie",
            "/companies",
            vec![("Cookie", cookie_staff.as_str()), ("Accept", "text/html")],
        ),
        (
            "staff page, first 32 characters as the cookie",
            "/companies",
            vec![("Cookie", thirty_two.as_str()), ("Accept", "text/html")],
        ),
        (
            "staff settings, bearer",
            "/settings/sessions",
            vec![("Authorization", bearer.as_str()), ("Accept", "text/html")],
        ),
        (
            "operator page, token as cookie",
            "/system/sessions",
            vec![("Cookie", cookie_staff.as_str()), ("Accept", "text/html")],
        ),
        (
            "portal page, token as the portal cookie",
            "/portal",
            vec![("Cookie", cookie_portal.as_str()), ("Accept", "text/html")],
        ),
        (
            "agent API, bearer",
            "/api/v1/search?q=company",
            vec![("Authorization", bearer.as_str())],
        ),
    ];
    for (label, path, headers) in cases {
        let reply = http(&addr, "GET", path, &headers, "").await;
        let location = reply.header("location").unwrap_or("").to_owned();
        run.expect(
            label,
            matches!(reply.status, 301..=303 | 401 | 403 | 404 | 405)
                && !reply.body.contains("Companies"),
            format!("GET {path} -> {} {location}", reply.status),
        );
    }
    let mcp = http(
        &addr,
        "POST",
        "/mcp",
        &[
            ("Authorization", bearer.as_str()),
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
        ],
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search","arguments":{"q":"company"}}}"#,
    )
    .await;
    run.expect(
        "MCP, bearer",
        mcp.status == 401,
        format!("POST /mcp -> {}", mcp.status),
    );
    let sign_out = http(
        &addr,
        "POST",
        "/logout",
        &[
            ("Cookie", cookie_staff.as_str()),
            ("Origin", host.as_str()),
            ("Content-Type", "application/x-www-form-urlencoded"),
        ],
        "csrf=x",
    )
    .await;
    run.expect(
        "sign-out, token as cookie",
        matches!(sign_out.status, 401 | 403),
        format!("POST /logout -> {}", sign_out.status),
    );
    for path in ["/live", "/portal/live"] {
        let mut request = format!("ws://{addr}{path}")
            .into_client_request()
            .expect("request");
        request
            .headers_mut()
            .insert("Origin", host.parse().expect("origin"));
        request.headers_mut().insert(
            "Cookie",
            format!("bilibi_session={t}; bilibi_portal={t}")
                .parse()
                .expect("cookie"),
        );
        let upgraded = tokio_tungstenite::connect_async(request).await;
        run.expect(
            &format!("live socket {path}, token as both cookies"),
            upgraded.is_err(),
            format!("{:?}", upgraded.as_ref().err().map(|e| e.to_string())),
        );
    }
    let visitor_route_with_cookie = http(
        &addr,
        "GET",
        "/visitor/v1/live",
        &[("Cookie", "bilibi_session=00000000000000000000000000000000")],
        "",
    )
    .await;
    run.expect(
        "the visitor socket takes no cookie: a staff cookie opens nothing there",
        visitor_route_with_cookie.status == 403 || visitor_route_with_cookie.status == 400,
        format!("{}", visitor_route_with_cookie.status),
    );
    run.expect(
        "no staff session row was created or touched by any of it",
        count(&client, "SELECT count(*) FROM sessions WHERE user_id IS NOT NULL AND last_activity > now() - interval '5 minutes'").await == 0,
        "sessions untouched",
    );

    let _ = stop.send(true);
    let _ = running.await;
    println!();
    if run.failures == 0 {
        println!("every case behaved as documented");
        Ok(())
    } else {
        println!("{} case(s) failed", run.failures);
        Err(Error::Internal)
    }
}
fn opened_refusal() -> String {
    json!({"error": "refused"}).to_string()
}
/// One file over the socket: the announcement, the chunks, the answer.
async fn send_file(ws: &mut Socket, id: u32, name: &str, bytes: &[u8]) -> Option<Value> {
    say(
        ws,
        json!({"type": "upload", "id": id, "name": name, "size": bytes.len()}),
    )
    .await;
    loop {
        let frame = next(ws, 30).await;
        match frame {
            Some(frame) if frame["type"] == "upload_ready" => break,
            Some(frame) => eprintln!("  (while waiting for upload_ready: {frame})"),
            None => {
                eprintln!("  (no upload_ready for {name})");
                return None;
            }
        }
    }
    let mut offset = 0usize;
    for chunk in bytes.chunks(64 * 1024) {
        let mut frame = id.to_be_bytes().to_vec();
        frame.extend_from_slice(&(offset as u32).to_be_bytes());
        frame.extend_from_slice(chunk);
        ws.send(Message::Binary(frame.into())).await.ok()?;
        offset += chunk.len();
    }
    loop {
        let frame = next(ws, 30).await?;
        if frame["type"] == "ack" || frame["type"] == "error" {
            return Some(frame);
        }
    }
}
