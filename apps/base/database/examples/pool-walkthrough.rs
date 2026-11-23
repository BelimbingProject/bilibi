//! Manual pool walkthrough on any reachable PostgreSQL database; no test
//! harness. It reads and writes no table: every statement is `SELECT 1`.
use bilibi_database::{pool, pool_with, Connection, Db, Error, PoolLimits, Result};
use std::process::Command;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(1);

/// Why this walkthrough holds several connections in one task: it fills the
/// pool by hand to prove the limits, which is not what a caller does.
const BY_HAND: &str = "the pool walkthrough fills the pool by hand";

/// One caller's answer from the pool, without keeping the connection.
async fn ask(db: &Db) -> Result<()> {
    db.get_detached(BY_HAND)
        .await
        .map(|_| ())
        .map_err(Error::from)
}

/// What a task that asks for a second connection gets, run as a child so
/// that its log lines are read here and not mistaken for a real hit.
async fn second_connection(url: &str) -> Result<()> {
    let db = pool_with(
        url,
        PoolLimits {
            size: 4,
            wait: WAIT,
        },
    )?;
    let held = db.get().await?;
    // Each second ask is refused immediately with both sites.
    for _ in 0..3 {
        let started = Instant::now();
        let error = match db.get().await {
            Ok(_) => return Err(Error::Internal),
            Err(error) => error.to_string(),
        };
        assert!(started.elapsed() < WAIT, "the refusal waited for the pool");
        assert_eq!(error.matches(concat!(file!(), ":")).count(), 2, "{error}");
        assert!(error.contains("get_detached"));
    }
    println!("second=refused");
    // None of these is a second connection of this task.
    let detached = db.get_detached(BY_HAND).await?;
    let other_task = tokio::spawn({
        let db = db.clone();
        async move { db.get().await.is_ok() }
    });
    assert!(other_task.await.map_err(|_| Error::Internal)?);
    drop((held, detached));
    drop(db.get().await?);
    let closed = Connection::take(db.get().await?);
    drop(db.get().await?);
    drop(closed);
    // A connection kept detached does not count against later asks.
    let kept = db.get_detached(BY_HAND).await?;
    drop(db.get().await?);
    drop(kept);
    println!("others=served");
    Ok(())
}

/// The child's verdict lines and its log.
fn second_connection_of(url: &str) -> Result<(bool, String, String)> {
    let mut child = Command::new(std::env::current_exe().map_err(|_| Error::Internal)?);
    child
        .arg("second-connection")
        .env("DATABASE_URL", url)
        .env_remove("RUST_BACKTRACE");
    let output = child.output().map_err(|_| Error::Internal)?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// The limits a child process reads from its environment, as it prints them.
fn limits_of(url: &str, size: Option<&str>, wait: Option<&str>) -> Result<Option<String>> {
    let mut child = Command::new(std::env::current_exe().map_err(|_| Error::Internal)?);
    child
        .arg("limits")
        .env("DATABASE_URL", url)
        .env_remove(PoolLimits::SIZE_ENV)
        .env_remove(PoolLimits::WAIT_ENV);
    if let Some(size) = size {
        child.env(PoolLimits::SIZE_ENV, size);
    }
    if let Some(wait) = wait {
        child.env(PoolLimits::WAIT_ENV, wait);
    }
    let output = child.output().map_err(|_| Error::Internal)?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?;
    if std::env::args().nth(1).as_deref() == Some("limits") {
        let db = pool(&url)?;
        println!(
            "size={} wait={}",
            db.status().max_size,
            db.timeouts().wait.map_or(0, |wait| wait.as_secs())
        );
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("second-connection") {
        return second_connection(&url).await;
    }

    let db = pool_with(
        &url,
        PoolLimits {
            size: 2,
            wait: WAIT,
        },
    )?;
    let first = db.get_detached(BY_HAND).await?;
    let second = db.get_detached(BY_HAND).await?;
    let started = Instant::now();
    let refused = ask(&db).await;
    let waited = started.elapsed();
    assert!(matches!(&refused, Err(error) if error.is_pool_exhausted()));
    assert!(waited >= WAIT && waited < WAIT * 3);
    println!("PASS empty pool: the third caller is refused after {waited:.1?}, not left waiting");
    let started = Instant::now();
    let refused = db.try_get_detached(BY_HAND).await.map_err(Error::from);
    assert!(matches!(&refused, Err(error) if error.is_pool_exhausted()));
    assert!(started.elapsed() < WAIT / 2);
    println!("PASS empty pool: a caller that will not wait is refused at once");

    first.query_one("SELECT 1", &[]).await?;
    drop(second);
    let third = db.get_detached(BY_HAND).await?;
    third.query_one("SELECT 1", &[]).await?;
    println!("PASS a returned connection serves the next caller");

    let waiter = tokio::spawn({
        let db = db.clone();
        async move { ask(&db).await }
    });
    tokio::time::sleep(WAIT / 4).await;
    drop(first);
    assert!(waiter.await.map_err(|_| Error::Internal)?.is_ok());
    println!("PASS a caller that waits less than the limit is served");
    drop(third);

    // The shape that used to lock a process for good: more callers than
    // connections, each holding one and asking for a second.
    let started = Instant::now();
    let mut callers = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let db = db.clone();
        callers.spawn(async move {
            let Ok(held) = db.get_detached(BY_HAND).await else {
                return (false, false);
            };
            let nested = ask(&db).await;
            held.query_one("SELECT 1", &[]).await.ok();
            (true, nested.is_ok())
        });
    }
    let (mut held, mut nested) = (0, 0);
    let answered = tokio::time::timeout(WAIT * 10, async {
        while let Some(caller) = callers.join_next().await {
            let (got, second) = caller.map_err(|_| Error::Internal)?;
            held += usize::from(got);
            nested += usize::from(second);
        }
        Ok::<_, Error>(())
    })
    .await;
    assert!(matches!(answered, Ok(Ok(()))));
    assert!(held >= 2);
    println!(
        "PASS nested asks: 6 callers on 2 connections all answered in {:.1?} ({held} held one, {nested} got a second)",
        started.elapsed()
    );
    ask(&db).await?;
    println!("PASS the pool serves again afterwards");

    assert!(matches!(
        pool_with(
            &url,
            PoolLimits {
                size: 0,
                wait: WAIT
            }
        ),
        Err(Error::Invalid)
    ));
    assert_eq!(
        limits_of(&url, None, None)?.as_deref(),
        Some("size=16 wait=5")
    );
    assert_eq!(
        limits_of(&url, Some("3"), Some("2"))?.as_deref(),
        Some("size=3 wait=2")
    );
    assert_eq!(
        limits_of(&url, Some(""), None)?.as_deref(),
        Some("size=16 wait=5")
    );
    for (size, wait) in [
        (Some("0"), None),
        (Some("1001"), None),
        (Some("many"), None),
        (None, Some("0")),
        (None, Some("301")),
    ] {
        assert_eq!(limits_of(&url, size, wait)?, None);
    }

    let (ok, verdict, log) = second_connection_of(&url)?;
    assert!(
        ok && verdict.contains("second=refused") && verdict.contains("others=served"),
        "{log}"
    );
    println!("PASS a second checkout is refused immediately with both call sites");
    println!("PASS served: another task, a detached connection, a connection taken out of the pool, an ask after the first was returned");
    println!("PASS environment: defaults 16 and 5 s, DATABASE_POOL_SIZE=3 and DATABASE_POOL_WAIT_SECONDS=2 are used, values outside the range refuse the start");
    Ok(())
}
