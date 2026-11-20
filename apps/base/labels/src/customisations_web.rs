//! Read-only inventory of customisations contributed by their owners.
use crate::receipt_display::{preview_text, receipt_columns, receipt_value, receipt_where};
use crate::{CustomisationRow, CustomisationSection, ReceiptCorrection, ReceiptSource, Undo, CUSTOMISATIONS};
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
    params, Alert, BuiltinColumn, Button, Card, Cell, Filter, FlashMap, FlexRow, FlexTable, Header,
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

static RECEIPT_SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        param_prefix: "receipt_".into(),
        filters: [("before".into(), Filter::string(""))].into(),
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("receipt list spec")
});

#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub settings: Arc<bilibi_settings::Settings>,
    pub contracts: Arc<PageContracts>,
    pub sections: Vec<&'static CustomisationSection>,
    pub receipt_sources: Vec<&'static ReceiptSource>,
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
    source = r#"{% import "dialog.html" as dialog %}{% call dialog::confirm_dialog(id="customisations-confirm", consequence=title, detail=detail, body=banner, confirm=item.undo.action, working="Working…", confirm_event=item.undo.event, cancel_event="cancel_confirm", confirm_key=key) %}{% endcall %}"#,
    ext = "html"
)]
struct UndoDialog<'a> {
    title: String,
    item: &'a Item,
    key: String,
    detail: String,
    banner: Html,
}
#[derive(Clone)]
struct ReceiptItem {
    key: String,
    correction: ReceiptCorrection,
}
#[derive(Template)]
#[template(
    source = r#"{% import "dialog.html" as dialog %}{% call dialog::confirm_dialog(id="receipt-confirm", consequence="Undo this change?", detail=detail, body=banner, confirm="Undo", working="Working…", confirm_event=item.correction.event, cancel_event="cancel_receipt", confirm_key=item.key) %}{% endcall %}"#,
    ext = "html"
)]
struct ReceiptDialog<'a> {
    item: &'a ReceiptItem,
    detail: &'a str,
    banner: Html,
}
#[derive(Template)]
#[template(
    source = r#"{% import "components.html" as ui %}{% import "feedback.html" as feedback %}<div data-shared-ui>{% call feedback::flash_group(messages=flash) %}{% endcall %}{% call ui::page(id="customisations-page") %}{{ header }}{{ restricted }}{{ card }}{{ receipts }}{{ dialog }}{% endcall %}</div>"#,
    ext = "html"
)]
struct View<'a> {
    flash: FlashMap<'a>,
    header: Html,
    restricted: Html,
    card: Html,
    receipts: Html,
    dialog: Html,
}
fn render(t: impl Template) -> Result<Html> {
    Html::render(&t).map_err(|_| Error::Internal)
}
struct Index {
    pages: Pages,
    list: ListState,
    receipt_list: ListState,
    columns: ListColumns,
    receipt_columns: ListColumns,
    items: Vec<Item>,
    confirm: Option<(Item, String)>,
    receipt_confirm: Option<(ReceiptItem, String)>,
    flash: Option<(StatusKind, String)>,
    shown: usize,
}
impl Index {
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            pages,
            list: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            receipt_list: ListState::parse(query, RECEIPT_SPEC.clone())
                .map_err(|_| Error::Internal)?,
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
            receipt_columns: receipt_columns()?,
            items: vec![],
            confirm: None,
            receipt_confirm: None,
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
            "receipt_grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.receipt_columns.handle(&values);
            }
            "grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.columns.handle(&values);
            }
            "request_undo" => {
                let key = value["key"].as_str().ok_or(Error::Invalid)?;
                if let Some(item) = self
                    .items
                    .iter()
                    .find(|item| item.action_key() == key)
                    .cloned()
                {
                    let call = Call {
                        scope,
                        credential,
                        service: CUSTOMISATIONS,
                        event: item.undo.event,
                        contract: item.undo.operation,
                    };
                    match self.pages.contracts.preview(&call, item.row.input.clone()).await {
                        Ok(reply) if reply["valid"] == true && reply["results"][0]["valid"] == true => {
                            let preview = &reply["results"][0]["preview"];
                            let facts = preview.get("result").unwrap_or(preview);
                            self.confirm = Some((item, preview_text(facts)));
                        }
                        Err(ContractError::Backend(error @ (Error::Unauthorized | Error::StepUpRequired { .. }))) => return Err(error),
                        _ => self.flash = Some((StatusKind::Error, "The change cannot be reviewed. It may have changed or you may not have permission.".into())),
                    }
                }
            }
            "cancel_confirm" => self.confirm = None,
            "request_receipt_undo" => {
                let key = value["key"].as_str().ok_or(Error::Invalid)?;
                self.receipt_confirm = None;
                let reply = self.read_receipts(scope).await?;
                if let Some(item) = self.receipt_item(&reply, key) {
                    let call = Call {
                        scope,
                        credential,
                        service: CUSTOMISATIONS,
                        event: item.correction.event,
                        contract: item.correction.contract,
                    };
                    match self.pages.contracts.preview(&call, item.correction.input.clone()).await {
                        Ok(reply) if reply["valid"] == true && reply["results"][0]["valid"] == true => {
                            let preview = &reply["results"][0]["preview"];
                            let facts = preview.get("result").unwrap_or(preview);
                            self.receipt_confirm = Some((item, preview_text(facts)));
                        }
                        Ok(reply) => {
                            let fields = &reply["results"][0]["error"]["fields"]["key"];
                            let detail = fields.as_array().into_iter().flatten()
                                .filter_map(Value::as_str).collect::<Vec<_>>().join(" ");
                            if !detail.is_empty() {
                                self.flash = Some((StatusKind::Error, detail));
                            } else {
                                self.flash = Some((StatusKind::Error, "This change cannot be undone. It may have changed or you may not have permission.".into()));
                            }
                        }
                        Err(ContractError::Backend(error @ (Error::Unauthorized | Error::StepUpRequired { .. }))) => return Err(error),
                        _ => self.flash = Some((StatusKind::Error, "This change cannot be undone. It may have changed or you may not have permission.".into())),
                    }
                }
            }
            "cancel_receipt" => self.receipt_confirm = None,
            event if event.starts_with("customisation.receipt_") => {
                let Some((item, _)) = self.receipt_confirm.take_if(|(item, _)| {
                    event == item.correction.event
                        && value["key"].as_str() == Some(item.key.as_str())
                }) else {
                    return Ok(());
                };
                let reply = self.read_receipts(scope).await?;
                if !self
                    .receipt_item(&reply, &item.key)
                    .is_some_and(|current| current.correction == item.correction)
                {
                    self.flash = Some((
                        StatusKind::Error,
                        "This change is no longer available to undo.".into(),
                    ));
                    return Ok(());
                }
                let call = Call {
                    scope,
                    credential,
                    service: CUSTOMISATIONS,
                    event: item.correction.event,
                    contract: item.correction.contract,
                };
                match self
                    .pages
                    .contracts
                    .commit(&call, item.correction.input)
                    .await
                {
                    Ok(_) => {
                        self.flash = Some((StatusKind::Success, "The change was undone.".into()))
                    }
                    Err(ContractError::Backend(
                        error @ (Error::Unauthorized | Error::StepUpRequired { .. }),
                    )) => return Err(error),
                    _ => self.flash = Some((
                        StatusKind::Error,
                        "The change could not be undone. Review the current version and try again."
                            .into(),
                    )),
                }
            }
            event if event.starts_with("customisation.") => {
                let Some((item, _)) = self.confirm.take_if(|(item, _)| {
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
                let message = match self.pages.contracts.commit(&call, item.row.input).await {
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
    async fn read_receipts(&self, scope: &Scope) -> Result<Value> {
        let mut input = serde_json::json!({"limit": self.receipt_list.page_size});
        if let Some(before) = self
            .receipt_list
            .filters
            .get("before")
            .and_then(|before| before.parse::<i64>().ok())
            .filter(|before| *before > 0)
        {
            input["before"] = serde_json::json!(before);
        }
        match self
            .pages
            .contracts
            .read(scope, "base.labels.receipts", &input)
            .await
        {
            Ok(reply) => Ok(reply["result"].clone()),
            Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                Ok(Value::Null)
            }
            Err(ContractError::Backend(error)) => Err(error),
            Err(_) => Err(Error::Internal),
        }
    }

    fn receipt_correction(&self, change: &Value) -> Option<ReceiptCorrection> {
        if change["can_undo"] != true {
            return None;
        }
        let source = self
            .pages
            .receipt_sources
            .iter()
            .find(|source| Some(source.kind) == change["kind"].as_str())?;
        (source.undo.as_ref()?.correction)(change, &change["effective"])
    }

    fn receipt_item(&self, reply: &Value, key: &str) -> Option<ReceiptItem> {
        let id = key.parse::<i64>().ok()?;
        let change = reply["receipts"]
            .as_array()?
            .iter()
            .flat_map(|group| group["changes"].as_array().into_iter().flatten())
            .find(|change| change["mutation_id"].as_i64() == Some(id))?;
        Some(ReceiptItem {
            key: key.into(),
            correction: self.receipt_correction(change)?,
        })
    }

    fn receipts(
        &mut self,
        reply: &Value,
        display: &bilibi_datetime::DisplayContext,
    ) -> Result<Html> {
        if let Some((item, _)) = &self.receipt_confirm {
            if !self
                .receipt_item(reply, &item.key)
                .is_some_and(|current| current.correction == item.correction)
            {
                self.receipt_confirm = None;
            }
        }
        if reply.is_null() {
            return render(Alert {
                id: "receipt-restricted",
                kind: StatusKind::Info,
                body: Html::text("You do not have access to customisation history."),
                ..Alert::default()
            });
        }
        let mut rows = Vec::new();
        for group in reply["receipts"].as_array().into_iter().flatten() {
            for change in group["changes"].as_array().into_iter().flatten() {
                let key = change["mutation_id"].to_string();
                let show = |key: &str| receipt_value(&change[key]);
                let when = change["when"]
                    .as_str()
                    .and_then(|when| when.parse().ok())
                    .map(|when| {
                        display.render(
                            when,
                            bilibi_datetime::Format::DateTime,
                            bilibi_datetime::Precision::Minute,
                        )
                    });
                let mut row = FlexRow::new(&key)
                    .slot(
                        "trace",
                        Html::text(group["trace"].as_str().unwrap_or_default()),
                    )
                    .slot(
                        "who",
                        Html::text(change["who"].as_str().unwrap_or("Unavailable")),
                    )
                    .slot("where", Html::text(&receipt_where(change)))
                    .slot("before", Html::text(&show("before")))
                    .slot("after", Html::text(&show("after")))
                    .slot("effective", Html::text(&show("effective")))
                    .slot(
                        "state",
                        Html::text(match change["changed_since"].as_bool() {
                            Some(true) => "Changed since",
                            Some(false) => "Current",
                            None => "—",
                        }),
                    );
                if let Some(when) = when {
                    row = row.cell(
                        "when",
                        Cell::instant(Instant::new(
                            &when.iso,
                            &when.text,
                            &when.company,
                            &when.utc,
                            when.mode.as_str(),
                        )),
                    );
                }
                if change["can_undo"] == true {
                    row.actions = render(IconButton {
                        event: "request_receipt_undo",
                        name: "key",
                        value: &key,
                        ..IconButton::new("unarchive", "Undo change")
                    })?;
                }
                rows.push(row);
            }
        }
        let mut table = FlexTable::new("receipt-table", self.receipt_columns.flex_columns(), rows);
        table.caption = "Customisation history by trace";
        table.actions = true;
        table.event = "receipt_grid";
        table.zoom = self.receipt_columns.zoom;
        table.suggestions = self.receipt_columns.flex_suggestions();
        table.add_query = &self.receipt_columns.add_query;
        table.framed = false;
        table.empty = Some(TableEmpty::Content(Html::text(
            "No changes to show on this page.",
        )));
        let mut body = render(table)?;
        let mut query = self.list.to_params();
        query.extend(self.receipt_list.to_params());
        if query.remove("receipt_before").is_some() {
            body.push(&render(Button {
                href: &params::address("/admin/customisations", &query),
                body: Html::text("Latest changes"),
                ..Button::default()
            })?);
        }
        if let Some(before) = reply["next_before"].as_i64() {
            query.insert("receipt_before".into(), before.to_string());
            body.push(&render(Button {
                href: &params::address("/admin/customisations", &query),
                body: Html::text("Older changes"),
                ..Button::default()
            })?);
        }
        render(Card {
            id: "receipt-card",
            title: "Change history",
            inner_class: "p-0",
            body,
            ..Card::default()
        })
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
                    .is_some_and(|(item, _)| std::ptr::eq(item.section, *section))
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
        let receipt_reply = self.read_receipts(scope).await?;
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
        let receipts = self.receipts(&receipt_reply, &display)?;
        let page = self.list.paginate(&self.items);
        let counts = page.counts;
        self.shown = page.entries.len();
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
            let mut row = FlexRow::new(item.key())
                .slot("section", Html::text(item.section.title))
                .slot("change", Html::text(&item.row.description))
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
        let banner = if self.receipt_confirm.is_some() || self.confirm.is_some() {
            bilibi_system::Instance::resolve(&self.pages.settings, &**client)
                .await?
                .banner("customisation-review-instance", "mb-3")
                .map_err(|_| Error::Internal)?
        } else {
            Html::EMPTY
        };
        let dialog = if let Some((item, preview)) = &self.receipt_confirm {
            render(ReceiptDialog {
                item,
                detail: preview,
                banner,
            })?
        } else if let Some((item, preview)) = &self.confirm {
            render(UndoDialog {
                title: format!("{}: {}?", item.undo.action, item.row.description),
                item,
                key: item.action_key(),
                detail: format!("{} {}", item.section.detail, preview),
                banner,
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
            receipts,
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
                        ListState::parse(values, SPEC.clone()).map_err(|_| Error::Internal)?;
                    self.receipt_list = ListState::parse(values, RECEIPT_SPEC.clone())
                        .map_err(|_| Error::Internal)?
                }
                LiveInput::Event(value) => {
                    self.event(request.scope, request.credential, value).await?
                }
            };
            let html = self.render(request.scope).await?;
            let mut query = self.list.to_params();
            query.extend(self.receipt_list.to_params());
            Ok(LiveBody {
                html,
                query,
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
