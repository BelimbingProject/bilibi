//! The operator pages (`core/currency.pages`): `/currencies` lists the ISO
//! currencies with the tenant's switch, base currency and per-company base
//! currencies; `/currencies/rates` lists the dated exchange rates, edited in
//! place, with a currency conversion calculator. The lists and the
//! in-row editors follow `base/picklists`.
//!
//! Every change goes through the `core.currency.*` contracts agents call
//! (`PageContracts::submit`), which recheck the capability in their own
//! transaction. A control the person may not use is hidden; a refused event is
//! a flash on the page, a refused inline edit an outcome on its field.
mod rates;

use crate::{
    facade::{normalize, Currencies, Currency},
    MANAGE, PAGES, RATES_PATH,
};
use askama::Template;
use bilibi_address::AddressService;
use bilibi_agent_api::{
    page::{Call, PageContracts},
    Error as ContractError,
};
use bilibi_authz::effective_capabilities;
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayPolicy, Mode};
use bilibi_locale::Locales;
use bilibi_party_ids::{OrganisationId, PartyId};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    list_state::FILTERS_FORM,
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, Badge, BadgeKind, BuiltinColumn, Button, Card, Choice, ColumnsOutcome, Combobox,
    CommitStates, CommitStatus, Fact, Facts, FilterToolbar, FlashMap, FlexRow, FlexTable, Fold,
    Header, Html, InlineChoice, InlineField, ListColumns, ListSpec, ListState, Pagination,
    SectionHeading, StatusKind, TableEmpty, Toggle, ToolbarControl, NO_FLASH,
};
use jiff::civil::Date;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
};

const PAGE_SIZES: [u64; 3] = [25, 50, 100];
const DENIED: &str = "You do not have permission to change currencies.";
/// How many companies the per-company base currencies list.
const COMPANIES_SHOWN: i64 = 100;

static CURRENCY_SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        filters: BTreeMap::from([(
            "status".into(),
            bilibi_ui::Filter::one_of(&["all", "enabled", "off"], "all"),
        )]),
        omit_blank: vec!["search".into()],
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("currency list spec")
});

/// The module's page adapter; every field is shared by mounted states.
#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub currencies: Arc<Currencies>,
    pub settings: Arc<Settings>,
    pub addresses: Arc<AddressService>,
    pub contracts: Arc<PageContracts>,
    pub display: Arc<DisplayPolicy>,
    pub locales: Arc<Locales>,
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
/// The shared dismiss, kept for every page that shows a flash.
fn clear_flash(flash: &mut Option<(StatusKind, String)>, key: Option<&str>) {
    if flash
        .as_ref()
        .is_some_and(|(kind, _)| Some(kind.name()) == key)
    {
        *flash = None;
    }
}
fn call<'a>(
    scope: &'a Scope,
    credential: &'a str,
    event: &'a str,
    contract: &'static str,
) -> Call<'a> {
    Call {
        scope,
        credential,
        service: PAGES,
        event,
        contract,
    }
}
/// A contract's answer as the page reports it: done, or the sentence to show.
fn outcome(
    result: std::result::Result<Value, ContractError>,
    denied: &str,
) -> Result<std::result::Result<(), String>> {
    Ok(match result {
        Ok(_) => Ok(()),
        // Base Step-up and a lost session: the host prompts and comes back.
        Err(ContractError::Backend(
            error @ (Error::Unauthorized | Error::StepUpRequired { .. }),
        )) => return Err(error),
        Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
            Err(denied.into())
        }
        Err(ContractError::NotFound | ContractError::Backend(Error::NotFound)) => {
            Err("This no longer exists; reload the page.".into())
        }
        Err(ContractError::InvalidInput(fields)) => Err(fields
            .iter()
            .flat_map(|(field, messages)| {
                messages
                    .iter()
                    .map(move |message| format!("{} {message}", field.replace('_', " ")))
            })
            .collect::<Vec<_>>()
            .join("; ")),
        Err(_) => Err(
            "The change was not saved. Try again, and tell your administrator if it keeps failing."
                .into(),
        ),
    })
}
/// An inline edit's outcome on its own fact. A refusal names the field itself,
/// so only its reasons are kept.
fn commit_status(
    result: std::result::Result<Value, ContractError>,
    label: &str,
    submitted: &str,
    denied: &str,
    notice: &mut String,
    statuses: &mut CommitStates,
) -> Result<CommitStatus> {
    Ok(match result {
        Err(ContractError::InvalidInput(fields)) => {
            let reasons: Vec<String> = fields.into_values().flatten().collect();
            let reasons: Vec<&str> = reasons.iter().map(String::as_str).collect();
            CommitStatus::refusal(label, submitted, &reasons)
        }
        other => match outcome(other, denied)? {
            Ok(()) => CommitStatus::Saved,
            Err(message) if message == denied => {
                *notice = denied.into();
                statuses.clear_saved();
                CommitStatus::Error(denied.into())
            }
            Err(message) => CommitStatus::refusal(label, submitted, &[&message]),
        },
    })
}

#[derive(Template)]
#[template(path = "currency.html")]
struct View<'a> {
    id: &'a str,
    flash: FlashMap<'a>,
    header: Html,
    summary: Html,
    toolbar: Html,
    card: Html,
    footer: Html,
    dialog: Html,
}

#[derive(Template)]
#[template(
    source = r#"<span class="font-mono text-xs text-ink">{{ text }}</span>"#,
    ext = "html"
)]
struct CodeCell<'a> {
    text: &'a str,
}
#[derive(Template)]
#[template(
    source = r#"<span class="text-sm text-ink-muted">{{ text }}</span>"#,
    ext = "html"
)]
struct MutedText<'a> {
    text: &'a str,
}
#[derive(Template)]
#[template(
    source = r#"<span class="inline-flex items-center gap-2">{% for badge in badges %}{{ badge }}{% endfor %}</span>"#,
    ext = "html"
)]
struct Badges {
    badges: Vec<Html>,
}

impl Pages {
    /// The calendar day in the reader's display zone, the default for a date a
    /// person types. `None` in the browser-local mode, where only the browser
    /// knows the day.
    async fn today(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Option<Date>> {
        let locale = self.locales.resolve(client, scope, None).await?.locale;
        let display = self.display.display(client, scope, None, locale).await?;
        Ok(
            (display.mode != Mode::Local)
                .then(|| display.clock(None, jiff::Timestamp::now()).today),
        )
    }
}

// ------------------------------------------------------------ the currencies

fn currency_columns() -> Vec<BuiltinColumn> {
    vec![
        BuiltinColumn::new("code", "Code"),
        BuiltinColumn::new("name", "Currency"),
        BuiltinColumn::new("numeric", "Number"),
        BuiltinColumn::new("places", "Decimal places"),
        BuiltinColumn::new("status", "Status"),
    ]
}

struct CurrencyPage {
    pages: Pages,
    state: ListState,
    columns: ListColumns,
    rows_changed: bool,
    shown: usize,
    statuses: CommitStates,
    notice: String,
    flash: Option<(StatusKind, String)>,
}
impl CurrencyPage {
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            pages,
            state: ListState::parse(query, CURRENCY_SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: ListColumns::mount("currencies", currency_columns())
                .map_err(|_| Error::Internal)?,
            rows_changed: true,
            shown: 0,
            statuses: CommitStates::default(),
            notice: String::new(),
            flash: None,
        })
    }

    async fn event(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let get = |key: &str| value.get(key).and_then(Value::as_str);
        self.notice.clear();
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
            "clear-flash" => clear_flash(&mut self.flash, get("key")),
            event @ ("enable" | "disable") => {
                let code = get("key").unwrap_or_default().to_owned();
                let enable = event == "enable";
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &call(scope, credential, event, "core.currency.enabled.set"),
                        json!({"code": code, "enabled": enable}),
                    )
                    .await;
                self.flash = Some(match result {
                    // The refusal names what holds the currency; it reads
                    // after the code rather than after the field name.
                    Err(ContractError::InvalidInput(fields)) => {
                        let reasons: Vec<String> = fields.into_values().flatten().collect();
                        (
                            StatusKind::Error,
                            format!(
                                "{code} cannot be turned {}: it {}.",
                                if enable { "on" } else { "off" },
                                reasons.join("; ")
                            ),
                        )
                    }
                    result => match outcome(result, DENIED)? {
                        Ok(()) => (
                            StatusKind::Success,
                            if enable {
                                format!("{code} is enabled. People can choose it in new records.")
                            } else {
                                format!("{code} is disabled. Records that hold it keep it.")
                            },
                        ),
                        Err(message) => (StatusKind::Error, message),
                    },
                });
            }
            "save_base" => {
                let submitted = get("value").unwrap_or_default().to_owned();
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &call(scope, credential, "save_base", "core.currency.base.set"),
                        base_input(&submitted, None),
                    )
                    .await;
                let status = commit_status(
                    result,
                    "base currency",
                    &submitted,
                    DENIED,
                    &mut self.notice,
                    &mut self.statuses,
                )?;
                self.statuses.put("base", status);
            }
            "save_company_base" => {
                let id: i64 = get("id")
                    .unwrap_or_default()
                    .parse()
                    .map_err(|_| Error::Invalid)?;
                let submitted = get("value").unwrap_or_default().to_owned();
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &call(
                            scope,
                            credential,
                            "save_company_base",
                            "core.currency.base.set",
                        ),
                        base_input(&submitted, Some(id)),
                    )
                    .await;
                let status = commit_status(
                    result,
                    "currency",
                    &submitted,
                    DENIED,
                    &mut self.notice,
                    &mut self.statuses,
                )?;
                self.statuses.put(format!("company-{id}"), status);
            }
            "use_suggestion" => {
                let code = get("key").unwrap_or_default().to_owned();
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &call(
                            scope,
                            credential,
                            "use_suggestion",
                            "core.currency.base.set",
                        ),
                        base_input(&code, None),
                    )
                    .await;
                self.flash = Some(match outcome(result, DENIED)? {
                    Ok(()) => (
                        StatusKind::Success,
                        format!("{code} is now the base currency."),
                    ),
                    Err(message) => (StatusKind::Error, message),
                });
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    /// The currency the primary company's country uses, when the tenant has
    /// no base currency yet. A hint only: it is never saved unasked.
    async fn suggestion(&self, scope: &Scope) -> Result<Option<(String, String)>> {
        let client = self.pages.db.get().await?;
        let Some(company) = bilibi_company::primary_company(&**client, scope.tenant()).await?
        else {
            return Ok(None);
        };
        drop(client);
        let Some(address) = self
            .pages
            .addresses
            .primary(scope, PartyId::Organisation(company.id))
            .await?
        else {
            return Ok(None);
        };
        let Some(iso) = address.address.address.country_iso.as_deref() else {
            return Ok(None);
        };
        let Some(country) = self.pages.addresses.geonames().get_country(iso).await? else {
            return Ok(None);
        };
        Ok(country
            .currency_code
            .filter(|code| normalize(code).is_some())
            .map(|code| (country.country, code)))
    }

    /// The base currency as a fact: read first, chosen in place.
    /// Every currency that is not withdrawn is offered: choosing one that is
    /// not enabled yet enables it in the same write.
    fn base_fact(
        &self,
        base: Option<&Currency>,
        choices: &[Currency],
        manage: bool,
    ) -> Result<Html> {
        let value = base.map_or("", |base| base.code.as_str());
        let display = match base {
            Some(base) => Html::text(&base.label()),
            None => render(MutedText {
                text: "Required for currency conversions.",
            })?,
        };
        let mut field = InlineField::new("currency-base", "base", "Edit base currency", value)
            .status(self.statuses.get("base"))
            .allow_empty();
        field.save_event = "save_base".into();
        field.editable = manage;
        let editor = render(currency_editor(
            "currency-base-input",
            "base",
            choices,
            value,
        ))?;
        render(&InlineChoice {
            field,
            display,
            editor,
        })
    }

    async fn render(&mut self, scope: &Scope) -> Result<String> {
        let pages = self.pages.clone();
        let mut client = pages.db.get().await?;
        let manage = effective_capabilities(&**client, scope)
            .await?
            .contains(MANAGE);
        let catalog = pages.currencies.catalog(&**client, scope).await?;
        let base = pages.currencies.base(&**client, scope, None).await?;
        let enabled: Vec<Currency> = catalog
            .iter()
            .filter(|currency| currency.enabled && !currency.withdrawn)
            .cloned()
            .collect();
        let base_choices: Vec<Currency> = catalog
            .iter()
            .filter(|currency| {
                !currency.withdrawn || base.as_ref().is_some_and(|base| base.code == currency.code)
            })
            .cloned()
            .collect();
        let max_age = pages
            .settings
            .get(&**client, scope, Target::Tenant, crate::MAX_AGE_KEY)
            .await?
            .value
            .as_i64()
            .unwrap_or(7);
        // Companies are read through their own owner; a person who may not
        // list them simply gets no per-company section.
        let companies = match bilibi_company::list(
            &mut client,
            scope,
            &bilibi_company::Params {
                size: COMPANIES_SHOWN,
                ..bilibi_company::Params::default()
            },
        )
        .await
        {
            Ok(page) => page.rows,
            Err(Error::Forbidden) => Vec::new(),
            Err(error) => return Err(error),
        };
        let mut company_bases = HashMap::new();
        for company in &companies {
            if let Ok(id) = OrganisationId::new(company.id) {
                // An archived company is listed but refuses its settings read;
                // it holds no usable base, so it shows none.
                match pages.currencies.own_base(&**client, scope, id).await {
                    Ok(Some(own)) => {
                        company_bases.insert(company.id, own.code);
                    }
                    Ok(None) | Err(Error::NotFound | Error::Forbidden) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        drop(client);
        let suggestion = if base.is_none() && manage {
            self.suggestion(scope).await?
        } else {
            None
        };

        // The summary card: the base currency, then each company's own.
        let mut facts = vec![Fact {
            title: "Base currency",
            id: "currency-base-fact",
            value: self.base_fact(base.as_ref(), &base_choices, manage)?,
        }];
        facts.push(Fact {
            title: "Oldest usable rate",
            id: "currency-max-age-fact",
            value: Html::text(&if max_age == 0 {
                "No limit".to_owned()
            } else if max_age == 1 {
                "1 day".to_owned()
            } else {
                format!("{max_age} days")
            }),
        });
        let mut summary = render(SectionHeading {
            title: "Base currency",
            ..SectionHeading::default()
        })?;
        summary.push(&render(Facts {
            id: "currency-summary",
            items: facts,
        })?);
        if let Some((country, code)) = &suggestion {
            // Setting the base enables the currency in the same write.
            let usable = base_choices.iter().any(|currency| &currency.code == code);
            let mut hint = Html::text(&format!(
                "Your main company is in {country}, which uses {code}. "
            ));
            if usable {
                hint.push(&render(Button {
                    id: "currency-use-suggestion",
                    event: "use_suggestion",
                    name: "key",
                    value: code,
                    button_type: "button",
                    working: "Saving…",
                    body: Html::text(&format!("Use {code} as the base currency")),
                    ..Button::default()
                })?);
            }
            summary.push(&render(MutedHtml { body: hint })?);
        }
        if !companies.is_empty() {
            let mut items = Vec::new();
            for company in &companies {
                let own = company_bases.get(&company.id).map_or("", String::as_str);
                let display = match own {
                    "" => render(MutedText {
                        text: &match &base {
                            Some(base) => format!("{} (the base currency)", base.code),
                            None => "Follows the base currency".to_owned(),
                        },
                    })?,
                    code => Html::text(code),
                };
                let mut field = InlineField::new(
                    format!("currency-company-{}", company.id),
                    "currency",
                    "Edit currency",
                    own,
                )
                .status(self.statuses.get(&format!("company-{}", company.id)))
                .allow_empty();
                field.save_event = "save_company_base".into();
                field.id_value = company.id.to_string();
                field.editable = manage;
                let editor = render(currency_editor(
                    &format!("currency-company-{}-input", company.id),
                    "currency",
                    &enabled,
                    own,
                ))?;
                items.push((
                    company.name.clone(),
                    format!("currency-company-fact-{}", company.id),
                    render(&InlineChoice {
                        field,
                        display,
                        editor,
                    })?,
                ));
            }
            let facts = render(Facts {
                id: "currency-companies",
                items: items
                    .iter()
                    .map(|(name, id, value)| Fact {
                        title: name,
                        id,
                        value: value.clone(),
                    })
                    .collect(),
            })?;
            summary.push(&render(Fold {
                id: "currency-companies-fold",
                title: "Currency by company",
                summary: "Leave a company empty to follow the base currency.",
                open: false,
                body: facts,
            })?);
        }
        let summary = render(Card {
            id: "currency-summary-card",
            body: summary,
            ..Card::default()
        })?;

        // The list.
        let search = self.state.search.to_lowercase();
        let status = self
            .state
            .filters
            .get("status")
            .map_or("all", String::as_str)
            .to_owned();
        let shown: Vec<Currency> = catalog
            .into_iter()
            .filter(|currency| match status.as_str() {
                "enabled" => currency.enabled,
                "off" => !currency.enabled,
                _ => true,
            })
            .filter(|currency| search.is_empty() || currency.matches(&search))
            .collect();
        let page = self.state.paginate(&shown);
        let (entries, counts) = (page.entries, page.counts);
        self.shown = entries.len();
        if self.rows_changed {
            self.columns.load(entries, |currency| currency.code.clone());
        }
        let header = render(Header {
            title: Html::text("Currencies"),
            subtitle:
                "Enable the currencies you use. Only enabled currencies appear in forms and rates.",
            actions: render(Button {
                id: "currency-rates-link",
                href: RATES_PATH,
                body: Html::text("Exchange rates"),
                ..Button::default()
            })?,
            ..Header::default()
        })?;
        let form = self.state.filters_form();
        let toolbar = render(FilterToolbar::new(
            "currency-filters",
            vec![
                ToolbarControl::search(
                    "currency-search",
                    "search",
                    "Search currencies",
                    &form["search"],
                    "Search by code or name, e.g. RMB...",
                ),
                ToolbarControl::select(
                    "currency-status",
                    "status",
                    "Show",
                    &form["status"],
                    vec![
                        Choice::new("All currencies", "all"),
                        Choice::new("Enabled", "enabled"),
                        Choice::new("Not enabled", "off"),
                    ],
                ),
            ],
        ))?;
        let mut rows = Vec::with_capacity(entries.len());
        for currency in entries {
            // One switch is the status and its control: a person without
            // currency manage, and a withdrawn currency that is off, read it.
            let (event, label) = if currency.enabled {
                ("disable", format!("Disable {}", currency.code))
            } else {
                ("enable", format!("Enable {}", currency.code))
            };
            let id = format!("currency-{}-enabled", currency.code.to_lowercase());
            let mut badges = vec![render(Toggle {
                on: currency.enabled,
                label: &label,
                id: &id,
                event,
                name: "key",
                value: &currency.code,
                disabled: !manage || (currency.withdrawn && !currency.enabled),
            })?];
            if currency.withdrawn {
                badges.push(render(Badge::status(BadgeKind::Warning, "Withdrawn"))?);
            }
            if base.as_ref().is_some_and(|base| base.code == currency.code) {
                badges.push(render(Badge::status(BadgeKind::Plain, "Base"))?);
            }
            let mut row = FlexRow::new(currency.code.clone())
                .slot(
                    "code",
                    render(CodeCell {
                        text: &currency.code,
                    })?,
                )
                .slot("name", Html::text(&currency.display_name()))
                .slot(
                    "numeric",
                    render(CodeCell {
                        text: &currency.numeric_code,
                    })?,
                )
                .slot("places", Html::text(&currency.minor_units.to_string()))
                .slot("status", render(Badges { badges })?);
            row.dom_id = Some(format!("currency-{}", currency.code.to_lowercase()));
            rows.push(row);
        }
        let mut table = FlexTable::new("currencies", self.columns.flex_columns(), rows);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Currencies";
        table.framed = false;
        if counts.total_entries == 0 {
            table.empty = Some(TableEmpty::Content(Html::text(
                if search.is_empty() && status == "all" {
                    "The currency list is not installed. Ask your administrator to install it."
                } else {
                    "No currency matches this search."
                },
            )));
        }
        let mut body = render(table)?;
        body.push(&render(Pagination::new(
            "currency-pagination",
            counts,
            &PAGE_SIZES,
        ))?);
        let card = render(Card {
            id: "currency-card",
            inner_class: "p-0",
            body,
            ..Card::default()
        })?;
        View {
            id: "currency-page",
            flash: flash_map(&self.flash),
            header,
            summary,
            toolbar,
            card,
            footer: Html::EMPTY,
            dialog: Html::EMPTY,
        }
        .render()
        .map_err(|_| Error::Internal)
    }
}

#[derive(Template)]
#[template(
    source = r#"<p class="mt-2 text-sm text-ink-muted">{{ body|safe }}</p>"#,
    ext = "html"
)]
struct MutedHtml {
    body: Html,
}

/// The editor of an inline currency choice: one searchable box over the
/// enabled currencies. Its `name` is the inline field's, which is how the
/// hook finds the control to commit.
fn currency_editor(id: &str, name: &str, enabled: &[Currency], value: &str) -> Combobox {
    Combobox::new(id, name)
        .options(enabled.iter().map(|currency| currency.choice()).collect())
        .value(value)
        .placeholder("Choose a currency")
        .aria_label("Currency")
        .wrapper_class("mb-0")
}
fn base_input(code: &str, company: Option<i64>) -> Value {
    let mut input = json!({});
    if !code.trim().is_empty() {
        input["code"] = json!(code.trim());
    }
    if let Some(company) = company {
        input["company"] = json!(company);
    }
    input
}

// ------------------------------------------------------------- live adapters

impl LiveState<Scope, Error> for CurrencyPage {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            self.rows_changed = true;
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.state = ListState::parse(values, CURRENCY_SPEC.clone())
                        .map_err(|_| Error::Internal)?
                }
                LiveInput::Event(value) => {
                    self.event(request.scope, request.credential, value).await?
                }
            }
            let html = self.render(request.scope).await?;
            Ok(LiveBody {
                html,
                query: self.state.to_params(),
                rows: self.shown,
                notice: self.notice.clone(),
                ..LiveBody::default()
            })
        })
    }
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        match operation {
            "currencies" => Some("Currencies"),
            "rates" => Some("Exchange rates"),
            _ => None,
        }
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let html = match request.operation {
                "currencies" => {
                    CurrencyPage::new(self.clone(), request.query)?
                        .render(scope)
                        .await?
                }
                "rates" => {
                    rates::Rates::new(self.clone(), request.query)?
                        .render(scope)
                        .await?
                }
                _ => return Err(Error::NotFound),
            };
            Ok(PageBody {
                html,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            let state: Box<dyn LiveState<Scope, Error>> = match request.operation {
                "currencies" => Box::new(CurrencyPage::new(self.clone(), request.query)?),
                "rates" => Box::new(rates::Rates::new(self.clone(), request.query)?),
                _ => return Ok(None),
            };
            Ok(Some(state))
        })
    }
}
