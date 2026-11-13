//! Walkthrough of issue 548: machine traffic cannot crowd sign-in out of its
//! per-sender limiter, and an IPv6 sender is counted by its /64. Run against an
//! explicitly supplied disposable, migrated DATABASE_URL. Nobody signs in and
//! nothing is seeded.
//!
//! It serves the real host on a free local port and sends requests as a
//! loopback proxy would forward them (`X-Forwarded-For`): one device
//! authorization request from each of 4,200 addresses in one minute, more than
//! the 4,096 keys the shared limiter used to hold; then, in the same minute, a
//! sign-in and a device request from addresses not seen before. Then sign-in
//! and "Forgot your password?" from a fresh address each time in one IPv6 /64.
//! The defaults apply: `user.sign_in.per_sender_per_minute` (10) and
//! `user.recovery.per_sender_per_minute` (5). Each line says what was sent and
//! what answered; a line starting `FAIL` makes the run exit non-zero.
//!
//! Environment: BILIBI_DISPOSABLE_THROTTLE_DEMO=yes and DATABASE_URL.
use bilibi_database::{pool, Error, Result};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::TcpStream};

const FLOOD: usize = 4_200;
const SIGN_IN_LIMIT: usize = 10;
const RECOVERY_LIMIT: usize = 5;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_THROTTLE_DEMO").as_deref() != Ok("yes") {
        eprintln!(
            "Set BILIBI_DISPOSABLE_THROTTLE_DEMO=yes: this writes to the database it is given."
        );
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
    let origin = format!("http://{addr}");
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            bilibi_web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let mut run = Run { failures: 0 };
    // Each attempt names its own unknown address, so no per-account lock
    // answers before the per-sender one.
    let sign_in = {
        let (addr, origin) = (addr.clone(), origin.clone());
        move |sender: String| {
            let (addr, origin) = (addr.clone(), origin.clone());
            async move {
                let form = format!(
                    "login%5Bemail%5D={}&login%5Bpassword%5D=not-the-password",
                    nobody(&sender)
                );
                post(&addr, &origin, "/login", &sender, &form).await
            }
        }
    };
    let device = {
        let (addr, origin) = (addr.clone(), origin.clone());
        move |sender: String| {
            let (addr, origin) = (addr.clone(), origin.clone());
            async move {
                post(
                    &addr,
                    &origin,
                    "/oauth/device_authorization",
                    &sender,
                    "client_id=walkthrough-unknown-client",
                )
                .await
            }
        }
    };

    println!("== {FLOOD} device requests, one from each address, in one minute");
    fresh_minute().await;
    let started = minute();
    let slots = Arc::new(tokio::sync::Semaphore::new(100));
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..FLOOD {
        let slots = slots.clone();
        let device = device.clone();
        tasks.spawn(async move {
            let _slot = slots.acquire_owned().await.expect("semaphore");
            device(format!("198.18.{}.{}", n / 256, n % 256)).await
        });
    }
    let mut flood = Vec::with_capacity(FLOOD);
    while let Some(done) = tasks.join_next().await {
        flood.push(done.map_err(internal)?);
    }
    let throttled = flood.iter().filter(|r| r.slow_down()).count();
    run.expect(
        "every address of the flood is answered, none told to slow down",
        throttled == 0 && flood.iter().all(|r| r.status == 400),
        format!("{}, {throttled} slow_down", summary(&flood)),
    );
    let reply = sign_in("203.0.113.7".into()).await;
    run.expect(
        "a person at an address not seen this minute reaches the password check",
        reply.status == 401,
        reply.status,
    );
    let reply = sign_in("198.18.0.5".into()).await;
    run.expect(
        "so does a person at an address of the flood: sign-in counts apart",
        reply.status == 401,
        reply.status,
    );
    let reply = device("203.0.113.8".into()).await;
    run.expect(
        "a machine at a new address is still answered",
        reply.status == 400 && !reply.slow_down(),
        format!("{} slow_down={}", reply.status, reply.slow_down()),
    );
    let finished = minute();
    run.expect(
        "the flood and these requests fell in one minute",
        started == finished,
        format!("minute {started} to {finished}"),
    );

    println!("== sign-in, a fresh address each time in one IPv6 /64");
    aligned().await;
    let mut statuses = Vec::with_capacity(SIGN_IN_LIMIT + 2);
    for n in 1..=SIGN_IN_LIMIT + 2 {
        statuses.push(sign_in(format!("2001:db8:7::{n:x}")).await.status);
    }
    run.expect(
        "a /64 shares one sign-in budget, whatever address in it sends",
        statuses[..SIGN_IN_LIMIT].iter().all(|s| *s == 401)
            && statuses[SIGN_IN_LIMIT..].iter().all(|s| *s == 429),
        format!("{statuses:?}"),
    );
    let reply = sign_in("2001:db8:8::1".into()).await;
    run.expect(
        "the next /64 has its own budget",
        reply.status == 401,
        reply.status,
    );

    println!("== \"Forgot your password?\", a fresh address each time in one IPv6 /64");
    aligned().await;
    let mut statuses = Vec::with_capacity(RECOVERY_LIMIT + 1);
    for n in 1..=RECOVERY_LIMIT + 1 {
        let sender = format!("2001:db8:9::{n:x}");
        let form = format!("forgot%5Bemail%5D={}", nobody(&sender));
        statuses.push(
            post(&addr, &origin, "/forgot-password", &sender, &form)
                .await
                .status,
        );
    }
    run.expect(
        "a /64 shares one recovery budget",
        statuses[..RECOVERY_LIMIT].iter().all(|s| *s == 200)
            && statuses[RECOVERY_LIMIT..].iter().all(|s| *s == 429),
        format!("{statuses:?}"),
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
/// An address no account has, made from the sender: `nobody-2001-db8-7--1`
/// at example.test, form-encoded.
fn nobody(sender: &str) -> String {
    format!("nobody-{}%40example.test", sender.replace([':', '.'], "-"))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}
fn minute() -> u64 {
    now() / 60
}
/// Waits for the start of a minute unless this one has just begun, so the
/// flood has the whole window.
async fn fresh_minute() {
    let second = now() % 60;
    if second > 2 {
        tokio::time::sleep(Duration::from_secs(61 - second)).await;
    }
}
/// Waits for a fresh minute when this one is nearly over, so one burst is
/// counted in one window.
async fn aligned() {
    let second = now() % 60;
    if second > 40 {
        tokio::time::sleep(Duration::from_secs(61 - second)).await;
    }
}
fn summary(replies: &[Reply]) -> String {
    let mut counts: BTreeMap<u16, usize> = BTreeMap::new();
    for reply in replies {
        *counts.entry(reply.status).or_default() += 1;
    }
    counts
        .iter()
        .map(|(status, count)| format!("{count}× {status}"))
        .collect::<Vec<_>>()
        .join(", ")
}
struct Reply {
    status: u16,
    body: String,
}
impl Reply {
    /// The OAuth limiter's refusal (RFC 8628 `slow_down`).
    fn slow_down(&self) -> bool {
        self.body.contains("slow_down")
    }
}
/// One form POST on a fresh connection, as a loopback proxy forwards it for
/// `sender`, from a page of this origin.
async fn post(addr: &str, origin: &str, path: &str, sender: &str, form: &str) -> Reply {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nOrigin: {origin}\r\nConnection: close\r\nX-Forwarded-For: {sender}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{form}",
        form.len()
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        body: body.to_owned(),
    }
}
