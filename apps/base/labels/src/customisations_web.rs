//! Read-only inventory of customisations contributed by their owners.
use crate::{CustomisationRow, CustomisationSection, Undo, CUSTOMISATIONS};
use askama::Template;
use bilibi_agent_api::{
    page::{Call, PageContracts},
    Error as ContractError,
};
use bilibi_database::{Db, Error, Result};
use bilibi_principal_directory::{PrincipalKind, PrincipalRef};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    list_state::FILTERS_FORM,
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, Alert, BuiltinColumn, Card, Cell, FlashMap, FlexRow, FlexTable, Header, Html,
    IconButton, Instant, ListColumns, ListSpec, ListState, Pagination, StatusKind, TableEmpty,
    NO_FLASH,
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
    pub contracts: Arc<PageContracts>,
    pub sections: Vec<&'static CustomisationSection>,
    pub display: Arc<bilibi_datetime::DisplayPolicy>,
    pub locales: Arc<bilibi_locale::Locales>,
    pub directory: Arc<bilibi_principal_directory::PrincipalDirectory>,
}
#[derive(Clone)]
struct Item {
    section: &'static CustomisationSection,
    undo: &'static Undo,
    row: CustomisationRow,
}
impl Item {
    fn key(&self) -> String {
        format!("{}:{}", self.row.audit_kind, self.row.audit_id)
    }

    fn action_key(&self) -> String {
        format!("{}@{}", self.key(), self.row.revision)
    }
}
#[derive(Template)]
#[template(
    source = r#"{% import "dialog.html" as dialog %}{% call dialog::confirm_dialog(id="customisations-confirm", consequence=title, detail=item.undo.detail, confirm=item.undo.action, working="Working…", confirm_event=item.undo.event, cancel_event="cancel_confirm", confirm_key=key) %}{% endcall %}"#,
    ext = "html"
)]
struct UndoDialog<'a> {
    title: String,
    item: &'a Item,
    key: String,
}
#[derive(Template)]
#[template(
    source = r#"{% import "components.html" as ui %}{% import "feedback.html" as feedback %}<div data-shared-ui>{% call feedback::flash_group(messages=flash) %}{% endcall %}{% call ui::page(id="customisations-page") %}{{ header }}{{ restricted }}{{ card }}{{ dialog }}{% endcall %}</div>"#,
    ext = "html"
)]
struct View<'a> {
    flash: FlashMap<'a>,
    header: Html,
    restricted: Html,
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
            "filters" => self.list.apply_filters(
                &params::form_values_checked(value, FILTERS_FORM, params::FIELD_LIMIT)
                    .map_err(Error::refused)?,
            ),
            "page" => self.list.put_page(value["page"].as_str()),
            "grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.columns.handle(&values);
            }
            "request_undo" => {
                let key = value["key"].as_str().ok_or(Error::Invalid)?;
                self.confirm = self
                    .items
                    .iter()
                    .find(|item| item.action_key() == key)
                    .cloned();
            }
            "cancel_confirm" => self.confirm = None,
            event if event.starts_with("customisation.") => {
                let Some(item) = self.confirm.take_if(|item| {
                    event == item.undo.event
                        && value["key"].as_str() == Some(item.action_key().as_str())
                }) else {
                    return Ok(());
                };
                let call = Call {
                    scope,
                    credential,
                    service: CUSTOMISATIONS,
                    event: item.undo.event,
                    contract: item.undo.operation,
                };
                let message = match self.pages.contracts.submit(&call, item.row.input).await {
                    Ok(_) => {
                        self.flash =
                            Some((StatusKind::Success, "The customisation was changed.".into()));
                        return Ok(());
                    }
                    Err(ContractError::InvalidInput(fields)) => fields
                        .into_values()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(". "),
                    Err(ContractError::Backend(Error::Refused(message))) => message,
                    Err(ContractError::Backend(Error::Conflict)) => {
                        "The customisation changed. Review the updated row before trying again."
                            .into()
                    }
                    Err(ContractError::NotFound | ContractError::Backend(Error::NotFound)) => {
                        "The customisation is no longer available.".into()
                    }
                    Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                        "You do not have permission to make this change.".into()
                    }
                    Err(ContractError::Backend(error)) => return Err(error),
                    Err(_) => return Err(Error::Internal),
                };
                self.flash = Some((StatusKind::Error, message));
            }
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
    async fn render(&mut self, scope: &Scope) -> Result<String> {
        let mut items = Vec::new();
        let mut reads = HashMap::new();
        let mut restricted = Vec::new();
        for section in &self.pages.sections {
            let Some(undo) = &section.undo else {
                continue;
            };
            let key = (undo.read, (undo.read_input)());
            let value = match reads.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let (read, input) = entry.key();
                    let value = match self.pages.contracts.read(scope, read, input).await {
                        Ok(value) => Some(value),
                        Err(
                            ContractError::Forbidden | ContractError::Backend(Error::Forbidden),
                        ) => None,
                        Err(ContractError::Backend(error)) => return Err(error),
                        Err(_) => return Err(Error::Internal),
                    };
                    entry.insert(value)
                }
            };
            let Some(value) = value else {
                restricted.push(section.title);
                if self
                    .confirm
                    .as_ref()
                    .is_some_and(|item| std::ptr::eq(item.section, *section))
                {
                    self.confirm = None;
                }
                continue;
            };
            items.extend((undo.rows)(&value["result"]).into_iter().map(|row| Item {
                section,
                undo,
                row,
            }));
        }
        self.items = items;
        let page = self.list.paginate(&self.items);
        let counts = page.counts;
        self.shown = page.entries.len();
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
            .map(|item| (item.row.audit_kind, item.row.audit_id, item.row.revision))
            .collect();
        let changes = bilibi_audit::latest_changes(&**client, scope, &refs).await?;
        let can_see_actor = bilibi_authz::effective_capabilities(&**client, scope)
            .await?
            .contains("admin.audit.log.list");
        let principal = |change: &bilibi_audit::LatestChange| {
            let kind = match change.actor_type.as_str() {
                "user" => PrincipalKind::User,
                "agent" => PrincipalKind::Agent,
                _ => return None,
            };
            PrincipalRef::new(kind, change.actor_id).ok()
        };
        let names = if can_see_actor {
            let candidates: Vec<_> = changes.iter().filter_map(principal).collect();
            Some(
                self.pages
                    .directory
                    .rank(&**client, scope, &candidates, &Default::default())
                    .await
                    .map_err(|_| Error::Internal)?,
            )
        } else {
            None
        };
        let mut rows = Vec::new();
        for item in page.entries {
            let change = changes.iter().find(|change| {
                change.kind == item.row.audit_kind
                    && change.id == item.row.audit_id
                    && change.revision == item.row.revision
            });
            let actor = match (names.as_ref(), change) {
                (Some(names), Some(change)) => {
                    let mut label =
                        principal(change)
                            .map(|p| names.label(p))
                            .unwrap_or_else(|| {
                                change
                                    .system_principal
                                    .clone()
                                    .unwrap_or_else(|| change.actor_type.clone())
                            });
                    if let Some(id) = change.impersonator_id {
                        label.push_str(&format!(" · impersonated by User #{id}"));
                    }
                    label
                }
                (None, _) => "Restricted".into(),
                (Some(_), None) => "Unavailable".into(),
            };
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
            let change = if let Some((title, details)) = item.row.description.split_once('\n') {
                let id = format!("customisation-{}", item.key());
                let body = render(bilibi_ui::inline::InlineLongText {
                    field: bilibi_ui::inline::InlineField::new(
                        format!("{id}-text"),
                        "change",
                        "Change",
                        details,
                    )
                    .readonly(),
                    rows: 4,
                })?;
                let mut change = Html::text(title);
                change.push(&render(bilibi_ui::Fold {
                    id: &id,
                    title: "View wording and preview",
                    body,
                    ..Default::default()
                })?);
                change
            } else {
                Html::text(&item.row.description)
            };
            let mut row = FlexRow::new(item.key())
                .slot("section", Html::text(item.section.title))
                .slot("change", change)
                .slot("actor", Html::text(&actor))
                .cell("when", when);
            row.actions = render(IconButton {
                event: "request_undo",
                name: "key",
                value: &item.action_key(),
                ..IconButton::new("unarchive", item.undo.action)
            })?;
            rows.push(row);
        }
        let mut table = FlexTable::new("customisations-table", self.columns.flex_columns(), rows);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Customisations";
        table.actions = true;
        table.framed = false;
        if counts.total_entries == 0 {
            table.empty = Some(TableEmpty::Content(Html::text(if restricted.is_empty() {
                "No customisations yet. Ask your AI assistant to make a change."
            } else {
                "No customisations to show."
            })));
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
            render(UndoDialog {
                title: format!(
                    "{}: {}?",
                    item.undo.action,
                    item.row.description.lines().next().unwrap_or_default()
                ),
                item,
                key: item.action_key(),
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
        let restricted = if restricted.is_empty() {
            Html::EMPTY
        } else {
            render(Alert {
                id: "customisations-restricted",
                kind: StatusKind::Warning,
                body: Html::text(&format!(
                    "You do not have access to these sections: {}.",
                    restricted.join(", ")
                )),
                ..Alert::default()
            })?
        };
        View {
            flash,
            restricted,
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
