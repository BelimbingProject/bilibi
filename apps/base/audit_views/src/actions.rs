//! `/audit/actions` (`base/audit_views.pages`, operation `actions`): the
//! operational action log, newest first.
//!
//! The route guard checks `admin.audit.log.list` on every request and live
//! event. `admin.audit.log.manage` only decides whether the retain control is
//! drawn; the write asks again, inside its own transaction, and a refusal
//! flashes instead of clearing the page. Retaining flips a bookmark beside the
//! immutable record (`bilibi_audit::toggle_retained`) and is itself audited.
//!
//! Dates are shown through the request's `DisplayContext`. The list reads one
//! period at a time (`period::ListPeriod`, the last 30 days by default), counts
//! its matches only up to `bilibi_audit::MAX_COUNT` and orders by time alone,
//! because the ledger only grows.
use crate::{
    period::{Changed, ListPeriod},
    present::{self, Presentation},
    MANAGE,
};
use askama::Template;
use bilibi_audit::{ActionEntry, ActionPage, ActionQuery, EVENT_FAMILIES, MAX_PAGE_SIZE};
use bilibi_authz::{authorize_key, effective_capabilities};
use bilibi_database::{Change, Db, Error, MutationBoundary, Result};
use bilibi_datetime::{DisplayContext, DisplayPolicy};
use bilibi_locale::Locales;
use bilibi_tenancy::{ClientIdentity, Scope, SelfReport};
use bilibi_ui::{
    choices,
    list_state::FILTERS_FORM,
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, BadgeKind, BuiltinColumn, ColumnType, ColumnsOutcome, Filter, FilterToolbar, FlashMap,
    FlexRow, FlexTable, Html, ListColumns, ListSpec, ListState, PageCounts, Pagination, SortDir,
    TableEmpty, ToolbarControl,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
    time::SystemTime,
};

const PAGE_SIZES: [u64; 4] = [25, 50, 100, 300];
const ACTOR_TYPES: [&str; 8] = [
    "user",
    "agent",
    "guest",
    "console",
    "scheduler",
    "queue",
    "system",
    "unknown",
];
const FORBIDDEN: &str = "You do not have permission to manage audit logs.";
const NOT_UPDATED: &str = "Could not update retention status.";

/// `occurred_at` is the only order (newest first, or oldest first); the
/// page-size key is `page_size`; diagnostics default to hidden.
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    let with_any = |values: &[&'static str]| -> Vec<&'static str> {
        let mut all = vec![""];
        all.extend_from_slice(values);
        all
    };
    ListSpec {
        sortable: BTreeMap::from([("occurred_at".into(), SortDir::Desc)]),
        default_sort: Some("occurred_at".into()),
        page_sizes: PAGE_SIZES.to_vec(),
        page_size_param: "page_size".into(),
        filters: BTreeMap::from([
            (
                "actor_type".into(),
                Filter::one_of(&with_any(&ACTOR_TYPES), ""),
            ),
            (
                "event_family".into(),
                Filter::one_of(&with_any(&EVENT_FAMILIES), ""),
            ),
            (
                "result".into(),
                Filter::one_of(&["", "failure", "retained"], ""),
            ),
            (
                "diagnostics".into(),
                Filter::one_of(&["hide", "show"], "hide"),
            ),
        ]),
        ..ListSpec::default()
    }
    .shared()
    .expect("audit actions list spec")
});

fn builtins() -> Vec<BuiltinColumn> {
    let sortable = |mut column: BuiltinColumn, key: &str| {
        column = column.sortable(key);
        column.sort_id = Some(format!("actions-sort-{key}"));
        column
    };
    let mut occurred = sortable(BuiltinColumn::new("occurred_at", "Occurred"), "occurred_at");
    occurred.kind = ColumnType::Datetime;
    vec![
        occurred,
        BuiltinColumn::new("actor_type", "Actor"),
        BuiltinColumn::new("event", "Action"),
        BuiltinColumn::new("context", "Context"),
        BuiltinColumn::new("result", "Result"),
        BuiltinColumn::new("trace_id", "Trace"),
        BuiltinColumn::new("agent_self_report", SelfReport::LABEL),
        BuiltinColumn::new("client_identity", ClientIdentity::LABEL),
        BuiltinColumn::new("retain", "Retain").right(),
    ]
}

#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub boundary: Arc<MutationBoundary>,
    pub display: Arc<DisplayPolicy>,
    pub locales: Arc<Locales>,
}
impl Pages {
    /// The reader's clock and date format.
    async fn display(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
    ) -> Result<DisplayContext> {
        let locale = self.locales.resolve(client, scope, None).await?.locale;
        self.display.display(client, scope, None, locale).await
    }
}

/// What the last read returned, kept so a column-arrangement press redraws
/// without another query.
struct Shown {
    page: ActionPage,
    display: DisplayContext,
    can_manage: bool,
    /// Why the chosen range lists nothing, when it cannot be resolved.
    range_error: Option<&'static str>,
}

struct Actions {
    pages: Pages,
    list: ListState,
    period: ListPeriod,
    columns: ListColumns,
    flash: Option<String>,
    shown: Option<Shown>,
}

fn parse(values: &HashMap<String, String>) -> Result<ListState> {
    ListState::parse(values, SPEC.clone()).map_err(|_| Error::Internal)
}
fn render(fragment: impl Template) -> Result<Html> {
    Html::render(&fragment).map_err(|_| Error::Internal)
}

impl Actions {
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            pages,
            list: parse(query)?,
            period: ListPeriod::new("actions-period", query),
            columns: ListColumns::mount("actions-table", builtins())
                .map_err(|_| Error::Internal)?,
            flash: None,
            shown: None,
        })
    }

    fn query(&self, since: SystemTime, until: SystemTime) -> ActionQuery {
        let filter = |key: &str| self.list.filters[key].clone();
        ActionQuery {
            search: self.list.search.clone(),
            actor_type: filter("actor_type"),
            event_family: filter("event_family"),
            result: filter("result"),
            diagnostics: self.list.filters["diagnostics"] == "show",
            since,
            until,
            descending: self.list.sort_dir != Some(SortDir::Asc),
            page: self.list.page,
            page_size: self.list.page_size.min(MAX_PAGE_SIZE),
        }
    }

    /// Reads the page, the reader's clock and what the reader may manage. An
    /// out-of-range page (a stale link, a shrunk filter) lands on the last real
    /// page instead of an empty one with no pager.
    async fn load(&mut self, scope: &Scope) -> Result<()> {
        let mut client = self.pages.db.get().await?;
        let display = self.pages.display(&**client, scope).await?;
        let can_manage = effective_capabilities(&**client, scope)
            .await?
            .contains(MANAGE);
        let (since, until, range_error) = self.period.resolve(&display);
        let mut page =
            bilibi_audit::list_actions(&mut client, scope, &self.query(since, until)).await?;
        // A read that timed out has no total to clamp against.
        if !page.timed_out
            && self
                .list
                .clamp_to_last_page(page.total_entries.div_ceil(self.list.page_size), true)
        {
            page =
                bilibi_audit::list_actions(&mut client, scope, &self.query(since, until)).await?;
        }
        // The rows changed: ListColumns forgets an unfinished add box.
        self.columns
            .load(&page.entries, |action| action.id.to_string());
        self.shown = Some(Shown {
            page,
            display,
            can_manage,
            range_error,
        });
        Ok(())
    }

    /// Flips one action's retention. `can_manage` only shows the control; this
    /// asks the capability again, so a grant removed after the page opened
    /// still refuses.
    async fn toggle_retain(&mut self, scope: &Scope, id: Option<&str>) -> Result<()> {
        let Some(id) = id
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|id| *id > 0)
        else {
            self.flash = Some(NOT_UPDATED.into());
            return Ok(());
        };
        let mut client = self.pages.db.get().await?;
        let outcome: Result<bool> = async {
            let mut tx = self
                .pages
                .boundary
                .begin(
                    &mut client,
                    scope.mutation_context("audit.action.retention"),
                )
                .await?;
            // Decided inside the write's own transaction, so a grant removed
            // a moment ago cannot slip between the check and the write.
            authorize_key(tx.sql(), scope, MANAGE).await?;
            match bilibi_audit::toggle_retained(tx.sql(), scope, id).await? {
                Some((before, after)) => {
                    tx.record(Change {
                        kind: "audit_action_retention".into(),
                        id,
                        old: json!({"is_retained": before}),
                        new: json!({"is_retained": after}),
                    })
                    .await?;
                    tx.commit().await?;
                    Ok(true)
                }
                None => {
                    tx.rollback().await?;
                    Ok(false)
                }
            }
        }
        .await;
        match outcome {
            Ok(true) => {}
            Err(Error::Forbidden) => self.flash = Some(FORBIDDEN.into()),
            Ok(false) | Err(Error::Invalid | Error::NotFound | Error::Database(_)) => {
                self.flash = Some(NOT_UPDATED.into());
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    async fn view(&mut self, scope: &Scope, requery: bool) -> Result<String> {
        if requery || self.shown.is_none() {
            self.load(scope).await?;
        }
        let shown = self.shown.as_ref().ok_or(Error::Internal)?;
        let mut flash = bilibi_ui::NO_FLASH;
        if let Some(message) = &self.flash {
            flash.error = message;
        }
        let form = self.list.filters_form();
        let mut toolbar = FilterToolbar::new(
            "actions-filters",
            vec![
                ToolbarControl::search(
                    "actions-search",
                    "search",
                    "Search actions",
                    &form["search"],
                    "Search event, URL, actor role, or trace...",
                ),
                ToolbarControl::select(
                    "actions-event-family",
                    "event_family",
                    "Filter by event family",
                    &form["event_family"],
                    choices(&[
                        ("All Events", ""),
                        ("HTTP", "http"),
                        ("Authentication", "auth"),
                        ("Console", "console"),
                        ("SQL console", "database"),
                        ("Queue", "queue"),
                        ("Domain", "domain"),
                    ]),
                ),
                ToolbarControl::select(
                    "actions-result",
                    "result",
                    "Filter by result",
                    &form["result"],
                    choices(&[
                        ("All Results", ""),
                        ("Failures", "failure"),
                        ("Retained", "retained"),
                    ]),
                ),
                ToolbarControl::select(
                    "actions-diagnostics",
                    "diagnostics",
                    "Diagnostic traffic",
                    &form["diagnostics"],
                    choices(&[("Hide Diagnostics", "hide"), ("Show Diagnostics", "show")]),
                ),
                ToolbarControl::select(
                    "actions-actor-type",
                    "actor_type",
                    "Filter by actor type",
                    &form["actor_type"],
                    choices(&[
                        ("All Actor Types", ""),
                        ("User", "user"),
                        ("Employee", "agent"),
                        ("Guest", "guest"),
                        ("Console", "console"),
                        ("Scheduler", "scheduler"),
                        ("Queue", "queue"),
                        ("System", "system"),
                        ("Unknown", "unknown"),
                    ]),
                ),
            ],
        );
        toolbar.class = "mb-0 min-w-0 flex-1";
        let toolbar = render(toolbar)?;
        let mut rows = Vec::new();
        for action in &shown.page.entries {
            rows.push(row(&shown.display, shown.can_manage, action)?);
        }
        let mut table = FlexTable::new("actions-table", self.columns.flex_columns(), rows)
            .sorted(self.list.sort_by.as_deref(), self.list.sort_dir);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.framed = false;
        table.caption = "Audit actions";
        if shown.page.total_entries == 0 {
            let narrowed = !(self.list.search.is_empty()
                && self.list.filters["actor_type"].is_empty()
                && self.list.filters["event_family"].is_empty()
                && self.list.filters["result"].is_empty());
            table.empty = Some(TableEmpty::Content(Html::text(if shown.page.timed_out {
                "Nothing to show: the search was stopped before it finished."
            } else if narrowed {
                "No actions in this period match the filters."
            } else {
                "No actions in this period. Choose a longer period, or Show Diagnostics to include background traffic."
            })));
        }
        let pagination = render(Pagination::new(
            "actions-pagination",
            PageCounts::new(
                self.list.page,
                self.list.page_size,
                shown.page.total_entries,
            ),
            &PAGE_SIZES,
        ))?;
        ActionsView {
            flash,
            toolbar,
            period: render(self.period.filter.clone())?,
            range_error: shown.range_error,
            timed_out: shown.page.timed_out,
            more: shown.page.more,
            hide_diagnostics: self.list.filters["diagnostics"] == "hide",
            table: render(table)?,
            pagination,
        }
        .render()
        .map_err(|_| Error::Internal)
    }
}

#[derive(Template)]
#[template(path = "actions.html")]
struct ActionsView<'a> {
    flash: FlashMap<'a>,
    toolbar: Html,
    period: Html,
    range_error: Option<&'static str>,
    timed_out: bool,
    more: bool,
    hide_diagnostics: bool,
    table: Html,
    pagination: Html,
}

enum Cell {
    Occurred(Html),
    Actor {
        label: String,
        sub: String,
    },
    Event {
        summary: String,
        source: &'static str,
        diagnostic: bool,
        event: String,
    },
    Context(String),
    Result {
        kind: BadgeKind,
        label: String,
    },
    Trace(Option<String>),
    Retain {
        id: i64,
        retained: bool,
        can_manage: bool,
        icon: &'static str,
    },
}
#[derive(Template)]
#[template(path = "actions_cell.html")]
struct CellView {
    cell: Cell,
}

fn row(display: &DisplayContext, can_manage: bool, action: &ActionEntry) -> Result<FlexRow> {
    let Presentation {
        source,
        summary,
        context,
        result,
        variant,
        diagnostic,
    } = present::present(action);
    let cell = |cell: Cell| render(CellView { cell });
    let mut flex = FlexRow::new(action.id.to_string());
    flex.dom_id = Some(format!("actions-{}", action.id));
    for (column, html) in [
        (
            "occurred_at",
            cell(Cell::Occurred(present::time(
                display,
                &format!("action-occurred-{}", action.id),
                action.occurred_at,
                "",
            )?))?,
        ),
        (
            "actor_type",
            cell(Cell::Actor {
                label: present::actor_label(&action.into()),
                sub: present::actor_sub(&action.into()),
            })?,
        ),
        (
            "event",
            cell(Cell::Event {
                summary,
                source,
                diagnostic,
                event: action.event.clone(),
            })?,
        ),
        ("context", cell(Cell::Context(context))?),
        (
            "result",
            cell(Cell::Result {
                kind: variant,
                label: result,
            })?,
        ),
        (
            "trace_id",
            cell(Cell::Trace(
                action.trace_id.clone().filter(|trace| !trace.is_empty()),
            ))?,
        ),
        // The same muted mono text as the trace: a recorded label, not a link.
        (
            "client_identity",
            cell(Cell::Trace(action.client_identity.clone()))?,
        ),
        (
            "agent_self_report",
            cell(Cell::Trace(action.agent_self_report.clone()))?,
        ),
        (
            "retain",
            cell(Cell::Retain {
                id: action.id,
                retained: action.is_retained,
                can_manage,
                icon: if action.is_retained {
                    "retained"
                } else {
                    "retain"
                },
            })?,
        ),
    ] {
        flex.slots.insert(column.into(), html);
    }
    Ok(flex)
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "actions").then_some("Audit Actions")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            if request.operation != "actions" {
                return Err(Error::NotFound);
            }
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let html = Actions::new(self.clone(), request.query)?
                .view(scope, true)
                .await?;
            Ok(PageBody {
                html,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            if request.operation != "actions" {
                return Err(Error::NotFound);
            }
            let state: Box<dyn LiveState<Scope, Error>> =
                Box::new(Actions::new(self.clone(), request.query)?);
            Ok(Some(state))
        })
    }
}

impl LiveState<Scope, Error> for Actions {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            let mut requery = true;
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.list = parse(values)?;
                    self.period.read(values);
                }
                LiveInput::Event(event) => {
                    let get = |key: &str| event.get(key).and_then(Value::as_str);
                    match get("type") {
                        Some("filters") => self.list.apply_filters(
                            &params::form_values_checked(event, FILTERS_FORM, params::FIELD_LIMIT)
                                .map_err(Error::refused)?,
                        ),
                        Some("page") => self.list.put_page(get("page")),
                        Some("grid") => {
                            let values: HashMap<String, String> = event
                                .as_object()
                                .into_iter()
                                .flatten()
                                .filter_map(|(key, value)| {
                                    Some((key.clone(), value.as_str()?.to_owned()))
                                })
                                .collect();
                            match self.columns.handle(&values) {
                                ColumnsOutcome::Sort(key) => {
                                    if !self.list.next_sort(&key) {
                                        return Err(Error::Invalid);
                                    }
                                }
                                ColumnsOutcome::Update => requery = false,
                                ColumnsOutcome::Noop => {}
                            }
                        }
                        Some("toggle_retain") => {
                            self.toggle_retain(request.scope, get("id")).await?
                        }
                        Some("clear-flash") => {
                            if get("key") == Some("error") {
                                self.flash = None;
                            }
                            requery = false;
                        }
                        Some("browser_timezone") => self.period.browser_timezone(get("timezone")),
                        Some(kind) => {
                            let display = {
                                let client = self.pages.db.get().await?;
                                self.pages.display(&**client, request.scope).await?
                            };
                            match self.period.event(kind, event, &display)? {
                                Some(Changed::Applied) => self.list.put_page(Some("1")),
                                Some(Changed::Control) => requery = false,
                                None => return Err(Error::Invalid),
                            }
                        }
                        None => return Err(Error::Invalid),
                    }
                }
            }
            let html = self.view(request.scope, requery).await?;
            let mut query = self.list.to_params();
            query.extend(self.period.url());
            Ok(LiveBody {
                html,
                query,
                ..LiveBody::default()
            })
        })
    }
}
