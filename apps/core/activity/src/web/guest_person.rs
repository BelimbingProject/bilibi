//! The guest panel's "Create person" dialog: an email-address guest that
//! matches no Core person becomes one. The dialog asks the name; the address
//! is the guest's. The write is `core.activity.create_person_guest`.
use super::render;
use bilibi_database::Result;
use bilibi_ui::{Alert, Button, ButtonVariant, Field, Form, Html, Modal, ModalWidth, StatusKind};

/// The open dialog: which guest, their address, the name typed and what the
/// last attempt refused.
pub(super) struct PersonDialog {
    pub guest: i64,
    pub email: String,
    pub name: String,
    pub errors: Vec<String>,
}

pub(super) fn dialog(dialog: &PersonDialog) -> Result<Html> {
    let mut body = Html::EMPTY;
    if !dialog.errors.is_empty() {
        body.push(&render(Alert {
            kind: StatusKind::Error,
            id: "activity-person-errors",
            class: "mb-3",
            body: Html::text(&dialog.errors.join(" ")),
        })?);
    }
    body.push(&render(
        Field::in_form("person", "name")
            .id("activity-person-name")
            .label("Name")
            .required()
            .value(&dialog.name)
            .attr("maxlength", "200")
            .hint(format!("{} becomes their email address.", dialog.email)),
    )?);
    let mut actions = render(Button {
        id: "activity-person-cancel",
        button_type: "button",
        event: "cancel_person",
        body: Html::text("Cancel"),
        ..Button::default()
    })?;
    actions.push(&render(Button {
        id: "activity-person-save",
        button_type: "submit",
        variant: ButtonVariant::Primary,
        working: "Creating…",
        body: Html::text("Create person"),
        ..Button::default()
    })?);
    let form = render(Form {
        id: "activity-person-form",
        event: "create_person",
        change: false,
        body,
        actions,
        ..Form::default()
    })?;
    render(Modal {
        id: "activity-person-dialog",
        title: "Create a person",
        cancel_event: "cancel_person",
        width: ModalWidth::Wide,
        description: "Adds them to People and puts them on this guest list in place of the address. Their answer and invitation stay.",
        body: form,
        ..Modal::default()
    })
}
