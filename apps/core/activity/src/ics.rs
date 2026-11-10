//! The iCalendar text of a calendar feed (RFC 5545): one event per call,
//! meeting and dated task, with only what a calendar app needs (subject,
//! time, location and join link). The internal notes, the description, the
//! guests and the related record never leave Bilibi through a feed.
use crate::{Activity, Kind};
use jiff::{tz::TimeZone, SignedDuration, Timestamp};

/// How long a task is drawn from its due time, as on the Calendar tab.
const TASK_MINUTES: i64 = 30;
/// Longest content line, in octets, before it is folded (RFC 5545 3.1).
const FOLD: usize = 75;

fn stamp(at: Timestamp) -> String {
    at.to_zoned(TimeZone::UTC)
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string()
}

/// TEXT values: backslash, semicolon, comma and line breaks escaped, other
/// control characters dropped.
fn text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// One content line, folded at 75 octets on a character boundary.
fn line(out: &mut String, content: &str) {
    let mut width = 0;
    for c in content.chars() {
        if width + c.len_utf8() > FOLD {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += c.len_utf8();
    }
    out.push_str("\r\n");
}

/// The feed for `activities`, as the reader's own view of them. A note, or a
/// task with no due time, has no time to show and is left out.
pub(crate) fn render(activities: &[Activity]) -> String {
    let mut out = String::new();
    line(&mut out, "BEGIN:VCALENDAR");
    line(&mut out, "VERSION:2.0");
    line(&mut out, "PRODID:-//Bilibi//core/activity//EN");
    line(&mut out, "CALSCALE:GREGORIAN");
    line(&mut out, "METHOD:PUBLISH");
    line(&mut out, "X-WR-CALNAME:Bilibi activities");
    for a in activities {
        let (start, end, prefix) = match (a.kind, a.starts_at, a.ends_at, a.due_at) {
            (Kind::Call | Kind::Meeting, Some(start), Some(end), _) => (start, end, ""),
            (Kind::Task, _, _, Some(due)) => {
                (due, due + SignedDuration::from_mins(TASK_MINUTES), "Task: ")
            }
            _ => continue,
        };
        line(&mut out, "BEGIN:VEVENT");
        line(&mut out, &format!("UID:{}@core-activity.bilibi", a.id));
        line(&mut out, &format!("DTSTAMP:{}", stamp(a.updated_at)));
        line(&mut out, &format!("DTSTART:{}", stamp(start)));
        line(&mut out, &format!("DTEND:{}", stamp(end)));
        line(
            &mut out,
            &format!("SUMMARY:{}", text(&format!("{prefix}{}", a.title))),
        );
        if let Some(location) = a.location.as_deref().filter(|l| !l.is_empty()) {
            line(&mut out, &format!("LOCATION:{}", text(location)));
        }
        if let Some(url) = a.join_url.as_deref().filter(|u| !u.is_empty()) {
            let clean: String = url.chars().filter(|c| !c.is_control()).collect();
            line(&mut out, &format!("URL:{clean}"));
            line(
                &mut out,
                &format!("DESCRIPTION:{}", text(&format!("Join: {clean}"))),
            );
        }
        line(&mut out, "END:VEVENT");
    }
    line(&mut out, "END:VCALENDAR");
    out
}
