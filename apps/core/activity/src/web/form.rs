//! The Log call, New meeting, New task and New note dialog, shared by the list, the
//! record page (Close and create new) and the record panels. Changes keep the
//! server's draft; Enter or Save submits it to the kind's create contract.
use super::{refusal, reminder_choices, render, Clock, Pages, Refusal};
use crate::{Draft, Guest, Kind, Priority, Reference, RECORD};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_records::{Hit, Resolved};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    choices, error_sentence, params, Alert, Button, ButtonVariant, ErrorSummary, Field, Html,
    InputKind, Modal, ModalWidth, RecordPicker, StatusKind,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const FORM: &str = "activity";
/// The weekly days, a multi-select: posted as an array under this key.
const DAYS: &str = "activity[repeat_days][]";
/// Long text fields post up to this many characters.
const LIMIT: usize = 10_000;

#[derive(Template)]
#[template(path = "activities_form.html")]
struct FormView<'a> {
    id: &'a str,
    submit: &'a str,
    actions: Html,
    body: Html,
}

/// A new activity being written in the dialog.
pub(crate) struct Dialog {
    pub kind: Kind,
    values: BTreeMap<String, String>,
    related: Option<Resolved>,
    query: String,
    hits: Vec<Hit>,
    /// The assignee and their name; the reader when `None`.
    assignee: Option<(i64, String)>,
    /// Readers of the related record see it too.
    shared: bool,
    guests: Vec<Guest>,
    /// Weekly: the days chosen, by code.
    days: Vec<String>,
    errors: BTreeMap<String, Vec<String>>,
    notice: String,
    submitted: bool,
    /// How many activities the last save created (a repeating one makes
    /// one per occurrence).
    created: usize,
}

/// The posted field each contract input path refuses.
fn field_of(path: &str) -> &str {
    let key = path.split('.').next().unwrap_or(path);
    match key {
        "assignee_user_id" => "assignee",
        "reminder_minutes" => "reminders",
        "guests" | "company" | "kind" => "notice",
        other => other,
    }
}
fn label_of(field: &str) -> &'static str {
    match field {
        "title" => "Subject",
        "related" => "Related to",
        "due_at" => "Due",
        "start_at" => "Start",
        "priority" => "Priority",
        "starts_at" => "Starts",
        "ends_at" => "Ends",
        "status" => "Status",
        "direction" => "Direction",
        "outcome" => "Outcome",
        "location" => "Location",
        "join_url" => "Join link",
        "meeting_type" => "Meeting type",
        "assignee" => "Assigned to",
        "description" => "Description",
        "internal_notes" => "Internal notes",
        "shared" => "Share with readers of the related record",
        "reminders" => "Reminders",
        "reminder_email" => "Email reminders",
        "repeat" | "repeat_every" | "repeat_count" | "repeat_until" => "Repeats",
        _ => "",
    }
}

impl Dialog {
    pub fn new(kind: Kind, related: Option<Resolved>) -> Self {
        let mut values = BTreeMap::new();
        values.insert("priority".into(), "medium".into());
        values.insert("status".into(), "held".into());
        values.insert("direction".into(), "outbound".into());
        values.insert("repeat".into(), "none".into());
        values.insert("reminders".into(), reminder_choices::DEFAULT.into());
        values.insert("reminder_email".into(), "default".into());
        values.insert("repeat_every".into(), "1".into());
        values.insert("repeat_end".into(), "count".into());
        values.insert("repeat_count".into(), "10".into());
        Self {
            kind,
            values,
            related,
            query: String::new(),
            hits: Vec::new(),
            assignee: None,
            shared: false,
            guests: Vec::new(),
            days: Vec::new(),
            errors: BTreeMap::new(),
            notice: String::new(),
            submitted: false,
            created: 0,
        }
    }
    /// The dialog for a slot chosen on the calendar: a call or meeting
    /// starts and ends there (a call in the future is planned, not held), a
    /// task falls due at its start.
    pub fn at(mut self, start: jiff::Timestamp, end: jiff::Timestamp, clock: &Clock) -> Self {
        match self.kind {
            Kind::Task => {
                self.values
                    .insert("due_at".into(), clock.local(Some(start)));
            }
            Kind::Call | Kind::Meeting => {
                self.values
                    .insert("starts_at".into(), clock.local(Some(start)));
                self.values.insert("ends_at".into(), clock.local(Some(end)));
                if self.kind == Kind::Call && start > jiff::Timestamp::now() {
                    self.values.insert("status".into(), "planned".into());
                }
            }
            Kind::Note => {}
        }
        self
    }
    /// What the page says after a save.
    pub fn created_message(&self) -> String {
        let noun = self.kind.label();
        if self.created > 1 {
            format!("{noun} created, repeating {} times.", self.created)
        } else {
            format!("{noun} created.")
        }
    }
    fn repeats(&self) -> bool {
        self.kind != Kind::Note
            && self.value("repeat") != "none"
            && !self.value("repeat").is_empty()
    }
    /// The dialog filled from a draft (Close and create new's copy).
    pub async fn from_draft(
        pages: &Pages,
        scope: &Scope,
        clock: &Clock,
        draft: &Draft,
    ) -> Result<Self> {
        let related = match &draft.related {
            Some(reference) => pages
                .records
                .resolve(scope, std::slice::from_ref(reference))
                .await?
                .into_iter()
                .next(),
            None => None,
        };
        let mut dialog = Self::new(draft.kind, related);
        let mut put = |key: &str, value: String| {
            dialog.values.insert(key.into(), value);
        };
        put("title", draft.title.clone());
        put("description", draft.description.clone().unwrap_or_default());
        put(
            "internal_notes",
            draft.internal_notes.clone().unwrap_or_default(),
        );
        put("due_at", clock.local(draft.due_at));
        put("start_at", clock.local(draft.start_at));
        put("starts_at", clock.local(draft.starts_at));
        put("ends_at", clock.local(draft.ends_at));
        put("outcome", String::new());
        put("location", draft.location.clone().unwrap_or_default());
        put("join_url", draft.join_url.clone().unwrap_or_default());
        put(
            "meeting_type",
            draft.meeting_type.clone().unwrap_or_default(),
        );
        if let Some(priority) = draft.priority {
            put("priority", priority.code().into());
        }
        if let Some(direction) = draft.direction {
            put("direction", direction.code().into());
        }
        // A follow-up call is planned.
        put("status", "planned".into());
        if let Some(user) = draft.assignee.filter(|user| *user != scope.user()) {
            let names = pages.names(scope, &[user]).await?;
            dialog.assignee = Some((user, names.get(&user).cloned().unwrap_or_default()));
        }
        dialog.shared = draft.shared;
        dialog.guests = draft.guests.clone();
        Ok(dialog)
    }
    pub fn submit_event(&self) -> &'static str {
        match self.kind {
            Kind::Call => "log_call",
            Kind::Meeting => "create_meeting",
            Kind::Note => "create_note",
            Kind::Task => "create_task",
        }
    }
    fn contract(&self) -> &'static str {
        match self.kind {
            Kind::Call => "core.activity.log_call",
            Kind::Meeting => "core.activity.create_meeting",
            Kind::Note => "core.activity.create_note",
            Kind::Task => "core.activity.create_task",
        }
    }
    fn value(&self, key: &str) -> &str {
        self.values.get(key).map_or("", String::as_str)
    }

    /// The dialog's own events; `false` for an event it does not own.
    pub async fn event(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        name: &str,
        value: &Value,
    ) -> Result<bool> {
        match name {
            "activity_change" => self.read(pages, scope, value).await?,
            "pick_related" => {
                let (kind, id) = value["record"]
                    .as_str()
                    .and_then(|v| v.split_once(':'))
                    .ok_or(Error::Invalid)?;
                self.related = pages
                    .records
                    .resolve(scope, &[Reference::new(kind, id)])
                    .await?
                    .into_iter()
                    .next();
                self.errors.remove("related");
                self.hits.clear();
                self.query.clear();
            }
            "clear_related" => {
                self.related = None;
                self.hits.clear();
                self.query.clear();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    async fn read(&mut self, pages: &Pages, scope: &Scope, value: &Value) -> Result<()> {
        let form = params::form_values_checked(value, FORM, LIMIT).map_err(Error::refused)?;
        for key in [
            "title",
            "due_at",
            "start_at",
            "priority",
            "starts_at",
            "ends_at",
            "status",
            "direction",
            "outcome",
            "location",
            "join_url",
            "meeting_type",
            "description",
            "internal_notes",
            "repeat",
            "repeat_every",
            "repeat_end",
            "repeat_count",
            "repeat_until",
            "repeat_working",
            "reminders",
            "reminder_email",
        ] {
            if let Some(v) = form.get(key) {
                self.values.insert(key.into(), v.clone());
            }
        }
        // The weekly days come as an array; absent is none chosen.
        if form.contains_key("repeat") && self.value("repeat") == "weekly" {
            self.days = value
                .get(DAYS)
                .and_then(Value::as_array)
                .map(|days| {
                    days.iter()
                        .filter_map(Value::as_str)
                        .filter(|day| crate::weekday(day).is_some())
                        .map(str::to_owned)
                        .take(7)
                        .collect()
                })
                .unwrap_or_default();
        }
        if form.get("assignee").map(String::as_str) == Some("me") {
            self.assignee = None;
        }
        if let Some(shared) = form.get("shared") {
            self.shared = shared == "true";
        }
        let query = form.get("related_search").cloned().unwrap_or_default();
        if query != self.query {
            self.query = query.chars().take(100).collect();
            self.hits = if self.query.trim().is_empty() {
                Vec::new()
            } else {
                let kinds: Vec<&str> = pages
                    .records
                    .kinds()
                    .map(|kind| kind.key)
                    .filter(|key| *key != RECORD)
                    .take(bilibi_records::MAX_SEARCH_KINDS)
                    .collect();
                pages.records.search(scope, &kinds, &self.query).await?
            };
        }
        if self.submitted {
            self.errors.clear();
        }
        Ok(())
    }

    /// The contract input, or the fields the dialog refuses before asking.
    fn input(&mut self, clock: &Clock) -> Option<Value> {
        let mut errors: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut time = |key: &str| match clock.parse(self.value(key)) {
            Ok(time) => time.map(|t| t.to_string()),
            Err(()) => {
                errors
                    .entry(key.into())
                    .or_default()
                    .push("is not a date and time".into());
                None
            }
        };
        let mut input = json!({"title": self.value("title")});
        match self.kind {
            Kind::Task => {
                for key in ["due_at", "start_at"] {
                    if let Some(at) = time(key) {
                        input[key] = json!(at);
                    }
                }
                input["priority"] = json!(self.value("priority"));
            }
            Kind::Call => {
                for key in ["starts_at", "ends_at"] {
                    match time(key) {
                        Some(at) => input[key] = json!(at),
                        None => input[key] = json!(""),
                    }
                }
                input["status"] = json!(self.value("status"));
                input["direction"] = json!(self.value("direction"));
                if !self.value("outcome").trim().is_empty() {
                    input["outcome"] = json!(self.value("outcome"));
                }
            }
            Kind::Meeting => {
                for key in ["starts_at", "ends_at"] {
                    match time(key) {
                        Some(at) => input[key] = json!(at),
                        None => input[key] = json!(""),
                    }
                }
                for key in ["location", "join_url", "meeting_type"] {
                    if !self.value(key).trim().is_empty() {
                        input[key] = json!(self.value(key));
                    }
                }
            }
            Kind::Note => {}
        }
        for key in ["description", "internal_notes"] {
            if !self.value(key).trim().is_empty()
                && (key == "description" || self.kind != Kind::Note)
            {
                input[key] = json!(self.value(key));
            }
        }
        if self.kind != Kind::Note {
            if let Some(minutes) = reminder_choices::parse(self.value("reminders")).flatten() {
                input["reminder_minutes"] = json!(minutes);
            }
            if let Some(email) =
                reminder_choices::parse_email(self.value("reminder_email")).flatten()
            {
                input["reminder_email"] = json!(email);
            }
        }
        if let Some(related) = &self.related {
            input["related"] = json!({"kind": related.reference.kind, "id": related.id});
        }
        if let Some((user, _)) = &self.assignee {
            input["assignee_user_id"] = json!(user);
        }
        if self.shared {
            input["shared"] = json!(true);
        }
        if self.repeats() {
            let mut repeat = json!({
                "frequency": self.value("repeat"),
                "timezone": clock.zone,
                "working_days_only": self.value("repeat_working") == "true",
            });
            match self.value("repeat_every").trim().parse::<i64>() {
                Ok(every) => repeat["every"] = json!(every),
                Err(_) => errors
                    .entry("repeat_every".into())
                    .or_default()
                    .push("must say how many days, weeks or months apart".into()),
            }
            if self.value("repeat") == "weekly" && !self.days.is_empty() {
                repeat["weekdays"] = json!(self.days);
            }
            if self.value("repeat_end") == "until" {
                match self
                    .value("repeat_until")
                    .trim()
                    .parse::<jiff::civil::Date>()
                {
                    Ok(until) => repeat["until"] = json!(until.to_string()),
                    Err(_) => errors
                        .entry("repeat_until".into())
                        .or_default()
                        .push("needs the last date".into()),
                }
            } else {
                match self.value("repeat_count").trim().parse::<i64>() {
                    Ok(count) => repeat["count"] = json!(count),
                    Err(_) => errors
                        .entry("repeat_count".into())
                        .or_default()
                        .push("needs the number of times".into()),
                }
            }
            input["repeat"] = repeat;
        }
        if !self.guests.is_empty() {
            input["guests"] = json!(self
                .guests
                .iter()
                .map(|g| match g {
                    Guest::User(id) => json!({"user_id": id}),
                    Guest::Person(id) => json!({"person_id": id.get()}),
                    Guest::Address { email, related } =>
                        json!({"email": email, "related": related}),
                })
                .collect::<Vec<_>>());
        }
        if errors.is_empty() {
            Some(input)
        } else {
            self.errors = errors;
            None
        }
    }

    /// Saves through the kind's contract. `Some` is the new activity's id.
    pub async fn submit(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        credential: &str,
        clock: &Clock,
        value: &Value,
    ) -> Result<Option<i64>> {
        self.read(pages, scope, value).await?;
        self.submitted = true;
        self.errors.clear();
        self.notice.clear();
        let Some(input) = self.input(clock) else {
            return Ok(None);
        };
        let event = self.submit_event();
        let call = pages.call(scope, credential, event, self.contract());
        match pages.contracts.submit(&call, input).await {
            Ok(result) => {
                self.created = result["result"]["occurrences"]
                    .as_array()
                    .map_or(1, Vec::len);
                Ok(result["result"]["id"].as_i64())
            }
            Err(error) => {
                match refusal(
                    error,
                    "You do not have permission to create activities.",
                    "The activity was not saved. Try again.",
                )? {
                    Refusal::Fields(fields) => {
                        for (path, messages) in fields {
                            let field = field_of(&path);
                            if field == "notice" {
                                self.notice = messages.join(" ");
                            } else {
                                self.errors
                                    .entry(field.to_owned())
                                    .or_default()
                                    .extend(messages);
                            }
                        }
                    }
                    Refusal::Message(message) => self.notice = message,
                }
                Ok(None)
            }
        }
    }

    fn sentences(&self, key: &str) -> Vec<String> {
        self.errors
            .get(key)
            .map(|messages| {
                messages
                    .iter()
                    .map(|m| error_sentence(label_of(key), m))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn field(&self, prefix: &str, key: &str, label: &str) -> Field {
        Field::in_form(FORM, key)
            .id(format!("{prefix}-{}", key.replace('_', "-")))
            .label(label)
            .value(self.value(key))
            .errors(self.sentences(key))
            .attr("data-debounce", "300")
    }

    /// How it repeats: the frequency, how far apart, the weekly days, when
    /// it ends and whether only working days count.
    fn repeat_fields(&self, prefix: &str) -> Vec<Field> {
        let mut fields = vec![self
            .field(prefix, "repeat", "Repeats")
            .kind(InputKind::Select)
            .options(choices(&[
                ("Does not repeat", "none"),
                ("Daily", "daily"),
                ("Weekly", "weekly"),
                ("Monthly", "monthly"),
            ]))];
        if !self.repeats() {
            return fields;
        }
        let unit = match self.value("repeat") {
            "daily" => "days",
            "weekly" => "weeks",
            _ => "months",
        };
        fields.push(
            self.field(prefix, "repeat_every", "Every")
                .kind(InputKind::Number)
                .attr("min", "1")
                .attr("max", "99")
                .hint(format!("How many {unit} apart.")),
        );
        if self.value("repeat") == "weekly" {
            fields.push(
                Field::named(DAYS)
                    .id(format!("{prefix}-repeat-days"))
                    .label("On")
                    .kind(InputKind::Select)
                    .multiple()
                    .values(self.days.clone())
                    .options(choices(
                        &crate::WEEKDAYS
                            .iter()
                            .map(|(_, code, label)| (*label, *code))
                            .collect::<Vec<_>>(),
                    ))
                    .hint("None chosen: the day it starts on."),
            );
        }
        fields.push(
            self.field(prefix, "repeat_end", "Ends")
                .kind(InputKind::Select)
                .options(choices(&[
                    ("After a number of times", "count"),
                    ("On a date", "until"),
                ])),
        );
        if self.value("repeat_end") == "until" {
            fields.push(
                self.field(prefix, "repeat_until", "Last date")
                    .kind(InputKind::Date)
                    .required(),
            );
        } else {
            fields.push(
                self.field(prefix, "repeat_count", "Times")
                    .kind(InputKind::Number)
                    .attr("min", "1")
                    .attr("max", &crate::MAX_OCCURRENCES.to_string())
                    .required(),
            );
        }
        fields.push(
            Field::in_form(FORM, "repeat_working")
                .id(format!("{prefix}-repeat-working"))
                .label("Working days only")
                .kind(InputKind::Checkbox)
                .checked(self.value("repeat_working") == "true")
                .hint("Leaves out the days your company's work calendar marks as rest, off or holiday."),
        );
        fields
    }

    /// The dialog: its fields for the kind, the related-record picker, and
    /// Cancel and Save.
    pub fn render(
        &self,
        pages: &Pages,
        clock: &Clock,
        prefix: &str,
        cancel_event: &str,
        framed: bool,
    ) -> Result<Html> {
        let mut fields = vec![self
            .field(prefix, "title", "Subject")
            .required()
            .attr("maxlength", "160")];
        let zone = clock.hint();
        match self.kind {
            Kind::Task => {
                fields.push(
                    self.field(prefix, "due_at", "Due")
                        .kind(InputKind::DatetimeLocal)
                        .hint(&zone),
                );
                fields.push(
                    self.field(prefix, "start_at", "Start")
                        .kind(InputKind::DatetimeLocal),
                );
                fields.push(
                    self.field(prefix, "priority", "Priority")
                        .kind(InputKind::Select)
                        .options(choices(
                            &Priority::ALL
                                .iter()
                                .map(|p| (p.label(), p.code()))
                                .collect::<Vec<_>>(),
                        )),
                );
            }
            Kind::Call => {
                fields.push(
                    self.field(prefix, "status", "Status")
                        .kind(InputKind::Select)
                        .options(choices(&[("Held", "held"), ("Planned", "planned")])),
                );
                fields.push(
                    self.field(prefix, "direction", "Direction")
                        .kind(InputKind::Select)
                        .options(choices(&[("Outbound", "outbound"), ("Inbound", "inbound")])),
                );
                fields.push(
                    self.field(prefix, "starts_at", "Starts")
                        .kind(InputKind::DatetimeLocal)
                        .required()
                        .hint(&zone),
                );
                fields.push(
                    self.field(prefix, "ends_at", "Ends")
                        .kind(InputKind::DatetimeLocal)
                        .required(),
                );
                fields.push(
                    self.field(prefix, "outcome", "Outcome")
                        .attr("maxlength", "100"),
                );
            }
            Kind::Meeting => {
                fields.push(
                    self.field(prefix, "starts_at", "Starts")
                        .kind(InputKind::DatetimeLocal)
                        .required()
                        .hint(&zone),
                );
                fields.push(
                    self.field(prefix, "ends_at", "Ends")
                        .kind(InputKind::DatetimeLocal)
                        .required(),
                );
                fields.push(
                    self.field(prefix, "location", "Location")
                        .attr("maxlength", "255"),
                );
                fields.push(
                    self.field(prefix, "join_url", "Join link")
                        .kind(InputKind::Url)
                        .attr("maxlength", "2000")
                        .hint("An https:// link to the video call, if there is one."),
                );
                fields.push(
                    self.field(prefix, "meeting_type", "Meeting type")
                        .attr("maxlength", "100"),
                );
            }
            Kind::Note => {}
        }
        if self.kind != Kind::Note {
            fields.extend(self.repeat_fields(prefix));
            let options = reminder_choices::options(None, "Default");
            fields.push(
                self.field(prefix, "reminders", "Reminders")
                    .kind(InputKind::Select)
                    .options(choices(
                        &options
                            .iter()
                            .map(|(l, v)| (l.as_str(), v.as_str()))
                            .collect::<Vec<_>>(),
                    ))
                    .hint("Default is your own setting, then the organisation's. Change yours with Reminder defaults on the Activities list."),
            );
            fields.push(
                self.field(prefix, "reminder_email", "Email reminders")
                    .kind(InputKind::Select)
                    .options(choices(reminder_choices::EMAIL))
                    .hint("Emails the assignee, and guests without an account who were sent the invitation."),
            );
        }
        let mut assignees = vec![("Me".to_owned(), "me".to_owned())];
        if let Some((user, name)) = &self.assignee {
            assignees.insert(0, (name.clone(), user.to_string()));
        }
        if self.kind != Kind::Note {
            fields.push(
                Field::in_form(FORM, "assignee")
                    .id(format!("{prefix}-assignee"))
                    .label("Assigned to")
                    .kind(InputKind::Select)
                    .value(
                        self.assignee
                            .as_ref()
                            .map_or("me".to_owned(), |(user, _)| user.to_string()),
                    )
                    .options(bilibi_ui::choices(
                        &assignees
                            .iter()
                            .map(|(l, v)| (l.as_str(), v.as_str()))
                            .collect::<Vec<_>>(),
                    ))
                    .errors(self.sentences("assignee")),
            );
        }
        fields.push(
            self.field(prefix, "description", "Description")
                .kind(InputKind::Textarea)
                .attr("rows", "3"),
        );
        if self.kind != Kind::Note {
            fields.push(
                self.field(prefix, "internal_notes", "Internal notes")
                    .kind(InputKind::Textarea)
                    .attr("rows", "2")
                    .hint("Never sent to guests."),
            );
        }
        let mut body = Html::EMPTY;
        if self.submitted && !self.errors.is_empty() {
            let mut summary = ErrorSummary::new(format!("{prefix}-errors"));
            for (key, messages) in &self.errors {
                for message in messages {
                    summary.push(
                        format!("{prefix}-{}", key.replace('_', "-")),
                        error_sentence(label_of(key), message),
                    );
                }
            }
            body.push(&render(summary)?);
        }
        if !self.notice.is_empty() {
            body.push(&render(Alert {
                kind: StatusKind::Error,
                id: &format!("{prefix}-notice"),
                body: Html::text(&self.notice),
                ..Alert::default()
            })?);
        }
        for field in fields {
            body.push(&render(field)?);
        }
        let selected = self.related.as_ref().map(|r| {
            (
                format!("{}:{}", r.reference.kind, r.id),
                r.pill(format!("{prefix}-related-pill"), framed),
            )
        });
        body.push(&render(RecordPicker::new(
            &format!("{prefix}-related"),
            FORM,
            "related",
            "Related to",
            &self.query,
            &pages.records.picker_hits(&self.hits),
            selected
                .as_ref()
                .map(|(value, pill)| (value.as_str(), pill.clone())),
            ("pick_related", "clear_related"),
            self.sentences("related"),
        ))?);
        body.push(&render(
            Field::in_form(FORM, "shared")
                .id(format!("{prefix}-shared"))
                .label("Share with readers of the related record")
                .kind(InputKind::Checkbox)
                .checked(self.shared)
                .errors(self.sentences("shared"))
                .hint("Otherwise only you, the assignee and the guests see it."),
        )?);
        if !self.guests.is_empty() {
            body.push(&Html::text(&format!(
                "{} guest{} from the closed activity will be invited again.",
                self.guests.len(),
                if self.guests.len() == 1 { "" } else { "s" }
            )));
        }
        let mut actions = render(Button {
            id: &format!("{prefix}-cancel"),
            button_type: "button",
            event: cancel_event,
            body: Html::text("Cancel"),
            ..Button::default()
        })?;
        actions.push(&render(Button {
            id: &format!("{prefix}-save"),
            button_type: "submit",
            variant: ButtonVariant::Primary,
            working: "Saving…",
            body: Html::text(match self.kind {
                Kind::Call => "Save call",
                Kind::Meeting => "Save meeting",
                Kind::Note => "Save note",
                Kind::Task => "Save task",
            }),
            ..Button::default()
        })?);
        let form = render(FormView {
            id: &format!("{prefix}-form"),
            submit: self.submit_event(),
            actions,
            body,
        })?;
        let title = match self.kind {
            Kind::Call => "Log a call",
            Kind::Meeting => "New meeting",
            Kind::Note => "New note",
            Kind::Task => "New task",
        };
        render(Modal {
            id: &format!("{prefix}-modal"),
            title,
            cancel_event,
            width: ModalWidth::Wide,
            body: form,
            ..Modal::default()
        })
    }
}
