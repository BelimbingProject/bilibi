//! The operator page (`core/currency.pages`): `/currencies`, one page with
//! two tabs. Base currency (the default) holds the tenant's base currency and
//! each company's own; Exchange rates (`?tab=rates`, `rates.rs`) holds the
//! dated rates, edited in place, and the currency conversion calculator.
//! Nothing is switched on: a currency becomes used the first time it is a
//! base, in a rate or on a record, and every currency control lists the used
//! ones first.
//!
//! Every change goes through the `core.currency.*` contracts agents call
//! (`PageContracts::submit`), which recheck the capability in their own
//! transaction. A control the person may not use is hidden; a refused event is
//! a flash on the page, a refused inline edit an outcome on its field.
mod rates;

use crate::{
    facade::{normalize, Currencies, Currency},
    CURRENCIES_PATH, MANAGE, PAGES,
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
use bilibi_settings::Settings;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, Button, Card, CommitStates, CommitStatus, Fact, Facts, FlashMap, Fold, Header, Html,
    InlineChoice, InlineField, StatusKind, Tab, Tabs, NO_FLASH,
};
use jiff::civil::Date;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

const PAGE_SIZES: [u64; 3] = [25, 50, 100];
const DENIED: &str = "You do not have permission to change currencies.";
/// How many companies the per-company base currencies list.
const COMPANIES_SHOWN: i64 = 100;

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
    tabs: Html,
    body: Html,
    dialog: Html,
}

#[derive(Template)]
#[template(
    source = r#"<span class="text-sm text-ink-muted">{{ text }}</span>"#,
    ext = "html"
)]
struct MutedText<'a> {
    text: &'a str,
}

/// What a tab puts on the page: its body and its dialog. The header stays
/// the same across tabs; a tab's guidance and actions live in the tab. The
/// flash is the tab's own.
struct TabBody {
    body: Html,
    dialog: Html,
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

// ----------------------------------------------------------- the base tab

struct BaseTab {
    pages: Pages,
    statuses: CommitStates,
    notice: String,
    flash: Option<(StatusKind, String)>,
}
impl BaseTab {
    fn new(pages: Pages) -> Self {
        Self {
            pages,
            statuses: CommitStates::default(),
            notice: String::new(),
            flash: None,
        }
    }

    async fn event(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let get = |key: &str| value.get(key).and_then(Value::as_str);
        self.notice.clear();
        match get("type").ok_or(Error::Invalid)? {
            "clear-flash" => clear_flash(&mut self.flash, get("key")),
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

    /// The base currency as a fact: read first, chosen in place from every
    /// current currency, the used ones first.
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

    async fn render(&mut self, scope: &Scope) -> Result<TabBody> {
        let pages = self.pages.clone();
        let mut client = pages.db.get().await?;
        let manage = effective_capabilities(&**client, scope)
            .await?
            .contains(MANAGE);
        let base = pages.currencies.base(&**client, scope, None).await?;
        let current = pages.currencies.current(&**client, scope).await?;
        // The base keeps showing even after ISO withdrew it, so it can be
        // read and replaced.
        let mut base_choices = current.clone();
        if let Some(base) = base.as_ref().filter(|base| base.withdrawn) {
            base_choices.insert(0, base.clone());
        }
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

        let mut body = render(Facts {
            id: "currency-summary",
            items: vec![Fact {
                title: "Base currency",
                id: "currency-base-fact",
                value: self.base_fact(base.as_ref(), &base_choices, manage)?,
            }],
        })?;
        if let Some((country, code)) = &suggestion {
            let mut hint = Html::text(&format!(
                "Your main company is in {country}, which uses {code}. "
            ));
            if current.iter().any(|currency| &currency.code == code) {
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
            body.push(&render(MutedHtml { body: hint })?);
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
                    &current,
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
            body.push(&render(Fold {
                id: "currency-companies-fold",
                title: "Currency by company",
                summary: "Leave a company empty to follow the base currency.",
                open: false,
                body: facts,
            })?);
        }
        Ok(TabBody {
            body: render(Card {
                id: "currency-summary-card",
                body,
                ..Card::default()
            })?,
            dialog: Html::EMPTY,
        })
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

/// The editor of an inline currency choice: one searchable box over every
/// current currency, the used ones first. Its `name` is the inline field's,
/// which is how the hook finds the control to commit.
fn currency_editor(id: &str, name: &str, choices: &[Currency], value: &str) -> bilibi_ui::Combobox {
    bilibi_ui::Combobox::new(id, name)
        .options(choices.iter().map(|currency| currency.choice()).collect())
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

// ------------------------------------------------------------- the page

/// Which tab shows, from the address (`?tab=rates`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Showing {
    Base,
    Rates,
}
impl Showing {
    fn parse(query: &HashMap<String, String>) -> Self {
        match query.get("tab").map(String::as_str) {
            Some("rates") => Self::Rates,
            _ => Self::Base,
        }
    }
}

struct CurrencyPage {
    showing: Showing,
    base: BaseTab,
    rates: rates::Rates,
}
impl CurrencyPage {
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            showing: Showing::parse(query),
            base: BaseTab::new(pages.clone()),
            rates: rates::Rates::new(pages, query)?,
        })
    }

    /// The address of the tab that shows: the rates tab keeps its list's
    /// page and page size.
    fn query(&self) -> HashMap<String, String> {
        match self.showing {
            Showing::Base => HashMap::new(),
            Showing::Rates => {
                let mut query = self.rates.query();
                query.insert("tab".into(), "rates".into());
                query
            }
        }
    }

    async fn render(&mut self, scope: &Scope) -> Result<String> {
        let (tab, flash) = match self.showing {
            Showing::Base => (self.base.render(scope).await?, &self.base.flash),
            Showing::Rates => (self.rates.render(scope).await?, self.rates.flash()),
        };
        let header = render(Header {
            title: Html::text("Currencies"),
            ..Header::default()
        })?;
        let rates_href = params::address(
            CURRENCIES_PATH,
            &HashMap::from([("tab".to_owned(), "rates".to_owned())]),
        );
        let tabs = render(Tabs {
            id: "currency-tabs",
            label: "Currency views",
            class: "mb-3",
            tabs: vec![
                Tab {
                    id: "currency-tab-base",
                    ..Tab::patch(
                        CURRENCIES_PATH,
                        "Base currency",
                        self.showing == Showing::Base,
                    )
                },
                Tab {
                    id: "currency-tab-rates",
                    ..Tab::patch(
                        &rates_href,
                        "Exchange rates",
                        self.showing == Showing::Rates,
                    )
                },
            ],
        })?;
        View {
            id: "currency-page",
            flash: flash_map(flash),
            header,
            tabs,
            body: tab.body,
            dialog: tab.dialog,
        }
        .render()
        .map_err(|_| Error::Internal)
    }
}

// ------------------------------------------------------------- live adapters

impl LiveState<Scope, Error> for CurrencyPage {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => self.rates.refresh(),
                LiveInput::Params(values) => {
                    self.showing = Showing::parse(values);
                    self.rates.params(values)?;
                }
                LiveInput::Event(value) => match self.showing {
                    Showing::Base => {
                        self.base
                            .event(request.scope, request.credential, value)
                            .await?
                    }
                    Showing::Rates => {
                        self.rates
                            .event(request.scope, request.credential, value)
                            .await?
                    }
                },
            }
            let html = self.render(request.scope).await?;
            let (rows, notice) = match self.showing {
                Showing::Base => (0, self.base.notice.clone()),
                Showing::Rates => (self.rates.shown(), self.rates.notice()),
            };
            Ok(LiveBody {
                html,
                query: self.query(),
                rows,
                notice,
                ..LiveBody::default()
            })
        })
    }
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        match operation {
            "currencies" => Some("Currencies"),
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
                _ => return Ok(None),
            };
            Ok(Some(state))
        })
    }
}
