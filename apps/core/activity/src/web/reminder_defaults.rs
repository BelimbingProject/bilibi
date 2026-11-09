//! The "Reminder defaults" dialog on the Activities list: the offsets and
//! email choice the reader's new activities start with, between the
//! organisation's setting and each activity's own. The write is
//! `core.activity.set_reminder_defaults`.
use super::{reminder_choices, render};
use bilibi_database::Result;
use bilibi_ui::{
    choices, Alert, Button, ButtonVariant, Field, Form, Html, InputKind, Modal, ModalWidth,
    StatusKind,
};
use serde_json::{json, Value};

/// The open dialog: the two choices as codes, and what the last save refused.
pub(super) struct DefaultsDialog {
    pub minutes: String,
    pub email: String,
    pub errors: Vec<String>,
}
impl DefaultsDialog {
    pub fn new(defaults: &crate::ReminderDefaults) -> Self {
        Self {
            minutes: reminder_choices::code(defaults.minutes.as_deref()),
            email: reminder_choices::email_code(defaults.email).into(),
            errors: Vec::new(),
        }
    }
    /// Takes the posted choices, refusing a code the dialog never offered.
    pub fn read(&mut self, form: &std::collections::HashMap<String, String>) -> bool {
        if let Some(minutes) = form.get("reminders") {
            if reminder_choices::parse(minutes).is_none() {
                return false;
            }
            self.minutes.clone_from(minutes);
        }
        if let Some(email) = form.get("reminder_email") {
            if reminder_choices::parse_email(email).is_none() {
                return false;
            }
            self.email.clone_from(email);
        }
        true
    }
    /// The contract input: each choice a value, or null for the default.
    pub fn input(&self) -> Value {
        let mut input = json!({
            "reminder_email": reminder_choices::parse_email(&self.email).flatten(),
        });
        if let Some(minutes) = reminder_choices::parse(&self.minutes).flatten() {
            input["reminder_minutes"] = json!(minutes);
        }
        input
    }
}

pub(super) fn dialog(dialog: &DefaultsDialog) -> Result<Html> {
    let current = reminder_choices::parse(&dialog.minutes).flatten();
    let offered = reminder_choices::options(current.as_deref(), "Organisation default");
    let mut body = Html::EMPTY;
    if !dialog.errors.is_empty() {
        body.push(&render(Alert {
            kind: StatusKind::Error,
            id: "activities-defaults-errors",
            class: "mb-3",
            body: Html::text(&dialog.errors.join(" ")),
        })?);
    }
    body.push(&render(
        Field::in_form("defaults", "reminders")
            .id("activities-defaults-reminders")
            .label("Remind me")
            .kind(InputKind::Select)
            .value(&dialog.minutes)
            .options(choices(
                &offered
                    .iter()
                    .map(|(label, code)| (label.as_str(), code.as_str()))
                    .collect::<Vec<_>>(),
            ))
            .hint("When a task, call or meeting you create has no reminder of its own."),
    )?);
    let email: Vec<(&str, &str)> = reminder_choices::EMAIL
        .iter()
        .map(|(label, code)| {
            if *code == "default" {
                ("Organisation default", *code)
            } else {
                (*label, *code)
            }
        })
        .collect();
    body.push(&render(
        Field::in_form("defaults", "reminder_email")
            .id("activities-defaults-email")
            .label("Email the reminders")
            .kind(InputKind::Select)
            .value(&dialog.email)
            .options(choices(&email))
            .hint("Emails the assignee, and for a call or meeting guests without an account who were sent the invitation."),
    )?);
    let mut actions = render(Button {
        id: "activities-defaults-cancel",
        button_type: "button",
        event: "cancel_defaults",
        body: Html::text("Cancel"),
        ..Button::default()
    })?;
    actions.push(&render(Button {
        id: "activities-defaults-save",
        button_type: "submit",
        variant: ButtonVariant::Primary,
        working: "Saving…",
        body: Html::text("Save"),
        ..Button::default()
    })?);
    let form = render(Form {
        id: "activities-defaults-form",
        event: "save_reminder_defaults",
        change: false,
        body,
        actions,
        ..Form::default()
    })?;
    render(Modal {
        id: "activities-defaults-dialog",
        title: "Reminder defaults",
        cancel_event: "cancel_defaults",
        width: ModalWidth::Wide,
        description: "Your choices apply to activities you create from now on. Activities you already made keep theirs.",
        body: form,
        ..Modal::default()
    })
}
