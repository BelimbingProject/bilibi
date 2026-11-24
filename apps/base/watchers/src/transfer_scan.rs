//! Scans a script step before it travels in a setup pack: a value that
//! identifies a person or a source record is refused, and free text is
//! flagged by path for the reviewer to check before apply.
use crate::definition::Problems;
use crate::script::Script;
use serde_json::Value;

/// Free text that could hold customer data, as a reviewer locates it.
const REVIEW: &str = "Review the literal for source record data.";
const PERSONAL: &str =
    "A script that contains an email address or phone number cannot travel. Remove it before export.";

/// Whether a text holds an email address or a phone number.
pub(crate) fn personal(text: &str) -> bool {
    let email = text.split_whitespace().any(|word| {
        word.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
        })
    });
    let mut digits = 0;
    let mut plus = false;
    let mut phone = false;
    for c in text.chars() {
        match c {
            '0'..='9' => digits += 1,
            '+' if digits == 0 => plus = true,
            ' ' | '-' | '(' | ')' | '.' => {}
            _ => {
                digits = 0;
                plus = false;
            }
        }
        // A date or an amount has at most eight digits; a phone number with
        // its country code has more, or starts with "+".
        phone |= digits >= 9 || (plus && digits >= 7);
    }
    email || phone
}

/// The string literals of JavaScript source, with their line and column.
/// Comments are skipped; a template literal counts as one literal.
pub(crate) fn literals(source: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<char> = source.chars().collect();
    let (mut at, mut line, mut column) = (0, 1, 1);
    let mut found = Vec::new();
    let step = |at: &mut usize, line: &mut usize, column: &mut usize| {
        if chars.get(*at) == Some(&'\n') {
            *line += 1;
            *column = 1;
        } else {
            *column += 1;
        }
        *at += 1;
    };
    while at < chars.len() {
        match (chars[at], chars.get(at + 1)) {
            ('/', Some('/')) => {
                while at < chars.len() && chars[at] != '\n' {
                    step(&mut at, &mut line, &mut column);
                }
            }
            ('/', Some('*')) => {
                step(&mut at, &mut line, &mut column);
                step(&mut at, &mut line, &mut column);
                while at < chars.len() && !(chars[at] == '*' && chars.get(at + 1) == Some(&'/')) {
                    step(&mut at, &mut line, &mut column);
                }
                step(&mut at, &mut line, &mut column);
                step(&mut at, &mut line, &mut column);
            }
            (quote @ ('\'' | '"' | '`'), _) => {
                let (start_line, start_column) = (line, column);
                step(&mut at, &mut line, &mut column);
                let mut text = String::new();
                while at < chars.len() && chars[at] != quote {
                    if chars[at] == '\\' {
                        step(&mut at, &mut line, &mut column);
                    }
                    if let Some(&c) = chars.get(at) {
                        text.push(c);
                    }
                    step(&mut at, &mut line, &mut column);
                }
                step(&mut at, &mut line, &mut column);
                found.push((start_line, start_column, text));
            }
            _ => step(&mut at, &mut line, &mut column),
        }
    }
    found
}

/// Every string inside a JSON value, with its JSON pointer below `path`.
fn strings<'a>(value: &'a Value, path: String, out: &mut Vec<(String, &'a str)>) {
    match value {
        Value::String(text) => out.push((path, text)),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                strings(item, format!("{path}/{index}"), out);
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                strings(item, format!("{path}/{}", pointer_key(key)), out);
            }
        }
        _ => {}
    }
}

pub(crate) fn pointer_key(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The script step at `/do/{step}`. `known` holds text the definition already
/// declares (operation keys, fields, choice values, declared inputs), which
/// the source may repeat without a new flag. `fact` tells whether an example
/// fact names a record identifier or personal field (refused) and whether it
/// is free text (flagged).
pub(crate) fn scan(
    script: &Script,
    step: usize,
    known: &[String],
    fact: &dyn Fn(&str) -> (bool, bool),
    problems: &mut Problems,
    warnings: &mut Vec<String>,
) {
    let at = format!("/do/{step}/script");
    let mut refuse = |why: &str| {
        if !problems.iter().any(|(_, seen)| seen == why) {
            problems.push(("do".into(), why.into()));
        }
    };
    for (line, column, text) in literals(&script.source) {
        if personal(&text) {
            refuse(PERSONAL);
        } else if !text.trim().is_empty() && !known.contains(&text) {
            warnings.push(format!(
                "Script source contains free text at {at}/source line {line}, column {column}. {REVIEW}"
            ));
        }
    }
    for (index, terms) in script.outbound.iter().enumerate() {
        warnings.push(format!(
            "Outbound terms at {at}/outbound/{index}/destination name the source tenant's recipient. Confirm or replace it before approval."
        ));
        if terms.data.iter().any(|field| personal(field)) {
            refuse(PERSONAL);
        }
    }
    let mut texts = Vec::new();
    for (index, example) in script.examples.iter().enumerate() {
        let path = format!("{at}/examples/{index}");
        for (field, value) in &example.facts {
            let (sensitive, free_text) = fact(field);
            if sensitive && !value.is_null() {
                refuse("An example fact for a record identifier or personal field cannot travel.");
            } else if free_text {
                strings(
                    value,
                    format!("{path}/facts/{}", pointer_key(field)),
                    &mut texts,
                );
            } else if value.as_str().is_some_and(personal) {
                refuse(PERSONAL);
            }
        }
        for (reply, fixture) in example.lookup_fixtures.iter().enumerate() {
            strings(
                &fixture.reply,
                format!("{path}/lookup_fixtures/{reply}/reply"),
                &mut texts,
            );
        }
    }
    for (path, text) in texts {
        if personal(text) {
            refuse(PERSONAL);
        } else if !known.iter().any(|known| known == text) {
            warnings.push(format!(
                "Script example or lookup at {path} contains free text. {REVIEW}"
            ));
        }
    }
}
