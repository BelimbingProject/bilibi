//! `/activities/{id}`: one activity, read first and edited in place. Close,
//! Close and create new, and Delete ask first; every write calls its
//! contract through `PageContracts`.
use super::{
    event, field_of, form::Dialog, icon, label, refusal, render, sentences, status_badge,
    BrowserZone, Clock, Pages, Refusal, Region, FIELDS,
};
use crate::{Activity, Direction, GuestRow, Kind, Priority, Reference, RECORD};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_records::{Hit, Resolved};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    choices, page::*, params, BackLink, Button, ButtonVariant, Card, CommitStates, CommitStatus,
    ConfirmDialog, Fact, Facts, Field, FlashGroup, Form, Header, Html, Icon, InlineChoice,
    InlineEdit, InlineField, InlineLongText, InputKind, PageVariant, RecordPicker, SectionHeading,
    StatusKind, NO_FLASH,
};
use serde_json::{json, Value};

const FORBIDDEN: &str = "You do not have permission to change this activity.";
const PRIVATE: &str = "People taking part";
const SHARED: &str = "Also readers of the related record";

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
            flash: None,
        })
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
            "request_delete" => self.confirm = Some(Confirm::Delete),
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
            "create_task" | "log_call" | "create_note" => {
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
                    let noun = dialog.kind.label();
                    self.dialog = None;
                    return Ok(Some(LiveBody {
                        redirect: Some(format!("/activities/{id}")),
                        flash: Some((StatusKind::Success, format!("{noun} created."))),
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

    async fn guests(&self, scope: &Scope, guests: &[GuestRow]) -> Result<Html> {
        let users: Vec<i64> = guests.iter().filter_map(|g| g.user_id).collect();
        let names = self.pages.names(scope, &users).await?;
        let people: Vec<Reference> = guests
            .iter()
            .filter_map(|g| {
                g.person
                    .map(|p| Reference::new("person", p.get().to_string()))
            })
            .collect();
        let named = self.pages.records.resolve(scope, &people).await?;
        let mut named = named.iter();
        let mut items = Vec::new();
        for guest in guests {
            let who = match (guest.user_id, guest.person, &guest.email) {
                (Some(user), _, _) => Html::text(&names.get(&user).cloned().unwrap_or_default()),
                (_, Some(_), _) => match named.next() {
                    Some(r) => render(r.pill(format!("activity-guest-{}", guest.id), self.framed))?,
                    None => Html::text("—"),
                },
                (_, _, Some(email)) => Html::text(email),
                _ => Html::text("—"),
            };
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
            ..SectionHeading::default()
        })?;
        body.push(&render(Facts {
            id: "activity-guest-facts",
            items: facts,
        })?);
        render(Card {
            id: "activity-guests",
            class: "mt-6",
            role: "region",
            labelledby: "activity-guests-heading",
            body,
            ..Card::default()
        })
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
        let a = self.pages.activities.get(scope, self.id).await?;
        let editable = self.pages.activities.may_edit(scope, &a).await?;
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
        details.push(&self.facts(scope, &clock, &a, editable).await?);
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
        body.push(&self.related(scope, &a, editable).await?);
        let guests = self.pages.activities.guests(scope, self.id).await?;
        body.push(&self.guests(scope, &guests).await?);
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
