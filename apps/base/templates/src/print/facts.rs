use super::{Checked, Problem, MAX_FIELDS};
use crate::catalogue::identifier;
use jiff::civil::Date;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, BTreeSet};

const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_FACT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ROWS: usize = 2_000;
const MAX_ADDRESS_LINES: usize = 12;
const MAX_RASTER_BYTES: usize = 256 * 1024;
const MAX_QR_URL_BYTES: usize = 512;

/// A runtime record or one row in a document table. Owners supply authorised snapshots.
pub type PrintFacts<'a> = &'a [(&'a str, PrintValue<'a>)];

#[derive(Clone, Copy, Debug)]
pub enum PrintValue<'a> {
    Text(&'a str),
    Integer(i64),
    Decimal(Decimal),
    Money { amount: Decimal, currency: &'a str },
    Date(Date),
    Boolean(bool),
    Address(&'a [&'a str]),
    Raster { mime: &'a str, bytes: &'a [u8] },
    QrCode(&'a str),
    Rows(&'a [PrintFacts<'a>]),
}

#[derive(Clone, Copy)]
pub enum PrintKind {
    Text,
    Integer,
    Decimal,
    Money,
    Date,
    Boolean,
    Address,
    Raster,
    QrCode,
    Rows(&'static [PrintField]),
}

pub struct PrintField {
    pub id: &'static str,
    pub labels: &'static [(&'static str, &'static str)],
    pub kind: PrintKind,
    pub optional: bool,
    pub required_output: bool,
    pub sample: PrintValue<'static>,
}

impl PrintField {
    pub(super) fn requires_output(&self) -> bool {
        self.required_output
            || matches!(self.kind, PrintKind::Rows(columns) if columns.iter().any(|column| column.required_output))
    }
}

pub(super) type CheckedFacts<'a> = BTreeMap<&'a str, PrintValue<'a>>;

pub(super) fn validate_fields(fields: &[PrintField], allow_rows: bool) -> Checked<()> {
    if fields.is_empty() || fields.len() > MAX_FIELDS {
        return Err(Problem::new(
            "fields",
            "Declare between 1 and 128 print fields.",
        ));
    }
    let mut ids = BTreeSet::new();
    for field in fields {
        if !identifier(field.id) || !ids.insert(field.id) {
            return Err(Problem::new(
                field.id,
                "Print fields need unique identifiers.",
            ));
        }
        if let PrintKind::Rows(columns) = field.kind {
            if !allow_rows {
                return Err(Problem::new(
                    field.id,
                    "A print table cannot contain another table.",
                ));
            }
            validate_fields(columns, false)?;
        }
        let mut budget = MAX_FACT_BYTES;
        check_value(field, field.sample, &mut budget, allow_rows)?;
    }
    Ok(())
}

pub(super) fn check_facts<'a>(
    fields: &[PrintField],
    facts: PrintFacts<'a>,
) -> Checked<CheckedFacts<'a>> {
    let mut budget = MAX_FACT_BYTES;
    read_facts(fields, facts, &mut budget, true)
}

fn read_facts<'a>(
    fields: &[PrintField],
    facts: PrintFacts<'a>,
    budget: &mut usize,
    allow_rows: bool,
) -> Checked<CheckedFacts<'a>> {
    if facts.len() > fields.len() {
        return Err(Problem::new("facts", "Too many print facts."));
    }
    let declared: BTreeMap<_, _> = fields.iter().map(|field| (field.id, field)).collect();
    let mut values = BTreeMap::new();
    for &(id, value) in facts {
        let field = declared
            .get(id)
            .ok_or_else(|| Problem::new(id, "The print contract does not declare this fact."))?;
        if values.insert(id, value).is_some() {
            return Err(Problem::new(id, "The same print fact was supplied twice."));
        }
        check_value(field, value, budget, allow_rows)?;
    }
    for field in fields {
        if !field.optional && !values.contains_key(field.id) {
            return Err(Problem::new(field.id, "Required print fact is missing."));
        }
    }
    Ok(values)
}

fn check_value(
    field: &PrintField,
    value: PrintValue<'_>,
    budget: &mut usize,
    allow_rows: bool,
) -> Checked<()> {
    let size = match value {
        PrintValue::Text(text) => text.len(),
        PrintValue::QrCode(url) => url.len(),
        PrintValue::Raster { bytes, .. } => bytes.len(),
        PrintValue::Address(lines) if lines.len() <= MAX_ADDRESS_LINES => lines
            .iter()
            .map(|line| line.len().min(MAX_TEXT_BYTES + 1))
            .sum(),
        _ => 32,
    };
    *budget = budget
        .checked_sub(size.max(32))
        .ok_or_else(|| Problem::new("facts", "Print facts exceed the document budget."))?;
    let valid_text = |text: &str| text.len() <= MAX_TEXT_BYTES && !text.trim().is_empty();
    let valid = match (field.kind, value) {
        (PrintKind::Text, PrintValue::Text(text)) => valid_text(text),
        (PrintKind::Integer, PrintValue::Integer(_))
        | (PrintKind::Decimal, PrintValue::Decimal(_))
        | (PrintKind::Date, PrintValue::Date(_))
        | (PrintKind::Boolean, PrintValue::Boolean(_)) => true,
        (PrintKind::Money, PrintValue::Money { currency, .. }) => {
            currency.len() == 3 && currency.bytes().all(|c| c.is_ascii_uppercase())
        }
        (PrintKind::Address, PrintValue::Address(lines)) => {
            !lines.is_empty()
                && lines.len() <= MAX_ADDRESS_LINES
                && lines.iter().all(|line| valid_text(line))
        }
        (PrintKind::Raster, PrintValue::Raster { mime, bytes }) if allow_rows => {
            check_raster(mime, bytes)
        }
        (PrintKind::QrCode, PrintValue::QrCode(url)) if allow_rows => {
            url.len() <= MAX_QR_URL_BYTES
                && url::Url::parse(url).is_ok_and(|url| {
                    url.scheme() == "https"
                        && url.has_host()
                        && url.username().is_empty()
                        && url.password().is_none()
                })
        }
        (PrintKind::Rows(columns), PrintValue::Rows(rows)) if allow_rows => {
            if rows.is_empty() && field.requires_output() && !field.optional {
                return Err(Problem::new(
                    field.id,
                    "Required print output needs at least one row.",
                ));
            }
            if rows.len() > MAX_ROWS {
                return Err(Problem::new(
                    field.id,
                    "The print table exceeds 2,000 rows.",
                ));
            }
            for (index, row) in rows.iter().enumerate() {
                read_facts(columns, row, budget, false).map_err(|problem| {
                    Problem::new(
                        &format!("{}[{}].{}", field.id, index + 1, problem.field),
                        problem.message,
                    )
                })?;
            }
            true
        }
        _ => false,
    };
    if !valid {
        return Err(Problem::new(
            field.id,
            "Print fact has the wrong type, is empty, or exceeds its limit.",
        ));
    }
    Ok(())
}

fn check_raster(mime: &str, bytes: &[u8]) -> bool {
    bytes.len() <= MAX_RASTER_BYTES
        && match mime {
            "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
            "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
            _ => false,
        }
}
