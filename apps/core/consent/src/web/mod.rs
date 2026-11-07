//! The consent panel on the Core person page and the suppressions page.
//! Reads use the `Consent` facade, which rechecks capabilities; every write
//! calls an operation contract through `PageContracts` (declared in
//! `PageWrites`). No Bilimbi counterpart; the shapes follow Core Person's
//! list and Core Company's record panels.
mod panel;
mod suppressions;

use crate::{Channel, Consent, Reason, Suppression, WriteError, LIFT_REASON_MAX};
use askama::Template;
use bilibi_agent_api::page::{Call, PageContracts};
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayContext, Format, Precision};
use bilibi_principal_directory::{PrincipalDirectory, PrincipalKind, PrincipalRef, Query};
use bilibi_records::Records;
use bilibi_tenancy::Scope;
use bilibi_ui::{page::*, Field, Form, Html, InputKind};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

pub const SERVICE: &str = "core/consent.pages";

pub struct Pages {
    pub consent: Arc<Consent>,
    pub records: Arc<Records>,
    pub contracts: Arc<PageContracts>,
    pub directory: Arc<PrincipalDirectory>,
    pub display: Arc<bilibi_datetime::DisplayPolicy>,
    pub locales: Arc<bilibi_locale::Locales>,
    pub db: Db,
}
impl Pages {
    fn shared(&self) -> Self {
        Self {
            consent: self.consent.clone(),
            records: self.records.clone(),
            contracts: self.contracts.clone(),
            directory: self.directory.clone(),
            display: self.display.clone(),
            locales: self.locales.clone(),
            db: self.db.clone(),
        }
    }
    fn call<'a>(
        &self,
        scope: &'a Scope,
        credential: &'a str,
        event: &'a str,
        contract: &'static str,
    ) -> Call<'a> {
        Call {
            scope,
            credential,
            service: SERVICE,
            event,
            contract,
        }
    }
    /// The reader's display context: their clock mode and their company's
    /// time zone.
    async fn display(&self, scope: &Scope) -> Result<DisplayContext> {
        let client = self.db.get().await?;
        let locale = self.locales.resolve(&**client, scope, None).await?.locale;
        self.display.display(&**client, scope, None, locale).await
    }
    /// Account names for the ids given, in one directory read.
    async fn names(&self, scope: &Scope, users: &[i64]) -> Result<HashMap<i64, String>> {
        let principals: Vec<PrincipalRef> = users
            .iter()
            .filter_map(|id| PrincipalRef::new(PrincipalKind::User, *id).ok())
            .collect();
        if principals.is_empty() {
            return Ok(HashMap::new());
        }
        let client = self.db.get().await?;
        let ranked = self
            .directory
            .rank(&**client, scope, &principals, &Query::default())
            .await
            .map_err(|_| Error::Internal)?;
        Ok(principals
            .iter()
            .map(|p| (p.id(), ranked.label(*p)))
            .collect())
    }
}

/// An instant as text in the reader's display mode.
pub(crate) fn when(display: &DisplayContext, at: jiff::Timestamp) -> String {
    display.render(at, Format::DateTime, Precision::Minute).text
}

pub(crate) fn render(template: impl Template) -> Result<Html> {
    Html::render(&template).map_err(|_| Error::Internal)
}

pub(crate) fn event(value: &Value) -> Result<&str> {
    value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid)
}

/// What a suppression is about: its address, or the person.
pub(crate) fn subject(suppression: &Suppression) -> String {
    match (&suppression.contact.address, suppression.contact.person) {
        (Some(address), _) => address.clone(),
        (None, Some(person)) => format!("Person #{}", person.get()),
        (None, None) => String::new(),
    }
}

/// What a refused contract call says on the page. `Unauthorized` and a
/// step-up request go back to the host unchanged.
pub(crate) enum Refusal {
    Fields(std::collections::BTreeMap<String, Vec<String>>),
    Message(String),
}
pub(crate) fn refusal(
    error: bilibi_agent_api::Error,
    forbidden: &str,
    failed: &str,
) -> Result<Refusal> {
    use bilibi_agent_api::Error as E;
    Ok(match error {
        E::Backend(error @ (Error::Unauthorized | Error::StepUpRequired { .. })) => {
            return Err(error)
        }
        E::Forbidden | E::Backend(Error::Forbidden) => Refusal::Message(forbidden.into()),
        E::Backend(Error::Conflict) | E::PlanRejected(_) => Refusal::Message(
            "This changed since the page loaded. Reload before trying again.".into(),
        ),
        E::NotFound => Refusal::Message(WriteError::NotFound.to_string()),
        E::InvalidInput(fields) if fields.contains_key("lifted") => {
            Refusal::Message(WriteError::Lifted.to_string())
        }
        E::InvalidInput(fields) if fields.contains_key("company_id") => {
            Refusal::Message(WriteError::CompanyArchived.to_string())
        }
        E::InvalidInput(fields) => Refusal::Fields(fields),
        _ => Refusal::Message(failed.into()),
    })
}

/// The lift confirmation: the suppression, the reason typed so far and its
/// error. The reason form reports each change, so Lift carries it.
pub(crate) struct LiftDraft {
    pub id: i64,
    pub subject: String,
    /// The channel as a sentence says it: `email`, `SMS`.
    pub channel: &'static str,
    pub reason: Reason,
    pub text: String,
    pub error: Option<String>,
}
impl LiftDraft {
    /// `subject` names who it is about in the confirmation and the outcome.
    pub fn new(suppression: &Suppression, subject: String) -> Self {
        Self {
            id: suppression.id,
            subject,
            channel: match suppression.channel {
                Channel::Sms => "SMS",
                other => other.code(),
            },
            reason: suppression.reason,
            text: String::new(),
            error: None,
        }
    }
    /// The `lift_reason` event's text.
    pub fn accept(&mut self, value: &Value) {
        self.text = value["reason"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(LIFT_REASON_MAX + 1)
            .collect();
        self.error = None;
    }
    /// The reason to submit, or `None` with the error shown in the dialog.
    pub fn ready(&mut self) -> Option<String> {
        let text = self.text.trim();
        if text.is_empty() {
            self.error = Some("Say why contact may resume.".into());
            return None;
        }
        if text.chars().count() > LIFT_REASON_MAX {
            self.error = Some("Keep the reason to 500 characters.".into());
            return None;
        }
        Some(text.to_owned())
    }
    pub fn render(&self, id: &str) -> Result<Html> {
        let field_id = format!("{id}-reason");
        let mut field = Field::named("reason")
            .id(&field_id)
            .label("Reason")
            .kind(InputKind::Textarea)
            .value(&self.text)
            .required()
            .attr("maxlength", "500")
            .attr("data-debounce", "300")
            .wrapper_class("mb-0");
        if let Some(error) = &self.error {
            field.errors.push(error.clone());
        }
        let body = render(Form {
            id: &format!("{id}-form"),
            event: "lift_reason",
            change: true,
            body: render(field)?,
            ..Form::default()
        })?;
        render(LiftDialog {
            id,
            consequence: &format!(
                "{} may be contacted again on {}.",
                self.subject, self.channel
            ),
            detail: match self.reason {
                Reason::Unsubscribe => {
                    "The unsubscribe stops applying; consent decisions still do."
                }
                Reason::HardBounce | Reason::Complaint => {
                    "Elective messages may go to it again when a current decision allows them."
                }
            },
            body,
        })
    }
}

#[derive(Template)]
#[template(path = "consent_lift.html")]
struct LiftDialog<'a> {
    id: &'a str,
    consequence: &'a str,
    detail: &'a str,
    body: Html,
}

/// Calls `core.consent.lift` for the confirmed draft; the message says how it
/// went.
pub(crate) async fn lift(
    pages: &Pages,
    scope: &Scope,
    credential: &str,
    draft: &LiftDraft,
    reason: &str,
) -> Result<std::result::Result<String, String>> {
    let call = pages.call(scope, credential, "confirm_lift", "core.consent.lift");
    match pages
        .contracts
        .submit(
            &call,
            serde_json::json!({"suppression_id": draft.id, "reason": reason}),
        )
        .await
    {
        Ok(_) => Ok(Ok(format!(
            "{} may be contacted on {} again.",
            draft.subject, draft.channel
        ))),
        Err(error) => Ok(Err(
            match refusal(
                error,
                "You do not have permission to lift suppressions.",
                "The suppression was not lifted. Try again.",
            )? {
                Refusal::Message(message) => message,
                Refusal::Fields(fields) => fields
                    .values()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
            },
        )),
    }
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        match operation {
            "suppressions" => Some("Suppressions"),
            _ => None,
        }
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let mut state = self.state(&request)?;
            let body = state
                .update(LiveRequest {
                    scope,
                    credential: "",
                    input: LiveInput::Refresh,
                })
                .await?;
            Ok(PageBody {
                html: body.html,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move { Ok(Some(self.state(&request)?)) })
    }
}
impl Pages {
    fn state(&self, request: &PageRequest<'_, Scope>) -> Result<Box<dyn LiveState<Scope, Error>>> {
        Ok(match request.operation {
            "suppressions" => Box::new(suppressions::Index::new(
                self.shared(),
                request.query,
                request.framed,
            )?),
            "person" => Box::new(panel::Panel::new(self.shared(), request)?),
            _ => return Err(Error::NotFound),
        })
    }
}
