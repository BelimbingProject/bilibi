//! Observable proof that the staff customer portal page (`/customer-portal`)
//! opens and its setup flow runs on the default worker stack (issue 903).
//!
//! Opening the page in a browser aborted the whole server: the live socket
//! mounts the page on a tokio worker thread (2 MiB of stack unless
//! `RUST_MIN_STACK` says otherwise), the live host's own frames already use
//! more than half of it in a debug build, and the page's template inlined
//! every form control and dialog into one render function whose frame took
//! the rest. A plain GET rendered the same template on a shallow stack and
//! passed, so this run takes the browser's path as well: the server is the
//! real staff router on a stock `#[tokio::main]` runtime, and each step is
//! one an operator takes.
//!
//! - GET `/customer-portal` as a signed-in admin: the page, with "Set up";
//! - a live socket mounts it as a browser tab does, with the shell;
//! - "Set up" opens the site form in place;
//! - saving it blank is refused field by field;
//! - saving an address and title stores the site and shows its facts;
//! - GET `/customer-portal` again shows the saved site.
//!
//! Each line says what was checked; a line starting `FAIL` makes the run exit
//! non-zero, and a stack overflow aborts it. Run it as the walkthroughs run
//! (`scripts/run-walkthroughs.sh customer-portal`): a debug build with
//! `RUST_MIN_STACK` unset.
//!
//! Environment: BILIBI_CUSTOMER_PORTAL_DEMO=disposable and DATABASE_URL, an
//! isolated database this installs the walkthrough fixture into.
use bilibi_base::session::session_id;
use bilibi_database::{pool, Db, Error, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{future::IntoFuture, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const PATH: &str = "/customer-portal";
/// `admin@example.test` in tenant 1: the fixture's grant-all role.
const ADMIN: i64 = 4;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn internal<E: std::fmt::Debug>(error: E) -> Error {
    eprintln!("walkthrough: {error:?}");
    Error::Internal
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

/// A browser session for the admin, as sign-in stores it.
async fn session(db: &Db) -> Result<String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let csrf = uuid::Uuid::new_v4().simple().to_string();
    db.get().await?.execute(
        "INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at,authentication_authority) VALUES($1,1,$2,$3,now()+interval '1 hour','portable')",
        &[&session_id(&token), &ADMIN, &csrf],
    ).await?;
    Ok(token)
}

/// The status and body of a GET as the browser sends it.
async fn get(addr: &str, token: &str) -> Result<(u16, String)> {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(internal)?;
    let request = format!(
        "GET {PATH} HTTP/1.1\r\nHost: {addr}\r\nCookie: __Host-bilibi_session={token}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(internal)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.map_err(internal)?;
    let response = String::from_utf8_lossy(&response).into_owned();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .ok_or(Error::Internal)?;
    Ok((status, response))
}

/// The next page frame within ten seconds, `None` when none came or the
/// socket closed (as it does when the server aborts).
async fn page(socket: &mut Socket) -> Option<Value> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let frame: Value = serde_json::from_str(&text).ok()?;
                if frame["type"] == "page" {
                    return Some(frame);
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

/// Sends one page event and returns the page it answers with.
async fn event(socket: &mut Socket, id: u64, payload: Value) -> Result<Option<Value>> {
    let message = json!({"type": "event", "payload": payload, "request_id": id});
    socket
        .send(Message::text(message.to_string()))
        .await
        .map_err(internal)?;
    Ok(page(socket).await)
}

fn html(frame: &Option<Value>) -> &str {
    frame
        .as_ref()
        .and_then(|frame| frame["html"].as_str())
        .unwrap_or("")
}

#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    if std::env::var("BILIBI_CUSTOMER_PORTAL_DEMO").as_deref() != Ok("disposable") {
        eprintln!(
            "Set BILIBI_CUSTOMER_PORTAL_DEMO=disposable: this installs the walkthrough fixture."
        );
        return Err(Error::Invalid);
    }
    if std::env::var_os("RUST_MIN_STACK").is_some() {
        eprintln!("Unset RUST_MIN_STACK: this proves the page on the default worker stack.");
        return Err(Error::Invalid);
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_web::migrate(&db).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let token = session(&db).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(internal)?;
    let addr = listener.local_addr().map_err(internal)?.to_string();
    let origin = format!("http://{addr}");
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    let server = tokio::spawn(
        axum::serve(
            listener,
            bilibi_web::staff_router(app)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .into_future(),
    );
    let mut run = Run { failures: 0 };

    println!("== the page as a GET");
    let (status, body) = get(&addr, &token).await?;
    run.expect(
        "GET /customer-portal answers with the page",
        status == 200 && body.contains("id=\"customer-portal-page\""),
        format!("status {status}"),
    );
    run.expect(
        "a tenant with no portal site is offered Set up",
        body.contains("id=\"customer-site-edit\""),
        "the Set up button is on the page",
    );

    println!("== the page in a browser tab: its live socket");
    let mut request = format!("ws://{addr}/live")
        .into_client_request()
        .map_err(internal)?;
    let headers = request.headers_mut();
    headers.insert("Origin", origin.parse().map_err(internal)?);
    headers.insert(
        "Cookie",
        format!("__Host-bilibi_session={token}")
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
    let mounted = page(&mut socket).await;
    run.expect(
        "the socket mounts the page, shell and all",
        html(&mounted).contains("id=\"customer-portal-page\""),
        format!("{} bytes of page", html(&mounted).len()),
    );

    println!("== setting up the portal site");
    let opened = event(&mut socket, 2, json!({"type": "edit_site"})).await?;
    run.expect(
        "Set up opens the site form in place",
        html(&opened).contains("id=\"customer-site-form\"")
            && html(&opened).contains("id=\"customer-site-address\""),
        "the form with its address field",
    );
    let refused = event(
        &mut socket,
        3,
        json!({"type": "save_site", "site[address]": "", "site[title]": ""}),
    )
    .await?;
    run.expect(
        "saving it blank is refused field by field",
        html(&refused).contains("id=\"customer-site-errors\"")
            && html(&refused).contains("be blank"),
        "the error summary names the blank fields",
    );
    let saved = event(
        &mut socket,
        4,
        json!({"type": "save_site", "site[address]": "walkthrough", "site[title]": "Walkthrough portal", "site[enabled]": "true"}),
    )
    .await?;
    run.expect(
        "saving an address and title shows the site's facts",
        html(&saved).contains("id=\"customer-site-facts\"")
            && html(&saved).contains("Walkthrough portal"),
        "the facts card with the title",
    );
    let stored: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM customer_portal_sites WHERE tenant_id=1 AND address='walkthrough' AND title='Walkthrough portal' AND enabled",
            &[],
        )
        .await?
        .get(0);
    run.expect(
        "the site is stored, on",
        stored == 1,
        format!("{stored} row"),
    );

    println!("== the page as a GET again");
    let (status, body) = get(&addr, &token).await?;
    run.expect(
        "GET /customer-portal shows the saved site",
        status == 200 && body.contains("id=\"customer-site-facts\""),
        format!("status {status}"),
    );

    server.abort();
    if run.failures > 0 {
        println!("{} check(s) failed", run.failures);
        return Ok(std::process::ExitCode::FAILURE);
    }
    println!("customer portal: the page and its setup flow passed on the default worker stack");
    Ok(std::process::ExitCode::SUCCESS)
}
