//! The record page's Customer sharing card (issue 524): who on the portal
//! sees this document, a picker that shares it with a contact (one customer)
//! or a company (every business customer who signs in for it), and Stop
//! sharing, which asks first. Both writes call their contracts
//! (`core.document.customer_share`, `core.document.customer_unshare`); the
//! card shows only to someone allowed to share the document, and only for a
//! document that keeps a file.
use super::{render, Clock, Pages, Refusal};
use crate::{CustomerShare, Document, Recipient, Source};
use bilibi_database::{Error, Result};
use bilibi_records::Hit;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    params, Button, ButtonVariant, Card, ConfirmDialog, Fact, Facts, Field, Form, Html, InputKind,
    RecordPicker, SectionHeading, StatusKind,
};
use serde_json::{json, Value};

/// The kinds a customer share names.
const KINDS: [&str; 2] = ["person", "company"];

/// What an event did to the card.
pub(super) enum Outcome {
    /// Not one of the card's events.
    NotMine,
    /// Handled, with the page flash to show, if any.
    Handled(Option<(StatusKind, String)>),
}

#[derive(Default)]
pub(super) struct Sharing {
    query: String,
    hits: Vec<Hit>,
    until: String,
    /// The share Stop sharing asks about.
    confirm: Option<i64>,
    error: Option<String>,
}

/// A refused contract call as sentences for the picker.
fn sentences(refusal: Refusal) -> String {
    match refusal {
        Refusal::Message(message) => message,
        Refusal::Fields(fields) => fields
            .into_iter()
            .flat_map(|(field, messages)| {
                let label = match field.as_str() {
                    "until" => "Available until",
                    "id" => "Document",
                    _ => "",
                };
                messages
                    .into_iter()
                    .map(move |message| bilibi_ui::error_sentence(label, &message))
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

impl Sharing {
    pub(super) async fn handle(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        credential: &str,
        document: i64,
        name: &str,
        value: &Value,
    ) -> Result<Outcome> {
        match name {
            "customer_change" => {
                let form = params::form_values_checked(value, "customer", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.until = form
                    .get("until")
                    .map(|until| until.trim().chars().take(10).collect())
                    .unwrap_or_default();
                let query: String = form
                    .get("customer_search")
                    .cloned()
                    .unwrap_or_default()
                    .chars()
                    .take(100)
                    .collect();
                if query != self.query {
                    self.query = query;
                    self.error = None;
                    self.hits = if self.query.trim().is_empty() {
                        Vec::new()
                    } else {
                        pages.records.search(scope, &KINDS, &self.query).await?
                    };
                }
            }
            "clear_customer_pick" => {
                self.query.clear();
                self.hits.clear();
                self.error = None;
            }
            "customer_share" => {
                let (kind, id) = value["record"]
                    .as_str()
                    .and_then(|v| v.split_once(':'))
                    .ok_or(Error::Invalid)?;
                let mut input = json!({"id": document, "recipient": {"kind": kind, "id": id}});
                if !self.until.is_empty() {
                    input["until"] = json!(self.until);
                }
                match pages
                    .submit(
                        scope,
                        credential,
                        "customer_share",
                        "core.document.customer_share",
                        input,
                        "The document was not shared. Try again.",
                    )
                    .await?
                {
                    Ok(_) => {
                        *self = Self::default();
                        return Ok(Outcome::Handled(Some((
                            StatusKind::Success,
                            "Shared. The customer sees it on the portal now.".into(),
                        ))));
                    }
                    Err(refusal) => self.error = Some(sentences(refusal)),
                }
            }
            "request_customer_unshare" => {
                let share = value["share"]
                    .as_str()
                    .and_then(|share| share.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                self.confirm = Some(share);
            }
            "cancel_customer_unshare" => self.confirm = None,
            "customer_unshare" => {
                let Some(share) = self.confirm.take() else {
                    return Ok(Outcome::Handled(None));
                };
                let flash = match pages
                    .submit(
                        scope,
                        credential,
                        "customer_unshare",
                        "core.document.customer_unshare",
                        json!({"id": document, "share": share}),
                        "Sharing was not stopped. Try again.",
                    )
                    .await?
                {
                    Ok(_) => (
                        StatusKind::Success,
                        "Sharing stopped. The customer no longer sees this document.".into(),
                    ),
                    Err(refusal) => (StatusKind::Error, refusal.text()),
                };
                return Ok(Outcome::Handled(Some(flash)));
            }
            _ => return Ok(Outcome::NotMine),
        }
        Ok(Outcome::Handled(None))
    }

    /// The card, or `None` when the reader may not share this document or
    /// it is a link.
    pub(super) async fn render(
        &self,
        pages: &Pages,
        scope: &Scope,
        clock: &Clock,
        document: &Document,
        framed: bool,
    ) -> Result<Option<Html>> {
        if document.source != Source::Internal {
            return Ok(None);
        }
        let Ok(shares) = pages.documents.customer_shares(scope, document.id).await else {
            return Ok(None);
        };
        let references: Vec<_> = shares.iter().map(|s| s.recipient.reference()).collect();
        let named = pages.records.resolve(scope, &references).await?;
        let mut body = render(SectionHeading {
            id: "document-customer-sharing-heading",
            title: "Customer sharing",
            description: "Customers see a shared document on the portal and download its latest file once it is scanned clean. Choose a contact for one customer, or a company for all its customer accounts.",
            ..SectionHeading::default()
        })?;
        let mut items: Vec<(&'static str, String, Html)> = Vec::with_capacity(shares.len());
        for (share, resolved) in shares.iter().zip(&named) {
            let mut value =
                render(resolved.pill(format!("document-customer-{}-record", share.id), framed))?;
            value.push(&Html::text(&ends(clock, share)));
            value.push(&render(Button {
                id: &format!("document-customer-{}-remove", share.id),
                button_type: "button",
                event: "request_customer_unshare",
                name: "share",
                value: &share.id.to_string(),
                variant: ButtonVariant::Danger,
                body: Html::text("Stop sharing"),
                ..Button::default()
            })?);
            let title = match share.recipient {
                Recipient::Person(_) => "Contact",
                Recipient::Company(_) => "Company (all its customers)",
            };
            items.push((title, format!("document-customer-{}", share.id), value));
        }
        body.push(&render(Facts {
            id: "document-customer-list",
            items: items
                .iter()
                .map(|(title, id, value)| Fact {
                    title,
                    id,
                    value: value.clone(),
                })
                .collect(),
        })?);
        let errors = self.error.iter().cloned().collect();
        let mut fields = render(RecordPicker::new(
            "document-customer",
            "customer",
            "customer",
            "Share with a customer",
            &self.query,
            &pages.records.picker_hits(&self.hits),
            None,
            ("customer_share", "clear_customer_pick"),
            errors,
        ))?;
        fields.push(&render(
            Field::in_form("customer", "until")
                .id("document-customer-until")
                .label("Available until")
                .kind(InputKind::Date)
                .value(&self.until)
                .hint("The last day (UTC) the customer sees it. Leave empty for no end; set it before you choose the customer."),
        )?);
        body.push(&render(Form {
            id: "document-customer-form",
            event: "customer_change",
            change: true,
            body: fields,
            ..Form::default()
        })?);
        let mut card = render(Card {
            id: "document-customer-sharing",
            class: "mt-6",
            role: "region",
            labelledby: "document-customer-sharing-heading",
            body,
            ..Card::default()
        })?;
        if self.confirm.is_some() {
            card.push(&render(ConfirmDialog {
                id: "document-customer-confirm",
                consequence: "The customer will no longer see this document.",
                detail: "It leaves their portal at once. A file they already downloaded stays with them.",
                confirm: "Stop sharing",
                working: "Stopping…",
                confirm_event: "customer_unshare",
                cancel_event: "cancel_customer_unshare",
            })?);
        }
        Ok(Some(card))
    }
}

/// When a share ends, as the access list says it.
fn ends(clock: &Clock, share: &CustomerShare) -> String {
    match share.expires_at {
        None => " · No end".into(),
        Some(until) if until <= jiff::Timestamp::now() => format!(" · Ended {}", clock.text(until)),
        Some(until) => format!(" · Until {}", clock.text(until)),
    }
}
