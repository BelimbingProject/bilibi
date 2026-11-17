//! Trusted lease lifecycle SQL, separate from audited owner writes. Every
//! statement that ends or renews a lease is fenced by its token, so a process
//! that lost a reading can never change the row its new owner holds.
use bilibi_database::{Db, Error, Result};
use std::time::{Duration, SystemTime};
use tokio_postgres::{GenericClient, Row};

/// What an operator is told about one reading. Derived when read, so it is
/// true even while no process is running to update the row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    /// One process holds a live lease and runs the reader.
    Running,
    /// The lease ran out without a heartbeat: the next poll takes it over.
    Lost,
    /// Due now and waiting for a process to claim it.
    Starting,
    /// Waiting out the delay after it ended or failed.
    Retrying,
}
impl State {
    pub const ALL: [Self; 4] = [Self::Running, Self::Starting, Self::Retrying, Self::Lost];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Lost => "lost",
            Self::Starting => "starting",
            Self::Retrying => "retrying",
        }
    }
    pub fn parse(state: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|known| known.as_str() == state)
    }
}
/// The four states, from the stored one and the clock.
const STATE: &str = "CASE WHEN state='running' AND lease_expires_at>clock_timestamp() THEN 'running' WHEN state='running' THEN 'lost' WHEN available_at>clock_timestamp() THEN 'retrying' ELSE 'starting' END";

/// One reading as operators see it: no lease token, and a key that is a
/// reference, never a person's data.
#[derive(Clone, Debug)]
pub struct Status {
    pub id: i64,
    pub tenant: i64,
    pub reader: String,
    pub key: String,
    pub state: State,
    /// Whether this server's composition declares the reader.
    pub mounted: bool,
    /// The process that holds, or last held, the lease.
    pub owner: Option<String>,
    pub heartbeat_at: Option<SystemTime>,
    pub started_at: Option<SystemTime>,
    /// When a waiting reading may start again.
    pub available_at: SystemTime,
    pub starts: i64,
    pub failures: i32,
    pub last_error: Option<String>,
    pub last_error_at: Option<SystemTime>,
}
impl Status {
    fn row(row: &Row) -> Result<Self> {
        Ok(Self {
            id: row.get("id"),
            tenant: row.get("tenant_id"),
            reader: row.get("reader_id"),
            key: row.get("key"),
            state: State::parse(row.get("shown")).ok_or(Error::Internal)?,
            mounted: false,
            owner: row.get("owner"),
            heartbeat_at: row.get("heartbeat_at"),
            started_at: row.get("started_at"),
            available_at: row.get("available_at"),
            starts: row.get("starts"),
            failures: row.get("failures"),
            last_error: row.get("last_error"),
            last_error_at: row.get("last_error_at"),
        })
    }
}
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Matches the reader ID, key or owner.
    pub search: String,
    pub state: Option<State>,
}
#[derive(Clone, Debug)]
pub struct Page {
    pub entries: Vec<Status>,
    pub total: i64,
}
/// Every tenant's readings, for a caller that already proved the operator.
pub(crate) async fn list(
    client: &impl GenericClient,
    filter: &Filter,
    page: i64,
    page_size: i64,
) -> Result<Page> {
    let pattern = {
        let search = filter.search.trim();
        (!search.is_empty()).then(|| {
            let escaped = search
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        })
    };
    let state = filter.state.map(State::as_str);
    let from = format!("FROM (SELECT *, {STATE} AS shown FROM base_resident_readings) r WHERE ($1::text IS NULL OR shown=$1) AND ($2::text IS NULL OR reader_id ILIKE $2 OR key ILIKE $2 OR owner ILIKE $2)");
    let total = client
        .query_one(&format!("SELECT count(*) {from}"), &[&state, &pattern])
        .await?
        .get(0);
    let offset = (page - 1).checked_mul(page_size).ok_or(Error::Invalid)?;
    let rows = client
        .query(
            &format!("SELECT id,tenant_id,reader_id,key,shown,owner,heartbeat_at,started_at,available_at,starts,failures,last_error,last_error_at {from} ORDER BY reader_id,tenant_id,key LIMIT $3 OFFSET $4"),
            &[&state, &pattern, &page_size, &offset],
        )
        .await?;
    Ok(Page {
        entries: rows.iter().map(Status::row).collect::<Result<_>>()?,
        total,
    })
}
/// The one-line status System Info shows: platform-wide counts only, no
/// reader, key, tenant or owner.
pub async fn health_status(client: &impl GenericClient) -> Result<String> {
    let row = client
        .query_one(
            &format!("SELECT count(*) FILTER (WHERE shown='running'), count(*) FILTER (WHERE shown='starting'), count(*) FILTER (WHERE shown='retrying'), count(*) FILTER (WHERE shown='lost') FROM (SELECT {STATE} AS shown FROM base_resident_readings) r"),
            &[],
        )
        .await?;
    let count = |index: usize| row.get::<_, i64>(index);
    if (0..4).map(count).sum::<i64>() == 0 {
        return Ok("None kept".into());
    }
    Ok(format!(
        "{} running, {} starting, {} waiting to retry{}",
        count(0),
        count(1),
        count(2),
        if count(3) > 0 {
            format!(", {} without a live owner", count(3))
        } else {
            String::new()
        }
    ))
}

/// A reading this process now owns.
pub(crate) struct Claim {
    pub id: i64,
    pub tenant: i64,
    pub reader: String,
    pub module: String,
    pub key: String,
    pub token: String,
    pub failures: i32,
}
/// Reaping and claiming are one transaction. An expired lease becomes due at
/// once, so the claim below can take it over in the same poll. `SKIP LOCKED`
/// lets independent processes claim different readings. Both searches compare
/// with `now()` (this transaction's start), never `clock_timestamp()`: a
/// volatile clock cannot bound an index scan (`base_resident_readings_leases`,
/// `base_resident_readings_due`). Only readers in `readers`, the ones this
/// process declares, are claimed.
pub(crate) async fn claim(
    db: &Db,
    owner: &str,
    readers: &[String],
    lease_seconds: i32,
    limit: i64,
) -> Result<Vec<Claim>> {
    let mut client = db.get().await?;
    let tx = client.transaction().await?;
    tx.execute("WITH expired AS (SELECT id FROM base_resident_readings WHERE state='running' AND lease_expires_at<=now() ORDER BY lease_expires_at,id FOR UPDATE SKIP LOCKED LIMIT 100) UPDATE base_resident_readings r SET state='waiting',lease_token=NULL,lease_expires_at=NULL,available_at=now(),last_error='owner_lost',last_error_at=clock_timestamp(),updated_at=clock_timestamp() FROM expired e WHERE r.id=e.id", &[]).await?;
    let rows = if limit > 0 {
        tx.query("WITH candidate AS (SELECT id FROM base_resident_readings WHERE state='waiting' AND available_at<=now() AND reader_id=ANY($1) ORDER BY available_at,id FOR UPDATE SKIP LOCKED LIMIT $2) UPDATE base_resident_readings r SET state='running',owner=$3,lease_token=gen_random_uuid()::text,lease_expires_at=clock_timestamp()+make_interval(secs=>$4::integer),started_at=clock_timestamp(),heartbeat_at=clock_timestamp(),starts=starts+1,updated_at=clock_timestamp() FROM candidate c WHERE r.id=c.id RETURNING r.id,r.tenant_id,r.reader_id,r.reader_module,r.key,r.lease_token,r.failures", &[&readers, &limit, &owner, &lease_seconds]).await?
    } else {
        Vec::new()
    };
    tx.commit().await?;
    Ok(rows
        .iter()
        .map(|row| Claim {
            id: row.get(0),
            tenant: row.get(1),
            reader: row.get(2),
            module: row.get(3),
            key: row.get(4),
            token: row.get(5),
            failures: row.get(6),
        })
        .collect())
}
/// What one heartbeat learned about the readings a process holds.
pub(crate) struct Beat {
    /// Renewed: the lease runs a full length from now.
    pub renewed: Vec<i64>,
    /// Still ours but not renewed, because the reader's own fenced
    /// transaction holds the row. Anything in neither list is lost.
    pub busy: Vec<i64>,
}
/// One statement renews every reading the process holds. A row its reader has
/// locked in `Reading::begin` is skipped, never waited for, so one slow owner
/// transaction cannot delay the renewal of every other reading.
pub(crate) async fn heartbeat(
    db: &Db,
    ids: &[i64],
    tokens: &[String],
    lease_seconds: i32,
) -> Result<Beat> {
    let client = db.get().await?;
    let renewed: Vec<i64> = client.query("WITH held AS (SELECT r.id FROM base_resident_readings r JOIN unnest($1::bigint[],$2::text[]) AS h(id,token) ON r.id=h.id AND r.lease_token=h.token WHERE r.state='running' AND r.lease_expires_at>clock_timestamp() FOR UPDATE OF r SKIP LOCKED) UPDATE base_resident_readings r SET lease_expires_at=clock_timestamp()+make_interval(secs=>$3::integer),heartbeat_at=clock_timestamp(),updated_at=clock_timestamp() FROM held WHERE r.id=held.id RETURNING r.id", &[&ids, &tokens, &lease_seconds]).await?.iter().map(|row| row.get(0)).collect();
    if renewed.len() == ids.len() {
        return Ok(Beat {
            renewed,
            busy: Vec::new(),
        });
    }
    let alive: Vec<i64> = client.query("SELECT r.id FROM base_resident_readings r JOIN unnest($1::bigint[],$2::text[]) AS h(id,token) ON r.id=h.id AND r.lease_token=h.token WHERE r.state='running' AND r.lease_expires_at>clock_timestamp()", &[&ids, &tokens]).await?.iter().map(|row| row.get(0)).collect();
    let busy = alive
        .into_iter()
        .filter(|id| !renewed.contains(id))
        .collect();
    Ok(Beat { renewed, busy })
}
/// Hands a lease back: the reading waits `delay` and may then be claimed by
/// any process. `code` records why it ended; `None` leaves the last error as
/// it was (a clean stop is not an error). Fenced by the token.
pub(crate) async fn end(
    db: &Db,
    id: i64,
    token: &str,
    delay: Duration,
    failures: i32,
    code: Option<&str>,
) -> Result<bool> {
    Ok(db.get().await?.execute("UPDATE base_resident_readings SET state='waiting',lease_token=NULL,lease_expires_at=NULL,available_at=clock_timestamp()+make_interval(secs=>$3::double precision),failures=$4,last_error=COALESCE($5,last_error),last_error_at=CASE WHEN $5::text IS NULL THEN last_error_at ELSE clock_timestamp() END,updated_at=clock_timestamp() WHERE id=$1 AND state='running' AND lease_token=$2", &[&id, &token, &delay.as_secs_f64(), &failures, &code]).await? == 1)
}
