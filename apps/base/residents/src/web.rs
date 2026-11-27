//! `/system/readers` (`base/residents.pages`, operation `index`): every
//! reading on the server for the platform operator, with the process that
//! owns it, its state, last heartbeat and last error. Read only. The list
//! follows the Events page: the shared flexible table over built-in columns,
//! bounded URL state (`ListState`), one search and a status filter.
use crate::{Filter, Residents, State, Status, PAGE_SIZES};
use askama::Template;
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayContext, DisplayPolicy, Format, Precision};
use bilibi_locale::Locales;
use bilibi_tenancy::{Scope, Tenants};
use bilibi_ui::{
    choices,
    list_state::FILTERS_FORM,
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, status_badge, BadgeKind, BuiltinColumn, Card, ColumnType, ColumnsOutcome, EmptyState,
    FilterToolbar, FlexRow, FlexTable, Header, Html, Instant, ListColumns, ListSpec, ListState,
    PageCounts, Pagination, RowMode, TableEmpty, ToolbarControl,
};
use jiff::Timestamp;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
    time::SystemTime,
};

pub const PATH: &str = "/system/readers";

static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        filters: BTreeMap::from([(
            "status".into(),
            bilibi_ui::Filter::one_of(&["", "running", "starting", "retrying", "lost"], ""),
        )]),
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("resident reader list spec")
});

fn builtins() -> Vec<BuiltinColumn> {
    let instant = |id: &str, label: &str| {
        let mut column = BuiltinColumn::new(id, label);
        column.kind = ColumnType::Datetime;
        column
    };
    vec![
        BuiltinColumn::new("reader", "Reader"),
        BuiltinColumn::new("key", "Key"),
        BuiltinColumn::new("tenant", "Tenant"),
        BuiltinColumn::new("status", "Status"),
        BuiltinColumn::new("owner", "Server process"),
        instant("heartbeat_at", "Last heartbeat"),
        instant("started_at", "Running since"),
        BuiltinColumn::new("last_error", "Last error"),
    ]
}

/// The module's page adapter; every field is shared by mounted states.
#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub residents: Arc<Residents>,
    pub tenants: Arc<Tenants>,
    pub display: Arc<DisplayPolicy>,
    pub locales: Arc<Locales>,
}

fn render(fragment: impl Template) -> Result<Html> {
    Html::render(&fragment).map_err(|_| Error::Internal)
}
fn parse(query: &HashMap<String, String>) -> Result<ListState> {
    ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)
}

#[derive(Template)]
#[template(
    source = r#"<span class="whitespace-nowrap font-mono text-xs text-ink">{{ text }}</span>"#,
    ext = "html"
)]
struct KeyCell<'a> {
    text: &'a str,
}

/// An instant through the shared timestamp markup.
#[derive(Template)]
#[template(
    source = r#"{% import "flex_table.html" as flex %}<span class="whitespace-nowrap">{% call flex::instant(id=id, i=i) %}{% endcall %}</span>"#,
    ext = "html"
)]
struct TimeCell<'a> {
    id: &'a str,
    i: Instant,
}

/// The failure code and, in a normal row, when it happened.
#[derive(Template)]
#[template(
    source = r#"{% import "flex_table.html" as flex %}<span class="whitespace-nowrap font-mono text-xs text-ink">{{ code }}</span>{% if let Some(i) = at %}<span class="block whitespace-nowrap text-xs text-ink-muted">{% call flex::instant(id=id, i=i) %}{% endcall %}</span>{% endif %}"#,
    ext = "html"
)]
struct ErrorCell<'a> {
    id: &'a str,
    code: &'a str,
    at: Option<Instant>,
}

/// The status badge and, in a normal row, what happens next.
#[derive(Template)]
#[template(
    source = r#"{% import "flex_table.html" as flex %}{{ badge }}{% if let Some(i) = next %}<span class="block whitespace-nowrap text-xs text-ink-muted">Starts again {% call flex::instant(id=id, i=i) %}{% endcall %}</span>{% endif %}"#,
    ext = "html"
)]
struct StatusCell<'a> {
    id: &'a str,
    badge: Html,
    next: Option<Instant>,
}

#[derive(Template)]
#[template(path = "readers.html")]
struct IndexView {
    header: Html,
    toolbar: Html,
    card: Html,
}

struct Index {
    pages: Pages,
    state: ListState,
    columns: ListColumns,
    rows_changed: bool,
    rows: usize,
}

impl Index {
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            pages,
            state: parse(query)?,
            columns: ListColumns::mount("resident-readers", builtins())
                .map_err(|_| Error::Internal)?,
            rows_changed: true,
            rows: 0,
        })
    }

    fn event(&mut self, value: &Value) -> Result<()> {
        let get = |key: &str| value.get(key).and_then(Value::as_str);
        match get("type").ok_or(Error::Invalid)? {
            "filters" => self.state.apply_filters(
                &params::form_values_checked(value, FILTERS_FORM, params::FIELD_LIMIT)
                    .map_err(Error::refused)?,
            ),
            "page" => self.state.put_page(get("page")),
            "grid" => match self.columns.handle(
                &params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?,
            ) {
                ColumnsOutcome::Sort(_) => return Err(Error::Invalid),
                ColumnsOutcome::Update | ColumnsOutcome::Noop => self.rows_changed = false,
            },
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    async fn render(&mut self, scope: &Scope) -> Result<String> {
        let pages = self.pages.clone();
        let filter = Filter {
            search: self.state.search.clone(),
            state: State::parse(&self.state.filters["status"]),
        };
        // The facade decides the operator and the capability on every render.
        let (display, found, proof) = {
            let client = pages.db.get().await?;
            let list =
                |page: u64, size: u64| pages.residents.list(&**client, scope, &filter, page, size);
            let mut found = list(self.state.page, self.state.page_size).await?;
            let total_pages = (found.total.max(0) as u64).div_ceil(self.state.page_size);
            // A page past the end (a stale link, a narrower filter) lands on
            // the last real one.
            if self.state.clamp_to_last_page(total_pages, true) {
                found = list(self.state.page, self.state.page_size).await?;
            }
            let locale = pages.locales.resolve(&**client, scope, None).await?.locale;
            let display = pages
                .display
                .display(&**client, scope, None, locale)
                .await?;
            let proof = bilibi_tenancy::prove_operator(&**client, scope).await?;
            (display, found, proof)
        };
        // Tenant names through Tenancy's own contract, after the connection
        // above went back to the pool; a miss shows the number alone.
        let mut names: HashMap<i64, String> = HashMap::new();
        for status in &found.entries {
            if let std::collections::hash_map::Entry::Vacant(slot) = names.entry(status.tenant) {
                if let Ok(Some(tenant)) = pages.tenants.fetch(&proof, status.tenant).await {
                    slot.insert(tenant.name);
                }
            }
        }
        self.rows = found.entries.len();
        if self.rows_changed {
            self.columns
                .load(&found.entries, |status| status.id.to_string());
        }
        let header = render(Header {
            title: Html::text("Resident readers"),
            subtitle:
                "Connections the server keeps open to outside systems, such as a phone system.",
            ..Header::default()
        })?;
        let form = self.state.filters_form();
        let toolbar = render(FilterToolbar::new(
            "readers-filters",
            vec![
                ToolbarControl::search(
                    "readers-search",
                    "search",
                    "Search readers",
                    &form["search"],
                    "Search by reader, key or server process...",
                ),
                ToolbarControl::select(
                    "readers-status",
                    "status",
                    "Status",
                    &form["status"],
                    choices(&[
                        ("All statuses", ""),
                        ("Running", "running"),
                        ("Starting", "starting"),
                        ("Waiting to retry", "retrying"),
                        ("Taking over", "lost"),
                    ]),
                ),
            ],
        ))?;
        let compact = self.columns.mode() == RowMode::Compact;
        let mut rows = Vec::with_capacity(found.entries.len());
        for status in &found.entries {
            rows.push(row(&display, status, names.get(&status.tenant), compact)?);
        }
        let mut table = FlexTable::new("resident-readers", self.columns.flex_columns(), rows)
            .phone_columns(&["key", "status", "last_error"]);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Resident readers";
        table.framed = false;
        if found.total == 0 {
            let filtered = !filter.search.is_empty() || filter.state.is_some();
            table.empty = Some(TableEmpty::State(if filtered {
                EmptyState::missing(
                    "No readers match these filters",
                    "Clear the search and status to see every reader.",
                )
            } else {
                EmptyState::missing(
                    "No readers yet",
                    "A reader appears here when a module starts keeping a connection open.",
                )
            }));
        }
        let mut body = render(table)?;
        body.push(&render(Pagination::new(
            "readers-pagination",
            PageCounts::new(
                self.state.page,
                self.state.page_size,
                found.total.max(0) as u64,
            ),
            &PAGE_SIZES,
        ))?);
        let card = render(Card {
            id: "readers-card",
            inner_class: "p-0",
            body,
            ..Card::default()
        })?;
        IndexView {
            header,
            toolbar,
            card,
        }
        .render()
        .map_err(|_| Error::Internal)
    }
}

fn instant(display: &DisplayContext, at: SystemTime) -> Result<Instant> {
    let at = Timestamp::try_from(at).map_err(|_| Error::Internal)?;
    let rendered = display.render(at, Format::DateTime, Precision::Second);
    Ok(Instant::new(
        &rendered.iso,
        &rendered.text,
        &rendered.company,
        &rendered.utc,
        rendered.mode.as_str(),
    ))
}

/// The badge an operator reads: status words stay sentence case and say what
/// the server is doing, never only a colour. A reading that is not running
/// and whose reader this server does not have will not start here.
fn badge(status: &Status) -> Html {
    match status.state {
        State::Running => status_badge(BadgeKind::Success, "Running"),
        _ if !status.mounted => status_badge(BadgeKind::Neutral, "Not mounted"),
        State::Lost => status_badge(BadgeKind::Warning, "Taking over"),
        State::Starting => status_badge(BadgeKind::Neutral, "Starting"),
        State::Retrying => status_badge(BadgeKind::Warning, "Waiting to retry"),
    }
}

fn row(
    display: &DisplayContext,
    status: &Status,
    tenant: Option<&String>,
    compact: bool,
) -> Result<FlexRow> {
    let dom = format!("reader-{}", status.id);
    let time = |slot: &str, at: Option<SystemTime>| -> Result<Html> {
        match at {
            Some(at) => render(TimeCell {
                id: &format!("{dom}-{slot}"),
                i: instant(display, at)?,
            }),
            None => Ok(Html::text("—")),
        }
    };
    let next = (!compact && status.mounted && status.state == State::Retrying)
        .then(|| instant(display, status.available_at))
        .transpose()?;
    let last_error = match &status.last_error {
        Some(code) => render(ErrorCell {
            id: &format!("{dom}-error-at"),
            code,
            at: status
                .last_error_at
                .filter(|_| !compact)
                .map(|at| instant(display, at))
                .transpose()?,
        })?,
        None => Html::text("—"),
    };
    // A run that is over shows no start: the column says "Running since".
    let running = status.state == State::Running;
    let mut row = FlexRow::new(status.id.to_string())
        .slot(
            "reader",
            render(KeyCell {
                text: &status.reader,
            })?,
        )
        .slot("key", render(KeyCell { text: &status.key })?)
        .slot(
            "tenant",
            Html::text(&tenant.map_or_else(
                || format!("Tenant {}", status.tenant),
                |name| format!("{name} (#{})", status.tenant),
            )),
        )
        .slot(
            "status",
            render(StatusCell {
                id: &format!("{dom}-next"),
                badge: badge(status),
                next,
            })?,
        )
        .slot("owner", Html::text(status.owner.as_deref().unwrap_or("—")))
        .slot("heartbeat_at", time("heartbeat", status.heartbeat_at)?)
        .slot(
            "started_at",
            time("started", status.started_at.filter(|_| running))?,
        )
        .slot("last_error", last_error);
    row.dom_id = Some(dom);
    Ok(row)
}

impl LiveState<Scope, Error> for Index {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            self.rows_changed = true;
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => self.state = parse(values)?,
                LiveInput::Event(value) => self.event(value)?,
            }
            let html = self.render(request.scope).await?;
            Ok(LiveBody {
                html,
                query: self.state.to_params(),
                rows: self.rows,
                ..LiveBody::default()
            })
        })
    }
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "index").then_some("Resident readers")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            if request.operation != "index" {
                return Err(Error::NotFound);
            }
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let mut index = Index::new(self.clone(), request.query)?;
            let html = index.render(scope).await?;
            Ok(PageBody {
                html,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            if request.operation != "index" {
                return Ok(None);
            }
            let state: Box<dyn LiveState<Scope, Error>> =
                Box::new(Index::new(self.clone(), request.query)?);
            Ok(Some(state))
        })
    }
}
