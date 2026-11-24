use bilibi_module_registry::{Consumer, Entry, RegistryError};
use jiff::civil::Date;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

pub const PLATFORM_CATALOGUE: &str = "base/templates.platform_catalogue";

pub const CATALOGUE: &str = "base/templates.catalogue";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    Staff,
    Customer,
    /// Staff and customers alike, such as a password reset.
    Anyone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    PlainText,
    PortalNotice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    Subject,
    PlainText,
}

#[derive(Clone, Copy, Debug)]
pub enum Value<'a> {
    Text(&'a str),
    Date(Date),
}

impl Value<'_> {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Date(_) => "date",
        }
    }
}

pub struct Variable {
    pub id: &'static str,
    pub label: &'static str,
    pub optional: bool,
    pub sample: Value<'static>,
    pub slots: &'static [&'static str],
}

pub struct Slot {
    pub id: &'static str,
    pub label: &'static str,
    pub context: Context,
    pub required: bool,
    pub max_bytes: usize,
}

impl Slot {
    /// The one-line subject of a message, within Base Mail's subject limit.
    pub const fn subject() -> Self {
        Self {
            id: "subject",
            label: "Subject",
            context: Context::Subject,
            required: true,
            max_bytes: 200,
        }
    }

    /// A required plain-text passage of a message.
    pub const fn text(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label,
            context: Context::PlainText,
            required: true,
            max_bytes: 4000,
        }
    }
}

impl Variable {
    /// A text fact the sender always supplies.
    pub const fn text(
        id: &'static str,
        label: &'static str,
        sample: &'static str,
        slots: &'static [&'static str],
    ) -> Self {
        Self {
            id,
            label,
            optional: false,
            sample: Value::Text(sample),
            slots,
        }
    }

    /// A text fact some messages lack; it then renders as nothing.
    pub const fn optional_text(
        id: &'static str,
        label: &'static str,
        sample: &'static str,
        slots: &'static [&'static str],
    ) -> Self {
        Self {
            id,
            label,
            optional: true,
            sample: Value::Text(sample),
            slots,
        }
    }
}

/// `text` cut to `max_bytes` at a character boundary, ending in an ellipsis
/// when cut and the marker fits: for a fact (a title, a name) placed in a bounded slot such as a
/// subject.
pub fn bounded(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let marker = if max_bytes >= "…".len() { "…" } else { "" };
    let mut end = max_bytes - marker.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &text[..end])
}

pub struct DefaultText {
    pub language: &'static str,
    pub version: &'static str,
    pub slots: &'static [(&'static str, &'static str)],
}

pub struct Template {
    pub key: &'static str,
    pub label: &'static str,
    pub audience: Audience,
    pub outputs: &'static [Output],
    pub slots: &'static [Slot],
    pub variables: &'static [Variable],
    pub base_language: &'static str,
    pub defaults: &'static [DefaultText],
}

pub type Text = BTreeMap<String, String>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum Origin {
    Packaged { version: String },
    Tenant { revision: i64 },
    Company { company_id: i64, revision: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub origin: Origin,
    pub language: String,
    pub slots: Text,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub field: String,
    pub message: String,
}

impl Problem {
    pub(crate) fn new(field: &str, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for Problem {}

type Checked<T> = Result<T, Problem>;

fn identifier(id: &str) -> bool {
    id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

pub(crate) fn language(input: &str) -> Option<String> {
    if input.len() > 80 {
        return None;
    }
    input
        .parse::<icu_locale::Locale>()
        .ok()
        .map(|tag| tag.to_string())
}

pub(crate) fn languages(
    requested: &str,
    fallback: Option<&str>,
    base: &'static str,
) -> Checked<Vec<String>> {
    let requested = language(requested)
        .ok_or_else(|| Problem::new("language", "Enter a language tag such as en or en-MY."))?;
    Ok(language_chain(
        std::iter::once(requested.as_str()).chain(fallback),
        base,
    ))
}

pub(crate) fn language_chain<'a>(
    preferred: impl IntoIterator<Item = &'a str>,
    base: &'static str,
) -> Vec<String> {
    let mut choices = Vec::new();
    for tag in preferred.into_iter().filter_map(language) {
        let parent = tag.split_once('-').map(|(parent, _)| parent.to_owned());
        choices.push(tag);
        choices.extend(parent);
    }
    choices.push(base.into());
    let mut seen = BTreeSet::new();
    choices.retain(|tag| seen.insert(tag.clone()));
    choices
}

impl DefaultText {
    pub(crate) fn resolve(&self) -> Resolved {
        Resolved {
            origin: Origin::Packaged {
                version: self.version.into(),
            },
            language: self.language.into(),
            slots: self
                .slots
                .iter()
                .map(|&(id, text)| (id.into(), text.into()))
                .collect(),
        }
    }
}

fn forbidden_character(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

fn check_text(slot: &Slot, text: &str) -> Checked<()> {
    if slot.required && text.trim().is_empty() {
        return Err(Problem::new(
            slot.id,
            format!("Enter {}.", slot.label.to_lowercase()),
        ));
    }
    if text.len() > slot.max_bytes {
        return Err(Problem::new(
            slot.id,
            format!("{} must fit in {} bytes.", slot.label, slot.max_bytes),
        ));
    }
    if text.chars().any(|c| {
        forbidden_character(c)
            && !(slot.context == Context::PlainText
                && matches!(c, '\n' | '\t' | '\u{2028}' | '\u{2029}'))
    }) {
        return Err(Problem::new(
            slot.id,
            format!("{} contains a character this field cannot use.", slot.label),
        ));
    }
    Ok(())
}

impl Template {
    /// Render declared slots without any lookups. Only named references use
    /// braces. A value is appended as data and is never parsed again.
    pub fn render(&self, text: &Text, values: &[(&str, Value<'_>)]) -> Checked<Text> {
        self.render_slots(text, values, &[], false)
    }

    pub(crate) fn render_delivery(
        &self,
        text: &Text,
        values: &[(&str, Value<'_>)],
        omitted: &[&str],
    ) -> Checked<Text> {
        self.render_slots(text, values, omitted, true)
    }

    fn render_slots(
        &self,
        text: &Text,
        values: &[(&str, Value<'_>)],
        omitted: &[&str],
        fit_subject: bool,
    ) -> Checked<Text> {
        for id in text.keys() {
            if !self.slots.iter().any(|slot| slot.id == id) {
                return Err(Problem::new("slots", "This template has an unknown field."));
            }
        }
        self.slots
            .iter()
            .filter(|slot| !omitted.contains(&slot.id))
            .filter(|slot| slot.required || text.contains_key(slot.id))
            .map(|slot| {
                let source = text.get(slot.id).map(String::as_str).unwrap_or_default();
                let output = self.render_slot(
                    slot,
                    source,
                    values,
                    fit_subject && slot.context == Context::Subject,
                )?;
                Ok((slot.id.into(), output))
            })
            .collect()
    }

    pub fn preview(&self, text: &Text) -> Checked<Text> {
        let sample: Vec<_> = self.variables.iter().map(|v| (v.id, v.sample)).collect();
        self.render(text, &sample)
    }

    fn render_slot(
        &self,
        slot: &Slot,
        source: &str,
        values: &[(&str, Value<'_>)],
        fit_subject: bool,
    ) -> Checked<String> {
        check_text(slot, source)?;
        let syntax = || {
            Problem::new(
                slot.id,
                "Use double braces around a field name, such as {{customer_name}}.",
            )
        };
        let mut remaining = source;
        let mut output = String::with_capacity(source.len());
        let mut facts = Vec::new();
        while let Some((literal, reference)) = remaining.split_once("{{") {
            if literal.contains("}}") {
                return Err(syntax());
            }
            output.push_str(literal);
            let (id, tail) = reference.split_once("}}").ok_or_else(syntax)?;
            let variable = self
                .variables
                .iter()
                .find(|v| v.id == id.trim())
                .ok_or_else(|| {
                    Problem::new(
                        slot.id,
                        "Remove the unknown variable or choose one of the available variables.",
                    )
                })?;
            if !variable.slots.contains(&slot.id) {
                return Err(Problem::new(
                    slot.id,
                    format!(
                        "{} cannot be used in {}.",
                        variable.label,
                        slot.label.to_lowercase()
                    ),
                ));
            }
            match values
                .iter()
                .find(|(id, _)| *id == variable.id)
                .map(|(_, value)| *value)
            {
                Some(value) if variable.sample.kind() == value.kind() => match value {
                    Value::Text(value) => {
                        if value.trim().is_empty() && !variable.optional {
                            return Err(Problem::new(
                                variable.id,
                                "Required template value is missing.",
                            ));
                        }
                        if !fit_subject && value.len() > slot.max_bytes {
                            return Err(Problem::new(
                                variable.id,
                                "Template value exceeds the field limit.",
                            ));
                        }
                        let start = output.len();
                        output.extend(value.chars().map(|c| {
                            if forbidden_character(c)
                                && !(slot.context == Context::PlainText
                                    && matches!(c, '\n' | '\t' | '\u{2028}' | '\u{2029}'))
                            {
                                ' '
                            } else {
                                c
                            }
                        }));
                        if fit_subject && start != output.len() {
                            facts.push(start..output.len());
                        }
                    }
                    Value::Date(date) => output
                        .push_str(&bilibi_datetime::DisplayContext::anonymous().render_date(date)),
                },
                Some(_) => {
                    return Err(Problem::new(
                        variable.id,
                        "Template value has the wrong type.",
                    ))
                }
                None if variable.optional => {}
                None => {
                    return Err(Problem::new(
                        variable.id,
                        "Required template value is missing.",
                    ))
                }
            }
            if !fit_subject && output.len() > slot.max_bytes {
                return Err(Problem::new(
                    slot.id,
                    format!("{} exceeds the rendered field limit.", slot.label),
                ));
            }
            remaining = tail;
        }
        if remaining.contains("}}") {
            return Err(syntax());
        }
        output.push_str(remaining);
        if fit_subject && output.len() > slot.max_bytes {
            output = fit_subject_facts(&output, &facts, slot.max_bytes).unwrap_or(output);
        }
        check_text(slot, &output)?;
        Ok(output)
    }
}

/// Shortens only the longest facts, to one shared length, until the subject
/// fits: a short fact (a name beside a long title) is never the one cut.
fn fit_subject_facts(text: &str, facts: &[Range<usize>], limit: usize) -> Option<String> {
    let total: usize = facts.iter().map(Range::len).sum();
    let budget = limit.checked_sub(text.len() - total)?;
    let fits = |cap: usize| facts.iter().map(|fact| fact.len().min(cap)).sum::<usize>() <= budget;
    let longest = facts.iter().map(Range::len).max().unwrap_or(0);
    let (mut low, mut high) = (0, longest);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let mut result = String::with_capacity(limit);
    let mut previous = 0;
    for fact in facts {
        result.push_str(&text[previous..fact.start]);
        result.push_str(&bounded(&text[fact.clone()], low));
        previous = fact.end;
    }
    result.push_str(&text[previous..]);
    Some(result)
}

fn validate(entries: &[Entry]) -> bilibi_module_registry::Result<()> {
    for entry in entries {
        let template = entry.get::<Template>()?;
        let invalid = |reason: &str| {
            RegistryError::Contribution(format!(
                "template {} at {}:{}: {reason}",
                entry.id, entry.provenance.source, entry.provenance.line
            ))
        };
        let owner = entry.provenance.module.replace('/', ".");
        if entry.id != template.key
            || !template
                .key
                .strip_prefix(&owner)
                .and_then(|suffix| suffix.strip_prefix('.'))
                .is_some_and(identifier)
            || template.label.trim().is_empty()
            || !matches!(
                template.outputs,
                [Output::PlainText] | [Output::PortalNotice]
            )
        {
            return Err(invalid(
                "declare an owner-prefixed key, label and supported output",
            ));
        }
        let mut slots = BTreeSet::new();
        if template.slots.is_empty()
            || template.slots.iter().any(|s| {
                !identifier(s.id)
                    || !slots.insert(s.id)
                    || s.label.trim().is_empty()
                    || s.max_bytes == 0
            })
        {
            return Err(invalid(
                "slots need unique IDs, labels and positive byte limits",
            ));
        }
        let mut variables = BTreeSet::new();
        if template.variables.iter().any(|v| {
            !identifier(v.id)
                || !variables.insert(v.id)
                || v.label.trim().is_empty()
                || v.slots.is_empty()
                || v.slots.iter().any(|s| !slots.contains(s))
        }) {
            return Err(invalid("variables need unique IDs, labels and valid slots"));
        }
        let mut languages = BTreeSet::new();
        for default in template.defaults {
            if language(default.language).as_deref() != Some(default.language)
                || !languages.insert(default.language)
                || default.version.trim().is_empty()
                || default.resolve().slots.len() != default.slots.len()
            {
                return Err(invalid(
                    "packaged defaults need unique canonical language tags, versions and slots",
                ));
            }
            template
                .preview(&default.resolve().slots)
                .map_err(|problem| invalid(&problem.to_string()))?;
        }
        if !languages.contains(template.base_language) {
            return Err(invalid(
                "a packaged default is required for the base language",
            ));
        }
    }
    Ok(())
}

inventory::submit! { Consumer::new::<Template>(CATALOGUE, "base/templates", validate) }

inventory::submit! { Consumer::new::<Template>(PLATFORM_CATALOGUE, "base/templates", validate) }
