//! A subscriber's watermark in one tenant (`base_events_watermarks`): the
//! subscriber has a delivery, delivered or still pending, for every event it
//! reads up to that event ID. An erased event counts as accounted for, since a
//! replay creates no delivery for it. Each delivery moves the watermark
//! forward over the next accounted events and it stops before the first
//! missed one, so a gap is found from here without reading the events before
//! it. It is not the last delivered event: that would pass over an older gap
//! as soon as a newer event was delivered.
//!
//! An event is accounted for as the reader's own snapshot sees it. One
//! recorded by a server that does not mount the subscriber, in a transaction
//! still open while the watermark passes its ID, would be left behind; a
//! replay from the gap it belongs to hands it over all the same.
use bilibi_database::Result;
use tokio_postgres::{GenericClient, Row};

/// Events looked at after the watermark each time a delivery advances it.
/// More than the one event a delivery accounts for, so a watermark that is
/// behind catches up.
const ADVANCE: i64 = 200;

/// The next events the subscriber reads after an event ID, oldest first, at
/// most `$5`: how many, how many of them it has no delivery for, the first of
/// those and the last event looked at. Each event name is read through
/// `base_events_name` and stops at the limit, so the read is bounded by the
/// limit whatever the tenant holds. `$1` tenant, `$2` the event names, `$3`
/// the subscriber, `$4` the event ID to start after.
pub(crate) const WINDOW: &str = "WITH w AS (\
 SELECT m.id,(m.erased OR EXISTS (SELECT 1 FROM base_events_deliveries d WHERE d.event_id=m.id AND d.subscriber=$3)) AS accounted \
 FROM unnest($2::text[]) AS n(name) CROSS JOIN LATERAL (\
 SELECT e.id,e.erased_at IS NOT NULL AS erased FROM base_events e WHERE e.tenant_id=$1 AND e.name=n.name AND e.id>$4 ORDER BY e.id LIMIT $5) m \
 ORDER BY m.id LIMIT $5) \
 SELECT count(*),count(*) FILTER (WHERE NOT accounted),min(id) FILTER (WHERE NOT accounted),max(id) FROM w";

/// One [`WINDOW`] row.
pub(crate) struct Window {
    /// Events looked at; fewer than the limit means the newest was reached.
    pub read: i64,
    pub missed: i64,
    pub first_missed: Option<i64>,
    pub last: Option<i64>,
}
impl Window {
    pub(crate) fn from_row(row: &Row) -> Self {
        Self {
            read: row.get(0),
            missed: row.get(1),
            first_missed: row.get(2),
            last: row.get(3),
        }
    }
}

/// The subscriber's watermark in `tenant`; 0 before its first delivery.
pub(crate) async fn read(
    client: &impl GenericClient,
    tenant: i64,
    subscriber: &str,
) -> Result<i64> {
    Ok(client
        .query_opt(
            "SELECT event_id FROM base_events_watermarks WHERE tenant_id=$1 AND subscriber=$2",
            &[&tenant, &subscriber],
        )
        .await?
        .map_or(0, |row| row.get(0)))
}

/// Moves the watermark forward over the next accounted events, in the
/// delivery's own transaction. It never moves back: concurrent deliveries
/// keep the greater value.
pub(crate) async fn advance(
    client: &impl GenericClient,
    tenant: i64,
    subscriber: &str,
    names: &[&str],
) -> Result<()> {
    let from = read(client, tenant, subscriber).await?;
    let row = client
        .query_one(WINDOW, &[&tenant, &names, &subscriber, &from, &ADVANCE])
        .await?;
    let window = Window::from_row(&row);
    let through = match (window.first_missed, window.last) {
        (Some(first), _) => first - 1,
        (None, Some(last)) => last,
        (None, None) => return Ok(()),
    };
    if through > from {
        client
            .execute(
                "INSERT INTO base_events_watermarks(tenant_id,subscriber,event_id) VALUES($1,$2,$3) \
                 ON CONFLICT (tenant_id,subscriber) DO UPDATE SET event_id=GREATEST(base_events_watermarks.event_id,EXCLUDED.event_id)",
                &[&tenant, &subscriber, &through],
            )
            .await?;
    }
    Ok(())
}
