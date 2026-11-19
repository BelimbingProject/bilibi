//! Read-only inventory of customisations contributed by their owners.
use crate::{CustomisationRow, CustomisationSection, CUSTOMISATIONS};
use askama::Template;
use bilibi_agent_api::{
    page::{Call, PageContracts},
    Registry,
};
use bilibi_database::{Db, Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    BuiltinColumn, Card, Cell, ColumnsOutcome, ConfirmDialog, FlashMap, FlexRow, FlexTable, Header,
    Html, IconButton, Instant, ListColumns, ListSpec, ListState, Pagination, StatusKind,
    TableEmpty, NO_FLASH,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

const PAGE_SIZES: [u64; 3] = [25, 50, 100];
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 50,
        page_size_param: "per_page".into(),
        ..ListSpec::default()
    }
    .shared()
    .expect("customisations list spec")
});

#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub registry: Arc<Registry>,
    pub contracts: Arc<PageContracts>,
    pub sections: Vec<&'static CustomisationSection>,
    pub display: Arc<bilibi_datetime::DisplayPolicy>,
    pub locales: Arc<bilibi_locale::Locales>,
    pub directory: Arc<bilibi_principal_directory::PrincipalDirectory>,
}
#[derive(Clone)]
struct Item {
    section: &'static CustomisationSection,
    row: CustomisationRow,
}
impl Item {
    fn key(&self) -> String {
        format!("{}:{}", self.row.audit_kind, self.row.audit_id)
    }
}
#[derive(Template)]
#[template(
    source = r#"{% import "components.html" as ui %}{% import "feedback.html" as feedback %}<div data-shared-ui>{% call feedback::flash_group(messages=flash) %}{% endcall %}{% call ui::page(id="customisations-page") %}{{ header }}{{ card }}{{ dialog }}{% endcall %}</div>"#,
    ext = "html"
)]
struct View<'a> {
    flash: FlashMap<'a>,
    header: Html,
    card: Html,
    dialog: Html,
}
fn render(t: impl Template) -> Result<Html> {
    Html::render(&t).map_err(|_| Error::Internal)
}
struct Index {
    pages: Pages,
    list: ListState,
    columns: ListColumns,
    items: Vec<Item>,
    confirm: Option<Item>,
    flash: Option<(StatusKind, String)>,
    shown: usize,
}
impl Index {
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            pages,
            list: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: ListColumns::mount(
                "customisations",
                vec![
                    BuiltinColumn::new("section", "Kind"),
                    BuiltinColumn::new("change", "Customisation"),
                    BuiltinColumn::new("actor", "Changed by"),
                    BuiltinColumn::new("when", "Changed"),
                ],
            )
            .map_err(|_| Error::Internal)?,
            items: vec![],
            confirm: None,
            flash: None,
            shown: 0,
        })
    }
    async fn event(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        match value["type"].as_str().unwrap_or_default() {
            "page" => self.list.put_page(value["page"].as_str()),
            "grid" => {
                let values =
                    bilibi_ui::params::string_values_checked(value, bilibi_ui::params::FIELD_LIMIT)
                        .map_err(Error::refused)?;
                if matches!(self.columns.handle(&values), ColumnsOutcome::Sort(_)) {
                    return Err(Error::Invalid);
                }
            }
            "request_undo" => {
                let key = value["key"].as_str().ok_or(Error::Invalid)?;
                self.confirm = self.items.iter().find(|item| item.key() == key).cloned();
            }
            "cancel_confirm" => self.confirm = None,
            event if event.starts_with("customisation.") => {
                let Some(item) = self.confirm.take() else {
                    return Ok(());
                };
                if event != item.section.event {
                    return Err(Error::Invalid);
                }
                let call = Call {
                    scope,
                    credential,
                    service: CUSTOMISATIONS,
                    event: item.section.event,
                    contract: item.section.undo,
                };
                let answer = self
                    .pages
                    .contracts
                    .submit(&call, item.row.input.clone())
                    .await;
                self.flash = Some(match answer {
                    Ok(_) => (StatusKind::Success, "The customisation was changed.".into()),
                    Err(bilibi_agent_api::Error::Backend(
                        e @ (Error::Unauthorized | Error::StepUpRequired { .. }),
                    )) => return Err(e),
                    Err(_) => {
                        self.confirm = Some(item);
                        (StatusKind::Error, "The change could not be completed. Close the dialog, refresh and try again.".into())
                    }
                });
            }
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
    async fn render(&mut self, scope: &Scope) -> Result<String> {
        let mut items = Vec::new();
        for section in &self.pages.sections {
            let value = self
                .pages
                .registry
                .read(&self.pages.db, scope, section.read, &(section.read_input)())
                .await
                .map_err(|_| Error::Forbidden)?;
            items.extend(
                (section.rows)(&value["result"])
                    .into_iter()
                    .map(|row| Item { section, row }),
            );
        }
        self.items = items;
        let page = self.list.paginate(&self.items);
        let counts = page.counts;
        self.shown = page.entries.len();
        self.columns.load(page.entries, |item| {
            format!("{}:{}", item.row.audit_kind, item.row.audit_id)
        });
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
        let refs: Vec<_> = page
            .entries
            .iter()
            .map(|item| (item.row.audit_kind, item.row.audit_id))
            .collect();
        let changes = bilibi_audit::latest_changes(&**client, scope, &refs).await?;
        let candidates: Vec<_> = changes
            .iter()
            .filter_map(|change| {
                let kind = match change.actor_type.as_str() {
                    "user" => bilibi_principal_directory::PrincipalKind::User,
                    "agent" => bilibi_principal_directory::PrincipalKind::Agent,
                    _ => return None,
                };
                bilibi_principal_directory::PrincipalRef::new(kind, change.actor_id).ok()
            })
            .collect();
        let names = self
            .pages
            .directory
            .rank(&**client, scope, &candidates, &Default::default())
            .await
            .map_err(|_| Error::Internal)?;
        let mut rows = Vec::new();
        for item in page.entries {
            let key = item.key();
            let change = changes.iter().find(|change| {
                change.kind == item.row.audit_kind && change.id == item.row.audit_id
            });
            let actor = change
                .map(|change| {
                    let kind = match change.actor_type.as_str() {
                        "user" => Some(bilibi_principal_directory::PrincipalKind::User),
                        "agent" => Some(bilibi_principal_directory::PrincipalKind::Agent),
                        _ => None,
                    };
                    kind.and_then(|kind| {
                        bilibi_principal_directory::PrincipalRef::new(kind, change.actor_id).ok()
                    })
                    .map(|p| names.label(p))
                    .unwrap_or_else(|| {
                        change
                            .system_principal
                            .clone()
                            .unwrap_or_else(|| change.actor_type.clone())
                    })
                })
                .unwrap_or_else(|| "Unavailable".into());
            let when = if let Some(change) = change {
                let timestamp =
                    jiff::Timestamp::try_from(change.occurred_at).map_err(|_| Error::Internal)?;
                let value = display.render(
                    timestamp,
                    bilibi_datetime::Format::DateTime,
                    bilibi_datetime::Precision::Minute,
                );
                Cell::instant(Instant::new(
                    &value.iso,
                    &value.text,
                    &value.company,
                    &value.utc,
                    value.mode.as_str(),
                ))
            } else {
                Cell::text("Unavailable")
            };
            let mut row = FlexRow::new(&key)
                .slot("section", Html::text(item.section.title))
                .slot("change", Html::text(&item.row.description))
                .slot("actor", Html::text(&actor))
                .cell("when", when);
            row.actions = render(IconButton {
                event: "request_undo",
                name: "key",
                value: &key,
                ..IconButton::new("unarchive", item.section.action)
            })?;
            rows.push(row);
        }
        let mut table = FlexTable::new("customisations-table", self.columns.flex_columns(), rows);
        table.caption = "Customisations";
        table.actions = true;
        table.framed = false;
        if counts.total_entries == 0 {
            table.empty = Some(TableEmpty::Content(Html::text(
                "No customisations yet. Ask your AI assistant to make a change.",
            )));
        }
        let mut body = render(table)?;
        body.push(&render(Pagination::new(
            "customisations-pagination",
            counts,
            &PAGE_SIZES,
        ))?);
        let card = render(Card {
            id: "customisations-card",
            inner_class: "p-0",
            body,
            ..Card::default()
        })?;
        let dialog = if let Some(item) = &self.confirm {
            render(ConfirmDialog {
                id: "customisations-confirm",
                consequence: &format!("{}: {}?", item.section.action, item.row.description),
                detail: item.section.detail,
                confirm: item.section.action,
                working: "Working…",
                confirm_event: item.section.event,
                cancel_event: "cancel_confirm",
            })?
        } else {
            Html::EMPTY
        };
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                StatusKind::Error => flash.error = message,
                StatusKind::Warning => flash.warning = message,
                StatusKind::Info => flash.info = message,
            }
        }
        View {
            flash,
            header: render(Header {
                title: Html::text("Customisations"),
                subtitle: "Review and undo changes made for this tenant.",
                ..Header::default()
            })?,
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
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.list =
                        ListState::parse(values, SPEC.clone()).map_err(|_| Error::Internal)?
                }
                LiveInput::Event(value) => {
                    self.event(request.scope, request.credential, value).await?
                }
            };
            let html = self.render(request.scope).await?;
            Ok(LiveBody {
                html,
                query: self.list.to_params(),
                rows: self.shown,
                ..LiveBody::default()
            })
        })
    }
}
impl Page<Scope, Error> for Pages {
    fn title(&self, op: &str) -> Option<&'static str> {
        (op == "index").then_some("Customisations")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            if request.operation != "index" {
                return Err(Error::NotFound);
            }
            let html = Index::new(self.clone(), request.query)?
                .render(request.scope.ok_or(Error::Unauthorized)?)
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
            Ok(if request.operation == "index" {
                Some(Box::new(Index::new(self.clone(), request.query)?)
                    as Box<dyn LiveState<Scope, Error>>)
            } else {
                None
            })
        })
    }
}
