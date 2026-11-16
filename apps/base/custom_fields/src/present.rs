//! How an owner's pages read and edit the structured values: the labels of
//! one or several pick-list codes, the options an editor offers, and a record
//! field's names and picker hits with the reader's own access. Every owner
//! shows them the same way through these, so a page never re-implements the
//! retired mark, the held-value option or the access rule.
use crate::definitions::{CustomFields, Definition, Kind};
use bilibi_database::{Error, Result};
use bilibi_records::{Reference, Resolved};
use bilibi_tenancy::Scope;
use bilibi_ui::{Choice, PickerHit};
use serde_json::Value;
use tokio_postgres::GenericClient;

/// The codes an editor posted for several choices, as the value a write
/// takes: an inline editor joins its ticks with commas (a pick-list code never
/// holds one), a form posts one value per tick. Blank and repeated codes are
/// dropped; none clears the field (`null`).
pub fn codes_value<'a>(codes: impl IntoIterator<Item = &'a str>) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for code in codes.into_iter().flat_map(|text| text.split(',')) {
        let code = code.trim();
        if !code.is_empty() && !out.iter().any(|held| held == code) {
            out.push(Value::String(code.to_owned()));
        }
    }
    if out.is_empty() {
        Value::Null
    } else {
        Value::Array(out)
    }
}

/// The record id a `RecordPicker` raised (`kind:id`) for a record field, when
/// it is of the field's kind. The owner's write still checks it.
pub fn picked(definition: &Definition, raised: &str) -> Option<String> {
    let kind = definition.target.as_deref()?;
    raised
        .strip_prefix(kind)?
        .strip_prefix(':')
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

impl CustomFields {
    /// A choice or several-choices value as it reads: each code's label in
    /// the list's order of choosing, a retired one marked "(Retired)", an
    /// unknown code as itself, joined by ", ". Empty when none is held.
    pub async fn choice_text(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        definition: &Definition,
        value: Option<&Value>,
    ) -> Result<String> {
        let list = definition.picklist.as_deref().ok_or(Error::Internal)?;
        let codes: Vec<&str> = match definition.kind {
            Kind::Choices => crate::codes_of(value),
            _ => value.and_then(Value::as_str).into_iter().collect(),
        };
        let mut labels = Vec::with_capacity(codes.len());
        for code in codes.into_iter().filter(|code| !code.is_empty()) {
            labels.push(
                match self.picklists.label(client, scope, list, code).await? {
                    Some(label) if label.retired => format!("{} (Retired)", label.text),
                    Some(label) => label.text,
                    None => code.to_owned(),
                },
            );
        }
        Ok(labels.join(", "))
    }

    /// What a choice or several-choices editor offers: the list's active
    /// values, then each held code the list no longer offers, marked retired,
    /// so saving another field never drops it.
    pub async fn choice_options(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        definition: &Definition,
        held: &[&str],
    ) -> Result<Vec<Choice>> {
        let list = definition.picklist.as_deref().ok_or(Error::Internal)?;
        let mut options = self.picklists.choices(client, scope, list, None).await?;
        for code in held.iter().filter(|code| !code.is_empty()) {
            if options.iter().any(|option| option.value == *code) {
                continue;
            }
            let label = self
                .picklists
                .label(client, scope, list, code)
                .await?
                .map_or_else(|| (*code).to_owned(), |l| format!("{} (Retired)", l.text));
            options.push(Choice::new(label, (*code).to_owned()));
        }
        Ok(options)
    }

    /// Names the records `ids` of a record field with the reader's own
    /// access, in the order given: a record they cannot see, a removed one
    /// and one of an unmounted kind come back unnamed (show the pill as
    /// unavailable), and a merged one as its survivor. Takes pooled
    /// connections: call it from a page, never inside a write's preview.
    pub async fn references(
        &self,
        scope: &Scope,
        definition: &Definition,
        ids: &[&str],
    ) -> Result<Vec<Resolved>> {
        let kind = definition.target.as_deref().ok_or(Error::Internal)?;
        let references: Vec<Reference> = ids.iter().map(|id| Reference::new(kind, *id)).collect();
        self.records.resolve(scope, &references).await
    }

    /// The records of a record field's kind whose name matches `text`, as
    /// picker hits, among those the reader may select.
    pub async fn reference_hits(
        &self,
        scope: &Scope,
        definition: &Definition,
        text: &str,
    ) -> Result<Vec<PickerHit>> {
        let kind = definition.target.as_deref().ok_or(Error::Internal)?;
        let hits = self.records.search(scope, &[kind], text).await?;
        Ok(self.records.picker_hits(&hits))
    }
}
