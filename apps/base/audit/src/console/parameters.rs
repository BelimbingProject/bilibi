//! A lexical pass, not an SQL parser. PostgreSQL remains the readonly boundary.
use bytes::BytesMut;
use std::{collections::BTreeMap, error::Error};
use tokio_postgres::types::{Format, IsNull, ToSql, Type};

/// Missing named values bind SQL NULL. Browser strings use PostgreSQL's text
/// input format for the inferred parameter type (integer, date, boolean, etc.).
pub type Parameters = BTreeMap<String, Option<String>>;

#[derive(Debug)]
struct Text(Option<String>);
impl ToSql for Text {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, Box<dyn Error + Send + Sync>> {
        match &self.0 {
            Some(value) => {
                out.extend_from_slice(value.as_bytes());
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }
    fn accepts(_: &Type) -> bool {
        true
    }
    fn encode_format(&self, _: &Type) -> Format {
        Format::Text
    }
    tokio_postgres::types::to_sql_checked!();
}
pub(super) struct Bound {
    pub sql: String,
    values: Vec<Text>,
}
impl Bound {
    pub fn values(&self) -> Vec<&(dyn ToSql + Sync)> {
        self.values
            .iter()
            .map(|v| v as &(dyn ToSql + Sync))
            .collect()
    }
}

pub fn extract_named_parameters(sql: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (_, _, name) in tokens(sql) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}
pub(super) fn bind(sql: &str, params: &Parameters) -> Bound {
    let mut names = Vec::new();
    let mut output = String::new();
    let mut copied = 0;
    for (start, end, name) in tokens(sql) {
        let index = match names.iter().position(|n| n == &name) {
            Some(index) => index,
            None => {
                names.push(name);
                names.len() - 1
            }
        };
        output.push_str(&sql[copied..start]);
        output.push_str(&format!("${}", index + 1));
        copied = end;
    }
    output.push_str(&sql[copied..]);
    Bound {
        sql: output,
        values: names
            .iter()
            .map(|name| Text(params.get(name).cloned().flatten()))
            .collect(),
    }
}
fn tokens(sql: &str) -> Vec<(usize, usize, String)> {
    let mask = code_mask(sql);
    let bytes = sql.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if mask[i]
            && bytes[i] == b':'
            && (i == 0 || bytes[i - 1] != b':')
            && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_')
        {
            let start = i;
            i += 2;
            while i < bytes.len()
                && mask[i]
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            result.push((start, i, sql[start + 1..i].to_owned()));
        } else {
            i += 1;
        }
    }
    result
}
pub(super) fn code_only(sql: &str) -> String {
    sql.bytes()
        .zip(code_mask(sql))
        .map(|(b, code)| {
            if code && b.is_ascii() {
                char::from(b)
            } else {
                ' '
            }
        })
        .collect()
}
/// Every name the text can use for a table or function: unquoted words
/// folded to lower case, then quoted identifiers as written. A Unicode-escaped
/// identifier (`U&"..."`) is `None`: it is not decoded, so a caller refuses it.
/// A word inside a literal or comment is not a name.
pub(super) fn names(sql: &str) -> Vec<Option<String>> {
    let (mask, quoted) = lex(sql);
    let b = sql.as_bytes();
    let code: String = b
        .iter()
        .zip(&mask)
        .map(|(byte, code)| {
            if *code && (byte.is_ascii_alphanumeric() || *byte == b'_') {
                char::from(byte.to_ascii_lowercase())
            } else {
                ' '
            }
        })
        .collect();
    let mut names: Vec<Option<String>> = code
        .split_whitespace()
        .map(|word| Some(word.to_owned()))
        .collect();
    for (start, end) in quoted {
        let unicode =
            start >= 2 && b[start - 1] == b'&' && b[start - 2].eq_ignore_ascii_case(&b'u');
        // An unclosed identifier is a syntax error; it is not read either.
        let closed = end >= start + 2 && b[end - 1] == b'"';
        names.push((closed && !unicode).then(|| sql[start + 1..end - 1].replace("\"\"", "\"")));
    }
    names
}
/// Masks literals, identifiers, nested block comments and dollar quotes without
/// changing byte offsets. E'...' backslash escapes are respected.
fn code_mask(sql: &str) -> Vec<bool> {
    lex(sql).0
}
/// [`code_mask`], and the span of each double-quoted identifier, quotes
/// included.
fn lex(sql: &str) -> (Vec<bool>, Vec<(usize, usize)>) {
    let b = sql.as_bytes();
    let mut mask = vec![true; b.len()];
    let mut quoted = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        if b[i] == b'\'' || b[i] == b'"' {
            let quote = b[i];
            let escape = quote == b'\''
                && i > 0
                && matches!(b[i - 1], b'e' | b'E')
                && (i < 2 || !(b[i - 2].is_ascii_alphanumeric() || b[i - 2] == b'_'));
            i += 1;
            while i < b.len() {
                if escape && b[i] == b'\\' {
                    i = (i + 2).min(b.len());
                } else if b[i] == quote {
                    i += 1;
                    if b.get(i) == Some(&quote) {
                        i += 1;
                    } else {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if quote == b'"' {
                quoted.push((start, i));
            }
        } else if b[i..].starts_with(b"--") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < b.len() && depth > 0 {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if b[i] == b'$'
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || matches!(b[i - 1], b'_' | b'$')))
        {
            let mut end = i + 1;
            if b.get(end)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
            {
                end += 1;
                while b
                    .get(end)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
                {
                    end += 1;
                }
            }
            if b.get(end) == Some(&b'$') {
                let delimiter = &sql[i..=end];
                i = sql[end + 1..]
                    .find(delimiter)
                    .map(|offset| end + 1 + offset + delimiter.len())
                    .unwrap_or(b.len());
            } else {
                i += 1;
                continue;
            }
        } else {
            i += 1;
            continue;
        }
        mask[start..i].fill(false);
    }
    (mask, quoted)
}
