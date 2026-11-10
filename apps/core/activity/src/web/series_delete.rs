//! The Delete dialog of an occurrence of a repeating series: which ones to
//! delete. The record page and the Activities list both draw it, and both
//! call `core.activity.delete_series` (or, for one occurrence, the plain
//! `core.activity.delete`) with the same refusals.
use super::{record::dialog_errors, render};
use bilibi_database::Result;
use bilibi_ui::{choices, Button, ButtonVariant, Field, Form, Html, InputKind, Modal, ModalWidth};

/// The words after a series delete: how many occurrences went.
pub(crate) fn deleted(count: usize) -> String {
    match count {
        1 => "Activity deleted.".to_owned(),
        n => format!("{n} occurrences deleted."),
    }
}

/// The occurrence a Delete series dialog is open for, as it was when the
/// dialog opened: the delete names that `version`, so the contract refuses
/// it when someone changed the occurrence while the dialog was up.
pub(crate) struct Open {
    pub id: i64,
    pub title: String,
    pub version: i64,
    /// The last refusal.
    pub errors: Vec<String>,
}

/// A refusal as the dialog says it: a stale version means the occurrence
/// changed while the dialog was open, and opening it again reads the new one.
pub(crate) fn refused(message: String) -> String {
    if message == super::STALE {
        "The activity changed while this dialog was open. Close it and open it again to delete."
            .into()
    } else {
        message
    }
}

/// Delete on an occurrence of a series: which ones. `offer_one` adds "Only
/// this occurrence" (the record page; a list row has its own Delete).
pub(crate) fn dialog(title: &str, errors: &[String], offer_one: bool) -> Result<Html> {
    let mut body = dialog_errors("activity-delete-errors", errors)?;
    let mut options = Vec::new();
    if offer_one {
        options.push(("Only this occurrence", "one"));
    }
    options.push(("This occurrence and the later ones", "following"));
    options.push(("Every occurrence, held and completed ones too", "all"));
    body.push(&render(
        Field::in_form("delete", "apply")
            .id("activity-delete-apply")
            .label("Delete")
            .kind(InputKind::Select)
            .value(if offer_one { "one" } else { "following" })
            .options(choices(&options)),
    )?);
    let mut actions = render(Button {
        id: "activity-delete-cancel",
        button_type: "button",
        event: "cancel_delete_series",
        body: Html::text("Cancel"),
        ..Button::default()
    })?;
    actions.push(&render(Button {
        id: "activity-delete-confirm",
        button_type: "submit",
        variant: ButtonVariant::Danger,
        working: "Deleting…",
        body: Html::text("Delete"),
        ..Button::default()
    })?);
    let form = render(Form {
        id: "activity-delete-form",
        event: "delete_series",
        change: false,
        body,
        actions,
        ..Form::default()
    })?;
    render(Modal {
        id: "activity-delete-dialog",
        title: &format!("Delete “{title}”"),
        cancel_event: "cancel_delete_series",
        width: ModalWidth::Narrow,
        description: "This is one occurrence of a repeating series. Deleted occurrences disappear from every list and record page.",
        body: form,
        ..Modal::default()
    })
}
