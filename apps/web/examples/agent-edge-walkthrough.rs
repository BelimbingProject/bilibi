//! Walkthrough of the agent edge's gate for unauthenticated traffic (issue
//! 544, `src/api_gate.rs`) against an explicitly supplied disposable, migrated
//! DATABASE_URL. Nobody signs in and nothing is seeded.
//!
//! It serves the real host on a free local port and sends requests as a
//! loopback proxy would forward them for several senders (`X-Forwarded-For`):
//! a discovery request that is recorded as before, a flood of unknown paths
//! with no token, a flood of made-up tokens, the two budgets kept apart, a
//! concurrent load from many senders over their budgets, and the minute's refusals arriving in
//! the audit log as counted rows. The default
//! `agent_access.oauth_requests_per_minute` (60) applies. Each line says what
//! was sent and what answered; a line starting `FAIL` makes the run exit
//! non-zero.
//!
//! Environment: BILIBI_DISPOSABLE_EDGE_DEMO=yes and DATABASE_URL.
use bilibi_database::{pool, Error, Result};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::TcpStream};

const LIMIT: usize = 60;
const DISCOVERY: &str = "/.well-known/oauth-authorization-server";

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_EDGE_DEMO").as_deref() != Ok("yes") {
        eprintln!("Set BILIBI_DISPOSABLE_EDGE_DEMO=yes: this writes to the database it is given.");
        return Err(Error::Invalid);
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_web::modules()?;
    bilibi_database::migrate(&db, &modules).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(internal)?;
    let addr = listener.local_addr().map_err(internal)?.to_string();
    let app = bilibi_web::App::new(db.clone(), &format!("http://{addr}"))?;
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            bilibi_web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let client = db.get().await?;
    let exchanges = || async {
        client
            .query_one("SELECT count(*) FROM base_integration_exchanges", &[])
            .await
            .map(|row| row.get::<_, i64>(0))
    };
    let audit_from: i64 = client
        .query_one("SELECT coalesce(max(id), 0) FROM base_audit_actions", &[])
        .await?
        .get(0);
    let mut run = Run { failures: 0 };
    // Every refusal this run expects in the audit log, by status.
    let mut expected: BTreeMap<u16, i64> = BTreeMap::new();

    println!("== a request that proves something is still one exchange row");
    let before = exchanges().await?;
    let reply = get(&addr, DISCOVERY, "198.51.100.1", None).await;
    let after = exchanges().await?;
    run.expect(
        "discovery answers and writes its row",
        reply.status == 200 && after - before == 1,
        format!("{} and {} new rows", reply.status, after - before),
    );

    println!("== unknown paths with no token, from one sender");
    aligned().await;
    let before = exchanges().await?;
    let statuses = burst(
        &addr,
        "/api/v1/nothing-here",
        "198.51.100.2",
        None,
        LIMIT + 10,
    )
    .await;
    let after = exchanges().await?;
    tally(&mut expected, &statuses);
    run.expect(
        "the budget answers 404, then 429; no refusal writes a row",
        statuses[..LIMIT].iter().all(|s| *s == 404)
            && statuses[LIMIT..].iter().all(|s| *s == 429)
            && after == before,
        format!("{}, {} new rows", summary(&statuses), after - before),
    );
    let reply = get(&addr, "/api/v1/nothing-here", "198.51.100.2", None).await;
    run.expect(
        "a sender out of budget is told when to come back",
        reply.status == 429 && reply.header("retry-after") == Some("60"),
        format!(
            "{} retry-after {:?}",
            reply.status,
            reply.header("retry-after")
        ),
    );
    tally(&mut expected, &[reply.status]);

    println!("== made-up tokens, from another sender");
    aligned().await;
    let before = exchanges().await?;
    let statuses = burst(
        &addr,
        "/api/v1/search?q=company",
        "198.51.100.3",
        Some("Bearer made-up-token"),
        LIMIT + 10,
    )
    .await;
    let after = exchanges().await?;
    tally(&mut expected, &statuses);
    run.expect(
        "refused tokens answer 401, then 429 before any lookup; no row",
        statuses[..LIMIT].iter().all(|s| *s == 401)
            && statuses[LIMIT..].iter().all(|s| *s == 429)
            && after == before,
        format!("{}, {} new rows", summary(&statuses), after - before),
    );
    let reply = get(&addr, DISCOVERY, "198.51.100.3", None).await;
    run.expect(
        "the same sender's requests without a token keep their own budget",
        reply.status == 200,
        reply.status,
    );
    let reply = get(
        &addr,
        "/api/v1/search?q=company",
        "198.51.100.2",
        Some("Bearer made-up-token"),
    )
    .await;
    run.expect(
        "a sender whose no-token budget is spent still reaches the token check",
        reply.status == 401,
        reply.status,
    );
    tally(&mut expected, &[reply.status]);

    println!("== load: 700 requests, 100 at a time, from 10 senders, no token");
    aligned().await;
    let before = exchanges().await?;
    // Both ends of every connection live in this process: stay well inside
    // the usual 1,024 open files.
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(100));
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..700 {
        let addr = addr.clone();
        let slots = slots.clone();
        let sender = format!("198.51.100.{}", 10 + n % 10);
        tasks.spawn(async move {
            let _slot = slots.acquire_owned().await.expect("semaphore");
            let path = if n % 2 == 0 { "/api/v1/x" } else { "/api/v1" };
            (sender.clone(), get(&addr, path, &sender, None).await.status)
        });
    }
    let mut by_sender: BTreeMap<String, Vec<u16>> = BTreeMap::new();
    while let Some(done) = tasks.join_next().await {
        let (sender, status) = done.map_err(internal)?;
        by_sender.entry(sender).or_default().push(status);
    }
    let after = exchanges().await?;
    let all: Vec<u16> = by_sender.values().flatten().copied().collect();
    tally(&mut expected, &all);
    let answered = by_sender
        .values()
        .map(|s| s.iter().filter(|s| **s == 404).count())
        .max()
        .unwrap_or(0);
    run.expect(
        "every sender is held to its budget and nothing is written",
        all.iter().all(|s| *s == 404 || *s == 429) && answered <= LIMIT && after == before,
        format!(
            "{}, at most {answered} answered per sender, {} new rows",
            summary(&all),
            after - before
        ),
    );

    println!("== the minute's refusals reach the audit log as counted rows");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_secs();
    tokio::time::sleep(Duration::from_secs(60 - now % 60 + 3)).await;
    let rows = client
        .query(
            "SELECT payload FROM base_audit_actions WHERE id > $1 AND event = 'agent_edge.refused' AND actor_type = 'guest' AND tenant_id IS NULL",
            &[&audit_from],
        )
        .await?;
    let mut counted: BTreeMap<u16, i64> = BTreeMap::new();
    for row in &rows {
        let payload: Value = row.get(0);
        let status = payload["status"].as_u64().unwrap_or(0) as u16;
        *counted.entry(status).or_default() += payload["count"].as_i64().unwrap_or(0);
    }
    run.expect(
        "the counts by status equal the refusals sent",
        counted == expected,
        format!("{} rows, {counted:?} for {expected:?}", rows.len()),
    );

    if run.failures > 0 {
        eprintln!("{} check(s) failed", run.failures);
        return Err(Error::Internal);
    }
    println!("all checks passed");
    Ok(())
}

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
/// Waits for a fresh minute when this one is nearly over, so one burst is
/// counted in one window.
async fn aligned() {
    let second = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() % 60);
    if second > 40 {
        tokio::time::sleep(Duration::from_secs(61 - second)).await;
    }
}
/// The statuses that are refusals the gate counts instead of recording.
fn tally(expected: &mut BTreeMap<u16, i64>, statuses: &[u16]) {
    for status in statuses {
        if [401, 404, 405, 429].contains(status) {
            *expected.entry(*status).or_default() += 1;
        }
    }
}
fn summary(statuses: &[u16]) -> String {
    let mut counts: BTreeMap<u16, usize> = BTreeMap::new();
    for status in statuses {
        *counts.entry(*status).or_default() += 1;
    }
    counts
        .iter()
        .map(|(status, count)| format!("{count}× {status}"))
        .collect::<Vec<_>>()
        .join(", ")
}
async fn burst(
    addr: &str,
    path: &str,
    sender: &str,
    authorization: Option<&str>,
    times: usize,
) -> Vec<u16> {
    let mut statuses = Vec::with_capacity(times);
    for _ in 0..times {
        statuses.push(get(addr, path, sender, authorization).await.status);
    }
    statuses
}
struct Reply {
    status: u16,
    headers: String,
}
impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}: ")))
    }
}
/// One GET on a fresh connection, as a loopback proxy forwards it for
/// `sender`.
async fn get(addr: &str, path: &str, sender: &str, authorization: Option<&str>) -> Reply {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nX-Forwarded-For: {sender}\r\n"
    );
    if let Some(authorization) = authorization {
        request.push_str(&format!("Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).to_string();
    let head = text
        .split_once("\r\n\r\n")
        .map_or(text.as_str(), |(h, _)| h);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .map(|l| {
            let (name, value) = l.split_once(": ").unwrap_or((l, ""));
            format!("{}: {value}\n", name.to_ascii_lowercase())
        })
        .collect();
    Reply { status, headers }
}
