//! The Exchange rates tab of the Currencies page: the dated rates, edited in
//! place, and the currency conversion calculator. Every currency control
//! offers every current currency, the used ones first.
use super::{
    call, clear_flash, commit_status, flash_map, outcome, render, MutedText, Pages, TabBody,
    PAGE_SIZES,
};
use crate::{
    facade::{Currency, Rate, RateQuery, Request, NOTE_MAX},
    MAX_AGE_KEY, RATES_MANAGE,
};
use askama::Template;
use bilibi_agent_api::Error as ContractError;
use bilibi_authz::effective_capabilities;
use bilibi_database::{Error, Result};
use bilibi_settings::Target;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    params, BuiltinColumn, Button, ButtonVariant, Card, Choice, ColumnsOutcome, Combobox,
    CommitStates, ConfirmDialog, CreateButton, Field, FlexRow, FlexTable, Form, Html, IconButton,
    IconKind, InlineEdit, InlineField, InputKind, ListColumns, ListSpec, ListState, Modal,
    PageCounts, Pagination, SectionHeading, StatusKind, TableEmpty,
};
use jiff::civil::Date;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    sync::{Arc, LazyLock},
};

const RATES_DENIED: &str = "You do not have permission to change exchange rates.";
static RATES_SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("exchange rate list spec")
});

/// Fields side by side, through the shared `field_row`.
#[derive(Template)]
#[template(
    source = r#"{%- import "forms.html" as form -%}{% call form::field_row(fields=fields) %}{% endcall %}"#,
    ext = "html"
)]
struct FieldRow {
    fields: Vec<Html>,
}
/// The calculator's first line: the date to convert on, then what the
/// conversion named, on one compact row.
#[derive(Template)]
#[template(
    source = r#"{%- import "forms.html" as form -%}<div class="ui-compact-controls">{% call form::control_group(controls=controls) %}{% endcall %}</div>"#,
    ext = "html"
)]
struct CheckLine {
    controls: Vec<Html>,
}

fn rate_columns() -> Vec<BuiltinColumn> {
    vec![
        BuiltinColumn::new("pair", "Pair"),
        BuiltinColumn::new("effective", "Applies from"),
        BuiltinColumn::new("rate", "Rate"),
        BuiltinColumn::new("note", "Source"),
    ]
}

/// The new-rate dialog: what the person typed and what the contract refused.
#[derive(Default)]
struct Draft {
    values: HashMap<String, String>,
    errors: BTreeMap<String, Vec<String>>,
}
/// The currency conversion calculator: two amounts, each with its currency,
/// the date to convert on, and the line that names the rate used or why
/// there is none. Empty codes are filled with defaults on the first render.
#[derive(Default)]
struct Check {
    top_amount: String,
    top: String,
    bottom_amount: String,
    bottom: String,
    date: String,
    /// The amount typed last; every render answers from it, so a rate
    /// added or changed meanwhile shows at once.
    side: Side,
    line: Option<std::result::Result<String, String>>,
    /// The pair and day no recent rate was found for, offered as a new rate.
    missing: Option<String>,
}
/// Which amount the person typed, so the other one is the answer.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Side {
    #[default]
    Top,
    Bottom,
}

pub(super) struct Rates {
    pages: Pages,
    state: ListState,
    columns: ListColumns,
    rows_changed: bool,
    shown: usize,
    /// Every rate of the page as last read.
    rates: Vec<Rate>,
    statuses: CommitStates,
    notice: String,
    draft: Option<Draft>,
    confirm: Option<Rate>,
    check: Check,
    flash: Option<(StatusKind, String)>,
}

impl Rates {
    /// The list's page and page size, for the address.
    pub(super) fn query(&self) -> HashMap<String, String> {
        let mut query = self.state.to_params();
        query.remove("search");
        query
    }
    pub(super) fn flash(&self) -> &Option<(StatusKind, String)> {
        &self.flash
    }
    pub(super) fn notice(&self) -> String {
        self.notice.clone()
    }
    pub(super) fn shown(&self) -> usize {
        self.shown
    }
    pub(super) fn refresh(&mut self) {
        self.rows_changed = true;
    }
    pub(super) fn params(&mut self, values: &HashMap<String, String>) -> Result<()> {
        self.rows_changed = true;
        self.state = ListState::parse(values, RATES_SPEC.clone()).map_err(|_| Error::Internal)?;
        Ok(())
    }

    pub(super) fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            pages,
            state: ListState::parse(query, RATES_SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: ListColumns::mount("currency-rates", rate_columns())
                .map_err(|_| Error::Internal)?,
            rows_changed: true,
            shown: 0,
            rates: Vec::new(),
            statuses: CommitStates::default(),
            notice: String::new(),
            draft: None,
            confirm: None,
            check: Check::default(),
            flash: None,
        })
    }
    fn rate(&self, id: i64) -> Option<&Rate> {
        self.rates.iter().find(|rate| rate.id == id)
    }

    pub(super) async fn event(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &Value,
    ) -> Result<()> {
        let get = |key: &str| value.get(key).and_then(Value::as_str);
        self.notice.clear();
        self.rows_changed = true;
        match get("type").ok_or(Error::Invalid)? {
            "page" => self.state.put_page(get("page")),
            "grid" => match self.columns.handle(
                &params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?,
            ) {
                ColumnsOutcome::Sort(_) => return Err(Error::Invalid),
                ColumnsOutcome::Update | ColumnsOutcome::Noop => self.rows_changed = false,
            },
            "clear-flash" => clear_flash(&mut self.flash, get("key")),
            "new" => {
                let today = {
                    let client = self.pages.db.get().await?;
                    self.pages.today(&**client, scope).await?
                };
                let mut draft = Draft::default();
                if let Some(today) = today {
                    draft
                        .values
                        .insert("effective_on".into(), today.to_string());
                }
                // The calculator's "Add this rate" names the missing pair
                // and the day it was asked for.
                if let Some(pair) = get("pair") {
                    let mut parts = pair.splitn(3, '/');
                    for key in ["from", "to", "effective_on"] {
                        if let Some(part) = parts.next().filter(|part| !part.is_empty()) {
                            draft.values.insert(key.into(), part.to_owned());
                        }
                    }
                }
                self.draft = Some(draft);
            }
            "close_new" => self.draft = None,
            "create_rate" => {
                let posted = params::form_values_checked(value, "rate", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let field =
                    |key: &str| posted.get(key).map_or("", String::as_str).trim().to_owned();
                let pair = format!("{}/{}", field("from"), field("to"));
                let mut input = json!({
                    "from": field("from"),
                    "to": field("to"),
                    "effective_on": field("effective_on"),
                    "rate": field("rate"),
                });
                if !field("source_note").is_empty() {
                    input["source_note"] = json!(field("source_note"));
                }
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &call(scope, credential, "create_rate", "core.currency.rates.set"),
                        input,
                    )
                    .await;
                let draft = Draft {
                    values: posted,
                    errors: BTreeMap::new(),
                };
                match result {
                    Ok(_) => {
                        self.draft = None;
                        self.flash = Some((
                            StatusKind::Success,
                            format!("The rate for {pair} was added."),
                        ));
                    }
                    Err(ContractError::InvalidInput(fields)) => {
                        self.draft = Some(Draft {
                            errors: fields,
                            ..draft
                        });
                    }
                    other => {
                        self.draft = Some(draft);
                        if let Err(message) = outcome(other, RATES_DENIED)? {
                            self.flash = Some((StatusKind::Error, message));
                        }
                    }
                }
            }
            "save_rate" => {
                let field = get("field").unwrap_or_default();
                if !["rate", "source_note"].contains(&field) {
                    return Err(Error::Invalid);
                }
                let id: i64 = get("id")
                    .unwrap_or_default()
                    .parse()
                    .map_err(|_| Error::Invalid)?;
                let submitted = get("value").unwrap_or_default();
                let version: i64 = get("version")
                    .unwrap_or_default()
                    .parse()
                    .map_err(|_| Error::Invalid)?;
                let Some(current) = self.rate(id).cloned() else {
                    self.notice = "This rate no longer exists; reload the page.".into();
                    return Ok(());
                };
                let mut input = json!({
                    "from": current.from,
                    "to": current.to,
                    "effective_on": current.effective_on.to_string(),
                    "rate": current.rate.to_string(),
                    "version": version,
                });
                if let Some(note) = &current.source_note {
                    input["source_note"] = json!(note);
                }
                input[field] = json!(submitted.trim());
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &call(scope, credential, "save_rate", "core.currency.rates.set"),
                        input,
                    )
                    .await;
                let label = if field == "rate" { "rate" } else { "source" };
                let status = commit_status(
                    result,
                    label,
                    submitted,
                    RATES_DENIED,
                    &mut self.notice,
                    &mut self.statuses,
                )?;
                self.statuses.put(format!("{id}-{field}"), status);
            }
            // The row's delete icon: the confirm names the rate it removes.
            "confirm_delete" => {
                let id: i64 = get("id")
                    .unwrap_or_default()
                    .parse()
                    .map_err(|_| Error::Invalid)?;
                match self.rate(id).cloned() {
                    Some(rate) => {
                        self.flash = None;
                        self.confirm = Some(rate);
                    }
                    None => self.notice = "This rate no longer exists; reload the page.".into(),
                }
            }
            "cancel_confirm" => self.confirm = None,
            "delete_rate" => {
                let Some(rate) = self.confirm.take() else {
                    return Ok(());
                };
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &call(
                            scope,
                            credential,
                            "delete_rate",
                            "core.currency.rates.delete",
                        ),
                        json!({"id": rate.id, "version": rate.version}),
                    )
                    .await;
                self.flash = Some(match outcome(result, RATES_DENIED)? {
                    Ok(()) => (
                        StatusKind::Success,
                        format!(
                            "The rate for {}/{} from {} was deleted.",
                            rate.from, rate.to, rate.effective_on
                        ),
                    ),
                    Err(message) => (StatusKind::Error, message),
                });
            }
            "convert" => {
                let posted = params::form_values_checked(value, "convert", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let field =
                    |key: &str| posted.get(key).map_or("", String::as_str).trim().to_owned();
                // Only the bottom amount changed: the person typed there, so
                // the top one is the answer. Anything else converts downward.
                let side = if field("bottom_amount") != self.check.bottom_amount
                    && field("top_amount") == self.check.top_amount
                {
                    Side::Bottom
                } else {
                    Side::Top
                };
                self.check.top_amount = field("top_amount");
                self.check.top = field("top");
                self.check.bottom_amount = field("bottom_amount");
                self.check.bottom = field("bottom");
                self.check.date = field("date");
                self.check.side = side;
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    /// Asks the shared conversion, as any module would, from the amount the
    /// person typed into the other row's currency; the line names the rate's
    /// date and where it came from, or why there is none.
    async fn calculate(&mut self, scope: &Scope, side: Side) -> Result<()> {
        let check = &mut self.check;
        let (amount, from, to) = match side {
            Side::Top => (&check.top_amount, &check.top, &check.bottom),
            Side::Bottom => (&check.bottom_amount, &check.bottom, &check.top),
        };
        let (amount, from, to) = (amount.clone(), from.clone(), to.clone());
        let answer = match side {
            Side::Top => &mut check.bottom_amount,
            Side::Bottom => &mut check.top_amount,
        };
        check.missing = None;
        if amount.is_empty() || from.is_empty() || to.is_empty() {
            answer.clear();
            check.line = None;
            return Ok(());
        }
        let Ok(amount) = Decimal::from_str(&amount) else {
            answer.clear();
            check.line = Some(Err("Enter the amount as a number, such as 1250.50.".into()));
            return Ok(());
        };
        let Ok(on) = check.date.parse::<Date>() else {
            answer.clear();
            check.line = Some(Err("Choose the date to convert on.".into()));
            return Ok(());
        };
        let client = self.pages.db.get().await?;
        let request = Request::new(amount, &from, &to, on);
        match self
            .pages
            .currencies
            .convert(&**client, scope, &request)
            .await?
        {
            Ok(conversion) => {
                *answer = conversion.amount.to_string();
                let quote = &conversion.quote;
                let note = match (quote.path, quote.rate_ids.first()) {
                    (crate::Path::Direct | crate::Path::Inverse, Some(id)) => self
                        .pages
                        .currencies
                        .rate_note(&**client, scope, *id)
                        .await?
                        .unwrap_or_else(|| "entered manually".into()),
                    _ => String::new(),
                };
                let source = match quote.path {
                    crate::Path::Same => "same currency, no rate needed".to_owned(),
                    crate::Path::Direct | crate::Path::Inverse => note,
                    crate::Path::ViaBase => "two rates through the base currency".to_owned(),
                };
                self.check.line = Some(Ok(format!(
                    "Rate of {} · {source}",
                    quote.rate_date.strftime("%-d %b %Y")
                )));
            }
            Err(crate::Refusal::NoRate { from, to, on }) => {
                answer.clear();
                self.check.line = Some(Err(format!(
                    "No recent rate from {from} to {to} on {}.",
                    on.strftime("%-d %b %Y")
                )));
                self.check.missing = Some(format!("{from}/{to}/{on}"));
            }
            Err(refusal) => {
                answer.clear();
                self.check.line = Some(Err(refusal.message()));
            }
        }
        Ok(())
    }

    /// The calculator's starting point: 1 of the base currency (or the first
    /// used one) on top, below it the other side of the latest rate that
    /// involves it (or the next used currency), on today. A tenant that has
    /// used none starts empty and picks both.
    fn start_check(&mut self, current: &[Currency], base: Option<&Currency>, today: Option<Date>) {
        let used: Vec<&Currency> = current.iter().filter(|currency| currency.used).collect();
        let top = base
            .filter(|base| current.iter().any(|currency| currency.code == base.code))
            .or_else(|| used.first().copied())
            .map(|currency| currency.code.clone())
            .unwrap_or_default();
        let latest = self.rates.iter().find_map(|rate| {
            if rate.from == top {
                Some(rate.to.clone())
            } else if rate.to == top {
                Some(rate.from.clone())
            } else {
                None
            }
        });
        let bottom = latest
            .or_else(|| {
                used.iter()
                    .find(|currency| currency.code != top)
                    .map(|currency| currency.code.clone())
            })
            .unwrap_or_default();
        self.check = Check {
            top_amount: "1".into(),
            top,
            bottom,
            date: today.map(|today| today.to_string()).unwrap_or_default(),
            ..Check::default()
        };
    }

    fn inline(
        &self,
        rate: &Rate,
        field: &str,
        label: &str,
        text: &str,
        edits: bool,
    ) -> InlineField {
        let mut inline = InlineField::new(
            format!("currency-rate-{}-{field}", rate.id),
            field,
            label,
            text,
        )
        .version(rate.version)
        .status(self.statuses.get(&format!("{}-{field}", rate.id)));
        inline.id_value = rate.id.to_string();
        inline.save_event = "save_rate".into();
        if field != "rate" {
            inline = inline.allow_empty();
        }
        if !edits {
            inline = inline.readonly();
        }
        inline
    }

    pub(super) async fn render(&mut self, scope: &Scope) -> Result<TabBody> {
        let pages = self.pages.clone();
        let client = pages.db.get().await?;
        let capabilities = effective_capabilities(&**client, scope).await?;
        let edits = capabilities.contains(RATES_MANAGE);
        let (limit, offset) = (self.state.page_size as i64, self.state.offset() as i64);
        let (rates, total) = pages
            .currencies
            .rates(
                &**client,
                scope,
                &RateQuery {
                    code: None,
                    limit,
                    offset,
                },
            )
            .await?;
        let current = pages.currencies.current(&**client, scope).await?;
        let today = pages.today(&**client, scope).await?;
        let max_age = pages
            .settings
            .get(&**client, scope, Target::Tenant, MAX_AGE_KEY)
            .await?
            .value
            .as_i64()
            .unwrap_or(7);
        drop(client);
        self.rates = rates;
        self.shown = self.rates.len();
        if self.rows_changed {
            self.columns.load(&self.rates, |rate| rate.id.to_string());
        }
        let counts = PageCounts::new(self.state.page, self.state.page_size, total as u64);
        let actions = if edits {
            render(CreateButton {
                noun: "Rate",
                id: "currency-rate-new",
                event: "new",
                button_type: "button",
                ..CreateButton::default()
            })?
        } else {
            Html::EMPTY
        };
        // The tab's guidance: the two rules a person entering rates needs.
        let guidance = match max_age {
            0 => "Each rate applies from its date until a later rate for the same pair.".to_owned(),
            1 => "Each rate applies from its date until a later rate for the same pair. Conversions skip a rate older than 1 day.".to_owned(),
            days => format!("Each rate applies from its date until a later rate for the same pair. Conversions skip a rate older than {days} days."),
        };
        let mut rows = Vec::with_capacity(self.rates.len());
        for rate in &self.rates {
            let mut row = FlexRow::new(rate.id.to_string())
                .slot(
                    "pair",
                    Html::text(&format!("1 {} → {}", rate.from, rate.to)),
                )
                .slot(
                    "effective",
                    Html::text(
                        &bilibi_datetime::DisplayContext::anonymous()
                            .render_date(rate.effective_on),
                    ),
                )
                .slot(
                    "rate",
                    render(InlineEdit {
                        field: self.inline(rate, "rate", "Rate", &rate.rate.to_string(), edits),
                    })?,
                )
                .slot(
                    "note",
                    render(InlineEdit {
                        field: self.inline(
                            rate,
                            "source_note",
                            "Source",
                            rate.source_note.as_deref().unwrap_or(""),
                            edits,
                        ),
                    })?,
                );
            // Delete acts on the rate itself, so it is the row's own icon,
            // not an item of a "Non-local actions" menu (DESIGN.md, Row actions).
            if edits {
                row.actions = render(IconButton {
                    id: &format!("currency-rate-{}-delete", rate.id),
                    kind: IconKind::Danger,
                    title: "Delete rate",
                    event: "confirm_delete",
                    name: "id",
                    value: &rate.id.to_string(),
                    ..IconButton::new(
                        "delete",
                        &format!(
                            "Delete the {} to {} rate from {}",
                            rate.from, rate.to, rate.effective_on
                        ),
                    )
                })?;
            }
            row.dom_id = Some(format!("currency-rate-{}", rate.id));
            rows.push(row);
        }
        let mut table = FlexTable::new("currency-rates", self.columns.flex_columns(), rows);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Exchange rates";
        table.framed = false;
        table.actions = edits;
        if total == 0 {
            let mut empty = Html::text("No exchange rates yet.");
            if edits {
                empty.push(&Html::text(" "));
                empty.push(&render(Button {
                    id: "currency-rate-empty-add",
                    event: "new",
                    button_type: "button",
                    body: Html::text("Add a rate"),
                    ..Button::default()
                })?);
            }
            table.empty = Some(TableEmpty::Content(empty));
        }
        let mut body = render(table)?;
        body.push(&render(Pagination::new(
            "currency-rate-pagination",
            counts,
            &PAGE_SIZES,
        ))?);
        let card = render(Card {
            id: "currency-rate-card",
            inner_class: "p-0",
            body,
            ..Card::default()
        })?;
        let mut body = render(SectionHeading {
            title: "Rates",
            id: "currency-rates-heading",
            count: Some(total as usize),
            description: &guidance,
            actions,
            ..SectionHeading::default()
        })?;
        body.push(&card);
        if self.check.top.is_empty() && self.check.bottom.is_empty() {
            let base = {
                let client = pages.db.get().await?;
                pages.currencies.base(&**client, scope, None).await?
            };
            self.start_check(&current, base.as_ref(), today);
        }
        self.calculate(scope, self.check.side).await?;
        body.push(&self.check_card(&current, edits)?);
        let dialog = match (&self.confirm, &self.draft) {
            (Some(rate), _) => render(ConfirmDialog {
                id: "currency-rate-delete-confirm",
                consequence: &format!(
                    "Delete the {} to {} rate from {}?",
                    rate.from, rate.to, rate.effective_on
                ),
                detail: "Conversions already stored keep the rate they used. Later conversions use the previous rate, if it is recent enough.",
                confirm: "Delete",
                working: "Deleting…",
                confirm_event: "delete_rate",
                cancel_event: "cancel_confirm",
            })?,
            (None, Some(draft)) => self.new_dialog(&current, draft)?,
            (None, None) => Html::EMPTY,
        };
        Ok(TabBody { body, dialog })
    }

    /// The currency conversion calculator: the line that names the rate's
    /// date and source with the date to convert on, then two rows of an
    /// amount and its currency. Typing in either row answers in the other.
    fn check_card(&self, current: &[Currency], edits: bool) -> Result<Html> {
        let check = &self.check;
        let options: Vec<Choice> = current.iter().map(|currency| currency.choice()).collect();
        let row = |amount: &str, code: &str, side: &str, label: &str| -> Result<Html> {
            render(FieldRow {
                fields: vec![
                    render(
                        Field::in_form("convert", &format!("{side}_amount"))
                            .id(format!("currency-check-{side}-amount"))
                            .label(format!("{label} amount"))
                            .value(amount)
                            .attr("inputmode", "decimal")
                            .attr("autocomplete", "off")
                            .attr("data-debounce", "400")
                            // Stacked on a phone, the amount keeps a gap above its currency.
                            .wrapper_class("mb-3 sm:mb-0"),
                    )?,
                    render(
                        Combobox::new(format!("convert_{side}"), format!("convert[{side}]"))
                            .label(format!("{label} currency"))
                            .options(options.clone())
                            .value(code)
                            .placeholder("Choose a currency")
                            .wrapper_class("mb-0"),
                    )?,
                ],
            })
        };
        let mut line = match &check.line {
            Some(Ok(text)) => render(MutedText { text })?,
            Some(Err(text)) => render(MutedText { text })?,
            None => Html::EMPTY,
        };
        if let Some(pair) = check.missing.as_deref().filter(|_| edits) {
            line.push(&render(Button {
                id: "currency-check-add-rate",
                event: "new",
                name: "pair",
                value: pair,
                button_type: "button",
                body: Html::text("Add this rate"),
                ..Button::default()
            })?);
        }
        let mut body = render(CheckLine {
            controls: vec![
                render(
                    Field::in_form("convert", "date")
                        .id("currency-check-date")
                        .label("On")
                        .kind(InputKind::Date)
                        .value(&check.date)
                        .wrapper_class("mb-0"),
                )?,
                line,
            ],
        })?;
        body.push(&row(&check.top_amount, &check.top, "top", "First")?);
        body.push(&row(
            &check.bottom_amount,
            &check.bottom,
            "bottom",
            "Second",
        )?);
        let mut card = render(SectionHeading {
            title: "Currency conversion calculator",
            ..SectionHeading::default()
        })?;
        card.push(&render(Form {
            id: "currency-check-form",
            event: "convert",
            change: true,
            body,
            ..Form::default()
        })?);
        render(Card {
            id: "currency-check-card",
            body: card,
            ..Card::default()
        })
    }

    fn new_dialog(&self, current: &[Currency], draft: &Draft) -> Result<Html> {
        let value = |key: &str| draft.values.get(key).cloned().unwrap_or_default();
        let errors = |key: &str| draft.errors.get(key).cloned().unwrap_or_default();
        let options: Vec<Choice> = current.iter().map(|currency| currency.choice()).collect();
        let combo = |key: &str, label: &str, hint: &str| {
            Combobox::new(format!("rate_{key}"), format!("rate[{key}]"))
                .label(label)
                .options(options.clone())
                .value(value(key))
                .placeholder("Choose a currency")
                .hint(hint)
                .errors(errors(key))
        };
        let mut body = render(combo("from", "From currency", "The currency you hold."))?;
        body.push(&render(combo(
            "to",
            "To currency",
            "The currency one unit of the first buys.",
        ))?);
        body.push(&render(
            Field::in_form("rate", "effective_on")
                .id("currency-rate-new-date")
                .label("Applies from")
                .kind(InputKind::Date)
                .value(value("effective_on"))
                .hint("It stays in use until a later rate for the same pair.")
                .errors(errors("effective_on"))
                .required(),
        )?);
        body.push(&render(
            Field::in_form("rate", "rate")
                .id("currency-rate-new-rate")
                .label("Rate")
                .value(value("rate"))
                .attr("inputmode", "decimal")
                .hint("How many of the second currency one of the first buys, such as 4.7250.")
                .errors(errors("rate"))
                .required(),
        )?);
        body.push(&render(
            Field::in_form("rate", "source_note")
                .id("currency-rate-new-note")
                .label("Source")
                .value(value("source_note"))
                .attr("maxlength", &NOTE_MAX.to_string())
                .hint("Optional. Where the rate came from, such as a bank.")
                .errors(errors("source_note")),
        )?);
        let mut actions = render(Button {
            event: "close_new",
            button_type: "button",
            body: Html::text("Cancel"),
            ..Button::default()
        })?;
        actions.push(&render(Button {
            variant: ButtonVariant::Primary,
            button_type: "submit",
            working: "Adding…",
            body: Html::text("Add rate"),
            ..Button::default()
        })?);
        let form = render(Form {
            id: "currency-rate-new-form",
            event: "create_rate",
            body,
            actions,
            ..Form::default()
        })?;
        render(Modal {
            id: "currency-rate-new-dialog",
            title: "New exchange rate",
            description: "Conversions on or after this date use it.",
            cancel_event: "close_new",
            flash: Some(flash_map(&self.flash)),
            body: form,
            ..Modal::default()
        })
    }
}
