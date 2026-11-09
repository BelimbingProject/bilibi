//! The reminder choices the pages share: the offsets an activity reminds at
//! and whether it emails them, as one coded choice each. A code is `default`
//! (follow the creator's default, then the tenant's), `none`, or the offsets
//! in minutes joined by commas, furthest first (`1440,15`). Any list the
//! contract accepts has a code, so an activity set through the agent door
//! still reads on the page.
use std::fmt::Write;

/// The offered offsets: one reminder each, then a few of two.
const PRESETS: &[&[i32]] = &[
    &[0],
    &[5],
    &[10],
    &[15],
    &[30],
    &[60],
    &[120],
    &[1440],
    &[2880],
    &[10080],
    &[60, 10],
    &[1440, 60],
    &[1440, 15],
    &[10080, 1440],
];

pub(super) const DEFAULT: &str = "default";
pub(super) const NONE: &str = "none";

fn phrase(minutes: i32) -> String {
    let (n, unit) = match minutes {
        0 => return "at the time".into(),
        m if m % 10080 == 0 => (m / 10080, "week"),
        m if m % 1440 == 0 => (m / 1440, "day"),
        m if m % 60 == 0 => (m / 60, "hour"),
        m => (m, "minute"),
    };
    format!("{n} {unit}{} before", if n == 1 { "" } else { "s" })
}

/// The code of a list of offsets, furthest first.
pub(super) fn code(minutes: Option<&[i32]>) -> String {
    match minutes {
        None => DEFAULT.into(),
        Some([]) => NONE.into(),
        Some(list) => {
            let mut code = String::new();
            for (index, minutes) in list.iter().enumerate() {
                if index > 0 {
                    code.push(',');
                }
                let _ = write!(code, "{minutes}");
            }
            code
        }
    }
}

/// The words for a list of offsets: "1 day and 15 minutes before".
pub(super) fn label(minutes: Option<&[i32]>) -> String {
    match minutes {
        None => "Default".into(),
        Some([]) => "No reminder".into(),
        Some(list) => {
            let mut phrases: Vec<String> = list.iter().map(|m| phrase(*m)).collect();
            // "1 day and 15 minutes before", not "1 day before and 15 minutes before".
            for phrase in phrases.iter_mut().rev().skip(1) {
                if let Some(trimmed) = phrase.strip_suffix(" before") {
                    *phrase = trimmed.to_owned();
                }
            }
            let mut text = match phrases.split_last() {
                Some((last, rest)) if !rest.is_empty() => {
                    format!("{} and {last}", rest.join(", "))
                }
                _ => phrases.concat(),
            };
            if let Some(first) = text.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            text
        }
    }
}

/// The offers for a select, with `current` added when it is none of them.
/// `with_default` words the first choice for where it appears.
pub(super) fn options(current: Option<&[i32]>, default_label: &str) -> Vec<(String, String)> {
    let mut options = vec![
        (default_label.to_owned(), DEFAULT.to_owned()),
        ("No reminder".to_owned(), NONE.to_owned()),
    ];
    for preset in PRESETS {
        options.push((label(Some(*preset)), code(Some(*preset))));
    }
    let own = code(current);
    if !options.iter().any(|(_, value)| *value == own) {
        options.push((label(current), own));
    }
    options
}

/// The offsets a posted code means: `Some(None)` is the default.
pub(super) fn parse(code: &str) -> Option<Option<Vec<i32>>> {
    match code.trim() {
        "" | DEFAULT => Some(None),
        NONE => Some(Some(Vec::new())),
        list => list
            .split(',')
            .map(|m| m.trim().parse::<i32>().ok())
            .collect::<Option<Vec<i32>>>()
            .map(Some),
    }
}

/// Email's three choices: follow the default, email, or do not.
pub(super) const EMAIL: &[(&str, &str)] = &[
    ("Default", "default"),
    ("Email the reminders", "on"),
    ("Do not email", "off"),
];

pub(super) fn email_code(email: Option<bool>) -> &'static str {
    match email {
        None => "default",
        Some(true) => "on",
        Some(false) => "off",
    }
}

pub(super) fn email_label(email: Option<bool>) -> &'static str {
    EMAIL
        .iter()
        .find(|(_, code)| *code == email_code(email))
        .map_or("Default", |(label, _)| label)
}

/// The email choice a posted code means: `Some(None)` is the default.
pub(super) fn parse_email(code: &str) -> Option<Option<bool>> {
    match code.trim() {
        "" | "default" => Some(None),
        "on" => Some(Some(true)),
        "off" => Some(Some(false)),
        _ => None,
    }
}
