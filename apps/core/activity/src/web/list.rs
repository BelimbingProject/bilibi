//! `/activities`: every activity the reader may see, with search, kind,
//! open or closed, and "mine" filters in the address, and the New call, New
//! task and New note dialog.
use super::{
    event, form::Dialog, icon, refusal, render, sentences, status_badge, BrowserZone, Clock, Pages,
    Refusal, Region,
};
use crate::{Activity, Filter, Kind, Mine, Reference, Sort, State};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    choices,
    list_state::{InvalidPageSize, FILTERS_FORM},
    lists::RecordLink,
    page::*,
    params, BuiltinColumn, Button, ButtonVariant, Card, ColumnsOutcome, ConfirmDialog, EmptyState,
    Filter as ListFilter, FilterToolbar, FlashGroup, FlexRow, FlexTable, Header, Html, Icon,
    IconButton, ListColumns, ListSpec, ListState, PageCounts, Pagination, SortDir, StatusKind,
    TableEmpty, ToolbarControl, NO_FLASH,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
};

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
    framed: bool,
    zone: BrowserZone,
    dialog: Option<Dialog>,
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
            framed,
            zone: BrowserZone::new(),
            dialog: None,
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
            "clear" => {
                self.related = None;
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
                    self.dialog = Some(Dialog::new(kind, related));
                } else {
                    self.flash = Some((
                        StatusKind::Error,
                        "You do not have permission to create activities.".into(),
                    ));
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
        row.dom_id = Some(format!("activities-{id}"));
        Ok(row)
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
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
            rows.push(self.row(scope, &clock, activity, related, &names).await?);
        }

        let may_create = self.pages.activities.may_create(scope).await?;
        let mut actions = Html::EMPTY;
        if may_create {
            for (kind, label) in [
                ("call", "Log call"),
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
        let subtitle = match &self.related {
            Some(_) => "About one record. Clear the filters to see every activity.",
            None => "",
        };
        body.push(&render(Header {
            title: Html::text("Activities"),
            subtitle,
            actions,
            ..Header::default()
        })?);
        let form = self.list.filters_form();
        body.push(&render(FilterToolbar::new(
            "activities-filters",
            vec![
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
            ],
        ))?);
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
        body.push(&render(Card {
            id: "activities-card",
            inner_class: "p-0",
            body: card,
            ..Card::default()
        })?);
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
        let mut query = self.list.to_params();
        if let Some(related) = &self.related {
            query.insert("related".into(), format!("{}:{}", related.kind, related.id));
        }
        Ok(LiveBody {
            html,
            query,
            rows: page.entries.len(),
            ..LiveBody::default()
        })
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
