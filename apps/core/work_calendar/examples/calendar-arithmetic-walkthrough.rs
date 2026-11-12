//! Native arithmetic walkthrough on an explicitly disposable database.
//! Calendar setup uses reviewed contracts; arithmetic uses the public facade
//! and the agent read contract. Expected instants are worked civil examples,
//! including independently counted opening hours, not computed by the owner.
use bilibi_agent_api::{
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
    Registry,
};
use bilibi_database::{pool, Error};
use bilibi_tenancy::{ProofFuture, Scope};
use bilibi_work_calendar::{TimeError, Timetable, WorkCalendars, SERVICE};
use jiff::{civil::Date, SignedDuration, Timestamp};
use serde_json::{json, Value};
use std::fmt::Debug;
use tokio_postgres::Transaction;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Failure = Box<dyn std::error::Error>;
struct Browser<'a> {
    app: &'a bilibi_web::App,
    token: &'a str,
}
impl PlanAuthority for Browser<'_> {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            Ok(PlanIdentity {
                scope: self.app.security.sessions.hold(tx, self.token).await?,
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}
async fn write(
    plans: &Plans,
    browser: &Browser<'_>,
    scope: &Scope,
    key: &str,
    input: Value,
) -> Result<(), Failure> {
    let operations = [PlannedOperation {
        key: key.into(),
        input,
    }];
    let preview = plans.preview(browser, &operations).await?;
    if preview["valid"] != json!(true) {
        return Err(format!("setup refused: {}", preview["results"]).into());
    }
    plans.commit(scope, browser, &operations).await?;
    Ok(())
}
fn week(periods: &[&str], weekends: bool) -> Value {
    let working = json!({"type": "working", "periods": periods});
    let saturday = if weekends {
        working.clone()
    } else {
        json!({"type": "off"})
    };
    let sunday = if weekends {
        working.clone()
    } else {
        json!({"type": "rest"})
    };
    json!({"monday": working, "tuesday": working, "wednesday": working,
        "thursday": working, "friday": working, "saturday": saturday, "sunday": sunday})
}
fn at(text: &str) -> Timestamp {
    text.parse().expect("walkthrough instant")
}
fn date(text: &str) -> Date {
    text.parse().expect("walkthrough date")
}
fn hours(n: i64) -> SignedDuration {
    SignedDuration::from_hours(n)
}
fn check<T: Debug + PartialEq>(label: &str, actual: T, expected: T) -> Result<(), Failure> {
    if actual != expected {
        return Err(format!("{label}: expected {expected:?}, got {actual:?}").into());
    }
    println!("PASS {label}: {actual:?}");
    Ok(())
}
fn shifted(
    table: &Timetable,
    label: &str,
    start: &str,
    n: i64,
    expected: &str,
) -> Result<(), Failure> {
    check(label, table.shift(at(start), hours(n)), Ok(at(expected)))
}
fn days_at(
    table: &Timetable,
    label: &str,
    start: &str,
    n: u32,
    expected: &str,
) -> Result<(), Failure> {
    check(
        label,
        table.add_working_days_at(at(start), n),
        Ok(at(expected)),
    )
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_WORK_CALENDAR_DEMO").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_WORK_CALENDAR_DEMO=disposable and a fresh disposable DATABASE_URL".into(),
        );
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_web::migrate(&db).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let calendars = app.services.get::<WorkCalendars>(SERVICE)?;
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let login = app
        .user
        .login("ai@agent.my", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let scope = app.security.guard(&login.token).await?;
    let browser = Browser {
        app: &app,
        token: &login.token,
    };

    for (code, timezone, periods, weekends) in [
        (
            "arithmetic-office",
            "UTC",
            vec!["09:00-12:00", "13:00-17:00"],
            false,
        ),
        ("arithmetic-night", "UTC", vec!["22:00-02:00"], true),
        (
            "arithmetic-dst",
            "America/New_York",
            vec!["00:00-04:00"],
            true,
        ),
        (
            "arithmetic-local",
            "Asia/Kuala_Lumpur",
            vec!["09:00-17:00"],
            false,
        ),
        ("arithmetic-all", "UTC", vec!["00:00-24:00"], true),
        ("arithmetic-week", "UTC", vec!["09:00-17:00"], false),
        ("arithmetic-shut", "UTC", vec!["09:00-17:00"], false),
    ] {
        write(&plans, &browser, &scope, "core.work_calendar.create",
            json!({"code":code,"name":code,"timezone":timezone,"effective_from":"2025-01-01","week":week(&periods,weekends)})).await?;
    }
    write(
        &plans,
        &browser,
        &scope,
        "core.work_calendar.holiday_set.create",
        json!({"code":"arithmetic-holidays","name":"Arithmetic holidays"}),
    )
    .await?;
    write(&plans, &browser, &scope, "core.work_calendar.holiday_set.holidays",
        json!({"code":"arithmetic-holidays","version":1,"add":[{"date":"2026-11-09","name":"Office holiday"}]})).await?;
    write(
        &plans,
        &browser,
        &scope,
        "core.work_calendar.include",
        json!({"code":"arithmetic-office","version":1,"holiday_set":"arithmetic-holidays"}),
    )
    .await?;
    write(&plans, &browser, &scope, "core.work_calendar.exception.add",
        json!({"code":"arithmetic-office","version":2,"starts_on":"2026-11-12","name":"Short day","type":"working","periods":["09:00-12:00"]})).await?;
    for (version, starts_on, ends_on) in [
        (1, "2025-02-01", "2025-12-31"),
        (2, "2026-01-01", "2026-02-05"),
    ] {
        write(&plans, &browser, &scope, "core.work_calendar.exception.add",
            json!({"code":"arithmetic-shut","version":version,"starts_on":starts_on,"ends_on":ends_on,"name":"Shut","type":"off"})).await?;
    }
    write(&plans, &browser, &scope, "core.work_calendar.create",
        json!({"code":"arithmetic-closed","name":"Closed calendar","timezone":"UTC","week":{
            "monday":{"type":"off"},"tuesday":{"type":"off"},"wednesday":{"type":"off"},
            "thursday":{"type":"off"},"friday":{"type":"off"},"saturday":{"type":"off"},"sunday":{"type":"rest"}}})).await?;

    // Load through the caller's transaction, just as a consuming mutation does.
    let mut client = db.get().await?;
    let tx = client.transaction().await?;
    calendars.check(&*tx, &scope, "arithmetic-office").await?;
    let office = Timetable::load(
        &*tx,
        &scope,
        "arithmetic-office",
        date("2026-01-01"),
        date("2027-12-31"),
    )
    .await?;
    check(
        "legacy negative rejection remains",
        office.add(at("2026-11-10T10:00:00Z"), hours(-1)),
        Err(TimeError::Negative),
    )?;
    for (label, start, n, end) in [
        (
            "positive skips weekend and holiday",
            "2026-11-06T16:00:00Z",
            2,
            "2026-11-10T10:00:00Z",
        ),
        (
            "negative skips weekend and holiday",
            "2026-11-10T10:00:00Z",
            -8,
            "2026-11-06T09:00:00Z",
        ),
        (
            "positive exact close",
            "2026-11-06T16:00:00Z",
            1,
            "2026-11-06T17:00:00Z",
        ),
        (
            "negative exact open",
            "2026-11-06T10:00:00Z",
            -1,
            "2026-11-06T09:00:00Z",
        ),
        (
            "negative closed start uses previous close",
            "2026-11-06T18:00:00Z",
            -1,
            "2026-11-06T16:00:00Z",
        ),
        (
            "negative at opening crosses closure",
            "2026-11-10T09:00:00Z",
            -1,
            "2026-11-06T16:00:00Z",
        ),
        (
            "negative lunch start",
            "2026-11-10T12:30:00Z",
            -1,
            "2026-11-10T11:00:00Z",
        ),
        (
            "negative lunch end",
            "2026-11-10T13:00:00Z",
            -1,
            "2026-11-10T11:00:00Z",
        ),
        (
            "positive at lunch close",
            "2026-11-10T12:00:00Z",
            1,
            "2026-11-10T14:00:00Z",
        ),
        (
            "zero while closed",
            "2026-11-08T12:00:00Z",
            0,
            "2026-11-08T12:00:00Z",
        ),
        (
            "zero while open",
            "2026-11-10T10:00:00Z",
            0,
            "2026-11-10T10:00:00Z",
        ),
    ] {
        shifted(&office, label, start, n, end)?;
    }
    check(
        "holiday is closed",
        office.is_open(at("2026-11-09T10:00:00Z")),
        Ok(false),
    )?;
    check(
        "lunch is closed",
        office.is_open(at("2026-11-10T12:30:00Z")),
        Ok(false),
    )?;
    let pause = at("2026-11-10T10:00:00Z");
    let overdue = -office.elapsed(at("2026-11-06T16:00:00Z"), pause)?;
    check("overdue remaining budget", overdue, hours(-2))?;
    check(
        "overdue resume excludes pause",
        office.shift(at("2026-11-11T10:00:00Z"), overdue),
        Ok(at("2026-11-10T16:00:00Z")),
    )?;
    let remaining = office.elapsed(pause, at("2026-11-10T15:00:00Z"))?;
    check("positive remaining budget", remaining, hours(4))?;
    check(
        "positive resume excludes pause",
        office.shift(at("2026-11-11T10:00:00Z"), remaining),
        Ok(at("2026-11-11T15:00:00Z")),
    )?;
    check(
        "date-only path unchanged",
        office.add_working_days(date("2026-11-06"), 1),
        Ok(date("2026-11-10")),
    )?;
    for (label, start, n, end) in [
        (
            "working-day local time",
            "2026-11-06T16:00:00Z",
            1,
            "2026-11-10T16:00:00Z",
        ),
        (
            "earlier destination close",
            "2026-11-11T16:00:00Z",
            1,
            "2026-11-12T12:00:00Z",
        ),
        (
            "zero working date retains subsecond time",
            "2026-11-10T10:23:45.123456789Z",
            0,
            "2026-11-10T10:23:45.123456789Z",
        ),
        (
            "zero non-working date rolls forward",
            "2026-11-08T16:00:00Z",
            0,
            "2026-11-10T16:00:00Z",
        ),
        (
            "zero caps at close",
            "2026-11-12T16:00:00Z",
            0,
            "2026-11-12T12:00:00Z",
        ),
        (
            "before opening stays before opening",
            "2026-11-06T08:00:00Z",
            1,
            "2026-11-10T08:00:00Z",
        ),
        (
            "lunch time stays in lunch",
            "2026-11-06T12:30:00Z",
            1,
            "2026-11-10T12:30:00Z",
        ),
    ] {
        days_at(&office, label, start, n, end)?;
    }
    check(
        "negative outside window",
        office.shift(at("2025-12-31T10:00:00Z"), hours(-1)),
        Err(TimeError::Beyond),
    )?;
    check(
        "working-day instant outside window",
        office.add_working_days_at(at("2025-12-31T10:00:00Z"), 1),
        Err(TimeError::Beyond),
    )?;
    check(
        "negative minimum seconds does not overflow",
        office.shift(pause, SignedDuration::from_secs(i64::MIN)),
        Err(TimeError::Beyond),
    )?;
    check(
        "positive search bounded",
        office.shift(pause, hours(366 * 24)),
        Err(TimeError::Beyond),
    )?;
    check(
        "negative search bounded",
        office.shift(at("2027-05-01T10:00:00Z"), hours(-366 * 24)),
        Err(TimeError::Beyond),
    )?;
    check(
        "working-day search bounded",
        office.add_working_days_at(pause, 366),
        Err(TimeError::Beyond),
    )?;
    tx.rollback().await?;

    let night = calendars
        .timetable(
            &scope,
            "arithmetic-night",
            date("2026-11-01"),
            date("2026-11-15"),
        )
        .await?;
    shifted(
        &night,
        "overnight forward exact close",
        "2026-11-06T23:00:00Z",
        3,
        "2026-11-07T02:00:00Z",
    )?;
    shifted(
        &night,
        "overnight backward exact open",
        "2026-11-07T01:00:00Z",
        -3,
        "2026-11-06T22:00:00Z",
    )?;
    days_at(
        &night,
        "overnight destination close is next date",
        "2026-11-06T23:15:00Z",
        1,
        "2026-11-07T23:15:00Z",
    )?;
    days_at(
        &night,
        "overnight source uses civil date",
        "2026-11-07T01:00:00Z",
        1,
        "2026-11-08T01:00:00Z",
    )?;
    let clipped = calendars
        .timetable(
            &scope,
            "arithmetic-night",
            date("2026-11-06"),
            date("2026-11-06"),
        )
        .await?;
    check(
        "overnight answer past loaded end",
        clipped.shift(at("2026-11-06T23:00:00Z"), hours(3)),
        Err(TimeError::Beyond),
    )?;
    let local = calendars
        .timetable(
            &scope,
            "arithmetic-local",
            date("2026-11-01"),
            date("2026-11-30"),
        )
        .await?;
    days_at(
        &local,
        "source local date differs from UTC",
        "2026-11-06T01:00:00+08:00",
        1,
        "2026-11-09T01:00:00+08:00",
    )?;
    write(&plans, &browser, &scope, "core.work_calendar.rule.set",
        json!({"code":"arithmetic-local","version":1,"effective_from":"2026-11-10","timezone":"UTC","week":week(&["09:00-17:00"],false)})).await?;
    let local = calendars
        .timetable(
            &scope,
            "arithmetic-local",
            date("2026-11-01"),
            date("2026-11-30"),
        )
        .await?;
    days_at(
        &local,
        "destination rule supplies its zone",
        "2026-11-09T10:00:00+08:00",
        1,
        "2026-11-10T10:00:00Z",
    )?;
    for (label, from, to) in [
        (
            "last instant before skipped span",
            "2026-11-09T23:59:59+08:00",
            "2026-11-10T17:00:00Z",
        ),
        (
            "skipped span start moves to next date",
            "2026-11-09T16:00:00Z",
            "2026-11-11T00:00:00Z",
        ),
        (
            "skipped span moves to next date",
            "2026-11-10T03:00:00+08:00",
            "2026-11-11T00:00:00Z",
        ),
        (
            "first instant after skipped span",
            "2026-11-10T00:00:00Z",
            "2026-11-11T00:00:00Z",
        ),
    ] {
        days_at(&local, label, from, 1, to)?;
    }

    let dst = calendars
        .timetable(
            &scope,
            "arithmetic-dst",
            date("2026-01-01"),
            date("2026-12-31"),
        )
        .await?;
    days_at(
        &dst,
        "gap moves to first valid instant",
        "2026-03-07T02:30:45-05:00",
        1,
        "2026-03-08T03:00:00-04:00",
    )?;
    days_at(
        &dst,
        "fold uses earlier occurrence",
        "2026-10-31T01:30:45.123-04:00",
        1,
        "2026-11-01T01:30:45.123-04:00",
    )?;
    shifted(
        &dst,
        "gap positive real hours",
        "2026-03-08T00:00:00-05:00",
        3,
        "2026-03-08T04:00:00-04:00",
    )?;
    shifted(
        &dst,
        "gap negative real hours",
        "2026-03-08T04:00:00-04:00",
        -3,
        "2026-03-08T00:00:00-05:00",
    )?;
    shifted(
        &dst,
        "fold positive real hours",
        "2026-11-01T00:00:00-04:00",
        5,
        "2026-11-01T04:00:00-05:00",
    )?;
    shifted(
        &dst,
        "fold negative real hours",
        "2026-11-01T04:00:00-05:00",
        -5,
        "2026-11-01T00:00:00-04:00",
    )?;
    let closed = calendars
        .timetable(
            &scope,
            "arithmetic-closed",
            date("2026-01-01"),
            date("2027-12-31"),
        )
        .await?;
    for n in [-1, 1] {
        check(
            "calendar without openings",
            closed.shift(pause, hours(n)),
            Err(TimeError::Beyond),
        )?;
    }
    check(
        "closed calendar zero shift",
        closed.shift(pause, hours(0)),
        Ok(pause),
    )?;
    check(
        "closed calendar working date",
        closed.add_working_days_at(pause, 0),
        Err(TimeError::Beyond),
    )?;
    for n in [-2117, 2117] {
        check(
            "closed calendar refuses a budget beyond one horizon",
            closed.shift(pause, hours(n)),
            Err(TimeError::Beyond),
        )?;
    }
    // Mon-Fri 09:00-17:00 in UTC, no closures: a three-hour timer started
    // 2024-01-01T09:00Z is due 12:00 and stays unresolved until paused
    // 2025-01-06T09:00Z, 53 weekday weeks (265 days of 8 hours less its own
    // 3 hours of budget) later; resumed 2026-01-05T09:00Z, 371 elapsed days
    // after the answer, beyond one 366-day horizon but inside the window.
    let started = at("2024-01-01T09:00:00Z");
    let paused = at("2025-01-06T09:00:00Z");
    let resumed = at("2026-01-05T09:00:00Z");
    let week_start = calendars
        .timetable(
            &scope,
            "arithmetic-week",
            date("2024-01-01"),
            date("2025-12-31"),
        )
        .await?;
    check(
        "three-hour timer due",
        week_start.add(started, hours(3)),
        Ok(at("2024-01-01T12:00:00Z")),
    )?;
    check(
        "53 weekday weeks of business time",
        week_start.elapsed(started, paused),
        Ok(hours(2120)),
    )?;
    check(
        "retained budget beyond one horizon",
        hours(3) - week_start.elapsed(started, paused)?,
        hours(-2117),
    )?;
    check(
        "positive budget beyond one horizon",
        week_start.add(started, hours(2120)),
        Ok(at("2025-01-03T17:00:00Z")),
    )?;
    let week = calendars
        .timetable(
            &scope,
            "arithmetic-week",
            date("2024-12-01"),
            date("2026-01-31"),
        )
        .await?;
    check(
        "overdue resume beyond one horizon",
        week.shift(resumed, hours(-2117)),
        Ok(at("2024-12-30T12:00:00Z")),
    )?;
    check(
        "positive resume beyond one horizon",
        week.shift(at("2024-12-02T09:00:00Z"), hours(2120)),
        Ok(at("2025-12-05T17:00:00Z")),
    )?;
    let short = calendars
        .timetable(
            &scope,
            "arithmetic-week",
            date("2025-06-01"),
            date("2026-01-31"),
        )
        .await?;
    check(
        "budget past the loaded window",
        short.shift(resumed, hours(-2117)),
        Err(TimeError::Beyond),
    )?;
    let shut = calendars
        .timetable(
            &scope,
            "arithmetic-shut",
            date("2025-01-01"),
            date("2026-03-01"),
        )
        .await?;
    check(
        "first opening over 366 days away forward",
        shut.add(at("2025-01-31T18:00:00Z"), hours(1)),
        Err(TimeError::Beyond),
    )?;
    check(
        "first opening within 366 days forward",
        shut.add(at("2025-02-10T10:00:00Z"), hours(1)),
        Ok(at("2026-02-06T10:00:00Z")),
    )?;
    check(
        "previous close over 366 days away",
        shut.shift(at("2026-02-06T09:00:00Z"), hours(-1)),
        Err(TimeError::Beyond),
    )?;
    check(
        "previous close within 366 days",
        shut.shift(at("2026-02-01T10:00:00Z"), hours(-1)),
        Ok(at("2025-01-31T16:00:00Z")),
    )?;
    let all = calendars
        .timetable(
            &scope,
            "arithmetic-all",
            date("2025-11-05"),
            date("2027-11-04"),
        )
        .await?;
    shifted(
        &all,
        "negative at exact 366-day distance",
        "2026-11-06T00:00:00Z",
        -366 * 24,
        "2025-11-05T00:00:00Z",
    )?;
    check(
        "one second before the window start",
        all.shift(
            at("2026-11-06T00:00:00Z"),
            SignedDuration::from_secs(-366 * 86400 - 1),
        ),
        Err(TimeError::Beyond),
    )?;

    let clock = Timetable::clock();
    shifted(
        &clock,
        "original negative-budget diagnostic through signed owner",
        "2026-11-06T12:00:00Z",
        -1,
        "2026-11-06T11:00:00Z",
    )?;
    days_at(
        &clock,
        "original civil-date diagnostic through instant owner",
        "2026-11-06T12:00:00Z",
        1,
        "2026-11-07T12:00:00Z",
    )?;
    check(
        "clock add still rejects negative",
        clock.add(pause, hours(-1)),
        Err(TimeError::Negative),
    )?;
    for n in [-2, 0, 2] {
        check(
            "clock signed budget",
            clock.shift(pause, hours(n)),
            Ok(at(match n {
                -2 => "2026-11-10T08:00:00Z",
                0 => "2026-11-10T10:00:00Z",
                _ => "2026-11-10T12:00:00Z",
            })),
        )?;
    }
    days_at(
        &clock,
        "clock working days are 24 hours",
        "2026-03-07T12:00:00-05:00",
        1,
        "2026-03-08T17:00:00Z",
    )?;
    days_at(
        &clock,
        "clock zero days",
        "2026-11-08T12:00:00Z",
        0,
        "2026-11-08T12:00:00Z",
    )?;
    check(
        "clock underflow",
        clock.shift(Timestamp::MIN, hours(-1)),
        Err(TimeError::Beyond),
    )?;
    check(
        "clock working-day overflow",
        clock.add_working_days_at(Timestamp::MAX, 1),
        Err(TimeError::Beyond),
    )?;

    for (input, expected) in [
        (
            json!({"operation":"shift","at":"2026-11-10T10:00:00Z","duration":"-PT8H"}),
            json!("2026-11-06T09:00:00Z"),
        ),
        (
            json!({"operation":"shift","at":"2026-11-06T16:00:00Z","duration":"PT2H"}),
            json!("2026-11-10T10:00:00Z"),
        ),
        (
            json!({"operation":"shift","at":"2026-11-08T12:00:00Z","duration":"PT0S"}),
            json!("2026-11-08T12:00:00Z"),
        ),
        (
            json!({"operation":"add_working_days_at","at":"2026-11-11T16:00:00Z","days":1}),
            json!("2026-11-12T12:00:00Z"),
        ),
    ] {
        let mut input = input;
        input["code"] = json!("arithmetic-office");
        let answer = registry
            .call(
                &**client,
                &scope,
                "core.work_calendar.business_time",
                &input,
            )
            .await?;
        check(
            "agent contract matches facade",
            answer["result"]["result"].clone(),
            expected,
        )?;
        check(
            "agent contract within horizon",
            answer["result"]["beyond"].clone(),
            json!(false),
        )?;
    }
    let other = app
        .user
        .login(
            "manager@other.example.test",
            bilibi_walkthrough_fixture::PASSWORD,
        )
        .await?;
    let other = app.security.guard(&other.token).await?;
    check(
        "calendar tenant isolation",
        calendars
            .timetable(
                &other,
                "arithmetic-office",
                date("2026-11-01"),
                date("2026-11-30"),
            )
            .await
            .err()
            .map(|e| e.to_string()),
        Some(Error::NotFound.to_string()),
    )?;
    write(
        &plans,
        &browser,
        &scope,
        "core.work_calendar.update",
        json!({"code":"arithmetic-office","version":3,"status":"retired"}),
    )
    .await?;
    let retired = calendars
        .timetable(
            &scope,
            "arithmetic-office",
            date("2026-11-01"),
            date("2026-11-30"),
        )
        .await?;
    shifted(
        &retired,
        "retired calendar still computes",
        "2026-11-10T10:00:00Z",
        -8,
        "2026-11-06T09:00:00Z",
    )?;
    check(
        "retired calendar refuses assignment",
        calendars
            .check(&**client, &scope, "arithmetic-office")
            .await
            .err()
            .map(|e| e.to_string()),
        Some(Error::Invalid.to_string()),
    )?;
    println!("PASS complete: signed budgets and working-day instants through Core owner contracts");
    Ok(())
}
