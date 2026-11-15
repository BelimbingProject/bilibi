//! `/rules` and `/rules/{id}` (`base/rules.pages`): the tenant's rules, and
//! one rule with the version that awaits approval or runs and its receipts.
//!
//! Rules are drafted through `base.rules.draft` (an agent turns a person's
//! words into one); these pages read them, approve a reviewed version
//! (`base.rules.approve`, through a confirmation) and switch a rule on and
//! off (`base.rules.switch`), calling the contracts agents call. The owner
//! rechecks every right and the approval review in its own transaction.
use crate::store::{RuleState, RuleView, Rules, RunView, VersionView};
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
    ColumnsOutcome, ConfirmDialog, Fact, Facts, FilterToolbar, FlashMap, FlexRow, FlexTable,
    Header, Html, Instant, ListColumns, ListSpec, ListState, Pagination, RecordLink,
    SectionHeading, StatusKind, TableEmpty, Toggle, ToolbarControl, NO_FLASH,
};
use jiff::Timestamp;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

mod show;

pub const INDEX: &str = "/rules";
pub const SHOW: &str = "/rules/{id}";
const PAGE_SIZES: [u64; 3] = [25, 50, 100];
const DENIED: &str = "You do not have permission to approve or switch rules.";

static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("rule list spec")
});

#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub rules: Arc<Rules>,
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
fn state_badge(state: RuleState, pending: bool) -> Result<Html> {
    render(match (state, pending) {
        (RuleState::Draft, _) => Badge::status(BadgeKind::Neutral, "Awaiting approval"),
        (RuleState::On, true) => {
            Badge::status(BadgeKind::Success, "On, new version awaiting approval")
        }
        (RuleState::On, false) => Badge::status(BadgeKind::Success, "On"),
        (RuleState::Off, _) => Badge::status(BadgeKind::Warning, "Off"),
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
#[template(path = "rules.html")]
struct IndexView<'a> {
    flash: FlashMap<'a>,
    header: Html,
    toolbar: Html,
    card: Html,
}

fn builtins() -> Vec<BuiltinColumn> {
    let mut last = BuiltinColumn::new("last_run", "Last run");
    last.kind = ColumnType::Datetime;
    vec![
        BuiltinColumn::new("name", "Rule"),
        BuiltinColumn::new("trigger", "When"),
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
}
impl Index {
    fn new(pages: Pages, query: &HashMap<String, String>, workspace: bool) -> Result<Self> {
        Ok(Self {
            pages,
            state: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: ListColumns::mount("rules", builtins()).map_err(|_| Error::Internal)?,
            rows_changed: true,
            workspace,
            flash: None,
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
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
    async fn render(&mut self, scope: &Scope) -> Result<String> {
        let pages = self.pages.clone();
        let client = pages.db.get().await?;
        let display = pages.display(&client, scope).await?;
        let size = self.state.page_size as i64;
        let offset = (self.state.page.saturating_sub(1) * self.state.page_size) as i64;
        let (rows, total) = pages
            .rules
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
            title: Html::text("Rules"),
            subtitle: "Actions that run on their own when something happens to a record.",
            ..Header::default()
        })?;
        let form = self.state.filters_form();
        let toolbar = render(FilterToolbar::new(
            "rules-filters",
            vec![ToolbarControl::search(
                "rules-search",
                "search",
                "Search rules",
                &form["search"],
                "Search by rule name...",
            )],
        ))?;
        let mut flex = Vec::with_capacity(rows.len());
        for rule in &rows {
            let id = rule.id.to_string();
            let href = format!("{INDEX}/{id}");
            let name = render(RecordLink {
                workspace: self.workspace,
                kind: "base/rules.rule",
                record_id: &id,
                href: &href,
                class: "font-medium hover:underline",
                body: Html::text(&rule.name),
            })?;
            let last = match rule.last_run {
                Some(at) => render(InstantCell {
                    id: &format!("rules-last-{id}"),
                    i: instant(&display, at),
                })?,
                None => Html::text("Not yet"),
            };
            let mut row = FlexRow::new(id.clone())
                .slot("name", name)
                .slot("trigger", Html::text(&rule.trigger_title))
                .slot("status", state_badge(rule.state, rule.pending)?)
                .slot(
                    "running",
                    Html::text(
                        &rule
                            .running
                            .map_or_else(|| "None".into(), |n| format!("Version {n}")),
                    ),
                )
                .slot("last_run", last);
            row.dom_id = Some(format!("rules-{id}"));
            flex.push(row);
        }
        let mut table = FlexTable::new("rules", self.columns.flex_columns(), flex);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Rules";
        table.framed = false;
        if total == 0 {
            table.empty = Some(TableEmpty::Content(Html::text(
                if !self.state.search.is_empty() {
                    "No rules match this search."
                } else if can_draft {
                    "No rules yet. Ask your AI agent to draft one from plain words (\u{201c}send new urgent cases to the priority queue\u{201d}); its draft appears here for approval."
                } else {
                    "No rules yet. An administrator who may draft rules can ask an AI agent to draft one."
                },
            )));
        }
        let mut body = render(table)?;
        body.push(&render(Pagination::new(
            "rules-pagination",
            bilibi_ui::PageCounts::new(self.state.page, self.state.page_size, total),
            &PAGE_SIZES,
        ))?);
        let card = render(Card {
            id: "rules-card",
            inner_class: "p-0",
            body,
            ..Card::default()
        })?;
        IndexView {
            flash: flash_map(&self.flash),
            header,
            toolbar,
            card,
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
                LiveInput::Event(value) => self.event(value)?,
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

fn rule_id(request: &PageRequest<'_, Scope>) -> Result<i64> {
    request
        .path
        .get("id")
        .and_then(|id| id.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .ok_or(Error::NotFound)
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        matches!(operation, "index" | "show").then_some("Rules")
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
                    let mut show = show::Show::new(self.clone(), rule_id(&request)?)?;
                    let html = show.render(scope).await?;
                    let client = self.db.get().await?;
                    let name = self
                        .rules
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
                "show" => Box::new(show::Show::new(self.clone(), rule_id(&request)?)?),
                _ => return Ok(None),
            };
            Ok(Some(state))
        })
    }
}
