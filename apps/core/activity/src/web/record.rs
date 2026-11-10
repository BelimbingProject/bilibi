//! `/activities/{id}`: one activity, read first and edited in place, with
//! its guests (add, remove, answers, Send invites) and a warning when its
//! time overlaps another call or meeting of the same people. Close, Close
//! and create new, Delete and Send invites ask first; every write calls its
//! contract through `PageContracts`. An occurrence of a repeating series is
//! edited in place on its own (an exception); Change series reaches it and
//! the later ones, or all of them, and Delete asks which.
use super::{
    event, field_of,
    form::Dialog,
    guest_person::{self, PersonDialog},
    icon, label, refusal, reminder_choices, render, sentences, series_delete, status_badge,
    BrowserZone, Clock, Pages, Refusal, Region, FIELDS,
};
use crate::{Activity, Direction, GuestRow, Kind, Priority, Reference, Response, RECORD};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_records::{Hit, Resolved};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    choices, page::*, params, ActionLink, Alert, BackLink, Badge, BadgeKind, Button, ButtonVariant,
    Card, CommitStates, CommitStatus, ConfirmDialog, Fact, Facts, Field, FlashGroup, Form, Header,
    Html, Icon, InlineChoice, InlineEdit, InlineField, InlineLongText, InputKind, Modal,
    ModalWidth, PageVariant, RecordPicker, SectionHeading, StatusKind, NO_FLASH,
};
use serde_json::{json, Value};
use std::collections::HashMap;

const FORBIDDEN: &str = "You do not have permission to change this activity.";
const PRIVATE: &str = "People taking part";
const SHARED: &str = "Also readers of the related record";
/// Long text in the series dialog posts up to this many characters.
const LIMIT: usize = 10_000;

/// The Change series dialog: which occurrences, the values to give them,
/// and what the last save refused.
struct SeriesForm {
    values: HashMap<String, String>,
    errors: Vec<String>,
}
impl SeriesForm {
    fn new(a: &Activity, clock: &Clock) -> Self {
        let values = HashMap::from([
            ("apply".to_owned(), "following".to_owned()),
            ("title".to_owned(), a.title.clone()),
            ("starts_at".to_owned(), clock.local(a.starts_at)),
            ("ends_at".to_owned(), clock.local(a.ends_at)),
            ("due_at".to_owned(), clock.local(a.due_at)),
            (
                "location".to_owned(),
                a.location.clone().unwrap_or_default(),
            ),
            (
                "description".to_owned(),
                a.description.clone().unwrap_or_default(),
            ),
        ]);
        Self {
            values,
            errors: Vec::new(),
        }
    }
    fn value(&self, key: &str) -> &str {
        self.values.get(key).map_or("", String::as_str)
    }
    /// The patch: only the values that differ from this occurrence's.
    fn patch(&self, a: &Activity, clock: &Clock) -> std::result::Result<Value, Vec<String>> {
        let mut patch = json!({});
        let mut errors = Vec::new();
        let title = self.value("title").trim();
        if title != a.title {
            patch["title"] = json!(title);
        }
        let keys: &[&str] = match a.kind {
            Kind::Call | Kind::Meeting => &["starts_at", "ends_at"],
            Kind::Task => &["due_at"],
            Kind::Note => &[],
        };
        for key in keys {
            let stored = match *key {
                "starts_at" => a.starts_at,
                "ends_at" => a.ends_at,
                _ => a.due_at,
            };
            match clock.parse(self.value(key)) {
                Ok(at) if at != stored => {
                    patch[*key] = json!(at.map(|t| t.to_string()).unwrap_or_default())
                }
                Ok(_) => {}
                Err(()) => errors.push(bilibi_ui::error_sentence(
                    label(key),
                    "is not a date and time",
                )),
            }
        }
        for (key, stored) in [
            ("location", a.location.as_deref()),
            ("description", a.description.as_deref()),
        ] {
            if key == "location" && a.kind != Kind::Meeting {
                continue;
            }
            let value = self.value(key).trim();
            if value != stored.unwrap_or_default() {
                patch[key] = json!(value);
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        if patch.as_object().is_none_or(|p| p.is_empty()) {
            return Err(vec!["Change at least one value first.".to_owned()]);
        }
        Ok(patch)
    }
}

#[derive(Template)]
#[template(path = "activities_detail.html")]
struct View<'a> {
    id: &'a str,
    body: Html,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Close,
    CloseAndNew,
    Delete,
    SendInvites,
}

pub(crate) struct Detail {
    pages: Pages,
    id: i64,
    framed: bool,
    zone: BrowserZone,
    statuses: CommitStates,
    confirm: Option<Confirm>,
    dialog: Option<Dialog>,
    query: String,
    hits: Vec<Hit>,
    guest_query: String,
    guest_hits: Vec<Hit>,
    guest_email: String,
    /// The Change series dialog, when open.
    series_form: Option<SeriesForm>,
    /// The Create person dialog for an address guest, when open.
    person_dialog: Option<PersonDialog>,
    /// The Delete dialog of an occurrence (which ones to delete), when
    /// open, with what the last attempt refused.
    delete_dialog: Option<Vec<String>>,
    flash: Option<(StatusKind, String)>,
}

impl Detail {
    pub async fn new(pages: Pages, scope: &Scope, id: i64, framed: bool) -> Result<Self> {
        // The page opens only for an activity the reader may see.
        pages.activities.get(scope, id).await?;
        Ok(Self {
            pages,
            id,
            framed,
            zone: BrowserZone::new(),
            statuses: CommitStates::default(),
            confirm: None,
            dialog: None,
            query: String::new(),
            hits: Vec::new(),
            guest_query: String::new(),
            guest_hits: Vec::new(),
            guest_email: String::new(),
            series_form: None,
            person_dialog: None,
            delete_dialog: None,
            flash: None,
        })
    }

    /// Change series: the dialog's values to this occurrence and the later
    /// ones, or to all of them.
    async fn update_series(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &Value,
    ) -> Result<()> {
        let form = params::form_values_checked(value, "series", LIMIT).map_err(Error::refused)?;
        let Some(dialog) = &mut self.series_form else {
            return Ok(());
        };
        for (key, value) in form {
            dialog.values.insert(key, value);
        }
        let a = self.pages.activities.get(scope, self.id).await?;
        let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
        let patch = match dialog.patch(&a, &clock) {
            Ok(patch) => patch,
            Err(errors) => {
                dialog.errors = errors;
                return Ok(());
            }
        };
        let apply = match dialog.value("apply") {
            "all" => "all",
            _ => "following",
        };
        let input = json!({"id": self.id, "version": a.version, "apply": apply, "patch": patch});
        match self
            .submit(
                scope,
                credential,
                "update_series",
                "core.activity.update_series",
                input,
            )
            .await?
        {
            Ok(result) => {
                let count = result["result"]["updated"].as_array().map_or(0, Vec::len);
                self.series_form = None;
                self.flash = Some((
                    StatusKind::Success,
                    match count {
                        1 => "1 occurrence changed.".to_owned(),
                        n => format!("{n} occurrences changed."),
                    },
                ));
            }
            Err(refusal) => {
                if let Some(dialog) = &mut self.series_form {
                    dialog.errors = vec![Self::refused(refusal)];
                }
            }
        }
        Ok(())
    }

    /// Delete on an occurrence: only it, it and the later ones, or all.
    async fn delete_series(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &Value,
    ) -> Result<Option<LiveBody>> {
        if self.delete_dialog.is_none() {
            return Ok(None);
        }
        let form = params::form_values_checked(value, "delete", params::FIELD_LIMIT)
            .map_err(Error::refused)?;
        let a = self.pages.activities.get(scope, self.id).await?;
        let apply = form.get("apply").map_or("one", String::as_str);
        let outcome = match apply {
            "following" | "all" => self
                .submit(
                    scope,
                    credential,
                    "delete_series",
                    "core.activity.delete_series",
                    json!({"id": self.id, "version": a.version, "apply": apply}),
                )
                .await?
                .map(|result| result["result"]["deleted"].as_array().map_or(0, Vec::len)),
            _ => self
                .submit(
                    scope,
                    credential,
                    "delete_series",
                    "core.activity.delete",
                    json!({"id": self.id, "version": a.version}),
                )
                .await?
                .map(|_| 1),
        };
        match outcome {
            Ok(count) => Ok(Some(LiveBody {
                redirect: Some("/activities".into()),
                flash: Some((StatusKind::Success, series_delete::deleted(count))),
                ..LiveBody::default()
            })),
            Err(refusal) => {
                self.delete_dialog = Some(vec![Self::refused(refusal)]);
                Ok(None)
            }
        }
    }

    /// A refused contract call as one sentence for the flash.
    fn refused(refusal: Refusal) -> String {
        match refusal {
            Refusal::Fields(fields) => sentences(&fields),
            Refusal::Message(message) => message,
        }
    }

    /// Adds a guest (`pick_guest`: a person; `add_guest_email`: an address).
    async fn add_guest(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
        guest: Value,
    ) -> Result<()> {
        match self
            .submit(
                scope,
                credential,
                event,
                "core.activity.add_guest",
                json!({"id": self.id, "guest": guest}),
            )
            .await?
        {
            Ok(_) => {
                self.guest_query.clear();
                self.guest_hits.clear();
                self.guest_email.clear();
                self.flash = Some((StatusKind::Success, "Guest added.".into()));
            }
            Err(refusal) => self.flash = Some((StatusKind::Error, Self::refused(refusal))),
        }
        Ok(())
    }

    async fn submit(
        &self,
        scope: &Scope,
        credential: &str,
        event: &str,
        contract: &'static str,
        input: Value,
    ) -> Result<std::result::Result<Value, Refusal>> {
        let call = self.pages.call(scope, credential, event, contract);
        match self.pages.contracts.submit(&call, input).await {
            Ok(value) => Ok(Ok(value)),
            Err(error) => Ok(Err(refusal(
                error,
                FORBIDDEN,
                "The change was not saved. Try again.",
            )?)),
        }
    }

    /// One field's save: the patch for that field alone.
    async fn save_field(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let field = value["field"].as_str().ok_or(Error::Invalid)?;
        if !FIELDS.iter().any(|(key, _)| *key == field)
            || value["id"].as_str() != Some(&self.id.to_string())
        {
            return Err(Error::Invalid);
        }
        let submitted = value["value"].as_str().ok_or(Error::Invalid)?;
        let version = value["version"]
            .as_str()
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or(Error::Invalid)?;
        let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
        let mut patch = json!({});
        match field {
            "starts_at" | "ends_at" | "due_at" | "start_at" => match clock.parse(submitted) {
                Ok(time) => patch[field] = json!(time.map(|t| t.to_string()).unwrap_or_default()),
                Err(()) => {
                    self.statuses.put(
                        field,
                        CommitStatus::refusal(label(field), submitted, &["is not a date and time"]),
                    );
                    return Ok(());
                }
            },
            "shared" => patch["shared"] = json!(submitted == "shared"),
            "reminders" => match reminder_choices::parse(submitted) {
                Some(Some(minutes)) => patch["reminder_minutes"] = json!(minutes),
                Some(None) => patch["reminder_minutes_default"] = json!(true),
                None => return Err(Error::Invalid),
            },
            "reminder_email" => match reminder_choices::parse_email(submitted) {
                Some(email) => patch["reminder_email"] = json!(email),
                None => return Err(Error::Invalid),
            },
            "assignee" => {
                let user = if submitted == "me" {
                    scope.user()
                } else {
                    submitted.parse::<i64>().map_err(|_| Error::Invalid)?
                };
                patch["assignee_user_id"] = json!(user);
            }
            _ => patch[field] = json!(submitted),
        }
        let status = match self
            .submit(
                scope,
                credential,
                "save_field",
                "core.activity.update",
                json!({"id": self.id, "version": version, "patch": patch}),
            )
            .await?
        {
            Ok(_) => CommitStatus::Saved,
            Err(Refusal::Fields(fields)) if fields.keys().all(|path| field_of(path) == field) => {
                let reasons: Vec<&str> = fields.values().flatten().map(String::as_str).collect();
                CommitStatus::refusal(label(field), submitted, &reasons)
            }
            // A rule on another field (Held on a call that has not ended)
            // names that field.
            Err(Refusal::Fields(fields)) => CommitStatus::Error(sentences(&fields)),
            Err(Refusal::Message(message)) => CommitStatus::Error(message),
        };
        self.statuses.put(field, status);
        Ok(())
    }

    /// Points the activity at another record (`pick_related`), or at none
    /// (`clear_related`).
    async fn save_related(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
        related: Option<Reference>,
    ) -> Result<()> {
        let activity = self.pages.activities.get(scope, self.id).await?;
        let related = related.map_or(
            json!({"kind": "", "id": ""}),
            |r| json!({"kind": r.kind, "id": r.id}),
        );
        let status = match self
            .submit(
                scope,
                credential,
                event,
                "core.activity.update",
                json!({"id": self.id, "version": activity.version, "patch": {"related": related}}),
            )
            .await?
        {
            Ok(_) => {
                self.query.clear();
                self.hits.clear();
                CommitStatus::Saved
            }
            Err(Refusal::Fields(fields)) => CommitStatus::Error(
                fields
                    .values()
                    .flatten()
                    .map(|m| bilibi_ui::error_sentence("Related to", m))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            Err(Refusal::Message(message)) => CommitStatus::Error(message),
        };
        self.statuses.put("related", status);
        Ok(())
    }

    async fn handle(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &Value,
    ) -> Result<Option<LiveBody>> {
        let name = event(value)?;
        if let Some(dialog) = &mut self.dialog {
            if dialog.event(&self.pages, scope, name, value).await? {
                return Ok(None);
            }
        }
        match name {
            "browser_timezone" => self.zone.accept(value),
            "save_field" => self.save_field(scope, credential, value).await?,
            "respond" => {
                let form = params::form_values_checked(value, "answer", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let guest = form
                    .get("guest")
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                let response = form
                    .get("response")
                    .and_then(|v| Response::parse(v))
                    .ok_or(Error::Invalid)?;
                let key = format!("response-{guest}");
                let status = match self
                    .submit(
                        scope,
                        credential,
                        "respond",
                        "core.activity.respond",
                        json!({"id": self.id, "guest_id": guest, "response": response.code()}),
                    )
                    .await?
                {
                    Ok(_) => CommitStatus::Saved,
                    Err(refusal) => CommitStatus::Error(Self::refused(refusal)),
                };
                self.statuses.put(&key, status);
            }
            "guest_change" => {
                let form = params::form_values_checked(value, "guest", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.guest_email = form
                    .get("email")
                    .cloned()
                    .unwrap_or_default()
                    .chars()
                    .take(254)
                    .collect();
                let query: String = form
                    .get("person_search")
                    .cloned()
                    .unwrap_or_default()
                    .chars()
                    .take(100)
                    .collect();
                if query != self.guest_query {
                    self.guest_query = query;
                    self.guest_hits = if self.guest_query.trim().is_empty() {
                        Vec::new()
                    } else {
                        self.pages
                            .records
                            .search(scope, &["person"], &self.guest_query)
                            .await?
                    };
                }
            }
            "pick_guest" => {
                let person = value["record"]
                    .as_str()
                    .and_then(|v| v.strip_prefix("person:"))
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                self.add_guest(scope, credential, name, json!({"person_id": person}))
                    .await?;
            }
            "clear_guest" => {
                self.guest_query.clear();
                self.guest_hits.clear();
            }
            "add_guest_email" => {
                let form = params::form_values_checked(value, "guest", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let email = form.get("email").cloned().unwrap_or_default();
                if email.trim().is_empty() {
                    self.flash = Some((
                        StatusKind::Error,
                        "Enter an email address, or find a person to add.".into(),
                    ));
                } else {
                    self.guest_email = email.clone();
                    self.add_guest(scope, credential, name, json!({"email": email.trim()}))
                        .await?;
                }
            }
            "request_person" => {
                let guest = value["guest"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                let rows = self.pages.activities.guests(scope, self.id).await?;
                let email = rows
                    .iter()
                    .find(|g| g.id == guest && g.user_id.is_none() && g.person.is_none())
                    .and_then(|g| g.email.clone())
                    .ok_or(Error::Invalid)?;
                self.person_dialog = Some(PersonDialog {
                    guest,
                    email,
                    name: String::new(),
                    errors: Vec::new(),
                });
            }
            "cancel_person" => self.person_dialog = None,
            "create_person" => {
                let form = params::form_values_checked(value, "person", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let Some(dialog) = &mut self.person_dialog else {
                    return Ok(None);
                };
                dialog.name = form.get("name").cloned().unwrap_or_default();
                let input = json!({"id": self.id, "guest_id": dialog.guest, "name": dialog.name});
                match self
                    .submit(
                        scope,
                        credential,
                        name,
                        "core.activity.create_person_guest",
                        input,
                    )
                    .await?
                {
                    Ok(_) => {
                        self.person_dialog = None;
                        self.flash = Some((
                            StatusKind::Success,
                            "Person created; they are on the guest list in place of the address."
                                .into(),
                        ));
                    }
                    Err(refusal) => {
                        if let Some(dialog) = &mut self.person_dialog {
                            dialog.errors = vec![Self::refused(refusal)];
                        }
                    }
                }
            }
            "remove_guest" => {
                let guest = value["guest"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                match self
                    .submit(
                        scope,
                        credential,
                        name,
                        "core.activity.remove_guest",
                        json!({"id": self.id, "guest_id": guest}),
                    )
                    .await?
                {
                    Ok(_) => self.flash = Some((StatusKind::Success, "Guest removed.".into())),
                    Err(refusal) => self.flash = Some((StatusKind::Error, Self::refused(refusal))),
                }
            }
            "request_send_invites" => self.confirm = Some(Confirm::SendInvites),
            "send_invites" => {
                if self.confirm.take() != Some(Confirm::SendInvites) {
                    return Ok(None);
                }
                match self
                    .submit(
                        scope,
                        credential,
                        name,
                        "core.activity.send_invites",
                        json!({"id": self.id}),
                    )
                    .await?
                {
                    Ok(result) => {
                        let emailed = result["result"]["emailed"].as_u64().unwrap_or(0);
                        let told = result["result"]["notified_in_bell"].as_u64().unwrap_or(0);
                        self.flash = Some((
                            StatusKind::Success,
                            format!("Invitations sent: {told} in the bell, {emailed} by email."),
                        ));
                    }
                    Err(Refusal::Message(message)) if message == FORBIDDEN => {
                        self.flash = Some((
                            StatusKind::Error,
                            "You do not have permission to send these invitations.".into(),
                        ))
                    }
                    Err(refusal) => self.flash = Some((StatusKind::Error, Self::refused(refusal))),
                }
            }
            "related_change" => {
                let form = params::form_values_checked(value, "related", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let query: String = form
                    .get("record_search")
                    .cloned()
                    .unwrap_or_default()
                    .chars()
                    .take(100)
                    .collect();
                if query != self.query {
                    self.query = query;
                    self.hits = if self.query.trim().is_empty() {
                        Vec::new()
                    } else {
                        let kinds: Vec<&str> = self
                            .pages
                            .records
                            .kinds()
                            .map(|kind| kind.key)
                            .filter(|key| *key != RECORD)
                            .take(bilibi_records::MAX_SEARCH_KINDS)
                            .collect();
                        self.pages
                            .records
                            .search(scope, &kinds, &self.query)
                            .await?
                    };
                }
            }
            "pick_related" => {
                let (kind, id) = value["record"]
                    .as_str()
                    .and_then(|v| v.split_once(':'))
                    .ok_or(Error::Invalid)?;
                self.save_related(scope, credential, name, Some(Reference::new(kind, id)))
                    .await?;
            }
            "clear_related" => self.save_related(scope, credential, name, None).await?,
            "request_close" => self.confirm = Some(Confirm::Close),
            "request_close_and_new" => self.confirm = Some(Confirm::CloseAndNew),
            "request_delete" => {
                let a = self.pages.activities.get(scope, self.id).await?;
                if a.series_id.is_some() {
                    self.delete_dialog = Some(Vec::new());
                } else {
                    self.confirm = Some(Confirm::Delete);
                }
            }
            "cancel_delete_series" => self.delete_dialog = None,
            "delete_series" => {
                if let Some(body) = self.delete_series(scope, credential, value).await? {
                    return Ok(Some(body));
                }
            }
            "request_series_edit" => {
                let a = self.pages.activities.get(scope, self.id).await?;
                let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
                if a.series_id.is_some() {
                    self.series_form = Some(SeriesForm::new(&a, &clock));
                }
            }
            "cancel_series" => self.series_form = None,
            "update_series" => self.update_series(scope, credential, value).await?,
            "cancel_confirm" => self.confirm = None,
            "close" | "close_and_new" | "delete" => {
                let wanted = match name {
                    "close" => Confirm::Close,
                    "close_and_new" => Confirm::CloseAndNew,
                    _ => Confirm::Delete,
                };
                if self.confirm.take() != Some(wanted) {
                    return Ok(None);
                }
                let activity = self.pages.activities.get(scope, self.id).await?;
                let contract = if wanted == Confirm::Delete {
                    "core.activity.delete"
                } else {
                    "core.activity.close"
                };
                let input = json!({"id": self.id, "version": activity.version});
                match self
                    .submit(scope, credential, name, contract, input)
                    .await?
                {
                    Ok(_) if wanted == Confirm::Delete => {
                        return Ok(Some(LiveBody {
                            redirect: Some("/activities".into()),
                            flash: Some((StatusKind::Success, "Activity deleted.".into())),
                            ..LiveBody::default()
                        }));
                    }
                    Ok(_) => {
                        self.flash = Some((
                            StatusKind::Success,
                            format!(
                                "{} {}.",
                                activity.kind.label(),
                                activity
                                    .kind
                                    .closing()
                                    .map_or("closed", |s| s.label())
                                    .to_lowercase()
                            ),
                        ));
                        if wanted == Confirm::CloseAndNew {
                            let clock =
                                Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
                            let copy = self.pages.activities.follow_up(scope, self.id).await?;
                            self.dialog =
                                Some(Dialog::from_draft(&self.pages, scope, &clock, &copy).await?);
                        }
                    }
                    Err(Refusal::Fields(fields)) => {
                        self.flash = Some((StatusKind::Error, sentences(&fields)))
                    }
                    Err(Refusal::Message(message)) => {
                        self.flash = Some((StatusKind::Error, message))
                    }
                }
            }
            "close_modal" => self.dialog = None,
            "create_task" | "log_call" | "create_meeting" | "create_note" => {
                let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
                let pages = self.pages.shared();
                let Some(dialog) = &mut self.dialog else {
                    return Ok(None);
                };
                if dialog.submit_event() != name {
                    return Err(Error::Invalid);
                }
                if let Some(id) = dialog
                    .submit(&pages, scope, credential, &clock, value)
                    .await?
                {
                    let message = dialog.created_message();
                    self.dialog = None;
                    return Ok(Some(LiveBody {
                        redirect: Some(format!("/activities/{id}")),
                        flash: Some((StatusKind::Success, message)),
                        ..LiveBody::default()
                    }));
                }
            }
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(None)
    }

    fn inline(
        &self,
        a: &Activity,
        key: &str,
        value: impl Into<String>,
        editable: bool,
    ) -> InlineField {
        let mut field = InlineField::new(
            format!("activity-{}", key.replace('_', "-")),
            key,
            format!("Edit {}", label(key).to_lowercase()),
            value,
        )
        .version(a.version)
        .status(self.statuses.get(key));
        field.id_value = a.id.to_string();
        if !editable {
            field = field.readonly();
        }
        field
    }
    fn choice(
        &self,
        a: &Activity,
        key: &str,
        current: (&str, &str),
        options: &[(&str, &str)],
        editable: bool,
    ) -> Result<Html> {
        let field = self.inline(a, key, current.0, editable);
        let editor = render(
            Field::named(key)
                .id(format!("activity-{}-editor", key.replace('_', "-")))
                .label_class("sr-only")
                .label(label(key))
                .kind(InputKind::Select)
                .value(current.0)
                .options(choices(options)),
        )?;
        render(InlineChoice {
            field,
            display: Html::text(current.1),
            editor,
        })
    }
    fn time(
        &self,
        clock: &Clock,
        a: &Activity,
        key: &str,
        at: Option<jiff::Timestamp>,
        editable: bool,
    ) -> Result<Html> {
        let mut field = self
            .inline(a, key, clock.local(at), editable)
            .input_type("datetime-local");
        if let Some(at) = at {
            field = field.display_value(clock.text(at));
        }
        if matches!(key, "due_at" | "start_at") {
            field = field.allow_empty();
        }
        render(InlineEdit { field })
    }

    async fn facts(
        &self,
        scope: &Scope,
        clock: &Clock,
        a: &Activity,
        series: Option<&crate::Series>,
        editable: bool,
    ) -> Result<Html> {
        let names = self
            .pages
            .names(scope, &[a.assignee_user_id, a.created_by])
            .await?;
        let name = |id: i64| names.get(&id).cloned().unwrap_or_default();
        let mut items: Vec<(&str, String, Html)> = Vec::new();
        items.push((
            "Subject",
            "title".into(),
            render(InlineEdit {
                field: self.inline(a, "title", a.title.clone(), editable),
            })?,
        ));
        let mut kind = render(Icon {
            name: icon(a.kind),
            class: "size-4",
        })?;
        kind.push(&Html::text(&format!(" {}", a.kind.label())));
        items.push(("Kind", "kind".into(), kind));
        if let Some(status) = a.status {
            let options: Vec<(&str, &str)> = a
                .kind
                .statuses()
                .iter()
                .map(|s| (s.label(), s.code()))
                .collect();
            let field = self.inline(a, "status", status.code(), editable);
            let editor = render(
                Field::named("status")
                    .id("activity-status-editor")
                    .label_class("sr-only")
                    .label("Status")
                    .kind(InputKind::Select)
                    .value(status.code())
                    .options(choices(&options)),
            )?;
            items.push((
                "Status",
                "status".into(),
                render(InlineChoice {
                    field,
                    display: status_badge(a)?,
                    editor,
                })?,
            ));
        }
        if let Some(priority) = a.priority {
            let options: Vec<(&str, &str)> = Priority::ALL
                .iter()
                .map(|p| (p.label(), p.code()))
                .collect();
            items.push((
                "Priority",
                "priority".into(),
                self.choice(
                    a,
                    "priority",
                    (priority.code(), priority.label()),
                    &options,
                    editable,
                )?,
            ));
        }
        if let Some(direction) = a.direction {
            let options: Vec<(&str, &str)> = Direction::ALL
                .iter()
                .map(|d| (d.label(), d.code()))
                .collect();
            items.push((
                "Direction",
                "direction".into(),
                self.choice(
                    a,
                    "direction",
                    (direction.code(), direction.label()),
                    &options,
                    editable,
                )?,
            ));
        }
        match a.kind {
            Kind::Call | Kind::Meeting => {
                items.push((
                    "Starts",
                    "starts_at".into(),
                    self.time(clock, a, "starts_at", a.starts_at, editable)?,
                ));
                items.push((
                    "Ends",
                    "ends_at".into(),
                    self.time(clock, a, "ends_at", a.ends_at, editable)?,
                ));
            }
            Kind::Task => {
                items.push((
                    "Due",
                    "due_at".into(),
                    self.time(clock, a, "due_at", a.due_at, editable)?,
                ));
                items.push((
                    "Start",
                    "start_at".into(),
                    self.time(clock, a, "start_at", a.start_at, editable)?,
                ));
            }
            Kind::Note => {}
        }
        if a.kind == Kind::Meeting {
            for (title, key, value) in [
                ("Location", "location", a.location.clone()),
                ("Join link", "join_url", a.join_url.clone()),
                ("Meeting type", "meeting_type", a.meeting_type.clone()),
            ] {
                let mut field = self
                    .inline(a, key, value.unwrap_or_default(), editable)
                    .allow_empty();
                if key == "join_url" {
                    field = field.input_type("url");
                }
                items.push((title, key.replace('_', "-"), render(InlineEdit { field })?));
            }
        }
        if a.kind == Kind::Call {
            items.push((
                "Outcome",
                "outcome".into(),
                render(InlineEdit {
                    field: self
                        .inline(
                            a,
                            "outcome",
                            a.outcome.clone().unwrap_or_default(),
                            editable,
                        )
                        .allow_empty(),
                })?,
            ));
        }
        let assignee = a.assignee_user_id.to_string();
        let assignee_name = name(a.assignee_user_id);
        let mut options = vec![(assignee_name.as_str(), assignee.as_str())];
        if a.assignee_user_id != scope.user() {
            options.push(("Me", "me"));
        }
        items.push((
            "Assigned to",
            "assignee".into(),
            self.choice(
                a,
                "assignee",
                (&assignee, &assignee_name),
                &options,
                editable,
            )?,
        ));
        // Sharing needs a related record whose readers it is shared with.
        let (visibility, visibility_label) = if a.shared {
            ("shared", SHARED)
        } else {
            ("private", PRIVATE)
        };
        items.push((
            "Who can see it",
            "shared".into(),
            self.choice(
                a,
                "shared",
                (visibility, visibility_label),
                &[(PRIVATE, "private"), (SHARED, "shared")],
                editable && a.related.is_some(),
            )?,
        ));
        if a.kind != Kind::Note {
            let own = a.reminder_minutes.as_deref();
            let offered = reminder_choices::options(own, "Default");
            let offered: Vec<(&str, &str)> = offered
                .iter()
                .map(|(label, code)| (label.as_str(), code.as_str()))
                .collect();
            items.push((
                "Reminders",
                "reminders".into(),
                self.choice(
                    a,
                    "reminders",
                    (&reminder_choices::code(own), &reminder_choices::label(own)),
                    &offered,
                    editable,
                )?,
            ));
            items.push((
                "Email reminders",
                "reminder-email".into(),
                self.choice(
                    a,
                    "reminder_email",
                    (
                        reminder_choices::email_code(a.reminder_email),
                        reminder_choices::email_label(a.reminder_email),
                    ),
                    reminder_choices::EMAIL,
                    editable,
                )?,
            ));
        }
        if let (Some(series), Some(index)) = (series, a.series_index) {
            let mut repeats = Html::text(&format!(
                "{}. This is occurrence {index}; {} in the series now. ",
                series.describe(),
                match series.live {
                    1 => "1 remains".to_owned(),
                    n => format!("{n} remain"),
                }
            ));
            repeats.push(&render(ActionLink {
                id: "activity-series-all",
                title: "See every occurrence of this series",
                href: &format!("/activities?series={}&state=all", series.id),
                body: Html::text("See all"),
                ..ActionLink::default()
            })?);
            items.push(("Repeats", "repeats".into(), repeats));
        }
        if let (Some(reminded), false) = (a.reminded_at, a.is_closed()) {
            items.push((
                "Reminded",
                "reminded".into(),
                clock.time("activity-reminded-at", reminded)?,
            ));
        }
        let mut created = Html::text(&format!("{} · ", name(a.created_by)));
        created.push(&clock.time("activity-created-at", a.created_at)?);
        items.push(("Created by", "created-by".into(), created));
        if let Some(closed) = a.closed_at {
            items.push((
                "Closed",
                "closed".into(),
                clock.time("activity-closed-at", closed)?,
            ));
        }
        let ids: Vec<String> = items
            .iter()
            .map(|(_, key, _)| format!("activity-fact-{key}"))
            .collect();
        let facts: Vec<Fact> = items
            .into_iter()
            .zip(&ids)
            .map(|((title, _, value), id)| Fact { title, id, value })
            .collect();
        render(Facts {
            id: "activity-facts",
            items: facts,
        })
    }

    async fn related(&self, scope: &Scope, a: &Activity, editable: bool) -> Result<Html> {
        let resolved: Option<Resolved> = match &a.related {
            Some(r) => self
                .pages
                .records
                .resolve(scope, std::slice::from_ref(r))
                .await?
                .into_iter()
                .next(),
            None => None,
        };
        let mut body = render(SectionHeading {
            id: "activity-related-heading",
            title: "Related to",
            description: if a.related.is_none() {
                "This activity is not about a record."
            } else {
                ""
            },
            ..SectionHeading::default()
        })?;
        let errors = match self.statuses.get("related") {
            CommitStatus::Error(message) => vec![message],
            _ => Vec::new(),
        };
        if editable {
            let selected = resolved.as_ref().map(|r| {
                (
                    format!("{}:{}", r.reference.kind, r.id),
                    r.pill("activity-related-pill", self.framed),
                )
            });
            let picker = render(RecordPicker::new(
                "activity-related",
                "related",
                "record",
                "Related to",
                &self.query,
                &self.pages.records.picker_hits(&self.hits),
                selected
                    .as_ref()
                    .map(|(value, pill)| (value.as_str(), pill.clone())),
                ("pick_related", "clear_related"),
                errors,
            ))?;
            body.push(&render(Form {
                id: "activity-related-form",
                event: "related_change",
                change: true,
                body: picker,
                ..Form::default()
            })?);
        } else if let Some(resolved) = &resolved {
            body.push(&render(
                resolved.pill("activity-related-pill", self.framed),
            )?);
        }
        render(Card {
            id: "activity-related-card",
            class: "mt-6",
            role: "region",
            labelledby: "activity-related-heading",
            body,
            ..Card::default()
        })
    }

    /// The guests: who, their role, their answer, whether an invitation went
    /// out, and a warning on a guest with no email address the reader may
    /// use. Editors add a person or an address and remove anyone but the
    /// assignee; an account guest answers for themselves.
    async fn guests(
        &self,
        scope: &Scope,
        a: &Activity,
        guests: &[GuestRow],
        (emails, creatable): (&HashMap<i64, String>, &std::collections::HashSet<i64>),
        clock: &Clock,
        editable: bool,
    ) -> Result<Html> {
        let users: Vec<i64> = guests.iter().filter_map(|g| g.user_id).collect();
        let names = self.pages.names(scope, &users).await?;
        let busy = self.availability(scope, a, guests).await?;
        let people: Vec<Reference> = guests
            .iter()
            .filter_map(|g| {
                g.person
                    .map(|p| Reference::new("person", p.get().to_string()))
            })
            .collect();
        let named = self.pages.records.resolve(scope, &people).await?;
        let mut named = named.iter();
        let options: Vec<(&str, &str)> = Response::ALL
            .iter()
            .map(|r| (r.label(), r.code()))
            .collect();
        let mut items: Vec<(String, &str, Html)> = Vec::new();
        for guest in guests {
            let mut who = match (guest.user_id, guest.person, &guest.email) {
                (Some(user), _, _) => Html::text(&names.get(&user).cloned().unwrap_or_default()),
                (_, Some(_), _) => match named.next() {
                    Some(r) => {
                        render(r.pill(format!("activity-guest-{}-pill", guest.id), self.framed))?
                    }
                    None => Html::text("—"),
                },
                (_, _, Some(email)) => Html::text(email),
                _ => Html::text("—"),
            };
            let key = format!("response-{}", guest.id);
            let own = guest.user_id == Some(scope.user());
            let response = Response::parse(&guest.response).unwrap_or(Response::NoResponse);
            let badge = match response {
                Response::Accepted => BadgeKind::Success,
                Response::Declined => BadgeKind::Danger,
                Response::Tentative => BadgeKind::Warning,
                Response::NoResponse => BadgeKind::Neutral,
            };
            // An account guest answers for themselves; editors for anyone.
            // A plain select in its own change form: an inline choice's
            // trigger would cover the Remove button beside it.
            let answer = if a.kind.timed() && (editable || own) {
                let mut fields = render(
                    Field::in_form("answer", "guest")
                        .id(format!("activity-guest-{}-answer-guest", guest.id))
                        .kind(InputKind::Hidden)
                        .value(guest.id.to_string()),
                )?;
                fields.push(&render(
                    Field::in_form("answer", "response")
                        .id(format!("activity-guest-{}-answer", guest.id))
                        .label("Answer")
                        .kind(InputKind::Select)
                        .value(response.code())
                        .options(choices(&options))
                        .errors(match self.statuses.get(&key) {
                            CommitStatus::Error(message) => vec![message],
                            _ => Vec::new(),
                        }),
                )?);
                render(Form {
                    id: &format!("activity-guest-{}-answer-form", guest.id),
                    event: "respond",
                    change: true,
                    class: "mt-2",
                    body: fields,
                    ..Form::default()
                })?
            } else if a.kind.timed() {
                render(Badge::status(badge, response.label()))?
            } else {
                Html::EMPTY
            };
            if let (Some(user), Some(busy)) = (guest.user_id, &busy) {
                if guest.response != Response::Declined.code() {
                    who.push(&Html::text(" "));
                    who.push(&match busy.get(&user) {
                        Some(with) => render(Badge::status(
                            BadgeKind::Warning,
                            &match with {
                                Some(title) => format!("Busy: {title}"),
                                None => "Busy".to_owned(),
                            },
                        ))?,
                        None => render(Badge::status(BadgeKind::Success, "Free"))?,
                    });
                }
            }
            if a.kind.timed() && guest.user_id.is_none() && !emails.contains_key(&guest.id) {
                who.push(&Html::text(" "));
                who.push(&render(Badge::status(
                    BadgeKind::Warning,
                    "No email address",
                ))?);
            }
            if let Some(at) = guest.invited_at {
                let sent = if guest.user_id.is_some() {
                    "told in the bell"
                } else if !emails.contains_key(&guest.id) {
                    "not emailed"
                } else if guest.invite_sent_at.is_some_and(|sent| sent >= at) {
                    "email sent"
                } else {
                    "email pending"
                };
                who.push(&Html::text(&format!(
                    " Invited {} · {sent}",
                    clock.text(at)
                )));
            }
            if editable && guest.role != "assignee" {
                who.push(&Html::text(" "));
                let id = guest.id.to_string();
                who.push(&render(Button {
                    id: &format!("activity-guest-{}-remove", guest.id),
                    button_type: "button",
                    event: "remove_guest",
                    name: "guest",
                    value: &id,
                    working: "Removing…",
                    body: Html::text("Remove"),
                    ..Button::default()
                })?);
            }
            if creatable.contains(&guest.id) {
                who.push(&Html::text(" "));
                let id = guest.id.to_string();
                who.push(&render(Button {
                    id: &format!("activity-guest-{}-create-person", guest.id),
                    button_type: "button",
                    event: "request_person",
                    name: "guest",
                    value: &id,
                    body: Html::text("Create person"),
                    ..Button::default()
                })?);
            }
            who.push(&answer);
            let role = match guest.role.as_str() {
                "creator" => "Creator",
                "assignee" => "Assignee",
                "related" => "Related",
                _ => "Invited",
            };
            items.push((format!("activity-guest-{}", guest.id), role, who));
        }
        let facts: Vec<Fact> = items
            .iter()
            .map(|(id, role, who)| Fact {
                title: role,
                id,
                value: who.clone(),
            })
            .collect();
        let mut body = render(SectionHeading {
            id: "activity-guests-heading",
            title: "Guests",
            description: if a.kind.timed() {
                "Free or Busy shows whether a colleague has another call or meeting at this time. Send invites tells colleagues in the bell and emails the others."
            } else {
                ""
            },
            ..SectionHeading::default()
        })?;
        body.push(&render(Facts {
            id: "activity-guest-facts",
            items: facts,
        })?);
        if editable && a.kind.timed() && !a.is_closed() {
            let mut fields = render(RecordPicker::new(
                "activity-guest",
                "guest",
                "person",
                "Add a person",
                &self.guest_query,
                &self.pages.records.picker_hits(&self.guest_hits),
                None,
                ("pick_guest", "clear_guest"),
                Vec::new(),
            ))?;
            fields.push(&render(
                Field::in_form("guest", "email")
                    .id("activity-guest-email")
                    .label("Or an email address")
                    .kind(InputKind::Email)
                    .value(&self.guest_email)
                    .attr("maxlength", "254")
                    .attr("data-debounce", "300"),
            )?);
            let add = render(Button {
                id: "activity-guest-add",
                button_type: "submit",
                working: "Adding…",
                body: Html::text("Add guest"),
                ..Button::default()
            })?;
            body.push(
                &Form {
                    id: "activity-guest-form",
                    event: "guest_change",
                    change: true,
                    class: "mt-4",
                    body: fields,
                    actions: add,
                }
                .submit_event("add_guest_email")
                .map_err(|_| Error::Internal)?,
            );
        }
        render(Card {
            id: "activity-guests",
            class: "mt-6",
            role: "region",
            labelledby: "activity-guests-heading",
            body,
            ..Card::default()
        })
    }

    /// Which account guests are busy for this slot, before anyone is told:
    /// each busy account maps to the title of the other call or meeting only
    /// when the reader may see it (`Activities::conflicts` decides), else to
    /// `None`. `None` overall when the slot has no times or the activity is
    /// closed, so no badge is drawn.
    async fn availability(
        &self,
        scope: &Scope,
        a: &Activity,
        guests: &[GuestRow],
    ) -> Result<Option<HashMap<i64, Option<String>>>> {
        let (Some(starts), Some(ends)) = (a.starts_at, a.ends_at) else {
            return Ok(None);
        };
        if !a.kind.timed() || a.is_closed() {
            return Ok(None);
        }
        let users: Vec<i64> = guests.iter().filter_map(|g| g.user_id).collect();
        if users.is_empty() {
            return Ok(None);
        }
        let conflicts = self
            .pages
            .activities
            .conflicts(scope, starts, ends, &users, Some(a.id))
            .await?;
        let mut busy = HashMap::new();
        for c in conflicts {
            let entry: &mut Option<String> = busy.entry(c.user).or_insert(None);
            if entry.is_none() {
                *entry = c.title;
            }
        }
        Ok(Some(busy))
    }

    /// Other open calls and meetings at the same time for the assignee or
    /// an account guest who has not declined.
    async fn overlaps(
        &self,
        scope: &Scope,
        clock: &Clock,
        a: &Activity,
        guests: &[GuestRow],
    ) -> Result<Html> {
        let (Some(starts), Some(ends)) = (a.starts_at, a.ends_at) else {
            return Ok(Html::EMPTY);
        };
        if !a.kind.timed() || a.is_closed() {
            return Ok(Html::EMPTY);
        }
        let mut users: Vec<i64> = guests
            .iter()
            .filter(|g| g.response != Response::Declined.code())
            .filter_map(|g| g.user_id)
            .collect();
        users.push(a.assignee_user_id);
        users.sort_unstable();
        users.dedup();
        let conflicts = self
            .pages
            .activities
            .conflicts(scope, starts, ends, &users, Some(a.id))
            .await?;
        if conflicts.is_empty() {
            return Ok(Html::EMPTY);
        }
        let ids: Vec<i64> = conflicts.iter().map(|c| c.user).collect();
        let names = self.pages.names(scope, &ids).await?;
        let lines: Vec<String> = conflicts
            .iter()
            .map(|c| {
                let what = c.title.as_deref().unwrap_or("another call or meeting");
                format!(
                    "{} has {what} from {} to {}.",
                    names.get(&c.user).cloned().unwrap_or_default(),
                    clock.text(c.starts_at),
                    clock.text(c.ends_at)
                )
            })
            .collect();
        render(Alert {
            kind: StatusKind::Warning,
            id: "activity-conflicts",
            class: "mt-6",
            body: Html::text(&format!("This overlaps: {}", lines.join(" "))),
        })
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
        let a = self.pages.activities.get(scope, self.id).await?;
        let editable = self.pages.activities.may_edit(scope, &a).await?;
        let series = self.pages.activities.series(scope, &a).await?;
        let mut actions = Html::EMPTY;
        if editable && !a.is_closed() && a.kind.closing().is_some() {
            actions.push(&render(Button {
                id: "activity-close",
                button_type: "button",
                event: "request_close",
                variant: ButtonVariant::Primary,
                body: Html::text("Close"),
                ..Button::default()
            })?);
            if self.pages.activities.may_create(scope).await? {
                actions.push(&render(Button {
                    id: "activity-close-and-new",
                    button_type: "button",
                    event: "request_close_and_new",
                    body: Html::text("Close and create new"),
                    ..Button::default()
                })?);
            }
        }
        let ahead = a.starts_at.is_some_and(|at| at > jiff::Timestamp::now());
        if editable && a.kind.timed() && !a.is_closed() && ahead {
            actions.push(&render(Button {
                id: "activity-send-invites",
                button_type: "button",
                event: "request_send_invites",
                body: Html::text("Send invites"),
                ..Button::default()
            })?);
        }
        if editable && series.is_some() {
            actions.push(&render(Button {
                id: "activity-series-edit",
                button_type: "button",
                event: "request_series_edit",
                body: Html::text("Change series"),
                ..Button::default()
            })?);
        }
        if editable {
            actions.push(&render(Button {
                id: "activity-delete",
                button_type: "button",
                event: "request_delete",
                variant: ButtonVariant::Danger,
                body: Html::text("Delete"),
                ..Button::default()
            })?);
        }
        actions.push(&render(BackLink {
            href: "/activities",
            id: "activity-back",
            title: "Back to activities",
            ..BackLink::default()
        })?);
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                _ => flash.error = message,
            }
        }
        let mut body = render(FlashGroup {
            id: "activity-flash",
            flash,
        })?;
        body.push(&render(Header {
            title: Html::text(&a.title),
            actions,
            ..Header::default()
        })?);
        let mut details = render(SectionHeading {
            id: "activity-details-heading",
            title: "Details",
            ..SectionHeading::default()
        })?;
        details.push(
            &self
                .facts(scope, &clock, &a, series.as_ref(), editable)
                .await?,
        );
        body.push(&render(Card {
            id: "activity-details",
            role: "region",
            labelledby: "activity-details-heading",
            body: details,
            ..Card::default()
        })?);
        let mut text = render(SectionHeading {
            id: "activity-text-heading",
            title: "Description",
            ..SectionHeading::default()
        })?;
        let mut long = vec![(
            "Description",
            "activity-fact-description",
            render(InlineLongText {
                field: self
                    .inline(
                        &a,
                        "description",
                        a.description.clone().unwrap_or_default(),
                        editable,
                    )
                    .allow_empty(),
                rows: 4,
            })?,
        )];
        if a.kind != Kind::Note {
            long.push((
                "Internal notes",
                "activity-fact-internal-notes",
                render(InlineLongText {
                    field: self
                        .inline(
                            &a,
                            "internal_notes",
                            a.internal_notes.clone().unwrap_or_default(),
                            editable,
                        )
                        .allow_empty()
                        .hint("Never sent to guests."),
                    rows: 3,
                })?,
            ));
        }
        text.push(&render(Facts {
            id: "activity-text-facts",
            items: long
                .into_iter()
                .map(|(title, id, value)| Fact { title, id, value })
                .collect(),
        })?);
        body.push(&render(Card {
            id: "activity-text",
            class: "mt-6",
            role: "region",
            labelledby: "activity-text-heading",
            body: text,
            ..Card::default()
        })?);
        let guests = self.pages.activities.guests(scope, self.id).await?;
        body.push(&self.overlaps(scope, &clock, &a, &guests).await?);
        body.push(&self.related(scope, &a, editable).await?);
        let emails = self.pages.activities.guest_emails(scope, &guests).await?;
        // An address guest nobody in People has: the panel offers to create them.
        let mut creatable = std::collections::HashSet::new();
        if editable {
            for guest in guests
                .iter()
                .filter(|g| g.user_id.is_none() && g.person.is_none())
            {
                if let Some(email) = &guest.email {
                    if self
                        .pages
                        .activities
                        .may_create_person_for(scope, email)
                        .await?
                    {
                        creatable.insert(guest.id);
                    }
                }
            }
        }
        body.push(
            &self
                .guests(scope, &a, &guests, (&emails, &creatable), &clock, editable)
                .await?,
        );
        if let Some(confirm) = self.confirm {
            let closing = a
                .kind
                .closing()
                .map_or("closed", |s| s.label())
                .to_lowercase();
            let (consequence, detail, verb, working, event) = match confirm {
                Confirm::Close => (
                    format!("“{}” will be marked {closing}.", a.title),
                    "It moves to the history of the records it is about.",
                    "Close",
                    "Closing…",
                    "close",
                ),
                Confirm::CloseAndNew => (
                    format!("“{}” will be marked {closing}.", a.title),
                    "A new one with the same details opens for you to save.",
                    "Close",
                    "Closing…",
                    "close_and_new",
                ),
                Confirm::Delete => (
                    format!("“{}” will be deleted.", a.title),
                    "It disappears from every list and record page.",
                    "Delete",
                    "Deleting…",
                    "delete",
                ),
                Confirm::SendInvites => (
                    format!("Every guest but you will be invited to “{}”.", a.title),
                    "Colleagues are told in the bell. The others are emailed the time, place, join link and description, never the internal notes.",
                    "Send invites",
                    "Sending…",
                    "send_invites",
                ),
            };
            body.push(&render(ConfirmDialog {
                id: "activity-confirm",
                consequence: &consequence,
                detail,
                confirm: verb,
                working,
                confirm_event: event,
                cancel_event: "cancel_confirm",
            })?);
        }
        if let Some(dialog) = &self.dialog {
            body.push(&dialog.render(
                &self.pages,
                &clock,
                "activity-new",
                "close_modal",
                self.framed,
            )?);
        }
        if let (Some(form), true) = (&self.series_form, editable) {
            body.push(&series_dialog(&a, form)?);
        }
        if let (Some(errors), true) = (&self.delete_dialog, editable) {
            body.push(&series_delete::dialog(&a, errors, true)?);
        }
        if let (Some(dialog), true) = (&self.person_dialog, editable) {
            body.push(&guest_person::dialog(dialog)?);
        }
        let region = render(Region {
            id: "activity-region",
            target: false,
            timezone_request: self.zone.request(),
            body,
        })?;
        let id = self.id.to_string();
        let html = render(bilibi_ui::Page {
            id: "activity-detail",
            variant: PageVariant::Detail,
            body: render(View {
                id: &id,
                body: region,
            })?,
            ..bilibi_ui::Page::default()
        })?
        .to_string();
        Ok(LiveBody {
            html,
            rows: 1,
            ..LiveBody::default()
        })
    }
}

impl LiveState<Scope, Error> for Detail {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                if let Some(body) = self
                    .handle(request.scope, request.credential, value)
                    .await?
                {
                    return Ok(body);
                }
            }
            self.view(request.scope).await
        })
    }
}

/// An error list above a dialog's fields.
pub(super) fn dialog_errors(id: &str, errors: &[String]) -> Result<Html> {
    if errors.is_empty() {
        return Ok(Html::EMPTY);
    }
    render(Alert {
        kind: StatusKind::Error,
        id,
        class: "mb-3",
        body: Html::text(&errors.join(" ")),
    })
}

/// Change series: which occurrences, and the values to give them.
fn series_dialog(a: &Activity, form: &SeriesForm) -> Result<Html> {
    let field = |key: &str, title: &str| {
        Field::in_form("series", key)
            .id(format!("activity-series-{}", key.replace('_', "-")))
            .label(title)
            .value(form.value(key))
    };
    let mut fields = vec![
        field("apply", "Change")
            .kind(InputKind::Select)
            .options(choices(&[
                ("This occurrence and the later ones", "following"),
                ("Every occurrence", "all"),
            ])),
        field("title", "Subject")
            .required()
            .attr("maxlength", "160"),
    ];
    match a.kind {
        Kind::Call | Kind::Meeting => {
            fields.push(field("starts_at", "Starts").kind(InputKind::DatetimeLocal));
            fields.push(field("ends_at", "Ends").kind(InputKind::DatetimeLocal));
        }
        Kind::Task => fields.push(field("due_at", "Due").kind(InputKind::DatetimeLocal)),
        Kind::Note => {}
    }
    if a.kind == Kind::Meeting {
        fields.push(field("location", "Location").attr("maxlength", "255"));
    }
    fields.push(
        field("description", "Description")
            .kind(InputKind::Textarea)
            .attr("rows", "3"),
    );
    let mut body = dialog_errors("activity-series-errors", &form.errors)?;
    for field in fields {
        body.push(&render(field)?);
    }
    let mut actions = render(Button {
        id: "activity-series-cancel",
        button_type: "button",
        event: "cancel_series",
        body: Html::text("Cancel"),
        ..Button::default()
    })?;
    actions.push(&render(Button {
        id: "activity-series-save",
        button_type: "submit",
        variant: ButtonVariant::Primary,
        working: "Saving…",
        body: Html::text("Change series"),
        ..Button::default()
    })?);
    let form = render(Form {
        id: "activity-series-form",
        event: "update_series",
        change: false,
        body,
        actions,
        ..Form::default()
    })?;
    render(Modal {
        id: "activity-series-dialog",
        title: "Change the series",
        cancel_event: "cancel_series",
        width: ModalWidth::Wide,
        description: "Only the values you change are copied. A new time moves each open occurrence by the same amount; held and completed ones keep theirs. Status changes one occurrence at a time.",
        body: form,
        ..Modal::default()
    })
}
