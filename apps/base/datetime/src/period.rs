//! Calendar-day period semantics for list and history pages. Presets include
//! both endpoints, "this month" is the whole month, and the default is the last
//! 30 days. `bounds` converts the days to a half-open UTC instant range in the
//! display clock's zone, so day length follows daylight saving: a gap at
//! midnight starts the day at the first valid instant, a fold at the earlier.
use crate::{valid_timezone, DisplayContext, Mode};
use jiff::{civil::Date, tz::TimeZone, Timestamp, ToSpan};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Preset {
    Today,
    ThisMonth,
    LastMonth,
    ThisQuarter,
    YearToDate,
    ThisFinancialYear,
    LastFinancialYear,
    Last7Days,
    Last30Days,
    Last90Days,
    Custom,
}
impl Preset {
    pub const ALL: [(&'static str, Preset); 11] = [
        ("Today", Self::Today),
        ("This month", Self::ThisMonth),
        ("Last month", Self::LastMonth),
        ("This quarter", Self::ThisQuarter),
        ("Year to date", Self::YearToDate),
        ("This financial year", Self::ThisFinancialYear),
        ("Last financial year", Self::LastFinancialYear),
        ("Last 7 days", Self::Last7Days),
        ("Last 30 days", Self::Last30Days),
        ("Last 90 days", Self::Last90Days),
        ("Custom range", Self::Custom),
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::ThisMonth => "this_month",
            Self::LastMonth => "last_month",
            Self::ThisQuarter => "this_quarter",
            Self::YearToDate => "year_to_date",
            Self::ThisFinancialYear => "this_financial_year",
            Self::LastFinancialYear => "last_financial_year",
            Self::Last7Days => "last_7_days",
            Self::Last30Days => "last_30_days",
            Self::Last90Days => "last_90_days",
            Self::Custom => "custom",
        }
    }
    pub fn needs_financial_year(self) -> bool {
        matches!(self, Self::ThisFinancialYear | Self::LastFinancialYear)
    }
    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .map(|(_, preset)| *preset)
            .find(|preset| preset.as_str() == value)
    }
}

/// The calendar a period is read against: the display mode's zone and the
/// date "today" falls on there. The browser zone matters only in Local mode,
/// is client-supplied, and so is validated; anything unusable becomes UTC.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Clock {
    pub timezone: String,
    pub today: Date,
}
impl Clock {
    pub fn resolve(display: &DisplayContext, browser_zone: Option<&str>, now: Timestamp) -> Self {
        let zone = match display.mode {
            Mode::Company => display.timezone.as_str(),
            Mode::Local => browser_zone
                .filter(|zone| valid_timezone(zone))
                .unwrap_or("Etc/UTC"),
            Mode::Utc => "Etc/UTC",
        };
        let (zone, tz) = TimeZone::get(zone)
            .map(|tz| (zone, tz))
            .unwrap_or_else(|_| ("Etc/UTC", TimeZone::UTC));
        Self {
            timezone: zone.to_owned(),
            today: now.to_zoned(tz).date(),
        }
    }
}

/// Caller-owned financial calendar: current boundaries and an optional previous start. Explicit
/// boundaries support short and long financial years without guessing a month.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinancialYears {
    previous_start: Option<Date>,
    current_start: Date,
    next_start: Date,
}
impl FinancialYears {
    pub fn new(
        previous_start: Date,
        current_start: Date,
        next_start: Date,
    ) -> Result<Self, &'static str> {
        if previous_start >= current_start || current_start >= next_start {
            return Err("Financial year starts must be in date order.");
        }
        Ok(Self {
            previous_start: Some(previous_start),
            current_start,
            next_start,
        })
    }
    /// A current year without a known immediately preceding year.
    pub fn current(current_start: Date, next_start: Date) -> Result<Self, &'static str> {
        if current_start >= next_start {
            return Err("Financial year starts must be in date order.");
        }
        Ok(Self {
            previous_start: None,
            current_start,
            next_start,
        })
    }
    pub(crate) fn supports(self, preset: Preset, today: Date) -> bool {
        self.contains(today)
            && (preset != Preset::LastFinancialYear || self.previous_start.is_some())
    }
    pub fn contains(self, today: Date) -> bool {
        self.current_start <= today && today < self.next_start
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Period {
    pub preset: Preset,
    pub from: Date,
    pub to: Date,
}
pub const BACKWARDS: &str = "Start date must be on or before end date.";
pub const INVALID_DATES: &str = "Choose a valid start and end date.";
pub const UNRESOLVABLE: &str = "The date range cannot be resolved in this time zone.";

impl Period {
    /// Reads `period`, `from` and `to`. An unknown preset is the default;
    /// unparseable custom dates fall back to the default window's ends.
    pub fn parse(param: impl Fn(&str) -> Option<String>, today: Date) -> Self {
        Self::parse_with_financial_years(param, today, None)
    }

    /// Like `parse`, with financial-year choices enabled only when the caller's
    /// current year contains today. An unavailable financial-year preset uses
    /// the default, including Last financial year without a previous start.
    /// Year to date always means 1 January through today.
    pub fn parse_with_financial_years(
        param: impl Fn(&str) -> Option<String>,
        today: Date,
        years: Option<FinancialYears>,
    ) -> Self {
        let years = years.filter(|years| years.contains(today));
        let preset = param("period")
            .as_deref()
            .and_then(Preset::parse)
            .filter(|preset| {
                !preset.needs_financial_year()
                    || years.is_some_and(|years| years.supports(*preset, today))
            })
            .unwrap_or(Preset::Last30Days);
        let back = |days: i64| today.checked_sub(days.days()).unwrap_or(today);
        let date = |name: &str, fallback: Date| {
            param(name)
                .and_then(|value| value.parse::<Date>().ok())
                .unwrap_or(fallback)
        };
        let (from, to) = match preset {
            Preset::Today => (today, today),
            Preset::ThisMonth => (today.first_of_month(), today.last_of_month()),
            Preset::LastMonth => {
                let last = today.first_of_month().yesterday().unwrap_or(today);
                (last.first_of_month(), last.last_of_month())
            }
            Preset::ThisQuarter => {
                let start =
                    Date::new(today.year(), ((today.month() - 1) / 3) * 3 + 1, 1).unwrap_or(today);
                let end = start
                    .checked_add(2.months())
                    .unwrap_or(start)
                    .last_of_month();
                (start, end)
            }
            Preset::YearToDate => (today.first_of_year(), today),
            Preset::ThisFinancialYear | Preset::LastFinancialYear => {
                // The preset filter above admits these only with a valid calendar.
                let years = years.expect("financial calendar checked above");
                if preset == Preset::ThisFinancialYear {
                    (
                        years.current_start,
                        years.next_start.yesterday().unwrap_or(years.current_start),
                    )
                } else {
                    let previous = years
                        .previous_start
                        .expect("previous financial year checked above");
                    (
                        previous,
                        years.current_start.yesterday().unwrap_or(previous),
                    )
                }
            }
            Preset::Last7Days => (back(6), today),
            Preset::Last90Days => (back(89), today),
            Preset::Custom => (date("from", back(29)), date("to", today)),
            Preset::Last30Days => (back(29), today),
        };
        Self { preset, from, to }
    }

    pub fn custom(from: &str, to: &str) -> Result<Self, &'static str> {
        let (Ok(from), Ok(to)) = (from.parse::<Date>(), to.parse::<Date>()) else {
            return Err(INVALID_DATES);
        };
        if from > to {
            return Err(BACKWARDS);
        }
        Ok(Self {
            preset: Preset::Custom,
            from,
            to,
        })
    }

    /// URL parameters for `parse`, or `parse_with_financial_years` with the
    /// caller's current calendar when using financial-year presets.
    pub fn params(&self) -> [(&'static str, String); 3] {
        [
            ("period", self.preset.as_str().to_owned()),
            ("from", self.from.to_string()),
            ("to", self.to.to_string()),
        ]
    }

    /// `[start, end)` as UTC instants; the upper bound is exclusive so no
    /// timestamp precision is lost at the end of the last day.
    pub fn bounds(&self, timezone: &str) -> Result<(Timestamp, Timestamp), &'static str> {
        if self.from > self.to {
            return Err(BACKWARDS);
        }
        let tz = TimeZone::get(timezone).map_err(|_| UNRESOLVABLE)?;
        let start = |date: Date| {
            date.to_zoned(tz.clone())
                .map(|zoned| zoned.timestamp())
                .map_err(|_| UNRESOLVABLE)
        };
        let after_last = self.to.tomorrow().map_err(|_| UNRESOLVABLE)?;
        Ok((start(self.from)?, start(after_last)?))
    }
}
