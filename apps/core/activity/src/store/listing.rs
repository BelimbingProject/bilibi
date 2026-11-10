//! The list read of `core_activities` and the per-day count a year calendar
//! draws. Both bind the same conditions, so a count can never see an
//! activity the list would hide.
use super::{from_row, reader, Reader, COLUMNS, MINE};
use crate::model::Kind;
use crate::{DayCount, Filter, Mine, Page, Sort, State, TASK_MINUTES};
use bilibi_database::{Error, Result};
use bilibi_tenancy::{ReadClient, Scope};
use jiff::Timestamp;
use std::time::SystemTime;
use tokio_postgres::types::ToSql;

/// The most days one count read answers for: a leap year.
pub(crate) const MAX_COUNT_DAYS: usize = 366;

/// What a list read binds as `$1` to `$15`: the reader and the filter.
struct Listing {
    reader: Reader,
    shared: bool,
    search: String,
    pattern: String,
    kind: Option<&'static str>,
    state: &'static str,
    mine: &'static str,
    related_kind: Option<String>,
    related_id: Option<String>,
    person: Option<i64>,
    from: Option<SystemTime>,
    to: Option<SystemTime>,
    series: Option<i64>,
}
impl Listing {
    /// `related_readable`: the reader may view `filter.related` (the caller
    /// ran `base/records.check`), so the shared activities on it count too.
    async fn new(
        client: &dyn ReadClient,
        scope: &Scope,
        filter: &Filter,
        related_readable: bool,
    ) -> Result<Self> {
        let reader = reader(client, scope).await?;
        let search = filter.search.trim().chars().take(200).collect::<String>();
        let pattern = format!(
            "%{}%",
            search
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        Ok(Self {
            reader,
            shared: related_readable && filter.related.is_some(),
            search,
            pattern,
            kind: filter.kind.map(Kind::code),
            state: match filter.state {
                State::Open => "open",
                State::Closed => "closed",
                State::All => "all",
            },
            mine: match filter.mine {
                Mine::Any => "any",
                Mine::Assignee => "assignee",
                Mine::Guest => "guest",
                Mine::TakingPart => "part",
            },
            related_kind: filter.related.as_ref().map(|r| r.kind.clone()),
            related_id: filter.related.as_ref().map(|r| r.id.clone()),
            // A person's activities include those they are a guest of.
            person: filter
                .related
                .as_ref()
                .filter(|r| r.kind == "person")
                .and_then(|r| r.id.parse::<i64>().ok()),
            from: filter.between.map(|(from, _)| SystemTime::from(from)),
            to: filter.between.map(|(_, to)| SystemTime::from(to)),
            series: filter.series,
        })
    }

    /// Visible: oversight, taking part, or shared on the record the list is
    /// narrowed to when the reader may view that record.
    fn conditions() -> String {
        format!(
            "a.tenant_id=$1 AND a.deleted_at IS NULL
         AND ($3 OR {MINE} OR ($4 AND a.shared AND a.related_kind = $10 AND a.related_id = $11))
         AND ($5 = '' OR a.subject ILIKE $6)
         AND ($7::text IS NULL OR a.kind = $7)
         AND ($8 = 'all' OR ($8 = 'open') = (a.closed_at IS NULL AND a.kind <> 'note'))
         AND ($9 = 'any'
              OR ($9 = 'assignee' AND a.assignee_user_id = $2)
              OR ($9 = 'guest' AND EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_user_id=$2 AND g.role IN ('related','invited')))
              OR ($9 = 'part' AND (a.created_by=$2 OR a.assignee_user_id=$2 OR EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_user_id=$2 AND g.response<>'declined'))))
         AND ($10::text IS NULL
              OR (a.related_kind = $10 AND a.related_id = $11)
              OR ($12::bigint IS NOT NULL AND EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_person_id=$12)))
         AND ($13::timestamptz IS NULL
              OR (a.kind IN ('call','meeting') AND a.starts_at < $14 AND a.ends_at > $13)
              OR (a.kind = 'task' AND a.due_at >= $13 AND a.due_at < $14))
         AND ($15::bigint IS NULL OR a.series_id = $15)"
        )
    }

    fn params(&self) -> [&(dyn ToSql + Sync); 15] {
        [
            &self.reader.tenant,
            &self.reader.user,
            &self.reader.all,
            &self.shared,
            &self.search,
            &self.pattern,
            &self.kind,
            &self.state,
            &self.mine,
            &self.related_kind,
            &self.related_id,
            &self.person,
            &self.from,
            &self.to,
            &self.series,
        ]
    }
}

/// `related_readable`: the reader may view `filter.related` (the caller ran
/// `base/records.check`), so the shared activities on it count too.
pub(crate) async fn list(
    client: &dyn ReadClient,
    scope: &Scope,
    filter: &Filter,
    related_readable: bool,
) -> Result<Page> {
    let listing = Listing::new(client, scope, filter, related_readable).await?;
    // A calendar reads the whole range it shows, up to what it can draw.
    let largest = if filter.between.is_some() {
        crate::CALENDAR_LIMIT
    } else {
        300
    };
    let page_size = filter.page_size.clamp(1, largest);
    let offset = (filter.page.max(1) - 1).saturating_mul(page_size);
    let conditions = Listing::conditions();
    let params = listing.params();
    let total: i64 = client
        .query_opt(
            &format!("SELECT count(*) FROM core_activities a WHERE {conditions}"),
            &params,
        )
        .await?
        .ok_or(Error::Internal)?
        .get(0);
    const WHEN: &str = "COALESCE(a.starts_at, a.due_at, a.created_at)";
    // The instant `Sort::Nearest` measures from, bound once so every page of
    // one read is cut from the same order.
    let nearest = match filter.sort {
        Sort::Nearest(at) => Some(SystemTime::from(at)),
        _ => None,
    };
    let order = match filter.sort {
        Sort::When if filter.series.is_some() => "a.series_index".to_owned(),
        Sort::When => WHEN.to_owned(),
        Sort::Nearest(_) => format!("abs(extract(epoch FROM ({WHEN} - $16::timestamptz)))"),
        Sort::Title => "lower(a.subject)".to_owned(),
        Sort::Status => "a.status".to_owned(),
        Sort::Created => "a.created_at".to_owned(),
    };
    let direction = if filter.descending { "DESC" } else { "ASC" };
    let sql = format!(
        "SELECT {COLUMNS} FROM core_activities a WHERE {conditions} ORDER BY {order} {direction} NULLS LAST, a.id {direction} LIMIT {page_size} OFFSET {offset}"
    );
    let mut bound: Vec<&(dyn ToSql + Sync)> = params.to_vec();
    if nearest.is_some() {
        bound.push(&nearest);
    }
    let entries = client
        .query(&sql, &bound)
        .await?
        .iter()
        .map(from_row)
        .collect::<Result<Vec<_>>>()?;
    Ok(Page { entries, total })
}

/// How many of the activities `filter` matches fall on each of `days`, and
/// how many of those are open and already late. `days` are the instants each
/// day begins at, in order; the last one ends at `end`. A call or meeting
/// counts on every day it runs into, a task on the day it is due (for the
/// [`TASK_MINUTES`] the calendar draws it), exactly as the calendar's other
/// views place them. One grouped query, under the list's own conditions.
pub(crate) async fn day_counts(
    client: &dyn ReadClient,
    scope: &Scope,
    filter: &Filter,
    related_readable: bool,
    days: &[Timestamp],
    end: Timestamp,
) -> Result<Vec<DayCount>> {
    let ordered = days.windows(2).all(|pair| pair[0] < pair[1]);
    let (Some(first), Some(last)) = (days.first(), days.last()) else {
        return Ok(Vec::new());
    };
    if days.len() > MAX_COUNT_DAYS || !ordered || *last >= end {
        return Err(Error::Invalid);
    }
    let filter = Filter {
        between: Some((*first, end)),
        ..filter.clone()
    };
    let listing = Listing::new(client, scope, &filter, related_readable).await?;
    let starts: Vec<SystemTime> = days.iter().copied().map(SystemTime::from).collect();
    let conditions = Listing::conditions();
    // `width_bucket` finds the day an instant falls in by binary search over
    // the day starts, so a busy year costs one pass over its activities.
    let sql = format!(
        "SELECT d.day::int,
                count(*),
                count(*) FILTER (WHERE a.closed_at IS NULL AND COALESCE(a.starts_at, a.due_at) < now())
           FROM core_activities a
          CROSS JOIN LATERAL generate_series(
                GREATEST(width_bucket(COALESCE(a.starts_at, a.due_at), $16::timestamptz[]), 1),
                width_bucket(COALESCE(a.ends_at, a.due_at + interval '{TASK_MINUTES} minutes') - interval '1 microsecond', $16::timestamptz[])
          ) AS d(day)
          WHERE {conditions}
          GROUP BY d.day"
    );
    let mut bound: Vec<&(dyn ToSql + Sync)> = listing.params().to_vec();
    bound.push(&starts);
    let mut counts = vec![DayCount::default(); days.len()];
    for row in client.query(&sql, &bound).await? {
        let day: i32 = row.get(0);
        if let Some(slot) = usize::try_from(day - 1)
            .ok()
            .and_then(|index| counts.get_mut(index))
        {
            *slot = DayCount {
                count: row.get(1),
                overdue: row.get(2),
            };
        }
    }
    Ok(counts)
}
