//! Operator-only diagnostics list and status-bar panel.
use crate::{
    diagnostics::{Provider, Row, PROVIDERS},
    VIEW,
};
use bilibi_database::{Db, Error, Result};
use bilibi_module_registry::{
    contributions::{Access, Embed, EmbedContext, Method, Route, EMBEDS, ROUTES},
    Catalog, Contribution, Provenance,
};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    BuiltinColumn, FlexRow, FlexTable, Header, Html, ListColumns, StatusbarTag, TableEmpty,
};
use std::{collections::HashMap, sync::Arc};
const PATH: &str = "/system/diagnostics";
const ROW_LIMIT: usize = 100;
const PROVIDER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
const SERVICE: &str = "base/perf.diagnostics-pages";

#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub settings: Arc<bilibi_settings::Settings>,
    pub providers: Arc<Vec<&'static Provider>>,
    pub display: Arc<bilibi_datetime::DisplayPolicy>,
    pub locales: Arc<bilibi_locale::Locales>,
}
impl Pages {
    async fn authorize(&self, scope: &Scope) -> Result<()> {
        let c = self.db.get().await?;
        bilibi_authz::authorize_operator(&**c, scope, VIEW).await?;
        Ok(())
    }
    async fn rows(&self) -> (Vec<Row>, bool) {
        let mut rows = Vec::new();
        let mut unavailable = false;
        let mut reads = tokio::task::JoinSet::new();
        for &provider in self.providers.iter() {
            let db = self.db.clone();
            let settings = self.settings.clone();
            reads.spawn(async move {
                tokio::time::timeout(PROVIDER_TIMEOUT, (provider.read)(&db, &settings)).await
            });
        }
        while let Some(result) = reads.join_next().await {
            match result {
                Ok(Ok(Ok(found))) => rows.extend(found),
                _ => unavailable = true,
            }
        }
        rows.sort_by_key(|r| (r.severity != "error", std::cmp::Reverse(r.last_seen)));
        (rows, unavailable)
    }
}
pub(crate) fn providers(
    catalog: &Catalog,
) -> bilibi_module_registry::Result<Vec<&'static Provider>> {
    catalog
        .entries::<Provider>(PROVIDERS)?
        .iter()
        .map(|entry| entry.get::<Provider>())
        .collect()
}
fn panel(
    rows: Vec<Row>,
    unavailable: bool,
    display: &bilibi_datetime::DisplayContext,
) -> Result<String> {
    let errors: i64 = rows
        .iter()
        .filter(|r| r.severity == "error")
        .map(|r| r.count)
        .sum();
    let warnings: i64 = rows
        .iter()
        .filter(|r| r.severity == "warning")
        .map(|r| r.count)
        .sum();
    let (count, word, state) = if errors > 0 {
        (errors, "error", "error")
    } else {
        (warnings, "warning", "warning")
    };
    let label = if count == 0 && unavailable {
        "Diagnostics unavailable".to_owned()
    } else {
        format!("{count} {word}{}", if count == 1 { "" } else { "s" })
    };
    let tag = Html::render(&StatusbarTag {
        id: "app-diagnostics-tag",
        label: &label,
        title: &label,
        controls: "app-diagnostics-panel",
        state,
        quiet: false,
    })
    .map_err(|_| Error::Internal)?;
    let link = Html::render(&bilibi_ui::ActionLink {
        id: "app-diagnostics-all",
        icon: "",
        title: "Open diagnostics",
        href: PATH,
        method: "",
        body: Html::text("All diagnostics"),
    })
    .map_err(|_| Error::Internal)?;
    let visible = unavailable || !rows.is_empty();
    let mut body = availability(unavailable)?;
    for (index, row) in rows.into_iter().take(ROW_LIMIT).enumerate() {
        let at = jiff::Timestamp::try_from(row.last_seen).map_err(|_| Error::Internal)?;
        let rendered = display.render(
            at,
            bilibi_datetime::Format::DateTime,
            bilibi_datetime::Precision::Minute,
        );
        let instant = bilibi_ui::Instant::new(
            &rendered.iso,
            &rendered.text,
            &rendered.company,
            &rendered.utc,
            rendered.mode.as_str(),
        );
        let mut content = Html::text(&format!("{} · {} · ", row.message, row.count));
        content.push(
            &Html::render(&bilibi_ui::InstantView {
                id: &format!("diagnostic-{index}-last"),
                instant: &instant,
            })
            .map_err(|_| Error::Internal)?,
        );
        body.push(
            &Html::render(&bilibi_ui::Card {
                title: &row.source,
                body: content,
                ..Default::default()
            })
            .map_err(|_| Error::Internal)?,
        );
    }
    body.push(&link);
    Ok(Html::render(&bilibi_ui::StatusbarPanel {
        id: "app-diagnostics",
        panel_id: "app-diagnostics-panel",
        label: "Diagnostics",
        visible,
        tag,
        body,
    })
    .map_err(|_| Error::Internal)?
    .to_string())
}
fn availability(unavailable: bool) -> Result<Html> {
    if !unavailable {
        return Ok(Html::EMPTY);
    }
    Html::render(&bilibi_ui::Alert {
        kind: bilibi_ui::StatusKind::Warning,
        body: Html::text("Some diagnostics could not be loaded."),
        ..Default::default()
    })
    .map_err(|_| Error::Internal)
}
struct State {
    pages: Pages,
    panel: bool,
    columns: ListColumns,
}
impl State {
    fn new(pages: Pages, panel: bool) -> Result<Self> {
        let columns = ListColumns::mount(
            "diagnostics-table",
            vec![
                BuiltinColumn::new("severity", "Severity"),
                BuiltinColumn::new("source", "Source"),
                BuiltinColumn::new("message", "Message"),
                BuiltinColumn::new("count", "Count"),
                BuiltinColumn::new("last", "Last seen"),
            ],
        )
        .map_err(|_| Error::Internal)?;
        Ok(Self {
            pages,
            panel,
            columns,
        })
    }
    async fn body(&self, scope: &Scope) -> Result<String> {
        self.pages.authorize(scope).await?;
        let client = self.pages.db.get().await?;
        let locale = self
            .pages
            .locales
            .resolve(&**client, scope, None)
            .await?
            .locale;
        let display = self
            .pages
            .display
            .display(&**client, scope, None, locale)
            .await?;
        drop(client);
        let (rows, unavailable) = self.pages.rows().await;
        if self.panel {
            return panel(rows, unavailable, &display);
        }
        let mut body = Html::render(&Header {
            title: Html::text("Diagnostics"),
            subtitle: "Review recent server errors and warnings.",
            ..Header::default()
        })
        .map_err(|_| Error::Internal)?;
        body.push(&availability(unavailable)?);
        let entries = rows
            .iter()
            .take(ROW_LIMIT)
            .enumerate()
            .map(|(i, row)| {
                let at = jiff::Timestamp::try_from(row.last_seen).map_err(|_| Error::Internal)?;
                let rendered = display.render(
                    at,
                    bilibi_datetime::Format::DateTime,
                    bilibi_datetime::Precision::Minute,
                );
                let instant = bilibi_ui::Instant::new(
                    &rendered.iso,
                    &rendered.text,
                    &rendered.company,
                    &rendered.utc,
                    rendered.mode.as_str(),
                );
                Ok(FlexRow::new(i.to_string())
                    .slot("severity", Html::text(&row.severity))
                    .slot("source", Html::text(&row.source))
                    .slot("message", Html::text(&row.message))
                    .slot("count", Html::text(&row.count.to_string()))
                    .cell("last", bilibi_ui::Cell::instant(instant)))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut table = FlexTable::new("diagnostics-table", self.columns.flex_columns(), entries);
        table.event = "diagnostics_grid";
        table.caption = "Diagnostics";
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.empty = Some(TableEmpty::Content(Html::text(if unavailable {
            "No results are available."
        } else {
            "No diagnostics have been recorded."
        })));
        body.push(&Html::render(&table).map_err(|_| Error::Internal)?);
        Ok(body.to_string())
    }
}
impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "index").then_some("Diagnostics")
    }
    fn render<'a>(&'a self, req: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let panel = match req.operation {
                "index" => false,
                "status" => true,
                _ => return Err(Error::NotFound),
            };
            let state = State::new(self.clone(), panel)?;
            Ok(PageBody {
                html: state.body(req.scope.ok_or(Error::Unauthorized)?).await?,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, req: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            self.authorize(req.scope.ok_or(Error::Unauthorized)?)
                .await?;
            let panel = match req.operation {
                "index" => false,
                "status" => true,
                _ => return Err(Error::NotFound),
            };
            Ok(Some(
                Box::new(State::new(self.clone(), panel)?) as Box<dyn LiveState<Scope, Error>>
            ))
        })
    }
}
impl LiveState<Scope, Error> for State {
    fn update<'a>(&'a mut self, req: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match req.input {
                LiveInput::Refresh => (),
                LiveInput::Event(v)
                    if !self.panel
                        && v.get("type").and_then(|v| v.as_str()) == Some("diagnostics_grid") =>
                {
                    let values: HashMap<String, String> = v
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter_map(|(k, v)| {
                            let value = match v {
                                serde_json::Value::String(s) => s.clone(),
                                serde_json::Value::Number(n) => n.to_string(),
                                _ => return None,
                            };
                            Some((k.clone(), value))
                        })
                        .collect();
                    self.columns.handle(&values);
                }
                _ => return Err(Error::Invalid),
            }
            Ok(LiveBody {
                html: self.body(req.scope).await?,
                ..LiveBody::default()
            })
        })
    }
}
const PROV: Provenance = Provenance {
    module: "base/perf",
    source: file!(),
    line: line!(),
};
inventory::submit! { Contribution::new(ROUTES, "base/perf.diagnostics", PROV, &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant, method:Method::Get, path:PATH, service:SERVICE, operation:"index", access:Access::OperatorCapability(VIEW), framed:true }) }
inventory::submit! { Contribution::new(EMBEDS, "base/perf.diagnostics-status", PROV, &Embed { slot:"shell.diagnostics", context:EmbedContext::Shell, service:SERVICE, operation:"status", access:Access::OperatorCapability(VIEW) }) }
bilibi_menu::contribute!(
    "base/perf",
    bilibi_menu::MenuItem::page("admin.system.diagnostics-list", "Diagnostics", PATH)
        .under("admin.system.diagnostics")
        .icon("information-circle")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(5)
);
