//! Running proof that the upload bytes a server holds in memory for staff
//! and portal pages fit one budget (issue 552), against an explicitly
//! supplied disposable, migrated DATABASE_URL.
//!
//! A fixture page (`/upload-reference`, which lives only in this executable)
//! declares one file input that keeps its bytes in memory, as every input
//! does whose browser sends no SHA-256. The real host serves `/live` on a
//! free local port with `session.live.upload_buffer_bytes` at its smallest
//! value, 1 MiB. Two accounts each open the page and send files:
//!
//! - a file that fits is received whole;
//! - another sign-in's file that would pass the budget is refused `busy` at
//!   the chunk that would pass it, and what it had sent is given back;
//! - bytes are given back when the page's owner consumes the file;
//! - bytes are given back when the page closes.
//!
//! Each line says what was sent and what answered; a line starting `FAIL`
//! makes the run exit non-zero.
//!
//! Environment: BILIBI_DISPOSABLE_LIVE_UPLOAD_DEMO=yes and DATABASE_URL.
use bilibi_base::settings::{Settings, Target};
use bilibi_database::{pool, Db, Error, MutationBoundary, Result};
use bilibi_module_registry::{
    contributions::{Access, CompanyScope, Contribution, Method, Route, ROUTES},
    Layer, Module, Provenance, Service, Services,
};
use bilibi_security::Scope;
use bilibi_tenancy::Authentication;
use bilibi_ui::page::{
    LiveBody, LiveFuture, LiveRequest, LiveState, MountFuture, Page, PageBody, PageFuture,
    PageRequest, PageService, PanelSpec, Subscription,
};
use bilibi_ui::{UploadSpec, Uploads};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const MODULE: &str = "extensions/upload_reference";
const SERVICE: &str = "extensions/upload_reference.pages";
const PATH: &str = "/upload-reference";
const UPLOAD: &str = "file";
const BUDGET: u64 = 1_048_576;
const CHUNK: usize = 64 * 1024;

/// The account whose page consumes its completed files on its next refresh,
/// and the bytes consumed so far.
static CONSUMER: Mutex<(Option<(i64, i64)>, usize)> = Mutex::new((None, 0));

struct Reference;
struct ReferenceState {
    uploads: Uploads,
}
fn body() -> String {
    "<section id=upload-reference><h1>Upload reference</h1></section>".into()
}
impl Page<Scope, Error> for Reference {
    fn render<'a>(&'a self, _request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            Ok(PageBody {
                html: body(),
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, _request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            let state: Box<dyn LiveState<Scope, Error>> = Box::new(ReferenceState {
                uploads: Uploads::new([
                    UploadSpec::new(UPLOAD, bilibi_ui::upload::MAX_FILE_SIZE).max_entries(20)
                ]),
            });
            Ok(Some(state))
        })
    }
}
impl LiveState<Scope, Error> for ReferenceState {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            let me = (request.scope.tenant(), request.scope.user());
            let mut consumer = CONSUMER.lock().unwrap();
            if consumer.0 == Some(me) {
                consumer.1 += self
                    .uploads
                    .consume(UPLOAD)
                    .iter()
                    .map(|file| file.bytes.len())
                    .sum::<usize>();
            }
            Ok(LiveBody {
                html: body(),
                ..LiveBody::default()
            })
        })
    }
    fn panels(&self) -> Vec<PanelSpec> {
        Vec::new()
    }
    fn subscriptions(&mut self) -> Vec<Subscription> {
        Vec::new()
    }
    fn uploads(&mut self) -> Option<&mut Uploads> {
        Some(&mut self.uploads)
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
    Contribution::new(ROUTES, "extensions/upload_reference.page", Provenance {module: MODULE, source: file!(), line: line!()}, &Route { company_scope: CompanyScope::Tenant,
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
/// One account's open page.
struct Tab {
    socket: Socket,
    requests: u64,
}
/// A browser session for the account, as sign-in stores it, and a framed
/// socket (no shell panels) with the fixture page mounted.
async fn open(db: &Db, addr: &str, host: &str, tenant: i64, user: i64) -> Result<Tab> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let csrf = uuid::Uuid::new_v4().simple().to_string();
    db.get().await?.execute(
        "INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at) VALUES($1,$2,$3,$4,now()+interval '24 hours')",
        &[&bilibi_base::session::session_id(&token), &tenant, &user, &csrf],
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
    let mounted = until(&mut socket, "page").await;
    if mounted.is_none() {
        eprintln!("walkthrough: account {user} could not mount {PATH}");
        return Err(Error::Internal);
    }
    Ok(Tab {
        socket,
        requests: 1,
    })
}
/// The next JSON frame of type `kind` within ten seconds; others are skipped.
async fn until(socket: &mut Socket, kind: &str) -> Option<Value> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let frame: Value = serde_json::from_str(&text).ok()?;
                if frame["type"] == kind {
                    return Some(frame);
                }
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => return None,
        }
    }
}
/// How sending one file ended.
#[derive(Debug, PartialEq, Eq)]
enum Sent {
    /// Every byte arrived.
    Whole,
    /// A chunk was refused with this code after this many bytes were held.
    Refused(String, u64),
    /// The host did not answer as the protocol says.
    Broken(String),
}
/// Announces a file of `size` bytes in the page's input and streams it, one
/// chunk at a time as the browser hook does.
async fn send_file(tab: &mut Tab, name: &str, size: usize) -> Sent {
    tab.requests += 1;
    let announce = json!({"type": "upload", "target": "page", "upload": UPLOAD, "request_id": tab.requests,
        "entries": [{"ref": name, "name": name, "size": size, "content_type": "application/octet-stream"}]});
    if tab
        .socket
        .send(Message::text(announce.to_string()))
        .await
        .is_err()
    {
        return Sent::Broken("the socket is closed".into());
    }
    let Some(registered) = until(&mut tab.socket, "upload").await else {
        return Sent::Broken("no answer to the announcement".into());
    };
    let Some(id) = registered["entries"][0]["id"].as_u64() else {
        return Sent::Broken(format!("the file was not accepted: {registered}"));
    };
    let bytes = vec![b'u'; size];
    let mut held = 0u64;
    for (index, chunk) in bytes.chunks(CHUNK).enumerate() {
        let mut frame = (id as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&((index * CHUNK) as u32).to_be_bytes());
        frame.extend_from_slice(chunk);
        if tab
            .socket
            .send(Message::Binary(frame.into()))
            .await
            .is_err()
        {
            return Sent::Broken("the socket closed mid-file".into());
        }
        let Some(reply) = until(&mut tab.socket, "chunk").await else {
            return Sent::Broken("no answer to a chunk".into());
        };
        if let Some(code) = reply["error"].as_str() {
            return Sent::Refused(code.to_owned(), held);
        }
        held = reply["received"].as_u64().unwrap_or(0);
        if reply["done"] == true {
            return Sent::Whole;
        }
    }
    Sent::Broken(format!("{held} of {size} bytes held and not done"))
}
async fn configure(db: &Db, services: &Services, key: &str, value: Value) -> Result<()> {
    let mut client = db.get().await?;
    client
        .execute(
            "INSERT INTO platform_operator(singleton,tenant_id) VALUES(true,1) ON CONFLICT DO NOTHING",
            &[],
        )
        .await?;
    let operator = services
        .get::<Authentication>("base/tenancy.authentication")?
        .authenticate(&**client, 1, 6, "walkthrough".into())
        .await?;
    let boundary = services.get::<MutationBoundary>("base/database.mutations")?;
    let settings = services.get::<Settings>("base/settings.store")?;
    let mut tx = boundary
        .begin(
            &mut client,
            operator.mutation_context("upload.walkthrough.configure"),
        )
        .await?;
    settings
        .put(&mut tx, &operator, Target::Global, key, value)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_LIVE_UPLOAD_DEMO").as_deref() != Ok("yes") {
        eprintln!(
            "Set BILIBI_DISPOSABLE_LIVE_UPLOAD_DEMO=yes: this installs the walkthrough fixture."
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
    // A socket reads the limit when it opens.
    configure(
        &db,
        &app.services,
        "session.live.upload_buffer_bytes",
        json!(BUDGET),
    )
    .await?;
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            bilibi_web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let mut run = Run { failures: 0 };
    let mut ada = open(&db, &addr, &host, 1, 1).await?;
    let mut lee = open(&db, &addr, &host, 1, 2).await?;
    println!("== two sign-ins have the page open; the server may hold {BUDGET} bytes of uploads");

    let first = send_file(&mut ada, "a.bin", 700_000).await;
    run.expect(
        "a 700,000-byte file that fits is received whole",
        first == Sent::Whole,
        format!("{first:?}"),
    );
    let second = send_file(&mut lee, "b.bin", 700_000).await;
    run.expect(
        "another sign-in's 700,000 bytes do not fit beside it: the chunk that would pass the budget is refused",
        second == Sent::Refused("busy".into(), 5 * CHUNK as u64),
        format!("{second:?}"),
    );
    let third = send_file(&mut lee, "c.bin", 300_000).await;
    run.expect(
        "the refused file gave back what it held: 300,000 bytes now fit",
        third == Sent::Whole,
        format!("{third:?}"),
    );
    let fourth = send_file(&mut ada, "d.bin", 100_000).await;
    run.expect(
        "the budget is the server's, not each page's: the first sign-in's next file is refused too",
        fourth == Sent::Refused("busy".into(), 0),
        format!("{fourth:?}"),
    );

    println!("== consuming and closing give the bytes back");
    // The page's owner takes its completed files on its next refresh; the
    // last byte of a file asks for one.
    *CONSUMER.lock().unwrap() = (Some((1, 2)), 0);
    let nudge = send_file(&mut lee, "e.bin", 1).await;
    let consumed = {
        let mut consumer = CONSUMER.lock().unwrap();
        consumer.0 = None;
        consumer.1
    };
    let fifth = send_file(&mut ada, "f.bin", 300_000).await;
    run.expect(
        "the owner consumed its files, so their bytes are free again",
        nudge == Sent::Whole && consumed == 300_001 && fifth == Sent::Whole,
        format!("{consumed} bytes consumed; the next 300,000-byte file: {fifth:?}"),
    );
    // Ada's page now holds 1,000,000 bytes; it closes without consuming them.
    let _ = ada.socket.close(None).await;
    drop(ada);
    let mut sixth = Sent::Broken("not tried".into());
    for attempt in 0..30 {
        sixth = send_file(&mut lee, &format!("g{attempt}.bin"), 1_000_000).await;
        if sixth == Sent::Whole || attempt >= 15 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    run.expect(
        "a closed page gives its bytes back: 1,000,000 bytes now fit",
        sixth == Sent::Whole,
        format!("{sixth:?}"),
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
