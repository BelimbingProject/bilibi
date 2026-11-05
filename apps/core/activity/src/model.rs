//! The activity vocabulary (kinds, statuses, priorities, directions), the
//! inputs the facade takes and the field rules every door shares. Fixed in
//! code: closing and the panels branch on these codes.
use bilibi_party_ids::PersonId;
use bilibi_records::Reference;
use jiff::Timestamp;

macro_rules! codes {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $code:literal, $label:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub fn code(self) -> &'static str {
                match self { $(Self::$variant => $code),+ }
            }
            pub fn label(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }
            pub fn parse(code: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|value| value.code() == code)
            }
        }
    };
}

codes! {
    /// What an activity is.
    Kind {
        Call => "call", "Call",
        Meeting => "meeting", "Meeting",
        Task => "task", "Task",
        Note => "note", "Note",
    }
}
codes! {
    /// Calls and meetings: scheduled, held, not held, canceled. Tasks: not
    /// started, in progress, pending input, completed, deferred. Notes have
    /// none.
    Status {
        Scheduled => "scheduled", "Scheduled",
        Held => "held", "Held",
        NotHeld => "not_held", "Not held",
        Canceled => "canceled", "Canceled",
        NotStarted => "not_started", "Not started",
        InProgress => "in_progress", "In progress",
        PendingInput => "pending_input", "Pending input",
        Completed => "completed", "Completed",
        Deferred => "deferred", "Deferred",
    }
}
codes! {
    /// A task's priority. A fixed list until governed pick-lists are used;
    /// the codes stay the same then.
    Priority {
        High => "high", "High",
        Medium => "medium", "Medium",
        Low => "low", "Low",
    }
}
codes! {
    /// A guest's answer to an invitation.
    Response {
        NoResponse => "none", "No answer",
        Accepted => "accepted", "Accepted",
        Declined => "declined", "Declined",
        Tentative => "tentative", "Tentative",
    }
}
codes! {
    /// Which way a call went.
    Direction {
        Inbound => "inbound", "Inbound",
        Outbound => "outbound", "Outbound",
    }
}

impl Kind {
    /// The statuses this kind may have; empty for a note.
    pub fn statuses(self) -> &'static [Status] {
        match self {
            Self::Call | Self::Meeting => &[
                Status::Scheduled,
                Status::Held,
                Status::NotHeld,
                Status::Canceled,
            ],
            Self::Task => &[
                Status::NotStarted,
                Status::InProgress,
                Status::PendingInput,
                Status::Completed,
                Status::Deferred,
            ],
            Self::Note => &[],
        }
    }
    /// The status a new or follow-up activity starts in.
    pub fn opening(self) -> Option<Status> {
        match self {
            Self::Call | Self::Meeting => Some(Status::Scheduled),
            Self::Task => Some(Status::NotStarted),
            Self::Note => None,
        }
    }
    /// The status Close sets: held for calls and meetings, completed for
    /// tasks. A note cannot be closed.
    pub fn closing(self) -> Option<Status> {
        match self {
            Self::Call | Self::Meeting => Some(Status::Held),
            Self::Task => Some(Status::Completed),
            Self::Note => None,
        }
    }
    /// Calls and meetings: they have a start and an end, guests may be
    /// invited to them and they can overlap.
    pub fn timed(self) -> bool {
        matches!(self, Self::Call | Self::Meeting)
    }
}
impl Status {
    /// Held, not held, canceled, completed and deferred end an activity.
    pub fn closed(self) -> bool {
        matches!(
            self,
            Self::Held | Self::NotHeld | Self::Canceled | Self::Completed | Self::Deferred
        )
    }
}

/// Someone invited: a colleague's account, a Core person, or an email
/// address (a lead with no Core person yet). `related` marks the address of
/// the record the activity is about, which lead conversion replaces with the
/// new person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Guest {
    /// An active account of the tenant: told and reminded in the bell.
    User(i64),
    Person(PersonId),
    Address {
        email: String,
        related: bool,
    },
}

/// How a logged call ended: held, or planned for later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallStatus {
    Held,
    Planned,
}

/// What every create takes.
#[derive(Clone, Debug, Default)]
pub struct Common {
    pub related: Option<Reference>,
    /// Readers of the related record may see it too; otherwise only the
    /// creator, the assignee and the account guests.
    pub shared: bool,
    pub title: String,
    /// An active account of the tenant; the creator when `None`.
    pub assignee: Option<i64>,
    pub guests: Vec<Guest>,
    pub description: Option<String>,
    pub internal_notes: Option<String>,
}

/// `create_task`.
#[derive(Clone, Debug, Default)]
pub struct TaskInput {
    pub common: Common,
    pub due_at: Option<Timestamp>,
    pub start_at: Option<Timestamp>,
    /// Medium when `None`.
    pub priority: Option<Priority>,
}

/// `log_call`. A held call has ended: `ends_at` is not in the future.
#[derive(Clone, Debug)]
pub struct CallInput {
    pub common: Common,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    pub status: CallStatus,
    pub outcome: Option<String>,
    pub direction: Direction,
}

/// `create_meeting`. A held meeting has ended: `ends_at` is not in the
/// future.
#[derive(Clone, Debug)]
pub struct MeetingInput {
    pub common: Common,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    /// Where it takes place, up to 255 characters.
    pub location: Option<String>,
    /// An `https://` or `http://` link to join it, up to 2,000 characters.
    pub join_url: Option<String>,
    /// Plain text up to 100 characters (a pick-list later).
    pub meeting_type: Option<String>,
}

/// A note: a title and its text, about a record.
#[derive(Clone, Debug, Default)]
pub struct NoteInput {
    pub common: Common,
}

/// A change to an activity. `None` leaves a field as it is; for a field that
/// can be empty, `Some(None)` clears it.
#[derive(Clone, Debug, Default)]
pub struct Patch {
    pub title: Option<String>,
    pub status: Option<Status>,
    pub priority: Option<Priority>,
    pub direction: Option<Direction>,
    pub starts_at: Option<Timestamp>,
    pub ends_at: Option<Timestamp>,
    pub due_at: Option<Option<Timestamp>>,
    pub start_at: Option<Option<Timestamp>>,
    pub outcome: Option<Option<String>>,
    pub location: Option<Option<String>>,
    pub join_url: Option<Option<String>>,
    pub meeting_type: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub internal_notes: Option<Option<String>>,
    pub related: Option<Option<Reference>>,
    pub shared: Option<bool>,
    pub assignee: Option<i64>,
}
impl Patch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.status.is_none()
            && self.priority.is_none()
            && self.direction.is_none()
            && self.starts_at.is_none()
            && self.ends_at.is_none()
            && self.due_at.is_none()
            && self.start_at.is_none()
            && self.outcome.is_none()
            && self.location.is_none()
            && self.join_url.is_none()
            && self.meeting_type.is_none()
            && self.description.is_none()
            && self.internal_notes.is_none()
            && self.related.is_none()
            && self.shared.is_none()
            && self.assignee.is_none()
    }
}

/// A field's refusal: the input path and a sentence ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: &'static str,
    pub message: String,
}
impl FieldError {
    pub(crate) fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

/// At most this many guests per activity.
pub const MAX_GUESTS: usize = 50;

/// An activity's values before it is saved: a reviewed create, the unsaved
/// copy Close and create new returns, or an activity's values before and
/// after a change. The field rules and every write work on this.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub kind: Kind,
    pub title: String,
    pub status: Option<Status>,
    pub priority: Option<Priority>,
    pub direction: Option<Direction>,
    pub starts_at: Option<Timestamp>,
    pub ends_at: Option<Timestamp>,
    pub due_at: Option<Timestamp>,
    pub start_at: Option<Timestamp>,
    pub outcome: Option<String>,
    pub location: Option<String>,
    pub join_url: Option<String>,
    pub meeting_type: Option<String>,
    pub description: Option<String>,
    pub internal_notes: Option<String>,
    pub related: Option<Reference>,
    pub shared: bool,
    /// The creator when `None`.
    pub assignee: Option<i64>,
    pub guests: Vec<Guest>,
}
impl Draft {
    /// An empty draft of `kind` in its opening status (medium priority for
    /// a task, outbound for a call).
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            title: String::new(),
            status: kind.opening(),
            priority: (kind == Kind::Task).then_some(Priority::Medium),
            direction: (kind == Kind::Call).then_some(Direction::Outbound),
            starts_at: None,
            ends_at: None,
            due_at: None,
            start_at: None,
            outcome: None,
            location: None,
            join_url: None,
            meeting_type: None,
            description: None,
            internal_notes: None,
            related: None,
            shared: false,
            assignee: None,
            guests: Vec::new(),
        }
    }
    fn common(kind: Kind, common: &Common) -> Self {
        Self {
            title: common.title.trim().to_owned(),
            description: clean(common.description.as_deref()),
            internal_notes: clean(common.internal_notes.as_deref()),
            related: common.related.clone(),
            shared: common.shared,
            assignee: common.assignee,
            guests: common.guests.clone(),
            ..Self::new(kind)
        }
    }
    pub fn task(input: &TaskInput) -> Self {
        Self {
            due_at: input.due_at,
            start_at: input.start_at,
            priority: Some(input.priority.unwrap_or(Priority::Medium)),
            ..Self::common(Kind::Task, &input.common)
        }
    }
    pub fn call(input: &CallInput) -> Self {
        Self {
            starts_at: Some(input.starts_at),
            ends_at: Some(input.ends_at),
            status: Some(match input.status {
                CallStatus::Held => Status::Held,
                CallStatus::Planned => Status::Scheduled,
            }),
            outcome: clean(input.outcome.as_deref()),
            direction: Some(input.direction),
            ..Self::common(Kind::Call, &input.common)
        }
    }
    pub fn meeting(input: &MeetingInput) -> Self {
        Self {
            starts_at: Some(input.starts_at),
            ends_at: Some(input.ends_at),
            location: clean(input.location.as_deref()),
            join_url: clean(input.join_url.as_deref()),
            meeting_type: clean(input.meeting_type.as_deref()),
            ..Self::common(Kind::Meeting, &input.common)
        }
    }
    pub fn note(input: &NoteInput) -> Self {
        Self::common(Kind::Note, &input.common)
    }
}

/// Trimmed text, or `None` when blank.
pub(crate) fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn text(errors: &mut Vec<FieldError>, field: &'static str, value: &Option<String>, max: usize) {
    if let Some(value) = value {
        if value.chars().count() > max {
            errors.push(FieldError::new(
                field,
                format!("must be at most {max} characters"),
            ));
        } else if value
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
        {
            errors.push(FieldError::new(
                field,
                "contains a character that cannot be stored",
            ));
        }
    }
}

/// The rules on the values alone (shape, kind, times). Records, accounts and
/// permissions are checked against the database by the store.
pub(crate) fn check(values: &Draft) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let title = values.title.trim();
    if title.is_empty() {
        errors.push(FieldError::new("title", "can't be blank"));
    } else if title.chars().count() > 160 {
        errors.push(FieldError::new("title", "must be at most 160 characters"));
    } else if title.chars().any(char::is_control) {
        errors.push(FieldError::new(
            "title",
            "contains a character that cannot be stored",
        ));
    }
    let kind = values.kind;
    match values.status {
        Some(status) if !kind.statuses().contains(&status) => errors.push(FieldError::new(
            "status",
            format!("is not a status a {} can have", kind.label().to_lowercase()),
        )),
        None if kind != Kind::Note => errors.push(FieldError::new("status", "can't be blank")),
        _ => {}
    }
    match (kind, values.priority) {
        (Kind::Task, None) => errors.push(FieldError::new("priority", "can't be blank")),
        (Kind::Task, Some(_)) | (_, None) => {}
        (_, Some(_)) => errors.push(FieldError::new("priority", "is only for tasks")),
    }
    match (kind, values.direction) {
        (Kind::Call, None) => errors.push(FieldError::new("direction", "can't be blank")),
        (Kind::Call, Some(_)) | (_, None) => {}
        (_, Some(_)) => errors.push(FieldError::new("direction", "is only for calls")),
    }
    if kind.timed() {
        match (values.starts_at, values.ends_at) {
            (None, _) => errors.push(FieldError::new("starts_at", "can't be blank")),
            (_, None) => errors.push(FieldError::new("ends_at", "can't be blank")),
            (Some(starts), Some(ends)) if starts >= ends => {
                errors.push(FieldError::new("ends_at", "must be after the start"))
            }
            _ => {}
        }
    } else {
        for (field, value) in [("starts_at", values.starts_at), ("ends_at", values.ends_at)] {
            if value.is_some() {
                errors.push(FieldError::new(field, "is only for calls and meetings"));
            }
        }
    }
    if kind == Kind::Task {
        if let (Some(start), Some(due)) = (values.start_at, values.due_at) {
            if start > due {
                errors.push(FieldError::new(
                    "start_at",
                    "must not be after the due date",
                ));
            }
        }
    } else {
        for (field, value) in [("due_at", values.due_at), ("start_at", values.start_at)] {
            if value.is_some() {
                errors.push(FieldError::new(field, "is only for tasks"));
            }
        }
    }
    errors.extend(held_ended(kind, values.status, values.ends_at));
    if kind != Kind::Call && values.outcome.is_some() {
        errors.push(FieldError::new("outcome", "is only for calls"));
    }
    text(&mut errors, "outcome", &values.outcome, 100);
    for (field, value, max) in [
        ("location", &values.location, 255),
        ("join_url", &values.join_url, 2000),
        ("meeting_type", &values.meeting_type, 100),
    ] {
        if kind != Kind::Meeting && value.is_some() {
            errors.push(FieldError::new(field, "is only for meetings"));
        }
        text(&mut errors, field, value, max);
    }
    if let Some(url) = &values.join_url {
        let scheme = url.get(..8).map(str::to_ascii_lowercase);
        let link = scheme.as_deref() == Some("https://")
            || url.get(..7).map(str::to_ascii_lowercase).as_deref() == Some("http://");
        if !link || url.chars().any(char::is_whitespace) {
            errors.push(FieldError::new(
                "join_url",
                "must be a link that starts with https:// or http://",
            ));
        }
    }
    text(&mut errors, "description", &values.description, 10_000);
    text(
        &mut errors,
        "internal_notes",
        &values.internal_notes,
        10_000,
    );
    if values.shared && values.related.is_none() {
        errors.push(FieldError::new(
            "shared",
            "needs a related record whose readers it is shared with",
        ));
    }
    if values.assignee.is_none_or(|id| id <= 0) {
        errors.push(FieldError::new("assignee", "can't be blank"));
    }
    if values.guests.len() > MAX_GUESTS {
        errors.push(FieldError::new(
            "guests",
            format!("must name at most {MAX_GUESTS} guests"),
        ));
    }
    for guest in &values.guests {
        if let Guest::User(user) = guest {
            if *user <= 0 {
                errors.push(FieldError::new("guests", "must name an account by its id"));
            }
        }
        if let Guest::Address { email, .. } = guest {
            if !email_shape(email) {
                errors.push(FieldError::new(
                    "guests",
                    format!("{email} is not an email address"),
                ));
            }
        }
    }
    errors
}

/// A held call or meeting has ended: every write that leaves one held (a
/// create, an update, Close) refuses an end in the future, so its timeline
/// line is never written before it happened. Five minutes' grace covers one
/// logged as it ends and a clock that runs ahead.
pub(crate) fn held_ended(
    kind: Kind,
    status: Option<Status>,
    ends_at: Option<Timestamp>,
) -> Option<FieldError> {
    let grace = jiff::SignedDuration::from_mins(5);
    (kind.timed()
        && status == Some(Status::Held)
        && ends_at.is_some_and(|ends| ends > Timestamp::now() + grace))
    .then(|| {
        FieldError::new(
            "ends_at",
            format!(
                "must not be in the future for a held {}",
                kind.label().to_lowercase()
            ),
        )
    })
}

/// The shape of an email address a guest may be invited by.
pub(crate) fn email_shape(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    email.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ',' | ';'))
        && !domain.contains('@')
}

/// The values after `patch`.
pub(crate) fn patched(before: &Draft, patch: &Patch) -> Draft {
    let mut after = before.clone();
    if let Some(title) = &patch.title {
        after.title = title.trim().to_owned();
    }
    if let Some(status) = patch.status {
        after.status = Some(status);
    }
    if let Some(priority) = patch.priority {
        after.priority = Some(priority);
    }
    if let Some(direction) = patch.direction {
        after.direction = Some(direction);
    }
    if let Some(starts) = patch.starts_at {
        after.starts_at = Some(starts);
    }
    if let Some(ends) = patch.ends_at {
        after.ends_at = Some(ends);
    }
    if let Some(due) = patch.due_at {
        after.due_at = due;
    }
    if let Some(start) = patch.start_at {
        after.start_at = start;
    }
    if let Some(outcome) = &patch.outcome {
        after.outcome = clean(outcome.as_deref());
    }
    if let Some(location) = &patch.location {
        after.location = clean(location.as_deref());
    }
    if let Some(url) = &patch.join_url {
        after.join_url = clean(url.as_deref());
    }
    if let Some(meeting_type) = &patch.meeting_type {
        after.meeting_type = clean(meeting_type.as_deref());
    }
    if let Some(description) = &patch.description {
        after.description = clean(description.as_deref());
    }
    if let Some(notes) = &patch.internal_notes {
        after.internal_notes = clean(notes.as_deref());
    }
    if let Some(related) = &patch.related {
        after.related = related.clone();
        // Clearing the related record leaves nobody to share it with.
        if after.related.is_none() && patch.shared.is_none() {
            after.shared = false;
        }
    }
    if let Some(shared) = patch.shared {
        after.shared = shared;
    }
    if let Some(assignee) = patch.assignee {
        after.assignee = Some(assignee);
    }
    after
}
