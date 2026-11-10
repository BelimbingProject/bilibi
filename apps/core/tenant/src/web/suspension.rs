//! The Suspend and Reactivate row actions: the reason dialog and the three
//! live events that open it, edit its reason and confirm it.
use super::{contract_error, render, TenantsPage, MOVE_DENIED};
use crate::{PAGES, REACTIVATE, SUSPEND};
use askama::Template;
use bilibi_agent_api::{page::Call, Error as ContractError};
use bilibi_database::{Error, Result};
use bilibi_tenancy::{Move, Scope, TenantStatus};
use bilibi_ui::{params, Field, Form, Html, InputKind, StatusKind};

/// The Suspend or Reactivate dialog while it is open: whom it is about, the
/// reason typed so far and its error. The reason form reports each change, so
/// the confirm carries it.
pub(super) struct MoveDraft {
    id: i64,
    name: String,
    change: Move,
    reason: String,
    error: Option<String>,
}
impl MoveDraft {
    fn event(&self) -> &'static str {
        match self.change {
            Move::Suspend => "confirm_suspend",
            Move::Reactivate => "confirm_reactivate",
        }
    }
    fn contract(&self) -> &'static str {
        match self.change {
            Move::Suspend => SUSPEND,
            Move::Reactivate => REACTIVATE,
        }
    }
    /// The reason to submit, or `None` with the error shown in the dialog.
    fn ready(&mut self) -> Option<String> {
        let text = self.reason.trim();
        if text.is_empty() {
            self.error = Some(match self.change {
                Move::Suspend => "Say why this tenant is suspended.".into(),
                Move::Reactivate => "Say why this tenant is reactivated.".into(),
            });
            return None;
        }
        if text.chars().count() > bilibi_tenancy::REASON_MAX {
            self.error = Some("Keep the reason to 500 characters.".into());
            return None;
        }
        Some(text.to_owned())
    }
    pub(super) fn render(&self) -> Result<Html> {
        let mut field = Field::named("reason")
            .id("tenant-move-reason")
            .label("Reason")
            .hint("Recorded with this change in the audit trail.")
            .kind(InputKind::Textarea)
            .value(&self.reason)
            .required()
            .attr("maxlength", "500")
            .attr("data-debounce", "300")
            .wrapper_class("mb-0");
        if let Some(error) = &self.error {
            field.errors.push(error.clone());
        }
        let body = render(Form {
            id: "tenant-move-form",
            event: "move_reason",
            change: true,
            body: render(field)?,
            ..Form::default()
        })?;
        let (consequence, detail, confirm, working) = match self.change {
            Move::Suspend => (
                format!("{} will be suspended.", self.name),
                "Everyone signed in to it is signed out now, and nobody can sign in until you reactivate it. Its data is kept.",
                "Suspend",
                "Suspending…",
            ),
            Move::Reactivate => (
                format!("{} will be reactivated.", self.name),
                "People can sign in again. Jobs cancelled while it was suspended do not restart.",
                "Reactivate",
                "Reactivating…",
            ),
        };
        render(MoveDialog {
            consequence: &consequence,
            detail,
            confirm,
            working,
            confirm_event: self.event(),
            body,
        })
    }
}

#[derive(Template)]
#[template(path = "tenant_move.html")]
struct MoveDialog<'a> {
    consequence: &'a str,
    detail: &'a str,
    confirm: &'a str,
    working: &'a str,
    confirm_event: &'a str,
    body: Html,
}

impl TenantsPage {
    /// `request_suspend` or `request_reactivate`: open the dialog for a tenant.
    pub(super) async fn request_move(
        &mut self,
        scope: &Scope,
        change: Move,
        id: Option<&str>,
    ) -> Result<bool> {
        let id = params::positive_id(id).ok_or(Error::Invalid)?;
        if !self.may(scope, change.capability()).await? {
            self.flash(StatusKind::Error, MOVE_DENIED);
            return Ok(false);
        }
        // The name and standing come from the directory, never from the
        // event: a stale or forged row cannot name another tenant.
        let result = self
            .pages
            .contracts
            .read(scope, crate::GET, &serde_json::json!({"tenant_id":id}))
            .await;
        let tenant = match result {
            Ok(value) => value["result"].clone(),
            Err(ContractError::NotFound) => {
                self.flash(StatusKind::Error, "That tenant no longer exists.");
                return Ok(true);
            }
            Err(error) => return Err(contract_error(error)),
        };
        let status = tenant["status"].as_str().and_then(TenantStatus::parse);
        if status.and_then(|status| status.after(change)).is_none()
            || (change == Move::Suspend && tenant["is_platform_operator"] == true)
        {
            self.flash(
                StatusKind::Error,
                "This tenant's status changed. Review it and try again.",
            );
            return Ok(true);
        }
        self.moving = Some(MoveDraft {
            id,
            name: tenant["name"].as_str().ok_or(Error::Internal)?.into(),
            change,
            reason: String::new(),
            error: None,
        });
        Ok(false)
    }

    pub(super) fn move_reason(&mut self, reason: Option<&str>) {
        if let Some(draft) = &mut self.moving {
            draft.reason = reason
                .unwrap_or_default()
                .chars()
                .take(bilibi_tenancy::REASON_MAX + 1)
                .collect();
            draft.error = None;
        }
    }

    pub(super) async fn confirm_move(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
    ) -> Result<bool> {
        let Some(draft) = &mut self.moving else {
            return Ok(false);
        };
        if draft.event() != event {
            return Err(Error::Invalid);
        }
        let Some(reason) = draft.ready() else {
            return Ok(false);
        };
        let call = Call {
            scope,
            credential,
            service: PAGES,
            event: draft.event(),
            contract: draft.contract(),
        };
        let input = serde_json::json!({"tenant_id":draft.id,"reason":reason});
        let done = match draft.change {
            Move::Suspend => format!("{} is suspended.", draft.name),
            Move::Reactivate => format!("{} is active again.", draft.name),
        };
        match self.pages.contracts.submit(&call, input).await {
            Ok(_) => {
                self.moving = None;
                self.flash(StatusKind::Success, done);
                Ok(true)
            }
            Err(ContractError::InvalidInput(mut fields)) => {
                if let Some(errors) = fields.remove("reason") {
                    draft.error = errors
                        .first()
                        .map(|error| bilibi_ui::error_sentence("Reason", error));
                    return Ok(false);
                }
                let message = fields
                    .remove("tenant_id")
                    .and_then(|errors| errors.into_iter().next())
                    .map(|error| format!("{} {error}.", draft.name))
                    .unwrap_or_else(|| "The tenant's status was not changed.".into());
                self.moving = None;
                self.flash(StatusKind::Error, message);
                Ok(true)
            }
            Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                self.moving = None;
                self.flash(StatusKind::Error, MOVE_DENIED);
                Ok(false)
            }
            Err(ContractError::NotFound) => {
                self.moving = None;
                self.flash(StatusKind::Error, "That tenant no longer exists.");
                Ok(true)
            }
            Err(error) => Err(contract_error(error)),
        }
    }
}
