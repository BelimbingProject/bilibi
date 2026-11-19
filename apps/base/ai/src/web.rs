//! The `base/ai.pages` adapter: `/system/ai-interactions` (operation
//! `interactions`) and `/system/ai-interactions/{id}` (`interaction`).
//! Mirrors the API Exchanges pages of `base/integration` (`src/web.rs`),
//! without payloads or deletion: an interaction row has no text to inspect
//! and expires by retention.
//!
//! The list is the shared flexible table over the built-in columns hosted by
//! Grid `PageColumns` (the arrangement is remembered per account), bounded
//! URL state (`ListState`), the outcome select and one search. The route
//! guard checks the list capability on every request and live event; the
//! operator scope sees every tenant. The record page shows the metadata.
use crate::{Ai, Filter, Interaction, Outcome};
use askama::Template;
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayContext, DisplayPolicy, Format, Precision};
use bilibi_grid::{Outcome as GridOutcome, PageColumns, PageViews};
use bilibi_locale::Locales;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    choices,
    list_state::FILTERS_FORM,
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, status_badge, BadgeKind, BuiltinColumn, Button, ColumnType, EmptyState, Facts,
    FilterToolbar, FlexRow, Html, Instant, ListSpec, ListState, PageCounts, Pagination, TableEmpty,
    ToolbarControl,
};
use jiff::Timestamp;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
    time::SystemTime,
};

pub const INDEX_PATH: &str = "/system/ai-interactions";
const PAGE_SIZES: [u64; 4] = [25, 50, 100, 300];
const OUTCOMES: [&str; 3] = ["", "generated", "unavailable"];
const API_EXCHANGES: &str = "/system/api-exchanges";

/// No sortable column (the log is newest first), page sizes 25/50/100/300
/// and the `status` filter.
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 25,
        filters: BTreeMap::from([("status".into(), bilibi_ui::Filter::one_of(&OUTCOMES, ""))]),
        ..ListSpec::default()
    }
    .shared()
    .expect("AI interaction list spec")
});

fn builtins() -> Vec<BuiltinColumn> {
    let typed = |id: &str, label: &str, kind: ColumnType| {
        let mut column = BuiltinColumn::new(id, label);
        column.kind = kind;
        column
    };
    vec![
        typed("created_at", "Time", ColumnType::Datetime),
        BuiltinColumn::new("module", "Module"),
        BuiltinColumn::new("model", "Provider / model"),
        BuiltinColumn::new("result", "Outcome"),
        typed("duration_ms", "Duration (ms)", ColumnType::Integer),
        BuiltinColumn::new("size", "Characters in / out"),
        BuiltinColumn::new("actor", "Person / principal"),
    ]
}

/// The module's page adapter; every field is shared by mounted states.
#[derive(Clone)]
pub struct Pages {
    pub db: Db,
    pub ai: Arc<Ai>,
    pub display: Arc<DisplayPolicy>,
    pub locales: Arc<Locales>,
    pub views: Arc<PageViews>,
    pub contracts: Arc<bilibi_agent_api::page::PageContracts>,
}

impl Pages {
    async fn context(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
    ) -> Result<DisplayContext> {
        let locale = self.locales.resolve(client, scope, None).await?.locale;
        self.display.display(client, scope, None, locale).await
    }
}

fn render(fragment: impl Template) -> Result<Html> {
    Html::render(&fragment).map_err(|_| Error::Internal)
}

fn parse(values: &HashMap<String, String>) -> Result<ListState> {
    ListState::parse(values, SPEC.clone()).map_err(|_| Error::Internal)
}

fn instant(display: &DisplayContext, at: SystemTime) -> Result<Instant> {
    let at = Timestamp::try_from(at).map_err(|_| Error::Internal)?;
    let rendered = display.render(at, Format::DateTime, Precision::Minute);
    Ok(Instant::new(
        &rendered.iso,
        &rendered.text,
        &rendered.company,
        &rendered.utc,
        rendered.mode.as_str(),
    ))
}

fn dash(value: Option<&str>) -> Html {
    Html::text(value.unwrap_or("—"))
}

fn number(value: Option<i64>) -> Html {
    dash(value.map(|value| value.to_string()).as_deref())
}

fn outcome_badge(interaction: &Interaction) -> Html {
    match interaction.reason.as_deref() {
        None => status_badge(BadgeKind::Success, "Generated"),
        Some("disabled") => status_badge(BadgeKind::Neutral, "AI off"),
        Some("not_configured") => status_badge(BadgeKind::Neutral, "No provider"),
        Some("timeout") => status_badge(BadgeKind::Warning, "Timed out"),
        Some(_) => status_badge(BadgeKind::Danger, "Provider error"),
    }
}

fn actor(interaction: &Interaction) -> String {
    match (&interaction.principal, interaction.user_id) {
        (Some(principal), _) => format!("System · {principal}"),
        (None, Some(user)) => user.to_string(),
        (None, None) => "—".to_owned(),
    }
}

/// The datetime tag through the shared instant markup.
#[derive(Template)]
#[template(
    source = r#"{% import "flex_table.html" as flex %}{% call flex::instant(id=id, i=i) %}{% endcall %}"#,
    ext = "html"
)]
struct TimeTag<'a> {
    id: &'a str,
    i: Instant,
}

#[derive(Template)]
#[template(source = r#"<a href="{{ href }}">{{ text }}</a>"#, ext = "html")]
struct Link<'a> {
    href: String,
    text: &'a str,
}

/// The provider over its model in normal rows.
#[derive(Template)]
#[template(
    source = r#"{{ provider }}{% if let Some(model) = model %}<span class="block text-xs text-muted">{{ model }}</span>{% endif %}"#,
    ext = "html"
)]
struct ModelCell<'a> {
    provider: &'a str,
    model: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "interactions.html")]
struct IndexView {
    toolbar: Html,
    operator: bool,
    table: Html,
    pagination: Html,
}

#[derive(Template)]
#[template(path = "interaction.html")]
struct ShowView<'a> {
    interaction_id: &'a str,
    all_link: Html,
    facts: Html,
}

/// The mounted list.
struct Index {
    pages: Pages,
    list: ListState,
    /// The column arrangement, remembered per account.
    columns: PageColumns,
    /// The rows changed since the column picker last saw them.
    reload: bool,
}

impl Index {
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        let views = pages.views.clone();
        Ok(Self {
            pages,
            list: parse(query)?,
            columns: PageColumns::mount(views, "ai-interactions", builtins())
                .and_then(|columns| columns.with_record_slots(fields::ALL))
                .map_err(|_| Error::Internal)?,
            reload: true,
        })
    }

    async fn open(&mut self, scope: &Scope, query: &HashMap<String, String>) -> Result<()> {
        self.columns.from_params(scope, query).await
    }

    fn query(&self) -> HashMap<String, String> {
        let mut query = self.list.to_params();
        query.extend(self.columns.params());
        query
    }

    fn row(&self, display: &DisplayContext, interaction: &Interaction) -> Result<FlexRow> {
        let compact = self.columns.mode() == bilibi_ui::RowMode::Compact;
        let mut row = self
            .columns
            .rows
            .iter()
            .find(|row| row.key == interaction.id)
            .cloned()
            .unwrap_or_else(|| FlexRow::new(interaction.id.clone()));
        row.dom_id = Some(format!("interaction-{}", interaction.id));
        row.slots.insert(
            "created_at".into(),
            render(TimeTag {
                id: &format!("interaction-time-{}", interaction.id),
                i: instant(display, interaction.created_at)?,
            })?,
        );
        row.slots.insert(
            "module".into(),
            render(Link {
                href: format!("{INDEX_PATH}/{}", interaction.id),
                text: &interaction.module,
            })?,
        );
        row.slots.insert(
            "model".into(),
            render(ModelCell {
                provider: interaction.provider.as_deref().unwrap_or("—"),
                model: interaction.model.as_deref().filter(|_| !compact),
            })?,
        );
        row.slots
            .insert("result".into(), outcome_badge(interaction));
        row.slots.insert(
            "duration_ms".into(),
            Html::text(&interaction.duration_ms.to_string()),
        );
        row.slots.insert(
            "size".into(),
            Html::text(&format!(
                "{} / {}",
                interaction.input_characters,
                interaction
                    .output_characters
                    .map_or_else(|| "—".to_owned(), |count| count.to_string())
            )),
        );
        row.slots
            .insert("actor".into(), Html::text(&actor(interaction)));
        if self.columns.columns.iter().any(|shown| {
            fields::ALL
                .iter()
                .any(|field| field.column == Some(shown.id.as_str()))
                && !row.slots.contains_key(&shown.id)
        }) {
            for (field, value) in interaction_facts(display, interaction, "interaction-at")? {
                if let Some(column) = field.column {
                    row.slots.entry(column.into()).or_insert(value);
                }
            }
        }
        Ok(row)
    }

    async fn render(&mut self, scope: &Scope) -> Result<String> {
        self.columns.refresh_views(scope).await?;
        let client = self.pages.db.get().await?;
        let display = self.pages.context(&**client, scope).await?;
        let filter = Filter {
            outcome: Outcome::parse(&self.list.filters["status"]),
            search: self.list.search.clone(),
        };
        let list = |page: u64, size: u64| {
            self.pages
                .ai
                .interactions(&**client, scope, &filter, page as i64, size as i64)
        };
        let mut page = list(self.list.page, self.list.page_size).await?;
        // An out-of-range page (a stale link, a narrower filter) lands on the
        // last real page instead of an empty one.
        if self
            .list
            .clamp_to_last_page(page.total_pages.max(0) as u64, true)
        {
            page = list(self.list.page, self.list.page_size).await?;
        }
        let operator = match bilibi_tenancy::prove_operator(&**client, scope).await {
            Ok(_) => true,
            Err(Error::Forbidden) => false,
            Err(error) => return Err(error),
        };
        // The column loader takes its own connection: release this one first.
        drop(client);
        if self.reload {
            let keys: Vec<Value> = page.entries.iter().map(|row| json!(row.id)).collect();
            self.columns.load(scope, &keys, &display).await?;
            self.reload = false;
        }
        let form = self.list.filters_form();
        let mut toolbar = FilterToolbar::new(
            "interaction-filters",
            vec![
                ToolbarControl::search(
                    "interaction-search",
                    "search",
                    "Search AI interactions",
                    &form["search"],
                    "Module, provider, model, person ID or principal",
                ),
                ToolbarControl::select(
                    "interaction-status",
                    "status",
                    "Outcome",
                    &form["status"],
                    choices(&[
                        ("All outcomes", ""),
                        ("Generated", "generated"),
                        ("Unavailable", "unavailable"),
                    ]),
                ),
            ],
        );
        toolbar.class = "mb-0 min-w-0 flex-1";
        let mut rows = Vec::with_capacity(page.entries.len());
        for interaction in &page.entries {
            rows.push(self.row(&display, interaction)?);
        }
        let mut table = self.columns.flex_table("interactions", rows);
        table.framed = false;
        table.caption = "AI interactions";
        table.empty = Some(TableEmpty::State(EmptyState::missing(
            "No AI interactions",
            "No AI calls match these filters.",
        )));
        IndexView {
            toolbar: render(toolbar)?,
            operator,
            table: render(table)?,
            pagination: render(Pagination::new(
                "interactions-pagination",
                PageCounts::new(
                    self.list.page,
                    self.list.page_size,
                    page.total_entries.max(0) as u64,
                )
                .or_more(page.more),
                &PAGE_SIZES,
            ))?,
        }
        .render()
        .map_err(|_| Error::Internal)
    }

    async fn event(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid)?;
        match kind {
            "filters" => {
                self.list.apply_filters(
                    &params::form_values_checked(value, FILTERS_FORM, params::FIELD_LIMIT)
                        .map_err(Error::refused)?,
                );
                self.reload = true;
            }
            "page" => {
                self.list
                    .put_page(value.get("page").and_then(Value::as_str));
                self.reload = true;
            }
            "grid" => {
                let query = self.query();
                // No column sorts: the log stays newest first.
                match self
                    .columns
                    .handle(
                        scope,
                        &PageColumns::event_values(value)?,
                        bilibi_grid::ViewCaller {
                            contracts: &self.pages.contracts,
                            credential,
                            service: "base/ai.pages",
                            event: "grid",
                            query,
                        },
                    )
                    .await?
                {
                    GridOutcome::Patch => self.reload = true,
                    GridOutcome::Update | GridOutcome::Noop | GridOutcome::Sort(_) => {}
                }
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
}

impl LiveState<Scope, Error> for Index {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.list = parse(values)?;
                    self.open(request.scope, values).await?;
                    self.reload = true;
                }
                LiveInput::Event(value) => {
                    self.event(request.scope, request.credential, value).await?
                }
            }
            let html = self.render(request.scope).await?;
            Ok(LiveBody {
                html,
                query: self.query(),
                ..LiveBody::default()
            })
        })
    }
}

/// The record page: read-only metadata.
async fn show(pages: &Pages, scope: &Scope, id: &str) -> Result<String> {
    let client = pages.db.get().await?;
    let interaction = pages.ai.interaction(&**client, scope, id).await?;
    let display = pages.context(&**client, scope).await?;
    ShowView {
        interaction_id: &interaction.id,
        all_link: render(Button {
            href: INDEX_PATH,
            id: "interaction-all",
            body: Html::text("All interactions"),
            ..Button::default()
        })?,
        facts: render(Facts {
            id: "interaction-facts",
            items: interaction_facts(&display, &interaction, "interaction-at")?
                .into_iter()
                .map(|(field, value)| field.fact(value))
                .collect(),
        })?,
    }
    .render()
    .map_err(|_| Error::Internal)
}

/// The id of `/system/ai-interactions/{id}`; anything unshaped is the same
/// not-found answer as a row another tenant owns.
fn interaction_id(request: &PageRequest<'_, Scope>) -> Result<String> {
    request
        .path
        .get("id")
        .filter(|id| !id.is_empty() && id.len() <= 40)
        .cloned()
        .ok_or(Error::NotFound)
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        Some(match operation {
            "interactions" => "AI Interactions",
            "interaction" => "AI Interaction",
            _ => return None,
        })
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let (html, live) = match request.operation {
                "interactions" => {
                    let mut index = Index::new(self.clone(), request.query)?;
                    index.open(scope, request.query).await?;
                    (index.render(scope).await?, true)
                }
                "interaction" => (show(self, scope, &interaction_id(&request)?).await?, false),
                _ => return Err(Error::NotFound),
            };
            Ok(PageBody {
                html,
                live,
                title: None,
            })
        })
    }

    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            match request.operation {
                "interactions" => {
                    let scope = request.scope.ok_or(Error::Unauthorized)?;
                    let mut index = Index::new(self.clone(), request.query)?;
                    index.open(scope, request.query).await?;
                    let state: Box<dyn LiveState<Scope, Error>> = Box::new(index);
                    Ok(Some(state))
                }
                "interaction" => Ok(None),
                _ => Err(Error::NotFound),
            }
        })
    }
}

// Metadata facts, shared with the list columns.
bilibi_ui::record_fields! { "base/ai", pub(crate) mod fields {
    MODULE: bilibi_ui::RecordField::new("module", "Module", bilibi_ui::RecordFieldType::String).excluded("The default Module column shows it"),
    PROVIDER: bilibi_ui::RecordField::new("provider", "Provider", bilibi_ui::RecordFieldType::String).excluded("The default Provider / model column shows it"),
    MODEL: bilibi_ui::RecordField::new("model", "Model", bilibi_ui::RecordFieldType::String).excluded("The default Provider / model column shows it"),
    OUTCOME: bilibi_ui::RecordField::new("outcome", "Outcome", bilibi_ui::RecordFieldType::String).excluded("The default Outcome column shows it"),
    DURATION: bilibi_ui::RecordField::new("duration", "Duration", bilibi_ui::RecordFieldType::Integer).excluded("The default Duration (ms) column shows it"),
    CREATED_AT: bilibi_ui::RecordField::new("created_at", "Time", bilibi_ui::RecordFieldType::DateTime),
    TENANT: bilibi_ui::RecordField::new("tenant", "Tenant", bilibi_ui::RecordFieldType::String),
    COMPANY: bilibi_ui::RecordField::new("company", "Company", bilibi_ui::RecordFieldType::String),
    PERSON: bilibi_ui::RecordField::new("person", "Person / principal", bilibi_ui::RecordFieldType::String).excluded("The default Person / principal column shows it"),
    CONNECTION: bilibi_ui::RecordField::new("connection", "Agent connection", bilibi_ui::RecordFieldType::String),
    IMPERSONATOR: bilibi_ui::RecordField::new("impersonator", "Impersonator", bilibi_ui::RecordFieldType::String),
    INPUT_CHARACTERS: bilibi_ui::RecordField::new("input_characters", "Characters sent", bilibi_ui::RecordFieldType::Integer),
    OUTPUT_CHARACTERS: bilibi_ui::RecordField::new("output_characters", "Characters answered", bilibi_ui::RecordFieldType::Integer),
    EXCHANGE: bilibi_ui::RecordField::new("exchange", "API exchange", bilibi_ui::RecordFieldType::String),
} }

fn interaction_facts(
    display: &DisplayContext,
    interaction: &Interaction,
    time_id: &str,
) -> Result<Vec<(bilibi_ui::RecordField, Html)>> {
    let outcome = match interaction.reason.as_deref() {
        None => "Generated".to_owned(),
        Some(reason) => format!("Unavailable ({})", reason.replace('_', " ")),
    };
    let exchange = match &interaction.exchange_id {
        Some(id) => render(Link {
            href: format!("{API_EXCHANGES}/{id}"),
            text: id,
        })?,
        None => dash(None),
    };
    Ok(vec![
        (fields::MODULE, Html::text(&interaction.module)),
        (fields::PROVIDER, dash(interaction.provider.as_deref())),
        (fields::MODEL, dash(interaction.model.as_deref())),
        (fields::OUTCOME, Html::text(&outcome)),
        (
            fields::DURATION,
            Html::text(&format!("{} ms", interaction.duration_ms)),
        ),
        (
            fields::CREATED_AT,
            render(TimeTag {
                id: time_id,
                i: instant(display, interaction.created_at)?,
            })?,
        ),
        (
            fields::TENANT,
            Html::text(&interaction.tenant_id.to_string()),
        ),
        (fields::COMPANY, number(interaction.company_id)),
        (fields::PERSON, Html::text(&actor(interaction))),
        (fields::CONNECTION, number(interaction.connection_id)),
        (fields::IMPERSONATOR, number(interaction.impersonator_id)),
        (
            fields::INPUT_CHARACTERS,
            number(Some(interaction.input_characters)),
        ),
        (
            fields::OUTPUT_CHARACTERS,
            number(interaction.output_characters),
        ),
        (fields::EXCHANGE, exchange),
    ])
}
