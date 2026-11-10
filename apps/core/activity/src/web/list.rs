//! `/activities`: every activity the reader may see, with search, kind,
//! open or closed, and "mine" filters in the address, and the New call, New
//! task and New note dialog. The same list shows as a table, as a
//! calendar (`display=calendar`, base/ui `Calendar`) or as the tasks in
//! tiles by status (`display=board`, base/ui `Board`): the search and
//! "whose" filters apply to all, and dragging on the calendar or a tile
//! calls the same change contract.
use super::{
    event,
    form::Dialog,
    icon, refusal,
    reminder_defaults::{self, DefaultsDialog},
    render, sentences, series_delete, status_badge, urgency, BrowserZone, Clock, Pages, Refusal,
    Region,
};
use crate::{Activity, Filter, Kind, Mine, Reference, Sort, State};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    board::{self, BoardMove, BoardState},
    calendar::{
        self, Calendar, CalendarItem, CalendarLane, CalendarMove, CalendarSlot, CalendarState,
        CalendarWhen,
    },
    choices,
    list_state::{InvalidPageSize, FILTERS_FORM},
    lists::RecordLink,
    page::*,
    params, BuiltinColumn, Button, ButtonVariant, Card, ColumnsOutcome, ConfirmDialog, EmptyState,
    Filter as ListFilter, FilterToolbar, FlashGroup, FlexRow, FlexTable, Header, Html, Icon,
    IconButton, IconKind, ListColumns, ListSpec, ListState, PageCounts, Pagination, SortDir,
    StatusKind, Tab, TableEmpty, Tabs, ToolbarControl, NO_FLASH,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
};

mod tiles;
use tiles::{board_columns, slot_times};

const SIZES: [u64; 3] = [25, 50, 100];
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        sortable: BTreeMap::from([
            ("when".into(), SortDir::Asc),
            ("title".into(), SortDir::Asc),
            ("status".into(), SortDir::Asc),
            ("created".into(), SortDir::Desc),
        ]),
        default_sort: Some("when".into()),
        page_sizes: SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        invalid_page_size: InvalidPageSize::Default,
        filters: BTreeMap::from([
            (
                "kind".into(),
                ListFilter::one_of(&["all", "call", "meeting", "task", "note"], "all"),
            ),
            (
                "state".into(),
                ListFilter::one_of(&["open", "closed", "all"], "open"),
            ),
            (
                "mine".into(),
                ListFilter::one_of(&["any", "assignee", "guest"], "any"),
            ),
        ]),
        omit_blank: vec!["search".into()],
        ..ListSpec::default()
    }
    .shared()
    .expect("activity list spec")
});

fn columns() -> Result<ListColumns> {
    ListColumns::mount(
        "activities",
        vec![
            BuiltinColumn::new("title", "Subject").sortable("title"),
            BuiltinColumn::new("kind", "Kind"),
            BuiltinColumn::new("status", "Status").sortable("status"),
            BuiltinColumn::new("when", "When").sortable("when"),
            BuiltinColumn::new("related", "Related to"),
            BuiltinColumn::new("assignee", "Assigned to"),
        ],
    )
    .map_err(|_| Error::Internal)
}

/// `series=<id>` narrows the list to one repeating series.
fn series(query: &HashMap<String, String>) -> Option<i64> {
    query
        .get("series")
        .and_then(|id| id.parse::<i64>().ok())
        .filter(|id| *id > 0)
}

/// How the list is shown: the address's `display`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Display {
    Table,
    Calendar,
    /// The tasks as tiles in a column for each status.
    Board,
}
impl Display {
    fn parse(query: &HashMap<String, String>) -> Self {
        match query.get("display").map(String::as_str) {
            Some("calendar") => Self::Calendar,
            Some("board") => Self::Board,
            _ => Self::Table,
        }
    }
}

/// The query keys the calendar and the status tiles keep in the address.
const CALENDAR_KEYS: [&str; 4] = ["view", "date", "lanes", "column"];
fn calendar_query(query: &HashMap<String, String>) -> HashMap<String, String> {
    query
        .iter()
        .filter(|(key, _)| CALENDAR_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.chars().take(200).collect()))
        .collect()
}

/// A kind's colour on the calendar (a `cat-*` data role, never a status).
fn tone(kind: Kind) -> u8 {
    match kind {
        Kind::Call => 1,
        Kind::Meeting => 2,
        Kind::Task => 3,
        Kind::Note => 4,
    }
}
/// How long a task shows on the calendar from the time it is due.
const TASK_MINUTES: i64 = 30;

/// `related=<kind>:<id>` narrows the list to one record's activities.
fn related(query: &HashMap<String, String>) -> Option<Reference> {
    let (kind, id) = query.get("related")?.split_once(':')?;
    (!kind.is_empty() && !id.is_empty() && kind.len() <= 100 && id.len() <= 64)
        .then(|| Reference::new(kind, id))
}

pub(crate) struct Index {
    pages: Pages,
    list: ListState,
    columns: ListColumns,
    related: Option<Reference>,
    series: Option<i64>,
    display: Display,
    /// The calendar's view, date and lanes (and the tiles' phone column)
    /// from the address.
    calendar_query: HashMap<String, String>,
    /// How far each status column of the tiles is shown, and the phone's
    /// column.
    board: BoardState,
    /// The tile whose Move dialog is open, and its errors.
    board_moving: Option<String>,
    board_errors: Vec<String>,
    board_notice: Option<(StatusKind, String)>,
    /// A free calendar slot waiting for its kind: start and end.
    slot: Option<(jiff::Timestamp, jiff::Timestamp)>,
    /// The repeating row whose Delete series dialog is open, and the
    /// last refusal.
    series_delete: Option<(i64, Vec<String>)>,
    /// The calendar item whose Move dialog is open, and its errors.
    moving: Option<String>,
    move_errors: Vec<String>,
    /// The last calendar move's outcome, announced on the calendar.
    notice: Option<(StatusKind, String)>,
    framed: bool,
    zone: BrowserZone,
    dialog: Option<Dialog>,
    /// The Reminder defaults dialog, when open.
    defaults: Option<DefaultsDialog>,
    /// The activity Close asks about: its id, title and version.
    confirm: Option<(i64, String, i64)>,
    flash: Option<(StatusKind, String)>,
}
impl Index {
    pub fn new(pages: Pages, query: &HashMap<String, String>, framed: bool) -> Result<Self> {
        Ok(Self {
            pages,
            list: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: columns()?,
            related: related(query),
            series: series(query),
            display: Display::parse(query),
            calendar_query: calendar_query(query),
            board: BoardState::parse(query, &board_columns(&[])),
            board_moving: None,
            board_errors: Vec::new(),
            board_notice: None,
            slot: None,
            series_delete: None,
            moving: None,
            move_errors: Vec::new(),
            notice: None,
            framed,
            zone: BrowserZone::new(),
            dialog: None,
            defaults: None,
            confirm: None,
            flash: None,
        })
    }
    fn filter(&self) -> Filter {
        Filter {
            search: self.list.search.trim().chars().take(200).collect(),
            kind: Kind::parse(&self.list.filters["kind"]),
            state: match self.list.filters["state"].as_str() {
                "closed" => State::Closed,
                "all" => State::All,
                _ => State::Open,
            },
            mine: match self.list.filters["mine"].as_str() {
                "assignee" => Mine::Assignee,
                "guest" => Mine::Guest,
                _ => Mine::Any,
            },
            related: self.related.clone(),
            between: None,
            series: self.series,
            sort: match self.list.sort_by.as_deref() {
                Some("title") => Sort::Title,
                Some("status") => Sort::Status,
                Some("created") => Sort::Created,
                _ => Sort::When,
            },
            descending: self.list.sort_dir == Some(SortDir::Desc),
            page: self.list.page as i64,
            page_size: self.list.page_size as i64,
        }
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
            "filters" => {
                let filters = params::form_values_checked(value, FILTERS_FORM, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.list.apply_filters(&filters);
            }
            "display" => {
                self.display = match value["tab"].as_str() {
                    Some("calendar") => Display::Calendar,
                    Some("board") => Display::Board,
                    _ => Display::Table,
                };
                self.moving = None;
                self.move_errors.clear();
                self.board_moving = None;
                self.board_errors.clear();
                self.slot = None;
            }
            calendar::SLOT => {
                let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
                if !self.pages.activities.may_create(scope).await? {
                    self.notice = Some((
                        StatusKind::Error,
                        "You do not have permission to create activities.".into(),
                    ));
                } else {
                    match CalendarSlot::from_event(value, &clock.zone) {
                        Ok(slot) => {
                            self.slot = Some(slot_times(&slot, &clock));
                            self.notice = None;
                        }
                        Err(sentence) => self.notice = Some((StatusKind::Error, sentence)),
                    }
                }
            }
            "cancel_slot" => self.slot = None,
            "slot_continue" => {
                let form = params::form_values_checked(value, "slot", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let kind = match form.get("kind").map(String::as_str) {
                    Some("call") => Kind::Call,
                    Some("task") => Kind::Task,
                    _ => Kind::Meeting,
                };
                if let Some((start, end)) = self.slot.take() {
                    if self.pages.activities.may_create(scope).await? {
                        let clock =
                            Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
                        self.dialog = Some(Dialog::new(kind, None).at(start, end, &clock));
                    }
                }
            }
            board::MOVE_OPEN => {
                self.board_moving = board::card_id(value).map(str::to_owned);
                self.board_errors.clear();
                self.board_notice = None;
            }
            board::MOVE_CANCEL => {
                self.board_moving = None;
                self.board_errors.clear();
            }
            board::MORE => {
                if let Some(column) = board::column_key(value) {
                    self.board.show_more(column);
                }
            }
            board::MOVE | board::MOVE_SUBMIT => {
                let outcome = match BoardMove::from_event(value) {
                    Ok(change) => self.set_status(scope, credential, name, &change).await?,
                    Err(sentence) => Err(sentence),
                };
                match outcome {
                    Ok(message) => {
                        self.board_notice = Some((StatusKind::Success, message));
                        self.board_moving = None;
                        self.board_errors.clear();
                    }
                    Err(message) if name == board::MOVE_SUBMIT => {
                        self.board_errors = vec![message];
                    }
                    Err(message) => self.board_notice = Some((StatusKind::Error, message)),
                }
            }
            "request_delete_series" => {
                let id = value["id"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                let activity = self.pages.activities.get(scope, id).await?;
                if activity.series_id.is_some()
                    && self.pages.activities.may_edit(scope, &activity).await?
                {
                    self.series_delete = Some((id, Vec::new()));
                }
            }
            "cancel_delete_series" => self.series_delete = None,
            "delete_series" => {
                let Some((id, _)) = self.series_delete else {
                    return Ok(None);
                };
                let form = params::form_values_checked(value, "delete", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let apply = match form.get("apply").map(String::as_str) {
                    Some("all") => "all",
                    _ => "following",
                };
                let activity = match self.pages.activities.get(scope, id).await {
                    Ok(activity) => activity,
                    Err(Error::NotFound | Error::Forbidden) => {
                        self.series_delete = None;
                        self.flash =
                            Some((StatusKind::Error, "The activity could not be found.".into()));
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                };
                let call = self.pages.call(
                    scope,
                    credential,
                    "delete_series",
                    "core.activity.delete_series",
                );
                match self
                    .pages
                    .contracts
                    .submit(
                        &call,
                        serde_json::json!({"id": id, "version": activity.version, "apply": apply}),
                    )
                    .await
                {
                    Ok(result) => {
                        let count = result["result"]["deleted"].as_array().map_or(0, Vec::len);
                        self.series_delete = None;
                        self.flash = Some((StatusKind::Success, series_delete::deleted(count)));
                    }
                    Err(error) => {
                        let message = match refusal(
                            error,
                            "You do not have permission to change this activity.",
                            "The series was not deleted. Try again.",
                        )? {
                            Refusal::Message(message) => message,
                            Refusal::Fields(fields) => sentences(&fields),
                        };
                        self.series_delete = Some((id, vec![message]));
                    }
                }
            }
            calendar::MOVE_OPEN => {
                self.moving = calendar::item_id(value).map(str::to_owned);
                self.move_errors.clear();
                self.notice = None;
            }
            calendar::MOVE_CANCEL => {
                self.moving = None;
                self.move_errors.clear();
            }
            calendar::MOVE | calendar::MOVE_SUBMIT => {
                let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
                let outcome = match CalendarMove::from_event(value, &clock.zone) {
                    Ok(change) => {
                        self.reschedule(scope, credential, name, &clock, &change)
                            .await?
                    }
                    Err(sentence) => Err(sentence),
                };
                match outcome {
                    Ok(message) => {
                        self.notice = Some((StatusKind::Success, message));
                        self.moving = None;
                        self.move_errors.clear();
                    }
                    Err(message) if name == calendar::MOVE_SUBMIT => {
                        self.move_errors = vec![message];
                    }
                    Err(message) => self.notice = Some((StatusKind::Error, message)),
                }
            }
            "clear" => {
                self.related = None;
                self.series = None;
                self.list.apply_filters(&HashMap::from([
                    ("search".into(), String::new()),
                    ("kind".into(), "all".into()),
                    ("state".into(), "all".into()),
                    ("mine".into(), "any".into()),
                ]));
            }
            "page" => self
                .list
                .put_page(value.get("page").and_then(Value::as_str)),
            "sort" => {
                self.list.next_sort(value["sort"].as_str().unwrap_or(""));
            }
            "grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                if let ColumnsOutcome::Sort(key) = self.columns.handle(&values) {
                    self.list.next_sort(&key);
                }
            }
            "new" => {
                if self.pages.activities.may_create(scope).await? {
                    let kind = match value["kind"].as_str() {
                        Some("call") => Kind::Call,
                        Some("meeting") => Kind::Meeting,
                        Some("note") => Kind::Note,
                        _ => Kind::Task,
                    };
                    let related = match &self.related {
                        Some(r) => self
                            .pages
                            .records
                            .resolve(scope, std::slice::from_ref(r))
                            .await?
                            .into_iter()
                            .next()
                            .filter(|r| r.label.is_some()),
                        None => None,
                    };
                    self.slot = None;
                    self.dialog = Some(Dialog::new(kind, related));
                } else {
                    self.flash = Some((
                        StatusKind::Error,
                        "You do not have permission to create activities.".into(),
                    ));
                }
            }
            "close_modal" => self.dialog = None,
            "request_defaults" => {
                if self.pages.activities.may_create(scope).await? {
                    let current = self.pages.activities.reminder_defaults(scope).await?;
                    self.defaults = Some(DefaultsDialog::new(&current));
                }
            }
            "cancel_defaults" => self.defaults = None,
            "save_reminder_defaults" => {
                let form = params::form_values_checked(value, "defaults", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let Some(dialog) = &mut self.defaults else {
                    return Ok(None);
                };
                if !dialog.read(&form) {
                    return Err(Error::Invalid);
                }
                let input = dialog.input();
                let call = self.pages.call(
                    scope,
                    credential,
                    "save_reminder_defaults",
                    "core.activity.set_reminder_defaults",
                );
                match self.pages.contracts.submit(&call, input).await {
                    Ok(_) => {
                        self.defaults = None;
                        self.flash = Some((
                            StatusKind::Success,
                            "Reminder defaults saved. They apply to activities you create from now on."
                                .into(),
                        ));
                    }
                    Err(error) => {
                        let message = match refusal(
                            error,
                            "You do not have permission to change reminder defaults.",
                            "The defaults were not saved. Try again.",
                        )? {
                            Refusal::Message(message) => message,
                            Refusal::Fields(fields) => sentences(&fields),
                        };
                        dialog.errors = vec![message];
                    }
                }
            }
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
            "request_close" => {
                let id = value["id"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                let activity = self.pages.activities.get(scope, id).await?;
                self.confirm = Some((activity.id, activity.title, activity.version));
            }
            "cancel_close" => self.confirm = None,
            "close" => {
                let Some((id, title, version)) = self.confirm.take() else {
                    return Ok(None);
                };
                let call = self
                    .pages
                    .call(scope, credential, "close", "core.activity.close");
                self.flash = Some(
                    match self
                        .pages
                        .contracts
                        .submit(&call, serde_json::json!({"id": id, "version": version}))
                        .await
                    {
                        Ok(_) => (StatusKind::Success, format!("“{title}” closed.")),
                        Err(error) => match refusal(
                            error,
                            "You do not have permission to close this activity.",
                            "The activity was not closed. Try again.",
                        )? {
                            Refusal::Message(message) => (StatusKind::Error, message),
                            Refusal::Fields(fields) => (StatusKind::Error, sentences(&fields)),
                        },
                    },
                );
            }
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(None)
    }

    async fn row(
        &self,
        scope: &Scope,
        clock: &Clock,
        activity: &Activity,
        related: Option<&bilibi_records::Resolved>,
        names: &HashMap<i64, String>,
    ) -> Result<FlexRow> {
        let id = activity.id.to_string();
        let mut kind = render(Icon {
            name: icon(activity.kind),
            class: "size-4",
        })?;
        kind.push(&Html::text(&format!(" {}", activity.kind.label())));
        let mut row = FlexRow::new(&id)
            .slot(
                "title",
                render(RecordLink {
                    workspace: self.framed,
                    kind: crate::MODULE,
                    record_id: &id,
                    href: &format!("/activities/{id}"),
                    class: "font-medium text-ink-strong hover:underline",
                    body: Html::text(&activity.title),
                })?,
            )
            .slot("kind", kind)
            .slot("status", status_badge(activity)?)
            .slot(
                "related",
                match related {
                    Some(resolved) => {
                        render(resolved.pill(format!("activities-{id}-related"), self.framed))?
                    }
                    None => Html::text("—"),
                },
            )
            .slot(
                "assignee",
                Html::text(
                    names
                        .get(&activity.assignee_user_id)
                        .map_or("—", String::as_str),
                ),
            );
        row.cells.insert(
            "when".into(),
            bilibi_ui::flex_table::Cell::instant(clock.instant(activity.when())),
        );
        if !activity.is_closed()
            && activity.kind.closing().is_some()
            && self.pages.activities.may_edit(scope, activity).await?
        {
            row.actions.push(&render(IconButton {
                id: &format!("activities-close-{id}"),
                event: "request_close",
                name: "id",
                value: &id,
                ..IconButton::new("check-circle", "Close")
            })?);
        }
        if activity.series_id.is_some() && self.pages.activities.may_edit(scope, activity).await? {
            row.actions.push(&render(IconButton {
                id: &format!("activities-delete-series-{id}"),
                event: "request_delete_series",
                name: "id",
                value: &id,
                kind: IconKind::Danger,
                title: "Delete series",
                ..IconButton::new("delete", "Delete series")
            })?);
        }
        row.dom_id = Some(format!("activities-{id}"));
        Ok(row)
    }

    /// A calendar drop, resize or Move dialog: the new time through the
    /// change contract, with the version the calendar drew. `Err` is the
    /// sentence the page shows; the item stays where the server drew it.
    async fn reschedule(
        &self,
        scope: &Scope,
        credential: &str,
        event: &str,
        clock: &Clock,
        change: &CalendarMove,
    ) -> Result<std::result::Result<String, String>> {
        let missing = || Ok(Err("The activity could not be found.".to_owned()));
        let (Ok(id), Ok(version)) = (change.id.parse::<i64>(), change.version.parse::<i64>())
        else {
            return missing();
        };
        let activity = match self.pages.activities.get(scope, id).await {
            Ok(activity) => activity,
            Err(Error::NotFound | Error::Forbidden) => return missing(),
            Err(error) => return Err(error),
        };
        if change
            .lane
            .as_ref()
            .is_some_and(|lane| *lane != activity.assignee_user_id.to_string())
        {
            return Ok(Err(
                "To give it to someone else, change Assigned to on its page.".to_owned(),
            ));
        }
        let CalendarWhen::Timed { start, end } = change.when else {
            return Ok(Err("Drop it on a time of day.".to_owned()));
        };
        let patch = match activity.kind {
            Kind::Call | Kind::Meeting => {
                serde_json::json!({"starts_at": start.to_string(), "ends_at": end.to_string()})
            }
            Kind::Task => {
                let mut patch = serde_json::json!({"due_at": start.to_string()});
                // The start keeps its distance from the due time.
                if let (Some(due), Some(begin)) = (activity.due_at, activity.start_at) {
                    if let Ok(moved) = begin.checked_add(start.duration_since(due)) {
                        patch["start_at"] = serde_json::json!(moved.to_string());
                    }
                }
                patch
            }
            Kind::Note => return Ok(Err("A note has no time to move.".to_owned())),
        };
        let call = self
            .pages
            .call(scope, credential, event, "core.activity.update");
        match self
            .pages
            .contracts
            .submit(
                &call,
                serde_json::json!({"id": id, "version": version, "patch": patch}),
            )
            .await
        {
            Ok(_) => Ok(Ok(format!(
                "“{}” moved to {}.",
                activity.title,
                clock.text(start)
            ))),
            Err(error) => Ok(Err(
                match refusal(
                    error,
                    "You do not have permission to move this activity.",
                    "The activity was not moved. Try again.",
                )? {
                    Refusal::Message(message) => message,
                    Refusal::Fields(fields) => sentences(&fields),
                },
            )),
        }
    }

    /// The activities in the calendar's range, drawn by base/ui `Calendar`
    /// with the work calendar's non-working days marked. Returns the HTML,
    /// its address query and how many items it drew.
    async fn calendar_view(
        &self,
        scope: &Scope,
        clock: &Clock,
        mut query: HashMap<String, String>,
    ) -> Result<(Html, HashMap<String, String>, usize)> {
        let today = jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::get(&clock.zone).unwrap_or(jiff::tz::TimeZone::UTC))
            .date();
        // The view and date decide the range; the lanes come from what it holds.
        let probe = CalendarState::parse(&self.calendar_query, today, &[]);
        let (from, to) = probe.range(&clock.zone).ok_or(Error::Internal)?;
        let mut filter = self.filter();
        filter.between = Some((from, to));
        filter.sort = Sort::When;
        filter.descending = false;
        filter.page = 1;
        filter.page_size = crate::CALENDAR_LIMIT;
        let page = self.pages.activities.list(scope, &filter).await?;
        let me = scope.user();
        let mut people: Vec<i64> = page.entries.iter().map(|a| a.assignee_user_id).collect();
        people.push(me);
        people.sort_unstable_by_key(|id| (*id != me, *id));
        people.dedup();
        let names = self.pages.names(scope, &people).await?;
        let lanes: Vec<CalendarLane> = people
            .iter()
            .map(|id| {
                let label = if *id == me {
                    "Me".to_owned()
                } else {
                    names
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| format!("Account {id}"))
                };
                CalendarLane::new(id.to_string(), label)
            })
            .collect();
        let state = CalendarState::parse(&self.calendar_query, today, &lanes);
        let shown: Vec<String> = state
            .shown_lanes(&lanes)
            .into_iter()
            .map(|lane| lane.key.clone())
            .collect();
        let may_create = self.pages.activities.may_create(scope).await?;
        let edit_all = may_create && self.pages.activities.may_edit_all(scope).await?;
        let now = jiff::Timestamp::now();
        let mut items = Vec::new();
        for a in &page.entries {
            let lane = a.assignee_user_id.to_string();
            if state.view == calendar::CalendarView::Shared && !shown.contains(&lane) {
                continue;
            }
            let item = match (a.kind, a.starts_at, a.ends_at, a.due_at) {
                (Kind::Call | Kind::Meeting, Some(start), Some(end), _) => {
                    CalendarItem::timed(a.id.to_string(), a.title.clone(), start, end)
                }
                (Kind::Task, _, _, Some(due)) => CalendarItem::timed(
                    a.id.to_string(),
                    a.title.clone(),
                    due,
                    due + jiff::SignedDuration::from_mins(TASK_MINUTES),
                ),
                _ => continue,
            }
            .kind(a.kind.label(), tone(a.kind))
            .href(format!("/activities/{}", a.id))
            .urgency(urgency(a, now))
            .lane(lane);
            let own = a.created_by == me || a.assignee_user_id == me;
            let item = if may_create && (own || edit_all) && !a.is_closed() {
                let item = item.movable(a.version.to_string());
                if a.kind.timed() {
                    item.resizable()
                } else {
                    item
                }
            } else {
                item
            };
            items.push(item);
        }
        let dates = state.dates();
        let mut marks = Vec::new();
        if let (Some(first), Some(last)) = (dates.first(), dates.last()) {
            for day in self
                .pages
                .activities
                .work_days(scope, *first, *last)
                .await?
            {
                use bilibi_work_calendar::{DayType, Reason};
                let kind = match day.day_type {
                    DayType::Working => continue,
                    DayType::Rest => bilibi_ui::DayKind::Rest,
                    DayType::Off => bilibi_ui::DayKind::Off,
                    DayType::Holiday => bilibi_ui::DayKind::Holiday,
                };
                let label = match &day.reason {
                    Reason::Holiday { name, .. }
                    | Reason::Substitute { name, .. }
                    | Reason::Exception { name } => name.clone(),
                    Reason::Week => String::new(),
                };
                marks.push(calendar::DayMark {
                    date: day.date,
                    kind,
                    label,
                });
            }
        }
        query.insert("display".into(), "calendar".into());
        let count = items.len();
        let mut drawn = Calendar::new(
            "activities-calendar",
            "/activities",
            state.clone(),
            clock.zone.clone(),
            today,
        );
        drawn.query = query.clone();
        drawn.lanes = lanes;
        drawn.items = items;
        drawn.day_marks = marks;
        drawn.follows_browser_zone = clock.display.mode == bilibi_datetime::Mode::Local;
        drawn.creatable = may_create;
        drawn.moving = self.moving.clone();
        drawn.notice = self.notice.clone().or_else(|| {
            (page.total > page.entries.len() as i64).then(|| {
                (
                    StatusKind::Warning,
                    format!(
                        "Showing the first {} of {} activities in this range. Narrow the view or the filters to see the rest.",
                        page.entries.len(),
                        page.total
                    ),
                )
            })
        });
        drawn.move_errors = self.move_errors.clone();
        let html = drawn.render().map_err(|_| Error::Internal)?;
        query.extend(state.to_params());
        Ok((html, query, count))
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
        let may_create = self.pages.activities.may_create(scope).await?;
        let mut actions = Html::EMPTY;
        if may_create {
            for (kind, label) in [
                ("call", "Log call"),
                ("meeting", "New meeting"),
                ("task", "New task"),
                ("note", "New note"),
            ] {
                actions.push(&render(Button {
                    id: &format!("activities-new-{kind}"),
                    button_type: "button",
                    event: "new",
                    name: "kind",
                    value: kind,
                    variant: if kind == "task" {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::default()
                    },
                    body: Html::text(label),
                    ..Button::default()
                })?);
            }
        }
        if may_create {
            actions.push(&render(Button {
                id: "activities-reminder-defaults",
                button_type: "button",
                event: "request_defaults",
                body: Html::text("Reminder defaults"),
                ..Button::default()
            })?);
        }
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                _ => flash.error = message,
            }
        }
        let mut body = render(FlashGroup {
            id: "activities-flash",
            flash,
        })?;
        let subtitle = match (&self.related, self.series) {
            (Some(_), _) => "About one record. Clear the filters to see every activity.",
            (None, Some(_)) => {
                "The occurrences of one repeating series. Clear the filters to see every activity."
            }
            (None, None) => "",
        };
        body.push(&render(Header {
            title: Html::text("Activities"),
            subtitle,
            actions,
            ..Header::default()
        })?);
        body.push(&render(Tabs {
            id: "activities-display",
            label: "Show activities as",
            class: "mb-4",
            tabs: vec![
                Tab {
                    id: "activities-display-list",
                    ..Tab::button("display", "list", "List", self.display == Display::Table)
                },
                Tab {
                    id: "activities-display-calendar",
                    ..Tab::button(
                        "display",
                        "calendar",
                        "Calendar",
                        self.display == Display::Calendar,
                    )
                },
                Tab {
                    id: "activities-display-board",
                    ..Tab::button(
                        "display",
                        "board",
                        "Status tiles",
                        self.display == Display::Board,
                    )
                },
            ],
        })?);
        let form = self.list.filters_form();
        let mut controls = vec![
            ToolbarControl::search(
                "activities-search",
                "search",
                "Search activities",
                &form["search"],
                "Search by subject...",
            ),
            ToolbarControl::select(
                "activities-kind",
                "kind",
                "Kind",
                &form["kind"],
                choices(&[
                    ("All kinds", "all"),
                    ("Calls", "call"),
                    ("Meetings", "meeting"),
                    ("Tasks", "task"),
                    ("Notes", "note"),
                ]),
            ),
            ToolbarControl::select(
                "activities-state",
                "state",
                "Open or closed",
                &form["state"],
                choices(&[
                    ("Open", "open"),
                    ("Closed", "closed"),
                    ("Open and closed", "all"),
                ]),
            ),
            ToolbarControl::select(
                "activities-mine",
                "mine",
                "Whose",
                &form["mine"],
                choices(&[
                    ("Everyone's I may see", "any"),
                    ("Assigned to me", "assignee"),
                    ("I am a guest", "guest"),
                ]),
            ),
        ];
        if self.display == Display::Board {
            // The tiles are the tasks, closed ones in their own columns.
            controls.retain(|control| {
                !matches!(control.id.as_str(), "activities-kind" | "activities-state")
            });
        }
        body.push(&render(FilterToolbar::new("activities-filters", controls))?);
        let mut query = HashMap::new();
        let rows = match self.display {
            Display::Calendar => {
                let (html, shown, count) =
                    self.calendar_view(scope, &clock, self.address()).await?;
                body.push(&html);
                query = shown;
                count
            }
            Display::Board => {
                let (html, shown, count) = self.board_view(scope, &clock).await?;
                body.push(&html);
                query = shown;
                count
            }
            Display::Table => {
                let (card, count) = self.table_view(scope, &clock).await?;
                body.push(&card);
                count
            }
        };
        if let Some((start, end)) = self.slot {
            body.push(&self.slot_dialog(&clock, start, end)?);
        }
        if let Some((id, errors)) = &self.series_delete {
            match self.pages.activities.get(scope, *id).await {
                Ok(activity) => body.push(&series_delete::dialog(&activity, errors, false)?),
                Err(Error::NotFound | Error::Forbidden) => self.series_delete = None,
                Err(error) => return Err(error),
            }
        }
        if let Some((_, title, _)) = &self.confirm {
            body.push(&render(ConfirmDialog {
                id: "activities-confirm",
                consequence: &format!("“{title}” will be closed."),
                detail: "A call or meeting is marked held, a task completed.",
                confirm: "Close",
                working: "Closing…",
                confirm_event: "close",
                cancel_event: "cancel_close",
            })?);
        }
        if let Some(dialog) = &self.defaults {
            body.push(&reminder_defaults::dialog(dialog)?);
        }
        if let Some(dialog) = &self.dialog {
            body.push(&dialog.render(
                &self.pages,
                &clock,
                "activities-new",
                "close_modal",
                self.framed,
            )?);
        }
        let region = render(Region {
            id: "activities-region",
            target: false,
            timezone_request: self.zone.request(),
            body,
        })?;
        let html = render(bilibi_ui::Page {
            id: "activities-index",
            body: region,
            ..bilibi_ui::Page::default()
        })?
        .to_string();
        if self.display == Display::Table {
            query = self.address();
        }
        Ok(LiveBody {
            html,
            query,
            rows,
            ..LiveBody::default()
        })
    }

    /// The list's own address: its filters, sort and page, and the record
    /// or series it is narrowed to.
    fn address(&self) -> HashMap<String, String> {
        let mut query = self.list.to_params();
        if let Some(related) = &self.related {
            query.insert("related".into(), format!("{}:{}", related.kind, related.id));
        }
        if let Some(series) = self.series {
            query.insert("series".into(), series.to_string());
        }
        query
    }

    /// The table of one page of activities, with its pagination.
    async fn table_view(&mut self, scope: &Scope, clock: &Clock) -> Result<(Html, usize)> {
        let mut page = self.pages.activities.list(scope, &self.filter()).await?;
        let total_pages = (page.total.max(0) as u64).div_ceil(self.list.page_size);
        if self.list.clamp_to_last_page(total_pages, true) {
            page = self.pages.activities.list(scope, &self.filter()).await?;
        }
        let references: Vec<Reference> = page
            .entries
            .iter()
            .filter_map(|a| a.related.clone())
            .collect();
        let named = self.pages.records.resolve(scope, &references).await?;
        let mut named = named.iter();
        let users: Vec<i64> = page.entries.iter().map(|a| a.assignee_user_id).collect();
        let names = self.pages.names(scope, &users).await?;
        let _ = self.columns.load(&page.entries, |a| a.id.to_string());
        let mut rows = Vec::with_capacity(page.entries.len());
        for activity in &page.entries {
            let related = activity.related.as_ref().and_then(|_| named.next());
            rows.push(self.row(scope, clock, activity, related, &names).await?);
        }

        let mut table = FlexTable::new("activities", self.columns.flex_columns(), rows)
            .sorted(self.list.sort_by.as_deref(), self.list.sort_dir);
        table.event = "grid";
        table.sort_event = "sort";
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Activities";
        table.actions = true;
        table.framed = false;
        if page.entries.is_empty() {
            let clear = render(Button {
                id: "activities-clear",
                button_type: "button",
                event: "clear",
                body: Html::text("Clear filters"),
                ..Button::default()
            })?;
            table.empty = Some(TableEmpty::State(
                EmptyState::missing(
                    "No activities match",
                    "Clear the filters to see every activity you may see.",
                )
                .action(clear),
            ));
        }
        let mut card = render(table)?;
        card.push(&render(Pagination::new(
            "activities-pagination",
            PageCounts::new(
                self.list.page,
                self.list.page_size,
                page.total.max(0) as u64,
            ),
            &SIZES,
        ))?);
        let card = render(Card {
            id: "activities-card",
            inner_class: "p-0",
            body: card,
            ..Card::default()
        })?;
        Ok((card, page.entries.len()))
    }
}
impl LiveState<Scope, Error> for Index {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(query) => {
                    self.list =
                        ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?;
                    self.related = related(query);
                    self.series = series(query);
                    self.display = Display::parse(query);
                    self.calendar_query = calendar_query(query);
                    self.notice = None;
                }
                LiveInput::Event(value) => {
                    if let Some(body) = self
                        .handle(request.scope, request.credential, value)
                        .await?
                    {
                        return Ok(body);
                    }
                }
            }
            self.view(request.scope).await
        })
    }
}
