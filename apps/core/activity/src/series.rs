//! Repeating series: the rule a call, meeting or task repeats by, and the
//! dates and times it gives. Each occurrence is stored as its own activity
//! (`series_id`, `series_index`), so it keeps its own status, guests' answers
//! and reminder; this module only works out when they happen.
use crate::model::{Draft, FieldError};
use jiff::{
    civil::{Date, DateTime, Weekday},
    tz::TimeZone,
    SignedDuration, Timestamp, ToSpan,
};

/// Occurrences one series creates at most. Every occurrence is written, with
/// its guests and audit rows, in the creating transaction; this bounds it.
pub const MAX_OCCURRENCES: i16 = 200;
/// "Working days only" reads the work calendar this many days past the first
/// occurrence at most (the work calendar's own window is 731 days).
const WORKING_DAYS_AHEAD: i64 = 730;
/// Candidate dates a rule may look at before it gives up.
const MAX_STEPS: usize = 5_000;
const LAST_YEAR: i16 = 2999;

/// How often a series repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
}
impl Frequency {
    pub const ALL: &'static [Self] = &[Self::Daily, Self::Weekly, Self::Monthly];
    pub fn code(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Daily => "Daily",
            Self::Weekly => "Weekly",
            Self::Monthly => "Monthly",
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.code() == code)
    }
    fn unit(self, every: i16) -> String {
        let unit = match self {
            Self::Daily => "day",
            Self::Weekly => "week",
            Self::Monthly => "month",
        };
        if every == 1 {
            format!("every {unit}")
        } else {
            format!("every {every} {unit}s")
        }
    }
}

/// The days of the week by their codes, Monday first.
pub const WEEKDAYS: [(Weekday, &str, &str); 7] = [
    (Weekday::Monday, "mon", "Mon"),
    (Weekday::Tuesday, "tue", "Tue"),
    (Weekday::Wednesday, "wed", "Wed"),
    (Weekday::Thursday, "thu", "Thu"),
    (Weekday::Friday, "fri", "Fri"),
    (Weekday::Saturday, "sat", "Sat"),
    (Weekday::Sunday, "sun", "Sun"),
];
pub fn weekday(code: &str) -> Option<Weekday> {
    WEEKDAYS
        .iter()
        .find(|(_, c, _)| *c == code)
        .map(|(day, _, _)| *day)
}
pub fn weekday_code(day: Weekday) -> &'static str {
    WEEKDAYS
        .iter()
        .find(|(d, _, _)| *d == day)
        .map_or("mon", |(_, code, _)| code)
}
fn weekday_label(day: Weekday) -> &'static str {
    WEEKDAYS
        .iter()
        .find(|(d, _, _)| *d == day)
        .map_or("Mon", |(_, _, label)| label)
}

/// How a new call, meeting or task repeats. It ends on a date or after a
/// number of occurrences, never open-ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repeat {
    pub frequency: Frequency,
    /// Every this many days, weeks or months, 1–99.
    pub every: i16,
    /// Weekly: the days of the week; empty is the first occurrence's own day.
    pub weekdays: Vec<Weekday>,
    /// The last date an occurrence may fall on, in `timezone`.
    pub until: Option<Date>,
    /// How many occurrences, at most `MAX_OCCURRENCES`.
    pub count: Option<i16>,
    /// Leave out the dates the filing company's work calendar does not make
    /// working days (the count counts only the dates kept).
    pub working_days_only: bool,
    /// The IANA zone the wall-clock time repeats in, so a meeting at 09:00
    /// stays at 09:00 across a daylight-saving change.
    pub timezone: String,
}

/// A stored series: the rule, the work calendar it skipped non-working days
/// by, and how many of its occurrences are still live.
#[derive(Clone, Debug)]
pub struct Series {
    pub id: i64,
    pub repeat: Repeat,
    /// Monthly: the day of the month it repeats on.
    pub month_day: Option<i16>,
    pub work_calendar: Option<String>,
    pub live: i64,
    pub created_by: i64,
    pub created_at: Timestamp,
}
impl Series {
    /// The rule in words: "Weekly on Mon and Wed, 10 times".
    pub fn describe(&self) -> String {
        self.repeat.describe(self.month_day)
    }
}

/// Which occurrences a series change reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Apply {
    /// The one named and every later one.
    Following,
    /// Every live occurrence, earlier and closed ones included.
    All,
}
impl Apply {
    pub fn code(self) -> &'static str {
        match self {
            Self::Following => "following",
            Self::All => "all",
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "following" => Some(Self::Following),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

fn days_list(days: &[Weekday]) -> String {
    let labels: Vec<&str> = days.iter().map(|d| weekday_label(*d)).collect();
    match labels.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

impl Repeat {
    /// The rule in words. `month_day` is the day a monthly series keeps.
    pub fn describe(&self, month_day: Option<i16>) -> String {
        let mut text = {
            let unit = self.frequency.unit(self.every);
            let mut first = unit.chars();
            first
                .next()
                .map(|c| c.to_uppercase().chain(first).collect::<String>())
                .unwrap_or_default()
        };
        match self.frequency {
            Frequency::Weekly if !self.weekdays.is_empty() => {
                text.push_str(&format!(" on {}", days_list(&self.weekdays)));
            }
            Frequency::Monthly => {
                if let Some(day) = month_day {
                    text.push_str(&format!(" on day {day}"));
                }
            }
            _ => {}
        }
        if self.working_days_only {
            text.push_str(", working days only");
        }
        match (self.count, self.until) {
            (Some(1), _) => text.push_str(", once"),
            (Some(count), _) => text.push_str(&format!(", {count} times")),
            (None, Some(until)) => {
                text.push_str(&format!(", until {}", until.strftime("%-d %b %Y")))
            }
            (None, None) => {}
        }
        text
    }

    /// The rule's own shape; the dates it gives are checked by `dates`.
    pub(crate) fn check(&self) -> Vec<FieldError> {
        let mut errors = Vec::new();
        if !(1..=99).contains(&self.every) {
            errors.push(FieldError::new(
                "repeat.every",
                "must be a whole number from 1 to 99",
            ));
        }
        if self.frequency != Frequency::Weekly && !self.weekdays.is_empty() {
            errors.push(FieldError::new(
                "repeat.weekdays",
                "is only for a weekly series",
            ));
        }
        match (self.count, self.until) {
            (None, None) => errors.push(FieldError::new(
                "repeat",
                "needs a last date or a number of occurrences",
            )),
            (Some(_), Some(_)) => errors.push(FieldError::new(
                "repeat",
                "takes a last date or a number of occurrences, not both",
            )),
            (Some(count), None) if !(1..=MAX_OCCURRENCES).contains(&count) => {
                errors.push(FieldError::new(
                    "repeat.count",
                    format!("must be from 1 to {MAX_OCCURRENCES}"),
                ))
            }
            _ => {}
        }
        if self.until.is_some_and(|until| until.year() > LAST_YEAR) {
            errors.push(FieldError::new(
                "repeat.until",
                format!("must be in {LAST_YEAR} or earlier"),
            ));
        }
        if !bilibi_datetime::valid_timezone(&self.timezone) {
            errors.push(FieldError::new(
                "repeat.timezone",
                "is not a time zone name such as Asia/Kuala_Lumpur",
            ));
        }
        errors
    }

    /// The last date `working` must answer for: the rule's end, or the work
    /// calendar's reach from `first`, whichever is earlier.
    pub(crate) fn working_window(&self, first: Date) -> Date {
        let reach = first
            .checked_add(WORKING_DAYS_AHEAD.days())
            .unwrap_or(first);
        self.until
            .map_or(reach, |until| until.min(reach).max(first))
    }

    /// The dates of the occurrences, from `first` (the first occurrence's
    /// date as entered). A first date off the pattern (a Tuesday for a
    /// series on Mondays) moves to the first date that matches. `working`
    /// answers whether a date is a working day, or `None` past what it knows.
    pub(crate) fn dates(
        &self,
        first: Date,
        working: Option<&dyn Fn(Date) -> Option<bool>>,
    ) -> Result<Vec<Date>, FieldError> {
        let every = i64::from(self.every.clamp(1, 99));
        let wanted = self
            .count
            .map(|count| count.clamp(1, MAX_OCCURRENCES) as usize);
        let mut kept: Vec<Date> = Vec::new();
        let mut steps = 0usize;
        // Each candidate in date order; `false` once the rule is done.
        let mut take = |date: Date| -> Result<bool, FieldError> {
            steps += 1;
            if steps > MAX_STEPS || date.year() > LAST_YEAR {
                return Err(FieldError::new(
                    "repeat",
                    "reaches too far ahead; choose an earlier last date or fewer occurrences",
                ));
            }
            if self.until.is_some_and(|until| date > until) {
                return Ok(false);
            }
            if wanted.is_some_and(|wanted| kept.len() >= wanted) {
                return Ok(false);
            }
            let keep = match working {
                None => true,
                Some(working) => working(date).ok_or_else(|| {
                    FieldError::new(
                        "repeat.working_days_only",
                        "reaches more than two years ahead; choose an earlier last date or fewer occurrences",
                    )
                })?,
            };
            if keep {
                if kept.len() >= MAX_OCCURRENCES as usize {
                    return Err(FieldError::new(
                        "repeat",
                        format!(
                            "would make more than {MAX_OCCURRENCES} occurrences; choose an earlier last date"
                        ),
                    ));
                }
                kept.push(date);
            }
            Ok(true)
        };
        match self.frequency {
            Frequency::Daily => {
                let mut date = first;
                while take(date)? {
                    date = date.checked_add(every.days()).map_err(|_| too_far())?;
                }
            }
            Frequency::Weekly => {
                let mut days = if self.weekdays.is_empty() {
                    vec![first.weekday()]
                } else {
                    self.weekdays.clone()
                };
                days.sort_by_key(|d| d.to_monday_zero_offset());
                days.dedup();
                let mut monday = first
                    .checked_sub(i64::from(first.weekday().to_monday_zero_offset()).days())
                    .map_err(|_| too_far())?;
                'weeks: loop {
                    for day in &days {
                        let date = monday
                            .checked_add(i64::from(day.to_monday_zero_offset()).days())
                            .map_err(|_| too_far())?;
                        if date < first {
                            continue;
                        }
                        if !take(date)? {
                            break 'weeks;
                        }
                    }
                    monday = monday
                        .checked_add((7 * every).days())
                        .map_err(|_| too_far())?;
                }
            }
            Frequency::Monthly => {
                let day = first.day();
                let mut month = first.first_of_month();
                loop {
                    // A month without that day uses its last day.
                    let date = month
                        .with()
                        .day(day.min(month.days_in_month()))
                        .build()
                        .map_err(|_| too_far())?;
                    if !take(date)? {
                        break;
                    }
                    month = month.checked_add(every.months()).map_err(|_| too_far())?;
                }
            }
        }
        if kept.is_empty() {
            return Err(FieldError::new(
                "repeat",
                "gives no occurrence; check the last date and the days chosen",
            ));
        }
        Ok(kept)
    }
}

fn too_far() -> FieldError {
    FieldError::new(
        "repeat",
        "reaches too far ahead; choose an earlier last date or fewer occurrences",
    )
}

/// `at` as a wall-clock time in `zone`.
pub(crate) fn civil(at: Timestamp, zone: &TimeZone) -> DateTime {
    at.to_zoned(zone.clone()).datetime()
}
/// A wall-clock time in `zone` as an instant: a time in a daylight-saving
/// gap is the first valid instant, one in a fold the earlier.
pub(crate) fn instant(at: DateTime, zone: &TimeZone) -> Option<Timestamp> {
    zone.to_ambiguous_zoned(at)
        .compatible()
        .ok()
        .map(|z| z.timestamp())
}
/// `at` moved by `by` on the wall clock in `zone`.
pub(crate) fn shifted(at: Timestamp, by: SignedDuration, zone: &TimeZone) -> Option<Timestamp> {
    civil(at, zone)
        .checked_add(by)
        .ok()
        .and_then(|moved| instant(moved, zone))
}
/// How far `to` is from `from` on the wall clock in `zone`.
pub(crate) fn wall_gap(from: Timestamp, to: Timestamp, zone: &TimeZone) -> SignedDuration {
    civil(from, zone).duration_until(civil(to, zone))
}

/// The first occurrence's date: a call's or meeting's start, a task's due
/// date, in `zone`. `None` when the draft has neither.
pub(crate) fn anchor(draft: &Draft, zone: &TimeZone) -> Option<Date> {
    draft
        .starts_at
        .or(draft.due_at)
        .map(|at| civil(at, zone).date())
}

/// One draft per date: the same wall-clock times as `draft` on each date,
/// and the same wall-clock length.
pub(crate) fn occurrences(
    draft: &Draft,
    zone: &TimeZone,
    dates: &[Date],
) -> Result<Vec<Draft>, FieldError> {
    let invalid = || FieldError::new("repeat", "gives a time that does not exist in its zone");
    let Some(anchor) = draft.starts_at.or(draft.due_at) else {
        return Err(FieldError::new(
            "repeat",
            "needs a start time for a call or meeting, or a due date for a task",
        ));
    };
    let base = civil(anchor, zone);
    let gap = |at: Option<Timestamp>| at.map(|at| wall_gap(anchor, at, zone));
    let ends = gap(draft.ends_at);
    let start = gap(draft.start_at);
    dates
        .iter()
        .map(|date| {
            let at = date.to_datetime(base.time());
            let place = |gap: Option<SignedDuration>| -> Result<Option<Timestamp>, FieldError> {
                gap.map(|gap| {
                    at.checked_add(gap)
                        .ok()
                        .and_then(|moved| instant(moved, zone))
                        .ok_or_else(invalid)
                })
                .transpose()
            };
            let when = instant(at, zone).ok_or_else(invalid)?;
            let mut occurrence = draft.clone();
            if draft.starts_at.is_some() {
                occurrence.starts_at = Some(when);
                occurrence.ends_at = place(ends)?;
            } else {
                occurrence.due_at = Some(when);
                occurrence.start_at = place(start)?;
            }
            Ok(occurrence)
        })
        .collect()
}
