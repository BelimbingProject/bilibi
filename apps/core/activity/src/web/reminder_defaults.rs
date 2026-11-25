//! Personal reminder defaults on the Calendar tab of My settings.
use super::{refusal, reminder_choices, render, sentences, Pages, Refusal};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{params, Button, ButtonVariant, Card, Choice, Combobox, Form, Html, StatusKind};
use serde_json::{json, Value};

pub(super) async fn settings(pages: &Pages, scope: &Scope) -> Result<Html> {
    if scope.impersonator().is_some() || !pages.activities.may_create(scope).await? {
        return Ok(Html::EMPTY);
    }
    let defaults = pages.activities.reminder_defaults(scope).await?;
    let minutes = defaults.minutes.as_deref();
    let mut body = render(
        Combobox::new("activities-defaults-reminders", "defaults[reminders]")
            .label("Remind me")
            .value(reminder_choices::code(minutes))
            .options(
                reminder_choices::options(minutes, "Organisation default")
                    .into_iter()
                    .map(|(label, code)| Choice::new(label, code))
                    .collect(),
            ),
    )?;
    body.push(&render(
        Combobox::new("activities-defaults-email", "defaults[reminder_email]")
            .label("Email reminders")
            .value(reminder_choices::email_code(defaults.email))
            .options(
                reminder_choices::EMAIL
                    .iter()
                    .map(|&(label, code)| {
                        Choice::new(
                            if code == "default" {
                                "Organisation default"
                            } else {
                                label
                            },
                            code,
                        )
                    })
                    .collect(),
            ),
    )?);
    let actions = render(Button {
        id: "activities-defaults-save",
        button_type: "submit",
        variant: ButtonVariant::Primary,
        working: "Saving…",
        body: Html::text("Save reminder defaults"),
        ..Button::default()
    })?;
    render(Card {
        id: "activities-defaults",
        title: "Reminders for new activities",
        body: render(Form {
            id: "activities-defaults-form",
            event: "save_reminder_defaults",
            body,
            actions,
            ..Form::default()
        })?,
        ..Card::default()
    })
}

pub(super) async fn save(
    pages: &Pages,
    scope: &Scope,
    credential: &str,
    value: &Value,
) -> Result<(StatusKind, String)> {
    let form = params::form_values_checked(value, "defaults", params::FIELD_LIMIT)
        .map_err(Error::refused)?;
    let email = form
        .get("reminder_email")
        .and_then(|v| reminder_choices::parse_email(v))
        .ok_or(Error::Invalid)?;
    let minutes = form
        .get("reminders")
        .and_then(|v| reminder_choices::parse(v))
        .ok_or(Error::Invalid)?;
    let mut input = json!({"reminder_email":email});
    if let Some(minutes) = minutes {
        input["reminder_minutes"] = json!(minutes);
    }
    let call = pages.call(
        scope,
        credential,
        "save_reminder_defaults",
        "core.activity.set_reminder_defaults",
    );
    match pages.contracts.submit(&call, input).await {
        Ok(_) => Ok((StatusKind::Success, "Reminder defaults saved.".into())),
        Err(error) => {
            let message = match refusal(
                error,
                "You do not have permission to change reminder defaults.",
                "The defaults were not saved. Try again.",
            )? {
                Refusal::Message(message) => message,
                Refusal::Fields(fields) => sentences(&fields),
            };
            Ok((StatusKind::Error, message))
        }
    }
}
