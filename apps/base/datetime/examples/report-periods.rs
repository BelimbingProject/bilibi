//! Executable report-period contract, using worked Gregorian dates as the oracle.
use bilibi_datetime::{Clock, DisplayContext, FinancialYears, Period, PeriodFilter, Preset};
use jiff::civil::Date;
use serde_json::json;

fn date(value: &str) -> Date {
    value.parse().expect("worked Gregorian date")
}
fn main() {
    let years = FinancialYears::new(date("2024-04-01"), date("2025-10-01"), date("2026-10-01"))
        .expect("18-month previous year, 12-month current year");
    let today = date("2026-02-18");
    for (preset, from, to) in [
        ("today", "2026-02-18", "2026-02-18"),
        ("this_month", "2026-02-01", "2026-02-28"),
        ("last_month", "2026-01-01", "2026-01-31"),
        ("this_quarter", "2026-01-01", "2026-03-31"),
        ("year_to_date", "2026-01-01", "2026-02-18"),
        ("this_financial_year", "2025-10-01", "2026-09-30"),
        ("last_financial_year", "2024-04-01", "2025-09-30"),
        ("last_7_days", "2026-02-12", "2026-02-18"),
        ("last_30_days", "2026-01-20", "2026-02-18"),
        ("last_90_days", "2025-11-21", "2026-02-18"),
    ] {
        let applied = Period::parse_with_financial_years(
            |key| (key == "period").then(|| preset.into()),
            today,
            Some(years),
        );
        assert_eq!(
            (applied.from, applied.to),
            (date(from), date(to)),
            "{preset}"
        );
        let params = applied.params();
        assert_eq!(
            applied,
            Period::parse_with_financial_years(
                |key| params
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.clone()),
                today,
                Some(years),
            )
        );
        let clock = Clock {
            today,
            timezone: "Etc/UTC".into(),
        };
        let mut filter = PeriodFilter::new("report", applied, &clock).financial_years(Some(years));
        assert_eq!(
            filter
                .event_checked(
                    "period_select:report",
                    &json!({"period_filter[period]": preset}),
                    &clock
                )
                .unwrap(),
            Some(bilibi_datetime::filter::Outcome::Applied(applied))
        );
    }
    for (today, preset, from, to) in [
        ("2024-03-01", "last_month", "2024-02-01", "2024-02-29"),
        ("2026-01-01", "last_month", "2025-12-01", "2025-12-31"),
        ("2026-12-31", "this_quarter", "2026-10-01", "2026-12-31"),
        ("2026-01-01", "year_to_date", "2026-01-01", "2026-01-01"),
    ] {
        let applied = Period::parse(|key| (key == "period").then(|| preset.into()), date(today));
        assert_eq!((applied.from, applied.to), (date(from), date(to)));
    }
    assert!(FinancialYears::new(today, today, date("2027-01-01")).is_err());
    for years in [None, Some(years)] {
        let applied = Period::parse_with_financial_years(
            |key| (key == "period").then(|| "this_financial_year".into()),
            date("2027-01-01"),
            years,
        );
        assert_eq!(applied.preset, Preset::Last30Days);
    }
    // First year and a current year after a calendar gap both have no
    // immediately preceding year, even if older years exist.
    for (start, next, today) in [
        ("2025-10-01", "2026-10-01", "2026-02-18"),
        ("2028-04-01", "2029-04-01", "2028-07-15"),
    ] {
        let years = FinancialYears::current(date(start), date(next)).unwrap();
        let clock = Clock {
            today: date(today),
            timezone: "Etc/UTC".into(),
        };
        let parse = |preset: &str| {
            Period::parse_with_financial_years(
                |key| (key == "period").then(|| preset.to_owned()),
                clock.today,
                Some(years),
            )
        };
        let current = parse("this_financial_year");
        assert_eq!(
            (current.from, current.to),
            (date(start), date(next).yesterday().unwrap())
        );
        assert_eq!(parse("last_financial_year").preset, Preset::Last30Days);
        assert_eq!(parse("year_to_date").from, clock.today.first_of_year());
        let params = current.params();
        assert_eq!(
            current,
            Period::parse_with_financial_years(
                |key| params
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.clone()),
                clock.today,
                Some(years)
            )
        );
        let mut filter = PeriodFilter::new("first", current, &clock).financial_years(Some(years));
        let choices = filter.selection().options;
        assert!(choices
            .iter()
            .any(|choice| choice.value == "this_financial_year"));
        assert!(choices.iter().any(|choice| choice.value == "year_to_date"));
        assert!(!choices
            .iter()
            .any(|choice| choice.value == "last_financial_year"));
        let custom = Period::custom("2026-01-02", "2026-01-15").unwrap();
        let params = custom.params();
        assert_eq!(
            custom,
            Period::parse_with_financial_years(
                |key| params
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.clone()),
                clock.today,
                Some(years)
            )
        );
        assert_eq!(
            filter
                .event_checked(
                    "period_select:first",
                    &json!({"period_filter[period]": "this_financial_year"}),
                    &clock
                )
                .unwrap(),
            Some(bilibi_datetime::filter::Outcome::Applied(current))
        );
        let later = Clock {
            today: date(next),
            ..clock.clone()
        };
        filter.set(
            Period::parse_with_financial_years(
                |key| (key == "period").then(|| "this_financial_year".into()),
                later.today,
                Some(years),
            ),
            &later,
        );
        assert_eq!(filter.period.preset, Preset::Last30Days);
    }
    assert!(FinancialYears::current(today, today).is_err());
    let mut display = DisplayContext::anonymous();
    for (locale, expected) in [
        ("en-MY", "Nov 2026"),
        ("fr-FR", "nov. 2026"),
        ("ja-JP", "2026/11"),
    ] {
        display.locale = locale;
        assert_eq!(display.month_label(date("2026-11-16")), expected);
    }
    println!("Report periods: calendar edges, variable financial years, URL/event round trips and localized month headings passed.");
}
