//! The calendar's year view and the day marks every view shares. A year is
//! drawn from one count of each day (`Activities::day_counts`), never from
//! the items, so its numbers are right however busy the year is.
use super::Index;
use crate::web::Clock;
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    calendar::{self, Calendar, CalendarLane, CalendarState, DayTotal},
    Html,
};
use std::collections::HashMap;

impl Index {
    /// The non-working days among `dates`, from the work calendar of the
    /// reader's company: rest, off, and a holiday with its name.
    pub(super) async fn day_marks(
        &self,
        scope: &Scope,
        dates: &[jiff::civil::Date],
    ) -> Result<Vec<calendar::DayMark>> {
        let mut marks = Vec::new();
        let (Some(first), Some(last)) = (dates.first(), dates.last()) else {
            return Ok(marks);
        };
        for day in self
            .pages
            .activities
            .work_days(scope, *first, *last)
            .await?
        {
            use bilibi_work_calendar::{DayType, Reason};
            let kind = match day.day_type {
                DayType::Working => continue,
                DayType::Rest => bilibi_ui::DayKind::Rest,
                DayType::Off => bilibi_ui::DayKind::Off,
                DayType::Holiday => bilibi_ui::DayKind::Holiday,
            };
            let label = match &day.reason {
                Reason::Holiday { name, .. }
                | Reason::Substitute { name, .. }
                | Reason::Exception { name } => name.clone(),
                Reason::Week => String::new(),
            };
            marks.push(calendar::DayMark {
                date: day.date,
                kind,
                label,
            });
        }
        Ok(marks)
    }

    /// The year the address names, each day with its count under the list's
    /// filters. `end` is the instant the year ends at in the reader's zone.
    /// Returns the HTML, its address query and how many items the year holds.
    pub(super) async fn year_view(
        &self,
        scope: &Scope,
        clock: &Clock,
        mut query: HashMap<String, String>,
        today: jiff::civil::Date,
        end: jiff::Timestamp,
    ) -> Result<(Html, HashMap<String, String>, usize)> {
        // The lanes chosen on the shared view ride along in the address, so
        // going back to it shows the same people.
        let lanes: Vec<CalendarLane> = self
            .calendar_query
            .get("lanes")
            .map(|lanes| {
                lanes
                    .split(',')
                    .filter(|key| key.parse::<i64>().is_ok_and(|id| id > 0))
                    .take(calendar::MAX_LANES)
                    .map(|key| CalendarLane::new(key, key))
                    .collect()
            })
            .unwrap_or_default();
        let state = CalendarState::parse(&self.calendar_query, today, &lanes);
        let dates = state.dates();
        let zone = jiff::tz::TimeZone::get(&clock.zone).unwrap_or(jiff::tz::TimeZone::UTC);
        let days = dates
            .iter()
            .map(|date| {
                zone.to_ambiguous_zoned(date.at(0, 0, 0, 0))
                    .compatible()
                    .map(|zoned| zoned.timestamp())
                    .map_err(|_| Error::Internal)
            })
            .collect::<Result<Vec<_>>>()?;
        let counts = self
            .pages
            .activities
            .day_counts(scope, &self.filter(), &days, end)
            .await?;
        let totals: Vec<DayTotal> = dates
            .iter()
            .zip(&counts)
            .filter(|(_, day)| day.count > 0)
            .map(|(date, day)| DayTotal {
                date: *date,
                count: day.count.max(0) as usize,
                late: day.overdue.max(0) as usize,
            })
            .collect();
        let held = totals.iter().map(|total| total.count).sum();
        query.insert("display".into(), "calendar".into());
        let mut drawn = Calendar::new(
            "activities-calendar",
            "/activities",
            state.clone(),
            clock.zone.clone(),
            today,
        );
        drawn.query = query.clone();
        drawn.day_totals = Some(totals);
        drawn.day_marks = self.day_marks(scope, &dates).await?;
        drawn.follows_browser_zone = clock.display.mode == bilibi_datetime::Mode::Local;
        drawn.creatable = self.pages.activities.may_create(scope).await?;
        drawn.notice = self.notice.clone();
        let html = drawn.render().map_err(|_| Error::Internal)?;
        query.extend(state.to_params());
        Ok((html, query, held))
    }
}
