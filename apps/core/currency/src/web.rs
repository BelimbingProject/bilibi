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
use crate::{
    facade::{normalize, Currencies, Currency, Rate, RateQuery, Request, NOTE_MAX},
    CURRENCIES_PATH, MANAGE, PAGES, RATES_MANAGE, RATES_PATH,
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
    params, AddToItem, AddToMenu, Badge, BadgeKind, BuiltinColumn, Button, ButtonVariant, Card,
    Choice, ColumnsOutcome, Combobox, CommitStates, CommitStatus, ConfirmDialog, CreateButton,
    Fact, Facts, Field, FilterToolbar, FlashMap, FlexRow, FlexTable, Fold, Form, Header, Html,
    InlineChoice, InlineEdit, InlineField, InputKind, ListColumns, ListSpec, ListState, Modal,
    PageCounts, Pagination, SectionHeading, StatusKind, TableEmpty, Toggle, ToolbarControl,
    NO_FLASH,
};
use jiff::civil::Date;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    sync::{Arc, LazyLock},
};

const PAGE_SIZES: [u64; 3] = [25, 50, 100];
const DENIED: &str = "You do not have permission to change currencies.";
const RATES_DENIED: &str = "You do not have permission to change exchange rates.";
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
static RATES_SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        omit_blank: vec!["search".into()],
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("exchange rate list spec")
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

// ----------------------------------------------------------------- the rates

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

struct Rates {
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
    /// A rate the row's Actions… menu asked to delete (`?delete=<id>`); the
    /// confirm opens once the rows are read.
    pending_delete: Option<i64>,
    check: Check,
    flash: Option<(StatusKind, String)>,
}

impl Rates {
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
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
            pending_delete: query.get("delete").and_then(|id| id.parse().ok()),
            check: Check::default(),
            flash: None,
        })
    }
    fn rate(&self, id: i64) -> Option<&Rate> {
        self.rates.iter().find(|rate| rate.id == id)
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
                let note = || {
                    quote
                        .rate_ids
                        .first()
                        .and_then(|id| self.rates.iter().find(|rate| rate.id == *id))
                        .and_then(|rate| rate.source_note.clone())
                        .unwrap_or_else(|| "entered manually".into())
                };
                let source = match quote.path {
                    crate::Path::Same => "same currency, no rate needed".to_owned(),
                    crate::Path::Direct | crate::Path::Inverse => note(),
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
    /// enabled one) on top, below it the other side of the latest rate that
    /// involves it (or the next enabled currency), on today.
    fn start_check(&mut self, enabled: &[Currency], base: Option<&Currency>, today: Option<Date>) {
        let top = base
            .filter(|base| enabled.iter().any(|currency| currency.code == base.code))
            .or_else(|| enabled.first())
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
                enabled
                    .iter()
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

    async fn render(&mut self, scope: &Scope) -> Result<String> {
        let pages = self.pages.clone();
        let client = pages.db.get().await?;
        let capabilities = effective_capabilities(&**client, scope).await?;
        let edits = capabilities.contains(RATES_MANAGE);
        let search = self.state.search.clone();
        let (limit, offset) = (self.state.page_size as i64, self.state.offset() as i64);
        let (rates, total) = pages
            .currencies
            .rates(
                &**client,
                scope,
                &RateQuery {
                    code: (!search.trim().is_empty()).then(|| search.clone()),
                    limit,
                    offset,
                },
            )
            .await?;
        let enabled = pages.currencies.enabled(&**client, scope).await?;
        let today = pages.today(&**client, scope).await?;
        drop(client);
        self.rates = rates;
        if let Some(id) = self.pending_delete.take().filter(|_| edits) {
            if let Some(rate) = self.rate(id).cloned() {
                self.flash = None;
                self.confirm = Some(rate);
            }
        }
        self.shown = self.rates.len();
        if self.rows_changed {
            self.columns.load(&self.rates, |rate| rate.id.to_string());
        }
        let counts = PageCounts::new(self.state.page, self.state.page_size, total as u64);
        let mut actions = render(Button {
            id: "currency-list-link",
            href: CURRENCIES_PATH,
            body: Html::text("Currencies"),
            ..Button::default()
        })?;
        if edits {
            actions.push(&render(CreateButton {
                noun: "Rate",
                id: "currency-rate-new",
                event: "new",
                button_type: "button",
                ..CreateButton::default()
            })?);
        }
        let header = render(Header {
            title: Html::text("Exchange rates"),
            subtitle: "Each rate applies from its date until a later one for the same pair.",
            actions,
            ..Header::default()
        })?;
        let form = self.state.filters_form();
        let toolbar = render(FilterToolbar::new(
            "currency-rate-filters",
            vec![ToolbarControl::search(
                "currency-rate-search",
                "search",
                "Search rates",
                &form["search"],
                "Search by currency code...",
            )],
        ))?;
        let mut rows = Vec::with_capacity(self.rates.len());
        for rate in &self.rates {
            let mut row = FlexRow::new(rate.id.to_string())
                .slot(
                    "pair",
                    Html::text(&format!("1 {} → {}", rate.from, rate.to)),
                )
                .slot("effective", Html::text(&rate.effective_on.to_string()))
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
            if edits {
                let mut query = self.state.to_params();
                query.insert("delete".into(), rate.id.to_string());
                row.actions = render(AddToMenu::new(
                    format!("currency-rate-{}-actions", rate.id),
                    format!(
                        "the {} to {} rate from {}",
                        rate.from, rate.to, rate.effective_on
                    ),
                    vec![AddToItem {
                        label: "Delete".into(),
                        href: params::address(RATES_PATH, &query),
                        disabled: None,
                    }],
                ))?;
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
            // Each empty state offers its next step: a rate needs two
            // enabled currencies.
            let (text, next) = if !search.is_empty() {
                ("No rate matches this search.", Html::EMPTY)
            } else if enabled.len() < 2 {
                (
                    "No exchange rates yet. Enable the currencies first.",
                    render(Button {
                        id: "currency-rate-empty-enable",
                        href: CURRENCIES_PATH,
                        body: Html::text("Enable currencies"),
                        ..Button::default()
                    })?,
                )
            } else if edits {
                (
                    "No exchange rates yet.",
                    render(Button {
                        id: "currency-rate-empty-add",
                        event: "new",
                        button_type: "button",
                        body: Html::text("Add a rate"),
                        ..Button::default()
                    })?,
                )
            } else {
                ("No exchange rates yet.", Html::EMPTY)
            };
            let mut empty = Html::text(text);
            if !next.is_empty() {
                empty.push(&Html::text(" "));
                empty.push(&next);
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
        if self.check.top.is_empty() && self.check.bottom.is_empty() && enabled.len() >= 2 {
            let base = {
                let client = pages.db.get().await?;
                pages.currencies.base(&**client, scope, None).await?
            };
            self.start_check(&enabled, base.as_ref(), today);
        }
        self.calculate(scope, self.check.side).await?;
        // A conversion needs two enabled currencies; until then the empty
        // state says so and the calculator stays out of the way.
        let check = if enabled.len() >= 2 {
            self.check_card(&enabled, edits)?
        } else {
            Html::EMPTY
        };
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
            (None, Some(draft)) => self.new_dialog(&enabled, draft)?,
            (None, None) => Html::EMPTY,
        };
        View {
            id: "currency-rates-page",
            flash: flash_map(&self.flash),
            header,
            summary: Html::EMPTY,
            toolbar,
            card,
            footer: check,
            dialog,
        }
        .render()
        .map_err(|_| Error::Internal)
    }

    /// The currency conversion calculator: the line that names the rate's
    /// date and source with the date to convert on, then two rows of an
    /// amount and its currency. Typing in either row answers in the other.
    fn check_card(&self, enabled: &[Currency], edits: bool) -> Result<Html> {
        let check = &self.check;
        let options: Vec<Choice> = enabled
            .iter()
            .map(|currency| currency.choice())
            .collect();
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

    fn new_dialog(&self, enabled: &[Currency], draft: &Draft) -> Result<Html> {
        let value = |key: &str| draft.values.get(key).cloned().unwrap_or_default();
        let errors = |key: &str| draft.errors.get(key).cloned().unwrap_or_default();
        let options: Vec<Choice> = enabled.iter().map(|currency| currency.choice()).collect();
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
impl LiveState<Scope, Error> for Rates {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            self.rows_changed = true;
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.state = ListState::parse(values, RATES_SPEC.clone())
                        .map_err(|_| Error::Internal)?;
                    self.pending_delete = values.get("delete").and_then(|id| id.parse().ok());
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
                    Rates::new(self.clone(), request.query)?
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
                "rates" => Box::new(Rates::new(self.clone(), request.query)?),
                _ => return Ok(None),
            };
            Ok(Some(state))
        })
    }
}
