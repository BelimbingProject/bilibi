//! Operation contracts for activities. The pages and panels call the write
//! contracts too, so every door shares one rule set. References are
//! `{kind, id}` in input; reads add `label` and `path`.
use crate::{
    Activities, Activity, Apply, CallStatus, Common, Direction, Draft, Filter, Frequency, Guest,
    GuestRow, Kind, Mine, Patch, Priority, Reference, Repeat, Response, State, Status, WriteError,
    MANAGE, MAX_OCCURRENCES, MAX_REMINDERS, MAX_REMINDER_MINUTES, VIEW,
};
use bilibi_agent_api::{
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_party_ids::PersonId;
use bilibi_records::Resolved;
use bilibi_tenancy::Scope;
use jiff::Timestamp;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

static OWNER: OnceLock<Arc<Activities>> = OnceLock::new();
pub(crate) fn bind(owner: Arc<Activities>) -> bilibi_module_registry::Result<()> {
    OWNER.set(owner).map_err(|_| {
        bilibi_module_registry::RegistryError::Service(
            "duplicate activity operations binding".into(),
        )
    })
}
pub(crate) fn owner() -> bilibi_agent_api::Result<&'static Activities> {
    OWNER.get().map(Arc::as_ref).ok_or(Error::Internal.into())
}

type Fields = BTreeMap<String, Vec<String>>;
fn refuse(fields: &mut Fields, path: &str, message: impl Into<String>) {
    fields.entry(path.into()).or_default().push(message.into());
}

/// Reads the typed input. A value the schema accepts but the rules refuse
/// (an unknown code, a malformed time) is a field error on its path.
struct Reader<'a> {
    input: &'a Value,
    prefix: &'static str,
    fields: Fields,
}
impl<'a> Reader<'a> {
    fn new(input: &'a Value, prefix: &'static str) -> Self {
        Self {
            input,
            prefix,
            fields: Fields::new(),
        }
    }
    fn at(&self) -> &'a Value {
        if self.prefix.is_empty() {
            self.input
        } else {
            &self.input[self.prefix]
        }
    }
    fn path(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.into()
        } else {
            format!("{}.{key}", self.prefix)
        }
    }
    fn refuse(&mut self, key: &str, message: &str) {
        let path = self.path(key);
        refuse(&mut self.fields, &path, message);
    }
    fn present(&self, key: &str) -> bool {
        self.at().get(key).is_some()
    }
    fn text(&self, key: &str) -> Option<String> {
        self.at()[key].as_str().map(str::to_owned)
    }
    /// `None` when omitted; `Some(None)` for an empty string.
    fn clearable_text(&self, key: &str) -> Option<Option<String>> {
        self.text(key).map(|v| (!v.trim().is_empty()).then_some(v))
    }
    fn time(&mut self, key: &str) -> Option<Option<Timestamp>> {
        let value = self.text(key)?;
        if value.trim().is_empty() {
            return Some(None);
        }
        match value.parse::<Timestamp>() {
            Ok(time) => Some(Some(time)),
            Err(_) => {
                self.refuse(key, "must be a date and time with an offset");
                None
            }
        }
    }
    fn code<T>(&mut self, key: &str, parse: fn(&str) -> Option<T>) -> Option<T> {
        let value = self.text(key)?;
        let parsed = parse(&value);
        if parsed.is_none() {
            self.refuse(key, "is not one of the allowed values");
        }
        parsed
    }
    fn reference(&mut self, key: &str) -> Option<Option<Reference>> {
        let value = self.at().get(key)?;
        let kind = value["kind"].as_str().unwrap_or_default();
        let id = value["id"].as_str().unwrap_or_default();
        Some((!kind.is_empty() || !id.is_empty()).then(|| Reference::new(kind, id)))
    }
    fn flag(&self, key: &str) -> Option<bool> {
        self.at()[key].as_bool()
    }
    /// `None` when omitted; else the offsets, each a whole number of minutes.
    fn minutes(&mut self, key: &str) -> Option<Option<Vec<i32>>> {
        let value = self.at().get(key)?;
        let parsed = value.as_array().and_then(|items| {
            items
                .iter()
                .map(|item| item.as_i64().and_then(|m| i32::try_from(m).ok()))
                .collect::<Option<Vec<i32>>>()
        });
        if parsed.is_none() {
            self.refuse(key, "must be a list of whole numbers of minutes");
        }
        parsed.map(Some)
    }
    /// `None` when omitted; `Some(None)` for null (the default).
    fn optional_flag(&mut self, key: &str) -> Option<Option<bool>> {
        let value = self.at().get(key)?;
        if value.is_null() {
            return Some(None);
        }
        if value.as_bool().is_none() {
            self.refuse(key, "must be true, false or null");
            return None;
        }
        Some(value.as_bool())
    }
    fn guests(&mut self) -> Vec<Guest> {
        let Some(items) = self.at().get("guests").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut guests = Vec::new();
        for (index, item) in items.iter().enumerate() {
            match guest(item) {
                Ok(guest) => guests.push(guest),
                Err((key, message)) => {
                    let path = if key.is_empty() {
                        format!("guests.{index}")
                    } else {
                        format!("guests.{index}.{key}")
                    };
                    self.refuse(&path, message)
                }
            }
        }
        guests
    }
    fn common(&mut self) -> Common {
        Common {
            related: self.reference("related").flatten(),
            shared: self.flag("shared").unwrap_or(false),
            title: self.text("title").unwrap_or_default(),
            assignee: self.at()["assignee_user_id"].as_i64(),
            guests: self.guests(),
            description: self.text("description"),
            internal_notes: self.text("internal_notes"),
            reminder_minutes: self.minutes("reminder_minutes").flatten(),
            reminder_email: self.optional_flag("reminder_email").flatten(),
        }
    }
    fn done<T>(self, value: T) -> bilibi_agent_api::Result<T> {
        if self.fields.is_empty() {
            Ok(value)
        } else {
            Err(bilibi_agent_api::Error::InvalidInput(self.fields))
        }
    }
}

/// One guest: an account by `user_id`, a person by `person_id`, or an
/// `email`. A refusal names the key it is on (empty for the guest itself).
fn guest(item: &Value) -> std::result::Result<Guest, (&'static str, &'static str)> {
    let user = item["user_id"].as_i64();
    let person = item["person_id"].as_i64();
    let email = item["email"].as_str().filter(|e| !e.trim().is_empty());
    match (user, person, email) {
        (Some(id), None, None) if id > 0 => Ok(Guest::User(id)),
        (Some(_), None, None) => Err(("user_id", "must be positive")),
        (None, Some(id), None) => PersonId::new(id)
            .map(Guest::Person)
            .map_err(|_| ("person_id", "must be positive")),
        (None, None, Some(email)) => Ok(Guest::Address {
            email: email.trim().to_owned(),
            related: item["related"].as_bool().unwrap_or(false),
        }),
        _ => Err(("", "must name exactly one of user_id, person_id or email")),
    }
}

/// The optional `repeat` of a create: how a call, meeting or task repeats.
fn repeat(input: &Value) -> bilibi_agent_api::Result<Option<Repeat>> {
    if input.get("repeat").is_none_or(Value::is_null) {
        return Ok(None);
    }
    let mut reader = Reader::new(input, "repeat");
    let frequency = reader.code("frequency", Frequency::parse);
    if !reader.present("frequency") {
        reader.refuse("frequency", "can't be blank");
    }
    let at = reader.at();
    let every = match at.get("every") {
        None => 1,
        Some(value) => match value.as_i64().and_then(|v| i16::try_from(v).ok()) {
            Some(every) => every,
            None => {
                reader.refuse("every", "must be a whole number from 1 to 99");
                1
            }
        },
    };
    let mut weekdays = Vec::new();
    if let Some(days) = at.get("weekdays").and_then(Value::as_array) {
        for day in days {
            match day.as_str().and_then(crate::weekday) {
                Some(day) => weekdays.push(day),
                None => reader.refuse(
                    "weekdays",
                    "must name days as mon, tue, wed, thu, fri, sat or sun",
                ),
            }
        }
    }
    let until = match reader.text("until") {
        Some(text) if !text.trim().is_empty() => match text.trim().parse::<jiff::civil::Date>() {
            Ok(date) => Some(date),
            Err(_) => {
                reader.refuse("until", "must be a date such as 2026-12-31");
                None
            }
        },
        _ => None,
    };
    let count = match at.get("count") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_i64().and_then(|v| i16::try_from(v).ok()) {
            Some(count) => Some(count),
            None => {
                reader.refuse("count", &format!("must be from 1 to {MAX_OCCURRENCES}"));
                None
            }
        },
    };
    let repeat = Repeat {
        frequency: frequency.unwrap_or(Frequency::Daily),
        every,
        weekdays,
        until,
        count,
        working_days_only: reader.flag("working_days_only").unwrap_or(false),
        timezone: reader.text("timezone").unwrap_or_default(),
    };
    reader.done(Some(repeat))
}

fn apply(input: &Value) -> bilibi_agent_api::Result<Apply> {
    input["apply"]
        .as_str()
        .and_then(Apply::parse)
        .ok_or_else(|| {
            bilibi_agent_api::Error::InvalidInput(BTreeMap::from([(
                "apply".to_owned(),
                vec!["must be following or all".to_owned()],
            )]))
        })
}

fn draft(kind: Kind, input: &Value) -> bilibi_agent_api::Result<Draft> {
    let mut reader = Reader::new(input, "");
    let common = reader.common();
    let draft = match kind {
        Kind::Task => {
            let due_at = reader.time("due_at").flatten();
            let start_at = reader.time("start_at").flatten();
            let priority = reader.code("priority", Priority::parse);
            Draft::task(&crate::TaskInput {
                common,
                due_at,
                start_at,
                priority,
            })
        }
        Kind::Call => {
            let starts_at = reader.time("starts_at").flatten();
            let ends_at = reader.time("ends_at").flatten();
            let status = match reader.text("status").as_deref() {
                Some("held") => CallStatus::Held,
                Some("planned") => CallStatus::Planned,
                _ => {
                    reader.refuse("status", "must be held or planned");
                    CallStatus::Held
                }
            };
            let direction = reader
                .code("direction", Direction::parse)
                .unwrap_or(Direction::Outbound);
            let outcome = reader.text("outcome");
            let mut draft = Draft::call(&crate::CallInput {
                common,
                starts_at: starts_at.unwrap_or_default(),
                ends_at: ends_at.unwrap_or_default(),
                status,
                outcome,
                direction,
            });
            draft.starts_at = starts_at;
            draft.ends_at = ends_at;
            draft
        }
        Kind::Meeting => {
            let starts_at = reader.time("starts_at").flatten();
            let ends_at = reader.time("ends_at").flatten();
            let mut draft = Draft::meeting(&crate::MeetingInput {
                common,
                starts_at: starts_at.unwrap_or_default(),
                ends_at: ends_at.unwrap_or_default(),
                location: reader.text("location"),
                join_url: reader.text("join_url"),
                meeting_type: reader.text("meeting_type"),
            });
            draft.starts_at = starts_at;
            draft.ends_at = ends_at;
            draft
        }
        Kind::Note => Draft::note(&crate::NoteInput { common }),
    };
    reader.done(draft)
}

fn patch(input: &Value) -> bilibi_agent_api::Result<Patch> {
    let mut reader = Reader::new(input, "patch");
    let patch = Patch {
        title: reader.text("title"),
        status: reader.code("status", Status::parse),
        priority: reader.code("priority", Priority::parse),
        direction: reader.code("direction", Direction::parse),
        starts_at: reader.time("starts_at").flatten(),
        ends_at: reader.time("ends_at").flatten(),
        due_at: reader.time("due_at"),
        start_at: reader.time("start_at"),
        outcome: reader.clearable_text("outcome"),
        location: reader.clearable_text("location"),
        join_url: reader.clearable_text("join_url"),
        meeting_type: reader.clearable_text("meeting_type"),
        description: reader.clearable_text("description"),
        internal_notes: reader.clearable_text("internal_notes"),
        related: reader.reference("related"),
        shared: reader.flag("shared"),
        assignee: reader.at()["assignee_user_id"].as_i64(),
        reminder_minutes: reader.minutes("reminder_minutes"),
        reminder_email: reader.optional_flag("reminder_email"),
    };
    let mut patch = patch;
    if reader.flag("reminder_minutes_default") == Some(true) {
        if patch.reminder_minutes.is_some() {
            reader.refuse(
                "reminder_minutes_default",
                "cannot come with reminder_minutes",
            );
        }
        patch.reminder_minutes = Some(None);
    }
    for key in ["starts_at", "ends_at"] {
        if reader.present(key) && reader.text(key).is_some_and(|v| v.trim().is_empty()) {
            reader.refuse(key, "can't be blank");
        }
    }
    reader.done(patch)
}

pub(crate) fn id(input: &Value) -> bilibi_agent_api::Result<i64> {
    Ok(input["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or(Error::Invalid)?)
}
fn version(input: &Value) -> bilibi_agent_api::Result<i64> {
    Ok(input["version"]
        .as_i64()
        .filter(|v| *v > 0)
        .ok_or(Error::Invalid)?)
}

/// An owner refusal, typed for every door. Field errors keep their input
/// path (`patch.` for an update).
pub(crate) fn invalid(error: WriteError, prefix: &str) -> bilibi_agent_api::Error {
    match error {
        WriteError::Access(error) => error.into(),
        WriteError::NotFound => bilibi_agent_api::Error::NotFound,
        WriteError::Forbidden => bilibi_agent_api::Error::Forbidden,
        WriteError::Stale => Error::Conflict.into(),
        WriteError::AlreadyClosed => {
            bilibi_agent_api::invalid("already_closed", WriteError::AlreadyClosed.to_string())
        }
        WriteError::AssigneeNotRemovable => {
            bilibi_agent_api::invalid("guest_id", WriteError::AssigneeNotRemovable.to_string())
        }
        WriteError::Invalid(errors) => {
            let mut fields = Fields::new();
            for e in errors {
                let path = if prefix.is_empty() || e.field == "company" {
                    e.field.to_owned()
                } else {
                    format!("{prefix}.{}", e.field)
                };
                refuse(&mut fields, &path, e.message);
            }
            bilibi_agent_api::Error::InvalidInput(fields)
        }
    }
}

fn iso(time: Option<Timestamp>) -> Value {
    time.map_or(Value::Null, |t| json!(t.to_string()))
}
fn reference(reference: &Reference, resolved: Option<&Resolved>) -> Value {
    let mut value = json!({"kind": reference.kind, "id": reference.id});
    if let Some(resolved) = resolved {
        value["label"] = json!(resolved.label);
        value["path"] = json!(resolved.path);
    }
    value
}
pub(crate) fn record(activity: &Activity, related: Option<&Resolved>) -> Value {
    json!({
        "id": activity.id,
        "kind": activity.kind.code(),
        "title": activity.title,
        "status": activity.status.map(Status::code),
        "priority": activity.priority.map(Priority::code),
        "direction": activity.direction.map(Direction::code),
        "starts_at": iso(activity.starts_at),
        "ends_at": iso(activity.ends_at),
        "due_at": iso(activity.due_at),
        "start_at": iso(activity.start_at),
        "outcome": activity.outcome,
        "location": activity.location,
        "join_url": activity.join_url,
        "meeting_type": activity.meeting_type,
        "description": activity.description,
        "internal_notes": activity.internal_notes,
        "related": activity.related.as_ref().map(|r| reference(r, related)),
        "shared": activity.shared,
        "reminded_at": iso(activity.reminded_at),
        "reminder_minutes": activity.reminder_minutes,
        "reminder_email": activity.reminder_email,
        "assignee_user_id": activity.assignee_user_id,
        "created_by": activity.created_by,
        "company_id": activity.company_id,
        "created_at": activity.created_at.to_string(),
        "updated_at": activity.updated_at.to_string(),
        "closed_at": iso(activity.closed_at),
        "series_id": activity.series_id,
        "series_index": activity.series_index,
        "version": activity.version,
    })
}
fn series_record(series: &crate::Series) -> Value {
    json!({
        "id": series.id,
        "rule": series.describe(),
        "frequency": series.repeat.frequency.code(),
        "every": series.repeat.every,
        "weekdays": series.repeat.weekdays.iter().map(|d| crate::weekday_code(*d)).collect::<Vec<_>>(),
        "month_day": series.month_day,
        "until": series.repeat.until.map(|d| d.to_string()),
        "count": series.repeat.count,
        "working_days_only": series.repeat.working_days_only,
        "work_calendar": series.work_calendar,
        "timezone": series.repeat.timezone,
        "live_occurrences": series.live,
    })
}
fn draft_record(records: &bilibi_records::Records, draft: &Draft) -> Value {
    json!({
        "kind": draft.kind.code(),
        "title": draft.title,
        "status": draft.status.map(Status::code),
        "priority": draft.priority.map(Priority::code),
        "direction": draft.direction.map(Direction::code),
        "starts_at": iso(draft.starts_at),
        "ends_at": iso(draft.ends_at),
        "due_at": iso(draft.due_at),
        "start_at": iso(draft.start_at),
        "outcome": draft.outcome,
        "location": draft.location,
        "join_url": draft.join_url,
        "meeting_type": draft.meeting_type,
        "description": draft.description,
        "internal_notes": draft.internal_notes,
        "related": draft.related.as_ref().map(|r| {
            let path = records.kind(&r.kind).and_then(|kind| kind.path_of(&r.id));
            let mut value = json!({"kind": r.kind, "id": r.id});
            if let Some(path) = path {
                value["path"] = json!(path);
            }
            value
        }),
        "shared": draft.shared,
        "assignee_user_id": draft.assignee,
        "guests": draft.guests.iter().map(guest_input).collect::<Vec<_>>(),
    })
}
fn guest_input(guest: &Guest) -> Value {
    match guest {
        Guest::User(id) => json!({"user_id": id}),
        Guest::Person(id) => json!({"person_id": id.get()}),
        Guest::Address { email, related } => json!({"email": email, "related": related}),
    }
}
pub(crate) fn guest_record(guest: &GuestRow) -> Value {
    json!({
        "id": guest.id,
        "user_id": guest.user_id,
        "person_id": guest.person.map(|p| p.get()),
        "email": guest.email,
        "role": guest.role,
        "response": guest.response,
        "invited_at": iso(guest.invited_at),
        "invite_sent_at": iso(guest.invite_sent_at),
    })
}

struct List;
impl ReadHandler for List {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn call<'a>(
        &'a self,
        _: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let mut reader = Reader::new(input, "");
            let kind = reader.code("kind", Kind::parse);
            let state = match reader.text("state").as_deref() {
                None | Some("open") => State::Open,
                Some("closed") => State::Closed,
                _ => State::All,
            };
            let mine = match reader.text("mine").as_deref() {
                Some("assignee") => Mine::Assignee,
                Some("guest") => Mine::Guest,
                _ => Mine::Any,
            };
            let related = reader.reference("related").flatten();
            let from = reader.time("from").flatten();
            let to = reader.time("to").flatten();
            let between = match (from, to) {
                (None, None) => None,
                (Some(from), Some(to)) if from < to => Some((from, to)),
                (Some(_), Some(_)) => {
                    reader.refuse("to", "must be after from");
                    None
                }
                _ => {
                    reader.refuse("from", "needs both from and to");
                    None
                }
            };
            let series = input["series_id"].as_i64().filter(|id| *id > 0);
            let filter = Filter {
                search: reader.text("search").unwrap_or_default(),
                kind,
                state,
                mine,
                related,
                between,
                series,
                descending: state == State::Closed,
                page: input["page"].as_i64().unwrap_or(1).max(1),
                page_size: input["page_size"].as_i64().unwrap_or(25).clamp(
                    1,
                    if between.is_some() {
                        crate::CALENDAR_LIMIT - 1
                    } else {
                        100
                    },
                ),
                ..Filter::default()
            };
            let filter = reader.done(filter)?;
            let owner = owner()?;
            let page = owner.list(scope, &filter).await?;
            let related: Vec<Reference> = page
                .entries
                .iter()
                .filter_map(|a| a.related.clone())
                .collect();
            let named = owner.records().resolve(scope, &related).await?;
            let mut names = named.iter();
            let activities: Vec<Value> = page
                .entries
                .iter()
                .map(|a| record(a, a.related.as_ref().and_then(|_| names.next())))
                .collect();
            Ok(json!({"activities": activities, "total": page.total, "page": filter.page, "page_size": filter.page_size}).into())
        })
    }
}

struct Get;
impl ReadHandler for Get {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn call<'a>(
        &'a self,
        _: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let owner = owner()?;
            let id = id(input)?;
            let activity = owner.get(scope, id).await?;
            let guests = owner.guests(scope, id).await?;
            let named = match &activity.related {
                Some(related) => {
                    owner
                        .records()
                        .resolve(scope, std::slice::from_ref(related))
                        .await?
                }
                None => Vec::new(),
            };
            let mut value = record(&activity, named.first());
            value["guests"] = json!(guests.iter().map(guest_record).collect::<Vec<_>>());
            if let Some(series) = owner.series(scope, &activity).await? {
                value["series"] = series_record(&series);
            }
            Ok(value.into())
        })
    }
}

struct Create(Kind);
impl WriteHandler for Create {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let draft = draft(self.0, input)?;
            let owner = owner()?;
            if let Some(repeat) = repeat(input)? {
                let (checked, occurrences) = owner
                    .preview_create_series(client, scope, &draft, &repeat)
                    .await
                    .map_err(|e| invalid(e, ""))?;
                let times: Vec<Value> = occurrences
                    .iter()
                    .map(|o| iso(o.starts_at.or(o.due_at)))
                    .collect();
                return Ok(json!({
                    "creates": draft_record(owner.records(), &checked),
                    "repeats": {"rule": repeat.describe(None), "occurrences": times.len(), "at": times},
                })
                .into());
            }
            let checked = owner
                .preview_create(client, scope, &draft)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"creates": draft_record(owner.records(), &checked)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let draft = draft(self.0, input)?;
            if let Some(repeat) = repeat(input)? {
                let created = owner()?
                    .create_series(tx, scope, &draft, &repeat)
                    .await
                    .map_err(|e| invalid(e, ""))?;
                let first = created.first().ok_or(Error::Internal)?;
                let mut value = record(first, None);
                value["occurrences"] = json!(created
                    .iter()
                    .map(|a| json!({"id": a.id, "at": iso(a.starts_at.or(a.due_at)), "version": a.version}))
                    .collect::<Vec<_>>());
                return Ok(value.into());
            }
            let activity = owner()?
                .create(tx, scope, &draft)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(record(&activity, None).into())
        })
    }
}

struct Update;
impl WriteHandler for Update {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version, patch) = (id(input)?, version(input)?, patch(input)?);
            let owner = owner()?;
            let (before, after) = owner
                .preview_update(client, scope, id, version, &patch)
                .await
                .map_err(|e| invalid(e, "patch"))?;
            Ok(json!({"before": record(&before, None), "after": draft_record(owner.records(), &after)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version, patch) = (id(input)?, version(input)?, patch(input)?);
            let activity = owner()?
                .update(tx, scope, id, version, &patch)
                .await
                .map_err(|e| invalid(e, "patch"))?;
            Ok(record(&activity, None).into())
        })
    }
}

struct Close;
impl WriteHandler for Close {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version) = (id(input)?, version(input)?);
            let activity = owner()?
                .preview_close(client, scope, id, version)
                .await
                .map_err(|e| invalid(e, ""))?;
            let status = activity.kind.closing().map(Status::code);
            Ok(json!({"closes": record(&activity, None), "status": status}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version) = (id(input)?, version(input)?);
            let activity = owner()?
                .close(tx, scope, id, version)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(record(&activity, None).into())
        })
    }
}

struct Delete;
impl WriteHandler for Delete {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version) = (id(input)?, version(input)?);
            let activity = owner()?
                .preview_delete(client, scope, id, version)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"deletes": record(&activity, None)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version) = (id(input)?, version(input)?);
            owner()?
                .delete(tx, scope, id, version)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"id": id, "deleted": true}).into())
        })
    }
}

struct UpdateSeries;
impl WriteHandler for UpdateSeries {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version, apply, patch) =
                (id(input)?, version(input)?, apply(input)?, patch(input)?);
            let owner = owner()?;
            let changes = owner
                .preview_update_series(client, scope, id, version, apply, &patch)
                .await
                .map_err(|e| invalid(e, "patch"))?;
            Ok(json!({"changes": changes
                .iter()
                .map(|(before, after)| json!({"before": record(before, None), "after": draft_record(owner.records(), after)}))
                .collect::<Vec<_>>()})
            .into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version, apply, patch) =
                (id(input)?, version(input)?, apply(input)?, patch(input)?);
            let updated = owner()?
                .update_series(tx, scope, id, version, apply, &patch)
                .await
                .map_err(|e| invalid(e, "patch"))?;
            Ok(
                json!({"updated": updated.iter().map(|a| record(a, None)).collect::<Vec<_>>()})
                    .into(),
            )
        })
    }
}

struct DeleteSeries;
impl WriteHandler for DeleteSeries {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version, apply) = (id(input)?, version(input)?, apply(input)?);
            let members = owner()?
                .preview_delete_series(client, scope, id, version, apply)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(
                json!({"deletes": members.iter().map(|a| record(a, None)).collect::<Vec<_>>()})
                    .into(),
            )
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version, apply) = (id(input)?, version(input)?, apply(input)?);
            let members = owner()?
                .delete_series(tx, scope, id, version, apply)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"deleted": members.iter().map(|a| a.id).collect::<Vec<_>>()}).into())
        })
    }
}

pub(crate) fn guest_id(input: &Value) -> bilibi_agent_api::Result<i64> {
    Ok(input["guest_id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or(Error::Invalid)?)
}

struct AddGuest;
fn guest_of(input: &Value) -> bilibi_agent_api::Result<Guest> {
    guest(&input["guest"]).map_err(|(key, message)| {
        let path = if key.is_empty() {
            "guest".to_owned()
        } else {
            format!("guest.{key}")
        };
        bilibi_agent_api::Error::InvalidInput(BTreeMap::from([(path, vec![message.to_owned()])]))
    })
}
impl WriteHandler for AddGuest {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, guest) = (id(input)?, guest_of(input)?);
            let (activity, guest) = owner()?
                .preview_add_guest(client, scope, id, &guest)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"activity": record(&activity, None), "adds": guest_input(&guest)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, guest) = (id(input)?, guest_of(input)?);
            let row = owner()?
                .add_guest(tx, scope, id, &guest)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(guest_record(&row).into())
        })
    }
}

struct RemoveGuest;
impl WriteHandler for RemoveGuest {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (activity, row) = owner()?
                .preview_remove_guest(client, scope, id(input)?, guest_id(input)?)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"activity": record(&activity, None), "removes": guest_record(&row)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let row = owner()?
                .remove_guest(tx, scope, id(input)?, guest_id(input)?)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"removed": guest_record(&row)}).into())
        })
    }
}

struct Respond;
fn response(input: &Value) -> bilibi_agent_api::Result<Response> {
    input["response"]
        .as_str()
        .and_then(Response::parse)
        .ok_or_else(|| {
            bilibi_agent_api::Error::InvalidInput(BTreeMap::from([(
                "response".to_owned(),
                vec!["is not one of the allowed values".to_owned()],
            )]))
        })
}
impl WriteHandler for Respond {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let answer = response(input)?;
            let (activity, row) = owner()?
                .preview_respond(client, scope, id(input)?, guest_id(input)?)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"activity": record(&activity, None), "guest": guest_record(&row), "response": answer.code()}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let answer = response(input)?;
            let row = owner()?
                .respond(tx, scope, id(input)?, guest_id(input)?, answer)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(guest_record(&row).into())
        })
    }
}

struct SendInvites;
fn invites_record(invites: &crate::Invites) -> Value {
    json!({
        "activity": record(&invites.activity, None),
        "guests": invites.guests.iter().map(guest_record).collect::<Vec<_>>(),
        "notified_in_bell": invites.guests.iter().filter(|g| g.user_id.is_some()).count(),
        "emailed": invites.emailable,
    })
}
impl WriteHandler for SendInvites {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let invites = owner()?
                .preview_send_invites(client, scope, id(input)?)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(invites_record(&invites).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let invites = owner()?
                .send_invites(tx, scope, id(input)?)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(invites_record(&invites).into())
        })
    }
}

// ---- Schemas ----------------------------------------------------------------

fn codes(values: &[&'static str], empty: bool) -> Value {
    let mut values: Vec<Value> = values.iter().map(|v| json!(v)).collect();
    if empty {
        values.push(json!(""));
    }
    Value::Array(values)
}
fn time_schema(description: &str) -> Value {
    json!({"type":"string","maxLength":40,"description":description})
}
fn reference_schema() -> Value {
    json!({"type":"object","required":["kind","id"],"description":"A record reference from base.records.search: its kind (person, company, …) and id.","properties":{
        "kind":{"type":"string","maxLength":100},
        "id":{"type":"string","maxLength":64}
    }})
}
fn shared_schema() -> Value {
    json!({"type":"boolean","description":"true: people who may view the related record see it too. false (the default): only you, the assignee and account guests."})
}
fn guest_schema() -> Value {
    json!({"type":"object","description":"Exactly one of user_id (a colleague's account), person_id (a Core person) or email. Set related on the email address of the record the activity is about (a lead).","properties":{
        "user_id":{"type":"integer","minimum":1},
        "person_id":{"type":"integer","minimum":1},
        "email":{"type":"string","format":"email","maxLength":254},
        "related":{"type":"boolean"}
    }})
}
fn guests_schema() -> Value {
    json!({"type":"array","description":"People invited: colleagues by user_id, Core people by person_id, or email addresses.","items":guest_schema()})
}
fn common_properties() -> serde_json::Map<String, Value> {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "title".into(),
        json!({"type":"string","maxLength":160,"description":"The subject, 1–160 characters."}),
    );
    properties.insert("related".into(), reference_schema());
    properties.insert("shared".into(), shared_schema());
    properties.insert(
        "assignee_user_id".into(),
        json!({"type":"integer","minimum":1,"description":"An active account; you when omitted."}),
    );
    properties.insert("guests".into(), guests_schema());
    properties.insert(
        "description".into(),
        json!({"type":"string","maxLength":10000}),
    );
    properties.insert(
        "internal_notes".into(),
        json!({"type":"string","maxLength":10000,"description":"Never sent to guests."}),
    );
    // Notes have no reminders; the rule refuses them there.
    properties.insert("reminder_minutes".into(), reminder_minutes_schema());
    properties.insert("reminder_email".into(), reminder_email_schema());
    properties
}
fn reminder_minutes_schema() -> Value {
    json!({"type":"array","items":{"type":"integer","minimum":0,"maximum":MAX_REMINDER_MINUTES},"description":format!("Minutes before it starts or is due to remind at, up to {MAX_REMINDERS} (0 reminds at the time). An empty list asks for no reminder; omitted follows your default, then the organisation's.")})
}
fn reminder_email_schema() -> Value {
    json!({"type":"boolean","nullable":true,"description":"Also email each reminder: to the assignee, and for a call or meeting to guests who were sent the invitation and have no account. Omitted follows your default, then the organisation's; in a change, null goes back to following them."})
}
fn repeat_schema() -> Value {
    json!({"type":"object","required":["frequency","timezone"],"description":"Repeat it: one activity per occurrence, each with its own status, answers and reminder. Give until or count, not both.","properties":{
        "frequency":{"type":"string","enum":["daily","weekly","monthly"]},
        "every":{"type":"integer","minimum":1,"maximum":99,"description":"Every this many days, weeks or months; 1 when omitted."},
        "weekdays":{"type":"array","items":{"type":"string","enum":["mon","tue","wed","thu","fri","sat","sun"]},"description":"Weekly: the days; the first occurrence's own day when omitted. A first date off these days moves to the next one that matches."},
        "until":{"type":"string","maxLength":10,"description":"The last date an occurrence may fall on, YYYY-MM-DD in timezone."},
        "count":{"type":"integer","minimum":1,"maximum":MAX_OCCURRENCES,"description":"How many occurrences."},
        "working_days_only":{"type":"boolean","description":"Leave out the days your company's work calendar does not make working days (weekends, holidays)."},
        "timezone":{"type":"string","maxLength":64,"description":"The IANA zone the time repeats in, such as Asia/Kuala_Lumpur, so 09:00 stays 09:00."}
    }})
}
fn task_schema() -> Value {
    let mut properties = common_properties();
    properties.insert(
        "due_at".into(),
        time_schema("When it is due: a date and time with an offset."),
    );
    properties.insert(
        "start_at".into(),
        time_schema("When work starts: a date and time with an offset."),
    );
    properties.insert("priority".into(), json!({"type":"string","enum":codes(&["high","medium","low"], false),"description":"Medium when omitted."}));
    properties.insert("repeat".into(), repeat_schema());
    json!({"type":"object","required":["title"],"properties":properties})
}
fn call_schema() -> Value {
    let mut properties = common_properties();
    properties.insert(
        "starts_at".into(),
        time_schema("When the call started or starts, with an offset."),
    );
    properties.insert(
        "ends_at".into(),
        time_schema("When it ended or ends; after starts_at. A held call has ended."),
    );
    properties.insert("status".into(), json!({"type":"string","enum":["held","planned"],"description":"held for a call that took place, planned for one to make."}));
    properties.insert(
        "outcome".into(),
        json!({"type":"string","maxLength":100,"description":"How the call went."}),
    );
    properties.insert(
        "direction".into(),
        json!({"type":"string","enum":["inbound","outbound"]}),
    );
    properties.insert("repeat".into(), repeat_schema());
    json!({"type":"object","required":["title","starts_at","ends_at","status","direction"],"properties":properties})
}
fn meeting_schema() -> Value {
    let mut properties = common_properties();
    properties.insert(
        "starts_at".into(),
        time_schema("When it starts, with an offset."),
    );
    properties.insert(
        "ends_at".into(),
        time_schema("When it ends, after starts_at."),
    );
    properties.insert(
        "location".into(),
        json!({"type":"string","maxLength":255,"description":"Where to meet."}),
    );
    properties.insert(
        "join_url".into(),
        json!({"type":"string","maxLength":2000,"description":"An https:// or http:// link to join it."}),
    );
    properties.insert(
        "meeting_type".into(),
        json!({"type":"string","maxLength":100,"description":"What kind of meeting, in plain words."}),
    );
    properties.insert("repeat".into(), repeat_schema());
    json!({"type":"object","required":["title","starts_at","ends_at"],"properties":properties})
}
fn add_guest_schema() -> Value {
    json!({"type":"object","required":["id","guest"],"properties":{
        "id":{"type":"integer","minimum":1},
        "guest":guest_schema()
    }})
}
fn guest_id_schema() -> Value {
    json!({"type":"object","required":["id","guest_id"],"properties":{
        "id":{"type":"integer","minimum":1},
        "guest_id":{"type":"integer","minimum":1,"description":"The guest's id, from core.activity.get."}
    }})
}
fn respond_schema() -> Value {
    json!({"type":"object","required":["id","guest_id","response"],"properties":{
        "id":{"type":"integer","minimum":1},
        "guest_id":{"type":"integer","minimum":1,"description":"The guest's id, from core.activity.get."},
        "response":{"type":"string","enum":["none","accepted","declined","tentative"]}
    }})
}
fn note_schema() -> Value {
    json!({"type":"object","required":["title"],"properties":common_properties()})
}
fn update_schema() -> Value {
    let statuses: Vec<&str> = Status::ALL.iter().map(|s| s.code()).collect();
    json!({"type":"object","required":["id","version","patch"],"properties":{
        "id":{"type":"integer","minimum":1},
        "version":{"type":"integer","minimum":1,"description":"The stored version; a changed activity refuses the save."},
        "patch":{"type":"object","description":"Only the fields to change. An empty string clears a field that may be empty; related with an empty kind and id clears the related record.","properties":{
            "title":{"type":"string","maxLength":160},
            "status":{"type":"string","enum":codes(&statuses, false)},
            "priority":{"type":"string","enum":codes(&["high","medium","low"], false)},
            "direction":{"type":"string","enum":codes(&["inbound","outbound"], false)},
            "starts_at":time_schema("Calls and meetings."),
            "ends_at":time_schema("Calls and meetings."),
            "due_at":time_schema("Tasks; empty clears."),
            "start_at":time_schema("Tasks; empty clears."),
            "outcome":{"type":"string","maxLength":100},
            "location":{"type":"string","maxLength":255,"description":"Meetings; empty clears."},
            "join_url":{"type":"string","maxLength":2000,"description":"Meetings; empty clears."},
            "meeting_type":{"type":"string","maxLength":100,"description":"Meetings; empty clears."},
            "description":{"type":"string","maxLength":10000},
            "internal_notes":{"type":"string","maxLength":10000},
            "related":{"type":"object","properties":{"kind":{"type":"string","maxLength":100},"id":{"type":"string","maxLength":64}}},
            "shared":shared_schema(),
            "assignee_user_id":{"type":"integer","minimum":1},
            "reminder_minutes":reminder_minutes_schema(),
            "reminder_minutes_default":{"type":"boolean","description":"true: forget this activity's own reminders and follow your default, then the organisation's. Not with reminder_minutes."},
            "reminder_email":reminder_email_schema()
        }}
    }})
}
fn update_series_schema() -> Value {
    let mut schema = update_schema();
    schema["required"] = json!(["id", "version", "apply", "patch"]);
    schema["properties"]["apply"] = apply_schema();
    schema["properties"]["patch"]["description"] = json!("Only the fields to change; not status, which changes one occurrence at a time. A new starts_at, ends_at or due_at moves every open occurrence reached by the same wall-clock amount; closed ones keep their times.");
    if let Some(patch) = schema["properties"]["patch"]["properties"].as_object_mut() {
        patch.remove("status");
    }
    schema
}
fn apply_schema() -> Value {
    json!({"type":"string","enum":["following","all"],"description":"following: this occurrence and the later ones; all: every live occurrence of the series, held and completed ones included."})
}
fn delete_series_schema() -> Value {
    json!({"type":"object","required":["id","version","apply"],"properties":{
        "id":{"type":"integer","minimum":1,"description":"An occurrence of the series."},
        "version":{"type":"integer","minimum":1},
        "apply":apply_schema()
    }})
}
fn version_schema() -> Value {
    json!({"type":"object","required":["id","version"],"properties":{
        "id":{"type":"integer","minimum":1},
        "version":{"type":"integer","minimum":1}
    }})
}
fn get_schema() -> Value {
    json!({"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1}}})
}
fn list_schema() -> Value {
    json!({"type":"object","properties":{
        "search":{"type":"string","maxLength":200,"description":"Words in the subject."},
        "kind":{"type":"string","enum":["call","meeting","task","note"]},
        "state":{"type":"string","enum":["open","closed","all"],"description":"Open by default."},
        "mine":{"type":"string","enum":["any","assignee","guest"],"description":"assignee: assigned to you; guest: you are invited."},
        "related":reference_schema(),
        "from":time_schema("With to: calls and meetings that overlap from–to and tasks due in it (a calendar). Then page_size may be up to 500."),
        "to":time_schema("The end of the range, after from."),
        "series_id":{"type":"integer","minimum":1,"description":"The occurrences of one repeating series, in order (series_id from core.activity.get)."},
        "page":{"type":"integer","minimum":1},
        "page_size":{"type":"integer","minimum":1,"maximum":500,"description":"At most 100, or 500 with from and to."}
    }})
}
fn activity_output() -> Value {
    json!({"type":"object","description":"An activity: id, kind (call, meeting, task, note), title, status, priority, direction, starts_at, ends_at, due_at, start_at, outcome, location, join_url, meeting_type, description, internal_notes, related {kind, id, label, path}, shared, reminded_at, reminder_minutes (the offsets it reminds at, null for the default) and reminder_email (null for the default), assignee_user_id, created_by, company_id, created_at, updated_at, closed_at, series_id and series_index (its number in a repeating series) and version. Times are ISO UTC strings; get adds guests, each with id, user_id, person_id, email, role, response, invited_at and invite_sent_at, and series (the rule in words and its parts, and how many occurrences are live) for an occurrence."})
}
fn list_output() -> Value {
    json!({"type":"object","description":"activities (each as core.activity.get without guests), total, page and page_size."})
}
fn review_output() -> Value {
    json!({"type":"object","description":"The review shows what will be stored; the commit returns the activity."})
}

static LIST: List = List;
static GET: Get = Get;
static CREATE_TASK: Create = Create(Kind::Task);
static LOG_CALL: Create = Create(Kind::Call);
static CREATE_MEETING: Create = Create(Kind::Meeting);
static CREATE_NOTE: Create = Create(Kind::Note);
static ADD_GUEST: AddGuest = AddGuest;
static REMOVE_GUEST: RemoveGuest = RemoveGuest;
static RESPOND: Respond = Respond;
static SEND_INVITES: SendInvites = SendInvites;
static UPDATE: Update = Update;
static CLOSE: Close = Close;
static DELETE: Delete = Delete;
static UPDATE_SERIES: UpdateSeries = UpdateSeries;
static DELETE_SERIES: DeleteSeries = DeleteSeries;
const KEYWORDS: &[&str] = &[
    "activity",
    "task",
    "call",
    "meeting",
    "note",
    "reminder",
    "follow-up",
    "invitation",
    "guest",
    "repeat",
    "recurring",
    "series",
    "calendar",
];
inventory::submit! { Contribution::new(DEFINITIONS, "core/activity.operations", Provenance { module: crate::MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "core.activity.list", title: "List activities", summary: "Calls, meetings, tasks and notes you may see, open or closed, optionally about one record, in a date range (a calendar), one repeating series, or yours only.", keywords: KEYWORDS, capability: VIEW, input: list_schema, output: list_output, handler: Handler::Read(&LIST), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.get", title: "Read an activity", summary: "One activity with its related record and guests.", keywords: KEYWORDS, capability: VIEW, input: get_schema, output: activity_output, handler: Handler::Read(&GET), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.create_task", title: "Create a task", summary: "A task with a due date, priority and assignee, optionally about a record and repeating.", keywords: KEYWORDS, capability: MANAGE, input: task_schema, output: review_output, handler: Handler::SafeWrite(&CREATE_TASK), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.log_call", title: "Log a call", summary: "A call that was held or is planned, with its direction and outcome, optionally about a record.", keywords: KEYWORDS, capability: MANAGE, input: call_schema, output: review_output, handler: Handler::SafeWrite(&LOG_CALL), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.create_meeting", title: "Schedule a meeting", summary: "A meeting with its time, place, join link and guests, optionally about a record and repeating. Send invites afterwards.", keywords: KEYWORDS, capability: MANAGE, input: meeting_schema, output: review_output, handler: Handler::SafeWrite(&CREATE_MEETING), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.create_note", title: "Add a note", summary: "A note about a record: a title and its text.", keywords: KEYWORDS, capability: MANAGE, input: note_schema, output: review_output, handler: Handler::SafeWrite(&CREATE_NOTE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.update", title: "Change an activity", summary: "Change the fields named in patch, using the stored version.", keywords: KEYWORDS, capability: MANAGE, input: update_schema, output: review_output, handler: Handler::SafeWrite(&UPDATE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.close", title: "Close an activity", summary: "A call or meeting becomes held, a task completed. Refused when already closed.", keywords: KEYWORDS, capability: MANAGE, input: version_schema, output: review_output, handler: Handler::SafeWrite(&CLOSE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.add_guest", title: "Invite a guest", summary: "Add a colleague's account, a person or an email address as a guest of an activity you may change.", keywords: KEYWORDS, capability: MANAGE, input: add_guest_schema, output: review_output, handler: Handler::SafeWrite(&ADD_GUEST), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.remove_guest", title: "Remove a guest", summary: "Remove a guest from an activity you may change. The assignee stays until the activity is reassigned.", keywords: KEYWORDS, capability: MANAGE, input: guest_id_schema, output: review_output, handler: Handler::SafeWrite(&REMOVE_GUEST), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.respond", title: "Answer an invitation", summary: "Record a guest's answer: accepted, declined, tentative or none. You answer for yourself; recording someone else's needs the right to change the activity.", keywords: KEYWORDS, capability: VIEW, input: respond_schema, output: review_output, handler: Handler::SafeWrite(&RESPOND), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.send_invites", title: "Send invitations", summary: "Invite every guest but you to an open call or meeting still ahead: colleagues in the bell, everyone else by email.", keywords: KEYWORDS, capability: MANAGE, input: get_schema, output: review_output, handler: Handler::SafeWrite(&SEND_INVITES), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.delete", title: "Delete an activity", summary: "Remove an activity, using the stored version. In a repeating series it removes only this occurrence.", keywords: KEYWORDS, capability: MANAGE, input: version_schema, output: review_output, handler: Handler::SafeWrite(&DELETE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.update_series", title: "Change a repeating series", summary: "Change the fields named in patch on one occurrence and the later ones, or on every occurrence of its series.", keywords: KEYWORDS, capability: MANAGE, input: update_series_schema, output: review_output, handler: Handler::SafeWrite(&UPDATE_SERIES), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.delete_series", title: "Delete a repeating series", summary: "Remove one occurrence and the later ones, or every occurrence of its series, held and completed ones included.", keywords: KEYWORDS, capability: MANAGE, input: delete_series_schema, output: review_output, handler: Handler::SafeWrite(&DELETE_SERIES), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[Guide { key: "core.activity", title: "Activities", summary: "Tasks, calls, meetings and notes on any record: who does what, when, about which record, who takes part and who may see it.", keywords: KEYWORDS, capability: VIEW, body: include_str!("../guide.md") }],
}) }
