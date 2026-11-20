//! TOON (Token-Oriented Object Notation) for what the command prints: the
//! same data as JSON in fewer tokens. Objects are `key: value` lines, a list
//! of like records is one `name[N]{a,b}:` header and a row each, a list of
//! plain values is `name[N]: a,b`. `--json` prints the server's JSON instead.
use serde_json::{Map, Value};
use std::fmt::Write;

/// What one command prints, in the order it was put together: the server's
/// JSON keeps its own (alphabetical) field order inside each value, and the
/// command decides which fields come first.
#[derive(Default)]
pub struct Doc(Vec<(String, Node)>);

enum Node {
    Value(Value),
    /// A table whose column order the command chose.
    Table {
        columns: Vec<&'static str>,
        rows: Vec<Vec<Value>>,
    },
    /// Plain lines, as the `help` list.
    Lines(Vec<String>),
}

impl Doc {
    pub fn put(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.0.push((name.to_owned(), Node::Value(value.into())));
        self
    }
    /// Whether a field of this name was put already.
    pub fn has(&self, name: &str) -> bool {
        self.0.iter().any(|(put, _)| put == name)
    }
    /// `put` when there is a value.
    pub fn put_some(self, name: &str, value: Option<impl Into<Value>>) -> Self {
        match value {
            Some(value) => self.put(name, value),
            None => self,
        }
    }
    pub fn table(mut self, name: &str, columns: &[&'static str], rows: Vec<Vec<Value>>) -> Self {
        self.0.push((
            name.to_owned(),
            Node::Table {
                columns: columns.to_vec(),
                rows,
            },
        ));
        self
    }
    /// The next commands worth running, last in every answer.
    pub fn help(mut self, lines: &[String]) -> Self {
        if !lines.is_empty() {
            self.0
                .push(("help".to_owned(), Node::Lines(lines.to_vec())));
        }
        self
    }
    /// The text as TOON, ending in a newline.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (name, node) in &self.0 {
            match node {
                Node::Value(value) => field(&mut out, name, value, 0),
                Node::Table { columns, rows } => {
                    let _ = writeln!(
                        out,
                        "{}[{}]{{{}}}:",
                        key(name),
                        rows.len(),
                        columns.join(",")
                    );
                    for row in rows {
                        let cells: Vec<String> = row.iter().map(scalar).collect();
                        let _ = writeln!(out, "  {}", cells.join(","));
                    }
                }
                Node::Lines(lines) => {
                    let _ = writeln!(out, "{}[{}]:", key(name), lines.len());
                    for line in lines {
                        let _ = writeln!(out, "  {line}");
                    }
                }
            }
        }
        out
    }
}

fn pad(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn object(out: &mut String, map: &Map<String, Value>, depth: usize) {
    for (name, value) in map {
        field(out, name, value, depth);
    }
}

fn field(out: &mut String, name: &str, value: &Value, depth: usize) {
    match value {
        Value::Object(map) => {
            pad(out, depth);
            let _ = writeln!(out, "{}:", key(name));
            object(out, map, depth + 1);
        }
        Value::Array(items) => array(out, &key(name), items, depth),
        other => {
            pad(out, depth);
            let _ = writeln!(out, "{}: {}", key(name), scalar(other));
        }
    }
}

/// The columns of a table: every item an object with the same keys in the
/// same order and no nested value.
fn columns(items: &[Value]) -> Option<Vec<&String>> {
    let first: Vec<&String> = items.first()?.as_object()?.keys().collect();
    if first.is_empty() {
        return None;
    }
    items
        .iter()
        .all(|item| {
            item.as_object().is_some_and(|map| {
                map.len() == first.len()
                    && map.keys().eq(first.iter().copied())
                    && map.values().all(|v| !v.is_object() && !v.is_array())
            })
        })
        .then_some(first)
}

fn array(out: &mut String, name: &str, items: &[Value], depth: usize) {
    pad(out, depth);
    let count = items.len();
    if items.iter().all(|v| !v.is_object() && !v.is_array()) {
        let values: Vec<String> = items.iter().map(scalar).collect();
        let _ = writeln!(
            out,
            "{name}[{count}]:{}{}",
            if count == 0 { "" } else { " " },
            values.join(",")
        );
        return;
    }
    if let Some(columns) = columns(items) {
        let names: Vec<String> = columns.iter().map(|name| key(name)).collect();
        let _ = writeln!(out, "{name}[{count}]{{{}}}:", names.join(","));
        for item in items {
            pad(out, depth + 1);
            let row: Vec<String> = item
                .as_object()
                .into_iter()
                .flat_map(Map::values)
                .map(scalar)
                .collect();
            let _ = writeln!(out, "{}", row.join(","));
        }
        return;
    }
    let _ = writeln!(out, "{name}[{count}]:");
    for item in items {
        match item {
            Value::Object(map) => {
                // The first field shares the hyphen's line; the rest align under it.
                let mut body = String::new();
                object(&mut body, map, depth + 2);
                pad(out, depth + 1);
                out.push_str("- ");
                out.push_str(body.trim_start_matches(' '));
                if map.is_empty() {
                    out.push('\n');
                }
            }
            Value::Array(inner) => {
                let mut body = String::new();
                array(&mut body, "", inner, depth + 2);
                pad(out, depth + 1);
                out.push_str("- ");
                out.push_str(body.trim_start_matches(' '));
            }
            other => {
                pad(out, depth + 1);
                let _ = writeln!(out, "- {}", scalar(other));
            }
        }
    }
}

fn key(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if plain {
        name.to_owned()
    } else {
        quoted(name)
    }
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => string(text),
        other => other.to_string(),
    }
}

/// A string as itself when that reads back as the same string, else quoted.
fn string(text: &str) -> String {
    let reserved = matches!(text, "true" | "false" | "null") || text.parse::<f64>().is_ok();
    let unsafe_char =
        |c: char| c.is_control() || matches!(c, ',' | ':' | '"' | '\\' | '[' | ']' | '{' | '}');
    if text.is_empty()
        || reserved
        || text != text.trim()
        || text.starts_with('-')
        || text.chars().any(unsafe_char)
    {
        quoted(text)
    } else {
        text.to_owned()
    }
}

fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
