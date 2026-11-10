//! Pages and panels: the activity list, an activity's record page and the
//! activities panels on record pages. Reads use the `Activities` facade;
//! every write calls an operation contract through `PageContracts`
//! (declared in `PageWrites`). No Bilimbi counterpart; the shapes follow
//! Core Person's list and record pages and Core Company's record panels.
mod feed;
mod form;
mod guest_person;
mod list;
mod panel;
mod record;
mod reminder_choices;
mod reminder_defaults;
mod series_delete;

use crate::{Activities, Activity, Kind, Status};
use askama::Template;
use bilibi_agent_api::page::{Call, PageContracts};
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayContext, Format, Mode, Precision};
use bilibi_principal_directory::{PrincipalDirectory, PrincipalKind, PrincipalRef, Query};
use bilibi_records::Records;
use bilibi_tenancy::Scope;
use bilibi_ui::{page::*, Badge, BadgeKind, Html};
use jiff::{civil, tz::TimeZone, Timestamp};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

pub const SERVICE: &str = "core/activity.pages";

pub struct Pages {
    pub activities: Arc<Activities>,
    pub records: Arc<Records>,
    pub contracts: Arc<PageContracts>,
    pub directory: Arc<PrincipalDirectory>,
    pub display: Arc<bilibi_datetime::DisplayPolicy>,
    pub locales: Arc<bilibi_locale::Locales>,
    pub step_up: Arc<bilibi_step_up::StepUp>,
    pub db: Db,
}
impl Pages {
    fn shared(&self) -> Self {
        Self {
            activities: self.activities.clone(),
            records: self.records.clone(),
            contracts: self.contracts.clone(),
            directory: self.directory.clone(),
            display: self.display.clone(),
            locales: self.locales.clone(),
            step_up: self.step_up.clone(),
            db: self.db.clone(),
        }
    }
    fn call<'a>(
        &self,
        scope: &'a Scope,
        credential: &'a str,
        event: &'a str,
        contract: &'static str,
    ) -> Call<'a> {
        Call {
            scope,
            credential,
            service: SERVICE,
            event,
            contract,
        }
    }
    /// The reader's display context: their clock mode and their company's
    /// time zone.
    async fn display(&self, scope: &Scope) -> Result<DisplayContext> {
        let client = self.db.get().await?;
        let locale = self.locales.resolve(&**client, scope, None).await?.locale;
        self.display.display(&**client, scope, None, locale).await
    }
    /// Account names for the ids given, in one directory read.
    async fn names(&self, scope: &Scope, users: &[i64]) -> Result<HashMap<i64, String>> {
        let principals: Vec<PrincipalRef> = users
            .iter()
            .filter_map(|id| PrincipalRef::new(PrincipalKind::User, *id).ok())
            .collect();
        if principals.is_empty() {
            return Ok(HashMap::new());
        }
        let client = self.db.get().await?;
        let ranked = self
            .directory
            .rank(&**client, scope, &principals, &Query::default())
            .await
            .map_err(|_| Error::Internal)?;
        Ok(principals
            .iter()
            .map(|p| (p.id(), ranked.label(*p)))
            .collect())
    }
}

/// The reader's clock: how instants are shown, and the zone a date-time
/// field is read in. Until the browser reports its zone, a reader in the
/// browser-local mode reads and enters times in the company's zone.
#[derive(Clone)]
pub(crate) struct Clock {
    pub display: DisplayContext,
    pub zone: String,
}
impl Clock {
    pub async fn new(pages: &Pages, scope: &Scope, browser: Option<&str>) -> Result<Self> {
        let display = pages.display(scope).await?;
        let zone = display.clock(browser, Timestamp::now()).timezone;
        Ok(Self { display, zone })
    }
    fn tz(&self) -> TimeZone {
        TimeZone::get(&self.zone).unwrap_or(TimeZone::UTC)
    }
    /// The shared typed instant for a table cell.
    pub fn instant(&self, at: Timestamp) -> bilibi_ui::flex_table::Instant {
        let r = self.display.render(at, Format::DateTime, Precision::Minute);
        bilibi_ui::flex_table::Instant::new(&r.iso, &r.text, &r.company, &r.utc, r.mode.as_str())
    }
    /// An instant for a read-only fact: the shared `<time>` markup, which
    /// follows the shell's display mode.
    pub fn time(&self, id: &str, at: Timestamp) -> Result<Html> {
        render(Time {
            id,
            i: self.instant(at),
        })
    }
    /// An instant as text for an inline time fact, in the Platform's format
    /// and in the zone its editor reads and enters times in.
    pub fn text(&self, at: Timestamp) -> String {
        DisplayContext {
            mode: Mode::Company,
            timezone: self.zone.clone(),
            ..self.display.clone()
        }
        .render(at, Format::DateTime, Precision::Minute)
        .text
    }
    /// The value of a `datetime-local` field.
    pub fn local(&self, at: Option<Timestamp>) -> String {
        at.map(|t| t.to_zoned(self.tz()).strftime("%Y-%m-%dT%H:%M").to_string())
            .unwrap_or_default()
    }
    /// A `datetime-local` value read in the reader's zone; `Ok(None)` when
    /// blank.
    pub fn parse(&self, value: &str) -> std::result::Result<Option<Timestamp>, ()> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        let civil = civil::DateTime::strptime("%Y-%m-%dT%H:%M", value)
            .or_else(|_| civil::DateTime::strptime("%Y-%m-%dT%H:%M:%S", value))
            .map_err(|_| ())?;
        civil
            .to_zoned(self.tz())
            .map(|z| Some(z.timestamp()))
            .map_err(|_| ())
    }
    pub fn hint(&self) -> String {
        format!("Times are in {}.", self.zone)
    }
}

/// The zone a page asks the browser for, once per mount.
#[derive(Clone, Default)]
pub(crate) struct BrowserZone {
    pub zone: Option<String>,
    request: String,
}
impl BrowserZone {
    pub fn new() -> Self {
        Self {
            zone: None,
            request: uuid::Uuid::new_v4().to_string(),
        }
    }
    /// The `browser_timezone` event's zone, when valid.
    pub fn accept(&mut self, value: &Value) {
        self.zone = Some(
            value["timezone"]
                .as_str()
                .filter(|zone| zone.len() <= 64 && bilibi_datetime::valid_timezone(zone))
                .unwrap_or("Etc/UTC")
                .to_owned(),
        );
    }
    /// The request id the hook reports for, empty once it has.
    pub fn request(&self) -> &str {
        if self.zone.is_none() {
            &self.request
        } else {
            ""
        }
    }
}

#[derive(Template)]
#[template(path = "activities_region.html")]
pub(crate) struct Region<'a> {
    pub id: &'a str,
    pub target: bool,
    pub timezone_request: &'a str,
    pub body: Html,
}

/// One instant through the shared markup (`flex_table.html`'s `instant`).
#[derive(Template)]
#[template(
    source = r#"{% import "flex_table.html" as flex %}{% call flex::instant(id=id, i=i) %}{% endcall %}"#,
    ext = "html"
)]
struct Time<'a> {
    id: &'a str,
    i: bilibi_ui::flex_table::Instant,
}

pub(crate) fn render(template: impl Template) -> Result<Html> {
    Html::render(&template).map_err(|_| Error::Internal)
}

pub(crate) fn event(value: &Value) -> Result<&str> {
    value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid)
}

/// A kind's icon, for rows and buttons.
pub(crate) fn icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Call => "phone",
        Kind::Meeting => "users",
        Kind::Task => "clipboard-document-check",
        Kind::Note => "pencil-square",
    }
}

/// How long before it starts or falls due an open activity counts as
/// imminent. Fixed for now; a tenant setting is the follow-up
/// (`core/activity` README, Calendar).
pub(crate) const IMMINENT_HOURS: i64 = 24;

/// An open activity's timing: overdue once its start or due time has
/// passed, imminent inside [`IMMINENT_HOURS`]. A closed one, a note and one
/// with no time are neither.
pub(crate) fn urgency(activity: &Activity, now: Timestamp) -> Option<bilibi_ui::calendar::Urgency> {
    use bilibi_ui::calendar::Urgency;
    if activity.is_closed() {
        return None;
    }
    let when = activity.starts_at.or(activity.due_at)?;
    if when < now {
        Some(Urgency::Overdue)
    } else if when < now + jiff::SignedDuration::from_hours(IMMINENT_HOURS) {
        Some(Urgency::Imminent)
    } else {
        None
    }
}

/// The status as a badge: closed ones neutral or green, open ones overdue in
/// red and imminent (within a day) in amber.
pub(crate) fn status_badge(activity: &Activity) -> Result<Html> {
    use bilibi_ui::calendar::Urgency;
    let Some(status) = activity.status else {
        return render(Badge::status(BadgeKind::Plain, "Note"));
    };
    let kind = if status.closed() {
        match status {
            Status::Held | Status::Completed => BadgeKind::Success,
            _ => BadgeKind::Neutral,
        }
    } else {
        match urgency(activity, Timestamp::now()) {
            Some(Urgency::Overdue) => BadgeKind::Danger,
            Some(Urgency::Imminent) => BadgeKind::Warning,
            None => BadgeKind::Neutral,
        }
    };
    render(Badge::status(kind, status.label()))
}

/// The fields a page edit may change, and their labels.
pub(crate) const FIELDS: &[(&str, &str)] = &[
    ("title", "Subject"),
    ("status", "Status"),
    ("priority", "Priority"),
    ("direction", "Direction"),
    ("starts_at", "Starts"),
    ("ends_at", "Ends"),
    ("due_at", "Due"),
    ("start_at", "Start"),
    ("outcome", "Outcome"),
    ("location", "Location"),
    ("join_url", "Join link"),
    ("meeting_type", "Meeting type"),
    ("assignee", "Assigned to"),
    ("guests", "Guests"),
    ("guest", "Guest"),
    ("shared", "Who can see it"),
    ("reminders", "Reminders"),
    ("reminder_email", "Email reminders"),
    ("description", "Description"),
    ("internal_notes", "Internal notes"),
];
pub(crate) fn label(field: &str) -> &'static str {
    FIELDS
        .iter()
        .find(|(key, _)| *key == field)
        .map_or("Field", |(_, label)| label)
}

/// The page field a refusal's input path names (`patch.ends_at` is `ends_at`).
pub(crate) fn field_of(path: &str) -> &str {
    match path.strip_prefix("patch.").unwrap_or(path) {
        "reminder_minutes" => "reminders",
        field => field,
    }
}

/// Field refusals as one notice, each a sentence with its field's label:
/// "Ends must not be in the future for a held call."
pub(crate) fn sentences(fields: &std::collections::BTreeMap<String, Vec<String>>) -> String {
    fields
        .iter()
        .flat_map(|(path, messages)| {
            let label = FIELDS
                .iter()
                .find(|(key, _)| *key == field_of(path))
                .map_or("", |(_, label)| label);
            messages
                .iter()
                .map(move |message| bilibi_ui::error_sentence(label, message))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a refused contract call says on the page. `Unauthorized` and a
/// step-up request go back to the host unchanged.
pub(crate) enum Refusal {
    Fields(std::collections::BTreeMap<String, Vec<String>>),
    Message(String),
}
pub(crate) fn refusal(
    error: bilibi_agent_api::Error,
    forbidden: &str,
    failed: &str,
) -> Result<Refusal> {
    use bilibi_agent_api::Error as E;
    Ok(match error {
        E::Backend(error @ (Error::Unauthorized | Error::StepUpRequired { .. })) => {
            return Err(error)
        }
        E::Forbidden | E::Backend(Error::Forbidden) => Refusal::Message(forbidden.into()),
        E::Backend(Error::Conflict) | E::PlanRejected(_) => Refusal::Message(
            "The activity changed since this page loaded. Reload before trying again.".into(),
        ),
        E::NotFound => Refusal::Message("The activity could not be found.".into()),
        E::InvalidInput(fields) => match fields.get("already_closed") {
            Some(_) => Refusal::Message("This activity is already closed.".into()),
            None => Refusal::Fields(fields),
        },
        _ => Refusal::Message(failed.into()),
    })
}

fn record_id(request: &PageRequest<'_, Scope>) -> Result<i64> {
    request
        .path
        .get("id")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .ok_or(Error::NotFound)
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        match operation {
            "list" | "detail" => Some("Activities"),
            "feed" => Some("Calendar feed"),
            _ => None,
        }
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let mut state = self.state(&request, scope).await?;
            let body = state
                .update(LiveRequest {
                    scope,
                    credential: "",
                    input: LiveInput::Refresh,
                })
                .await?;
            Ok(PageBody {
                html: body.html,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            Ok(Some(self.state(&request, scope).await?))
        })
    }
}
impl Pages {
    async fn state(
        &self,
        request: &PageRequest<'_, Scope>,
        scope: &Scope,
    ) -> Result<Box<dyn LiveState<Scope, Error>>> {
        Ok(match request.operation {
            "list" => Box::new(list::Index::new(
                self.shared(),
                request.query,
                request.framed,
            )?),
            "detail" => Box::new(
                record::Detail::new(self.shared(), scope, record_id(request)?, request.framed)
                    .await?,
            ),
            "feed" => Box::new(feed::Feed::new(self.shared())?),
            "company" | "person" | "record" => Box::new(panel::Panel::new(self.shared(), request)?),
            _ => return Err(Error::NotFound),
        })
    }
}
