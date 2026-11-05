//! Operation contracts for activities. The pages and panels call the write
//! contracts too, so every door shares one rule set. References are
//! `{kind, id}` in input; reads add `label` and `path`.
use crate::{
    Activities, Activity, CallStatus, Common, Direction, Draft, Filter, Guest, GuestRow, Kind,
    Mine, Patch, Priority, Reference, State, Status, WriteError, MANAGE, VIEW,
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
fn owner() -> bilibi_agent_api::Result<&'static Activities> {
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
    fn guests(&mut self) -> Vec<Guest> {
        let Some(items) = self.at().get("guests").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut guests = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let person = item["person_id"].as_i64();
            let email = item["email"].as_str().filter(|e| !e.trim().is_empty());
            match (person, email) {
                (Some(id), None) => match PersonId::new(id) {
                    Ok(id) => guests.push(Guest::Person(id)),
                    Err(_) => self.refuse(&format!("guests.{index}.person_id"), "must be positive"),
                },
                (None, Some(email)) => guests.push(Guest::Address {
                    email: email.trim().to_owned(),
                    related: item["related"].as_bool().unwrap_or(false),
                }),
                _ => self.refuse(
                    &format!("guests.{index}"),
                    "must name either a person_id or an email",
                ),
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
        _ => Draft::note(&crate::NoteInput { common }),
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
        description: reader.clearable_text("description"),
        internal_notes: reader.clearable_text("internal_notes"),
        related: reader.reference("related"),
        shared: reader.flag("shared"),
        assignee: reader.at()["assignee_user_id"].as_i64(),
    };
    for key in ["starts_at", "ends_at"] {
        if reader.present(key) && reader.text(key).is_some_and(|v| v.trim().is_empty()) {
            reader.refuse(key, "can't be blank");
        }
    }
    reader.done(patch)
}

fn id(input: &Value) -> bilibi_agent_api::Result<i64> {
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
    let field = |key: &str, message: String| {
        bilibi_agent_api::Error::InvalidInput(BTreeMap::from([(key.into(), vec![message])]))
    };
    match error {
        WriteError::Access(error) => error.into(),
        WriteError::NotFound => bilibi_agent_api::Error::NotFound,
        WriteError::Forbidden => bilibi_agent_api::Error::Forbidden,
        WriteError::Stale => Error::Conflict.into(),
        WriteError::AlreadyClosed => field("already_closed", WriteError::AlreadyClosed.to_string()),
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
        "description": activity.description,
        "internal_notes": activity.internal_notes,
        "related": activity.related.as_ref().map(|r| reference(r, related)),
        "shared": activity.shared,
        "reminded_at": iso(activity.reminded_at),
        "assignee_user_id": activity.assignee_user_id,
        "created_by": activity.created_by,
        "company_id": activity.company_id,
        "created_at": activity.created_at.to_string(),
        "updated_at": activity.updated_at.to_string(),
        "closed_at": iso(activity.closed_at),
        "version": activity.version,
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
        "guests": draft.guests.iter().map(|g| match g {
            Guest::Person(id) => json!({"person_id": id.get()}),
            Guest::Address { email, related } => json!({"email": email, "related": related}),
        }).collect::<Vec<_>>(),
    })
}
fn guest_record(guest: &GuestRow) -> Value {
    json!({
        "user_id": guest.user_id,
        "person_id": guest.person.map(|p| p.get()),
        "email": guest.email,
        "role": guest.role,
        "response": guest.response,
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
            let filter = Filter {
                search: reader.text("search").unwrap_or_default(),
                kind,
                state,
                mine,
                related,
                descending: state == State::Closed,
                page: input["page"].as_i64().unwrap_or(1).max(1),
                page_size: input["page_size"].as_i64().unwrap_or(25).clamp(1, 100),
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
fn guests_schema() -> Value {
    json!({"type":"array","description":"People invited: a Core person by person_id, or an email address. Set related on the email address of the record the activity is about (a lead).","items":{"type":"object","properties":{
        "person_id":{"type":"integer","minimum":1},
        "email":{"type":"string","format":"email","maxLength":254},
        "related":{"type":"boolean"}
    }}})
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
    properties
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
    json!({"type":"object","required":["title","starts_at","ends_at","status","direction"],"properties":properties})
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
            "description":{"type":"string","maxLength":10000},
            "internal_notes":{"type":"string","maxLength":10000},
            "related":{"type":"object","properties":{"kind":{"type":"string","maxLength":100},"id":{"type":"string","maxLength":64}}},
            "shared":shared_schema(),
            "assignee_user_id":{"type":"integer","minimum":1}
        }}
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
        "page":{"type":"integer","minimum":1},
        "page_size":{"type":"integer","minimum":1,"maximum":100}
    }})
}
fn activity_output() -> Value {
    json!({"type":"object","description":"An activity: id, kind (call, meeting, task, note), title, status, priority, direction, starts_at, ends_at, due_at, start_at, outcome, description, internal_notes, related {kind, id, label, path}, shared, reminded_at, assignee_user_id, created_by, company_id, created_at, updated_at, closed_at and version. Times are ISO UTC strings; get adds guests."})
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
static CREATE_NOTE: Create = Create(Kind::Note);
static UPDATE: Update = Update;
static CLOSE: Close = Close;
static DELETE: Delete = Delete;
const KEYWORDS: &[&str] = &["activity", "task", "call", "note", "reminder", "follow-up"];
inventory::submit! { Contribution::new(DEFINITIONS, "core/activity.operations", Provenance { module: crate::MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "core.activity.list", title: "List activities", summary: "Calls, meetings, tasks and notes you may see, open or closed, optionally about one record or yours only.", keywords: KEYWORDS, capability: VIEW, input: list_schema, output: list_output, handler: Handler::Read(&LIST), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.get", title: "Read an activity", summary: "One activity with its related record and guests.", keywords: KEYWORDS, capability: VIEW, input: get_schema, output: activity_output, handler: Handler::Read(&GET), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.create_task", title: "Create a task", summary: "A task with a due date, priority and assignee, optionally about a record.", keywords: KEYWORDS, capability: MANAGE, input: task_schema, output: review_output, handler: Handler::SafeWrite(&CREATE_TASK), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.log_call", title: "Log a call", summary: "A call that was held or is planned, with its direction and outcome, optionally about a record.", keywords: KEYWORDS, capability: MANAGE, input: call_schema, output: review_output, handler: Handler::SafeWrite(&LOG_CALL), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.create_note", title: "Add a note", summary: "A note about a record: a title and its text.", keywords: KEYWORDS, capability: MANAGE, input: note_schema, output: review_output, handler: Handler::SafeWrite(&CREATE_NOTE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.update", title: "Change an activity", summary: "Change the fields named in patch, using the stored version.", keywords: KEYWORDS, capability: MANAGE, input: update_schema, output: review_output, handler: Handler::SafeWrite(&UPDATE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.close", title: "Close an activity", summary: "A call or meeting becomes held, a task completed. Refused when already closed.", keywords: KEYWORDS, capability: MANAGE, input: version_schema, output: review_output, handler: Handler::SafeWrite(&CLOSE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.delete", title: "Delete an activity", summary: "Remove an activity, using the stored version.", keywords: KEYWORDS, capability: MANAGE, input: version_schema, output: review_output, handler: Handler::SafeWrite(&DELETE), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[Guide { key: "core.activity", title: "Activities", summary: "Tasks, calls and notes on any record: who does what, when, about which record, and who may see it.", keywords: KEYWORDS, capability: VIEW, body: include_str!("../guide.md") }],
}) }
