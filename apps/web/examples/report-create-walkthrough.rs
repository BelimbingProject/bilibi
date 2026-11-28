//! Observable proof that New Report creates a report through the live page
//! (BelimbingProject/bilibi#965). The staff router serves on this process's
//! Tokio worker threads with their default stack, as the app does; the
//! socket mounts `/reports`, opens New report and creates a list report over
//! Companies, which submits the `base.reports.create` agent API plan. An
//! unoptimised build once overflowed the worker stack on that create and
//! aborted the process. Only run against a disposable migrated PostgreSQL.
use bilibi_base::session::session_id;
use bilibi_database::{pool, Db, Error, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{future::IntoFuture, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const STAFF_COOKIE: &str = "__Host-bilibi_session";
const NAME: &str = "Companies by status review";

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn status(port: u16, host: &str, path: &str, token: &str) -> Result<u16> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|_| Error::Internal)?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nCookie: {STAFF_COOKIE}={token}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| Error::Internal)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .await
        .map_err(|_| Error::Internal)?;
    line.split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .ok_or(Error::Internal)
}

/// A session for the fixture's administrator (user 4, a grant-all role).
async fn session(db: &Db) -> Result<String> {
    bilibi_walkthrough_fixture::install(db).await?;
    let client = db.get().await?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    client
        .execute(
            "INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at,authentication_authority)
             VALUES($1,1,4,$2,now()+interval '1 hour','portable')",
            &[
                &session_id(&token),
                &uuid::Uuid::new_v4().simple().to_string(),
            ],
        )
        .await?;
    Ok(token)
}

async fn connect(port: u16, host: &str, token: &str) -> Result<Socket> {
    let mut request = format!("ws://127.0.0.1:{port}/live")
        .into_client_request()
        .map_err(|_| Error::Internal)?;
    let headers = request.headers_mut();
    headers.insert("Host", host.parse().map_err(|_| Error::Internal)?);
    headers.insert(
        "Origin",
        format!("http://{host}")
            .parse()
            .map_err(|_| Error::Internal)?,
    );
    headers.insert(
        "Cookie",
        format!("{STAFF_COOKIE}={token}")
            .parse()
            .map_err(|_| Error::Internal)?,
    );
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|_| Error::Internal)?;
    Ok(socket)
}

/// Sends one message and returns the reply that answers it. A closed socket
/// is the failure #965 showed: the server process had aborted.
async fn send(socket: &mut Socket, id: u64, label: &str, mut message: Value) -> Result<Value> {
    message["request_id"] = json!(id);
    socket
        .send(Message::text(message.to_string()))
        .await
        .map_err(|_| Error::Internal)?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(message) = socket.next().await {
            let message = message.map_err(|_| Error::Internal)?;
            if let Message::Text(text) = message {
                let reply: Value = serde_json::from_str(&text).map_err(|_| Error::Internal)?;
                if reply["request_id"] == id {
                    return Ok(reply);
                }
            }
        }
        eprintln!("FAIL  {label}: the socket closed without a reply");
        Err(Error::Internal)
    })
    .await
    .map_err(|_| {
        eprintln!("FAIL  {label}: timed out");
        Error::Internal
    })?
}

fn html(reply: &Value) -> &str {
    reply["html"].as_str().unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<()> {
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_web::migrate(&db).await?;
    let token = session(&db).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| Error::Internal)?;
    let port = listener.local_addr().map_err(|_| Error::Internal)?.port();
    let host = format!("staff.localhost:{port}");
    let app = bilibi_web::App::new(db.clone(), &format!("http://{host}"))?;
    let server = tokio::spawn(
        axum::serve(
            listener,
            bilibi_web::staff_router(app)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .into_future(),
    );

    assert_eq!(
        status(port, &host, "/reports", &token).await?,
        200,
        "GET /reports"
    );
    let mut socket = connect(port, &host, &token).await?;
    let event = |payload| json!({"type": "event", "target": "page", "payload": payload});

    let mounted = send(
        &mut socket,
        1,
        "mount reports",
        json!({"type": "mount", "path": "/reports", "params": {}}),
    )
    .await?;
    assert_eq!(mounted["type"], "page", "mount reports: {mounted}");
    assert!(
        html(&mounted).contains("id=\"reports-new\""),
        "mount reports: {mounted}"
    );
    println!("ok    mount /reports");

    let opened = send(
        &mut socket,
        2,
        "open New report",
        event(json!({"type": "open_new"})),
    )
    .await?;
    assert!(
        html(&opened).contains("name=\"report[name]\""),
        "open New report: {opened}"
    );
    println!("ok    open New report");

    let created = send(
        &mut socket,
        3,
        "create report",
        event(json!({
            "type": "create",
            "report[name]": NAME,
            "report[record_type]": "companies",
            "report[kind]": "rows",
        })),
    )
    .await?;
    let redirect = created["redirect"].as_str().unwrap_or_default();
    let id: i64 = redirect
        .strip_prefix("/reports/")
        .and_then(|rest| rest.strip_suffix("?tab=definition"))
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "create report: expected a redirect to the new report's definition, got {created}"
            )
        });
    println!("ok    create report: redirected to {redirect}");

    let client = db.get().await?;
    let row = client
        .query_one(
            "SELECT name, record_type, kind FROM base_reports WHERE tenant_id=1 AND id=$1",
            &[&id],
        )
        .await?;
    assert_eq!(
        (
            row.get::<_, String>(0),
            row.get::<_, String>(1),
            row.get::<_, String>(2)
        ),
        (NAME.to_owned(), "companies".to_owned(), "rows".to_owned()),
        "the saved report"
    );
    println!("ok    report {id} saved as a Companies list");

    assert_eq!(
        status(port, &host, redirect, &token).await?,
        200,
        "the report builder answers"
    );
    println!("ok    GET {redirect}");

    server.abort();
    println!("report create: New Report saved through the live page and its plan, and the server kept running");
    Ok(())
}
