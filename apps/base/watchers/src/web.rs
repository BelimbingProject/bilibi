//! `/watchers` and `/watchers/{id}` (`base/watchers.pages`): the tenant's watchers, and
//! one watcher with the version that awaits approval or runs and its receipts.
//!
//! Watchers are drafted through `base.watchers.draft` (an agent turns a person's
//! words into one); these pages read them, approve a reviewed version
//! (`base.watchers.approve`, through a confirmation) and switch a watcher on and
//! off (`base.watchers.switch`), calling the contracts agents call. The owner
//! rechecks every right and the approval review in its own transaction.
use crate::store::{RunView, VersionView, WatcherState, WatcherView, Watchers};
use crate::{APPROVE, MANAGE};
use askama::Template;
use bilibi_agent_api::{
    page::{Call, PageContracts},
    Error as ContractError,
};
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayContext, DisplayPolicy, Format, Precision};
use bilibi_locale::Locales;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    list_state::FILTERS_FORM,
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, Alert, Badge, BadgeKind, BuiltinColumn, Button, ButtonVariant, Card, ColumnType,
    ColumnsOutcome, ConfirmDialog, CreateButton, Fact, Facts, Field, FilterToolbar, FlashMap,
    FlexRow, FlexTable, Form, Header, Html, InputKind, Instant, ListColumns, ListSpec, ListState,
    Pagination, RecordLink, SectionHeading, StatusKind, TableEmpty, Toggle, ToolbarControl,
    NO_FLASH,
};
use jiff::Timestamp;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

mod editor;
mod script_run;
mod show;
mod words;

pub const INDEX: &str = "/watchers";
pub const SHOW: &str = "/watchers/{id}";
const PAGE_SIZES: [u64; 3] = [25, 50, 100];
const DENIED: &str = "You do not have permission to approve or switch watchers.";
const DENIED_DRAFT: &str = "You do not have permission to draft watchers.";

static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("watcher list spec")
});

#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub watchers: Arc<Watchers>,
    pub contracts: Arc<PageContracts>,
    pub display: Arc<DisplayPolicy>,
    pub locales: Arc<Locales>,
}
impl Pages {
    async fn display(
        &self,
        client: &tokio_postgres::Client,
        scope: &Scope,
    ) -> Result<DisplayContext> {
        let locale = self.locales.resolve(client, scope, None).await?.locale;
        self.display.display(client, scope, None, locale).await
    }
}

fn render(fragment: impl Template) -> Result<Html> {
    Html::render(&fragment).map_err(|_| Error::Internal)
}
fn flash_map(flash: &Option<(StatusKind, String)>) -> FlashMap<'_> {
    let mut map = NO_FLASH;
    if let Some((kind, message)) = flash {
        match kind {
            StatusKind::Success => map.success = message,
            StatusKind::Error => map.error = message,
            StatusKind::Warning => map.warning = message,
            StatusKind::Info => map.info = message,
        }
    }
    map
}
fn instant(display: &DisplayContext, at: Timestamp) -> Instant {
    let rendered = display.render(at, Format::DateTime, Precision::Minute);
    Instant::new(
        &rendered.iso,
        &rendered.text,
        &rendered.company,
        &rendered.utc,
        rendered.mode.as_str(),
    )
}
fn state_badge(state: WatcherState, pending: bool) -> Result<Html> {
    render(match (state, pending) {
        (WatcherState::NeedsReview, _) => Badge::status(BadgeKind::Warning, "Needs review"),
        (WatcherState::Draft, _) => Badge::status(BadgeKind::Neutral, "Awaiting approval"),
        (WatcherState::On, true) => {
            Badge::status(BadgeKind::Success, "On, new version awaiting approval")
        }
        (WatcherState::On, false) => Badge::status(BadgeKind::Success, "On"),
        (WatcherState::Off, _) => Badge::status(BadgeKind::Warning, "Off"),
    })
}

#[derive(Template)]
#[template(
    source = r#"{% import "flex_table.html" as flex %}{% call flex::instant(id=id, i=i) %}{% endcall %}"#,
    ext = "html"
)]
struct InstantCell<'a> {
    id: &'a str,
    i: Instant,
}
#[derive(Template)]
#[template(
    source = r#"{% for line in lines %}<span class="block">{{ line }}</span>{% endfor %}"#,
    ext = "html"
)]
struct Lines<'a> {
    lines: &'a [String],
}
#[derive(Template)]
#[template(
    source = r#"<ul class="list-disc pl-5">{% for line in lines %}<li>{{ line }}</li>{% endfor %}</ul>"#,
    ext = "html"
)]
struct Bullets<'a> {
    lines: &'a [String],
}

#[derive(Template)]
#[template(path = "watchers.html")]
struct IndexView<'a> {
    flash: FlashMap<'a>,
    header: Html,
    toolbar: Html,
    card: Html,
    dialog: Html,
}

fn builtins() -> Vec<BuiltinColumn> {
    let mut last = BuiltinColumn::new("last_run", "Last run");
    last.kind = ColumnType::Datetime;
    vec![
        BuiltinColumn::new("name", "Watcher"),
        BuiltinColumn::new("trigger", "Watch"),
        BuiltinColumn::new("status", "Status"),
        BuiltinColumn::new("running", "Running version"),
        last,
    ]
}

struct Index {
    pages: Pages,
    state: ListState,
    columns: ListColumns,
    rows_changed: bool,
    workspace: bool,
    flash: Option<(StatusKind, String)>,
    editor: Option<editor::Editor>,
}
impl Index {
    fn new(pages: Pages, query: &HashMap<String, String>, workspace: bool) -> Result<Self> {
        Ok(Self {
            pages,
            state: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: ListColumns::mount("watchers", builtins()).map_err(|_| Error::Internal)?,
            rows_changed: true,
            workspace,
            flash: None,
            editor: None,
        })
    }
    /// Answers `Some((watcher, version))` when the editor saved a draft.
    async fn event(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &Value,
    ) -> Result<Option<(i64, i64)>> {
        let get = |key: &str| value.get(key).and_then(Value::as_str);
        let kind = get("type").ok_or(Error::Invalid)?;
        match kind {
            "draft_new" => {
                self.flash = None;
                self.editor = Some(editor::Editor::new());
            }
            kind if kind.starts_with("draft_") => {
                let call = Call {
                    scope,
                    credential,
                    service: crate::PAGES,
                    event: "draft",
                    contract: "base.watchers.draft",
                };
                if let editor::Outcome::Saved {
                    watcher_id,
                    version,
                } =
                    editor::Editor::event(&mut self.editor, &self.pages, call, kind, value).await?
                {
                    return Ok(Some((watcher_id, version)));
                }
            }
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
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(None)
    }
    async fn render(&mut self, scope: &Scope) -> Result<String> {
        let pages = self.pages.clone();
        let client = pages.db.get().await?;
        let display = pages.display(&client, scope).await?;
        let size = self.state.page_size as i64;
        let offset = (self.state.page.saturating_sub(1) * self.state.page_size) as i64;
        let (rows, total) = pages
            .watchers
            .list(&**client, scope, &self.state.search, size, offset)
            .await?;
        let can_draft = bilibi_authz::decide(&**client, scope, MANAGE)
            .await?
            .allowed;
        drop(client);
        if self.rows_changed {
            self.columns.load(&rows, |row| row.id.to_string());
        }
        let header = render(Header {
            title: Html::text("Watchers"),
            subtitle: "A watcher waits for something to happen in Bilibi and then acts on it.",
            actions: if can_draft {
                render(CreateButton {
                    noun: "Watcher",
                    id: "watcher-new",
                    event: "draft_new",
                    button_type: "button",
                    ..CreateButton::default()
                })?
            } else {
                Html::EMPTY
            },
            ..Header::default()
        })?;
        let form = self.state.filters_form();
        let toolbar = render(FilterToolbar::new(
            "watchers-filters",
            vec![ToolbarControl::search(
                "watchers-search",
                "search",
                "Search watchers",
                &form["search"],
                "Search by watcher name...",
            )],
        ))?;
        let mut flex = Vec::with_capacity(rows.len());
        for watcher in &rows {
            let id = watcher.id.to_string();
            let href = format!("{INDEX}/{id}");
            let name = render(RecordLink {
                workspace: self.workspace,
                kind: "base/watchers.watcher",
                record_id: &id,
                href: &href,
                class: "font-medium hover:underline",
                body: Html::text(&watcher.name),
            })?;
            let last = match watcher.last_run {
                Some(at) => render(InstantCell {
                    id: &format!("watchers-last-{id}"),
                    i: instant(&display, at),
                })?,
                None => Html::text("Not yet"),
            };
            let mut row = FlexRow::new(id.clone())
                .slot("name", name)
                .slot("trigger", Html::text(&watcher.trigger_title))
                .slot("status", state_badge(watcher.state, watcher.pending)?)
                .slot(
                    "running",
                    Html::text(
                        &watcher
                            .running
                            .map_or_else(|| "None".into(), |n| format!("Version {n}")),
                    ),
                )
                .slot("last_run", last);
            row.dom_id = Some(format!("watchers-{id}"));
            flex.push(row);
        }
        let mut table = FlexTable::new("watchers", self.columns.flex_columns(), flex);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Watchers";
        table.framed = false;
        if total == 0 {
            table.empty = Some(TableEmpty::Content(Html::text(
                if !self.state.search.is_empty() {
                    "No watchers match this search."
                } else if can_draft {
                    "No watchers yet. Choose New watcher to draft one, or ask your AI agent to draft one from plain words (\u{201c}send new urgent cases to the priority queue\u{201d}); a draft waits here for approval."
                } else {
                    "No watchers yet. An administrator who may draft watchers can draft one, or ask an AI agent to."
                },
            )));
        }
        let mut body = render(table)?;
        body.push(&render(Pagination::new(
            "watchers-pagination",
            bilibi_ui::PageCounts::new(self.state.page, self.state.page_size, total),
            &PAGE_SIZES,
        ))?);
        let card = render(Card {
            id: "watchers-card",
            inner_class: "p-0",
            body,
            ..Card::default()
        })?;
        let dialog = match &self.editor {
            Some(editor) => editor.dialog(&pages, scope).await?,
            None => Html::EMPTY,
        };
        IndexView {
            flash: flash_map(&self.flash),
            header,
            toolbar,
            card,
            dialog,
        }
        .render()
        .map_err(|_| Error::Internal)
    }
}
impl LiveState<Scope, Error> for Index {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            self.rows_changed = true;
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.state =
                        ListState::parse(values, SPEC.clone()).map_err(|_| Error::Internal)?
                }
                LiveInput::Event(value) => {
                    let saved = self.event(request.scope, request.credential, value).await?;
                    if let Some((watcher_id, version)) = saved {
                        // A finished draft lands on the watcher, where an approver acts.
                        return Ok(LiveBody {
                            redirect: Some(format!("{INDEX}/{watcher_id}")),
                            flash: Some((
                                StatusKind::Success,
                                format!("Version {version} is saved. It runs once an administrator approves it."),
                            )),
                            ..LiveBody::default()
                        });
                    }
                }
            }
            let html = self.render(request.scope).await?;
            Ok(LiveBody {
                html,
                query: self.state.to_params(),
                ..LiveBody::default()
            })
        })
    }
}

fn watcher_id(request: &PageRequest<'_, Scope>) -> Result<i64> {
    request
        .path
        .get("id")
        .and_then(|id| id.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .ok_or(Error::NotFound)
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        matches!(operation, "index" | "show").then_some("Watchers")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            match request.operation {
                "index" => {
                    let mut index = Index::new(self.clone(), request.query, request.framed)?;
                    let html = index.render(scope).await?;
                    Ok(PageBody {
                        html,
                        live: true,
                        title: None,
                    })
                }
                "show" => {
                    let mut show = show::Show::new(self.clone(), watcher_id(&request)?)?;
                    let html = show.render(scope).await?;
                    let client = self.db.get().await?;
                    let name = self
                        .watchers
                        .get(&**client, scope, show.id)
                        .await?
                        .summary
                        .name;
                    Ok(PageBody {
                        html,
                        live: true,
                        title: Some(name),
                    })
                }
                _ => Err(Error::NotFound),
            }
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            let state: Box<dyn LiveState<Scope, Error>> = match request.operation {
                "index" => Box::new(Index::new(self.clone(), request.query, request.framed)?),
                "show" => Box::new(show::Show::new(self.clone(), watcher_id(&request)?)?),
                _ => return Ok(None),
            };
            Ok(Some(state))
        })
    }
}
