use crate::{Price, Product, Products, MANAGE, PAGES, PRICES_MANAGE};
use askama::Template;
use bilibi_agent_api::page::{Call, PageContracts};
use bilibi_database::{Db, Error, Result};
use bilibi_grid::{PageColumns, PageViews};
use bilibi_module_registry::{
    contributions::{Access, Embed, EmbedContext, Viewer, EMBEDS},
    Contribution,
};
use bilibi_tenancy::Scope;
use bilibi_ui::{page::*, *};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
};
const SIZES: [u64; 3] = [25, 50, 100];
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: SIZES.to_vec(),
        filters: [
            (
                "status".into(),
                Filter::one_of(&["", "active", "retired"], ""),
            ),
            ("category".into(), Filter::string("")),
        ]
        .into(),
        omit_defaults: true,
        omit_blank: vec!["search".into(), "category".into()],
        ..ListSpec::default()
    }
    .shared()
    .expect("product list spec")
});
#[derive(Clone)]
pub(crate) struct RecordPanel {
    id: String,
    embed: &'static str,
    access: Access,
}
pub(crate) fn record_panels() -> bilibi_module_registry::Result<Vec<RecordPanel>> {
    let mut panels = Vec::new();
    for c in inventory::iter::<Contribution>
        .into_iter()
        .filter(|c| c.consumer == EMBEDS)
    {
        let e = c.get::<Embed>()?;
        if matches!(
            e.context,
            EmbedContext::Record {
                kind: "core/product"
            }
        ) {
            panels.push(RecordPanel {
                id: format!("product-panel-{}", c.id.replace(['.', '/'], "-")),
                embed: c.id,
                access: e.access,
            });
        }
    }
    panels.sort_by(|a, b| a.embed.cmp(b.embed));
    Ok(panels)
}
#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub products: Arc<Products>,
    pub contracts: Arc<PageContracts>,
    pub panels: Vec<RecordPanel>,
    pub views: Arc<PageViews>,
    pub display: Arc<bilibi_datetime::DisplayPolicy>,
    pub locales: Arc<bilibi_locale::Locales>,
}
impl Pages {
    fn state(&self, r: &PageRequest<'_, Scope>) -> Result<State> {
        let id = if r.operation == "record" {
            Some(
                r.path
                    .get("id")
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &i64| *v > 0)
                    .ok_or(Error::NotFound)?,
            )
        } else if r.operation == "list" {
            None
        } else {
            return Err(Error::NotFound);
        };
        Ok(State {
            pages: self.clone(),
            id,
            framed: r.framed,
            list: ListState::parse(r.query, SPEC.clone()).map_err(|_| Error::Invalid)?,
            columns: PageColumns::mount(self.views.clone(), "products", cols(false))
                .map_err(|_| Error::Internal)?,
            price_columns: PageColumns::mount(self.views.clone(), "product-prices", cols(true))
                .map_err(|_| Error::Internal)?,
            products: Vec::new(),
            product: None,
            prices: Vec::new(),
            total: 0,
            company: None,
            can_manage: false,
            can_price: false,
            currency_choices: Vec::new(),
            panels: Vec::new(),
            draft: None,
            flash: String::new(),
            success: false,
            redirect: None,
            commits: CommitStates::default(),
            reload: true,
            initial_query: Some(r.query.clone()),
        })
    }
}
impl bilibi_ui::page::Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        Some(if operation == "list" {
            "Products"
        } else {
            "Product"
        })
    }
    fn render<'a>(&'a self, r: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = r.scope.ok_or(Error::Unauthorized)?;
            let mut state = self.state(&r)?;
            let body = state
                .update(LiveRequest {
                    scope,
                    credential: "",
                    input: LiveInput::Refresh,
                })
                .await?;
            Ok(PageBody {
                html: body.html,
                live: true,
                title: state.product.as_ref().map(|p| p.name.clone()),
            })
        })
    }
    fn mount<'a>(&'a self, r: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            Ok(Some(
                Box::new(self.state(&r)?) as Box<dyn LiveState<Scope, Error>>
            ))
        })
    }
}
fn cols(price: bool) -> Vec<BuiltinColumn> {
    let fields = if price {
        vec![
            ("effective_on", "Applies from"),
            ("currency", "Currency"),
            ("amount", "Amount per base unit"),
        ]
    } else {
        vec![
            ("code", "Code"),
            ("name", "Name"),
            ("kind", "Kind"),
            ("base_unit", "Base unit"),
            ("category", "Category"),
            ("status", "Status"),
        ]
    };
    fields
        .into_iter()
        .map(|(k, v)| {
            if k == "amount" {
                BuiltinColumn::new(k, v).right()
            } else {
                BuiltinColumn::new(k, v)
            }
        })
        .collect()
}
#[derive(Default)]
struct Draft {
    values: HashMap<String, String>,
    errors: BTreeMap<String, Vec<String>>,
}
struct State {
    pages: Pages,
    id: Option<i64>,
    framed: bool,
    list: ListState,
    columns: PageColumns,
    price_columns: PageColumns,
    products: Vec<Product>,
    product: Option<Product>,
    prices: Vec<Price>,
    total: u64,
    company: Option<i64>,
    can_manage: bool,
    can_price: bool,
    currency_choices: Vec<Choice>,
    panels: Vec<PanelSpec>,
    draft: Option<Draft>,
    flash: String,
    success: bool,
    redirect: Option<String>,
    commits: CommitStates,
    reload: bool,
    initial_query: Option<HashMap<String, String>>,
}
fn render(t: impl Template) -> Result<Html> {
    Html::render(&t).map_err(|_| Error::Internal)
}
fn button(label: &str, event: &str) -> Result<Html> {
    render(Button {
        body: Html::text(label),
        event,
        button_type: "button",
        ..Button::default()
    })
}
fn form_values(v: &Value, ns: &str) -> Result<HashMap<String, String>> {
    params::form_values_checked(v, ns, params::FIELD_LIMIT).map_err(Error::refused)
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}
impl State {
    async fn load(&mut self, scope: &Scope) -> Result<()> {
        if let Some(query) = self.initial_query.take() {
            let columns = if self.id.is_some() {
                &mut self.price_columns
            } else {
                &mut self.columns
            };
            columns.from_params(scope, &query).await?;
        }
        let connection = self.pages.db.get().await?;
        let client = &**connection;
        let locale = self
            .pages
            .locales
            .resolve(client, scope, None)
            .await?
            .locale;
        let display = self
            .pages
            .display
            .display(client, scope, None, locale)
            .await?;
        let held = bilibi_authz::effective_capabilities(client, scope).await?;
        self.can_manage = held.contains(MANAGE);
        self.can_price = held.contains(PRICES_MANAGE);
        self.company = scope.current_company().map(|id| id.get());
        let limit = self.list.page_size as i64;
        let offset = (self.list.page - 1) as i64 * limit;
        if let Some(id) = self.id {
            self.product = Some(self.pages.products.get(client, scope, id).await?);
            if let Some(company) = self.company {
                match self
                    .pages
                    .products
                    .prices(client, scope, id, company, limit, offset)
                    .await
                {
                    Ok((prices, total)) => {
                        let (prices, total) = if self
                            .list
                            .clamp_to_last_page((total as u64).div_ceil(self.list.page_size), true)
                        {
                            self.reload = true;
                            self.pages
                                .products
                                .prices(
                                    client,
                                    scope,
                                    id,
                                    company,
                                    limit,
                                    (self.list.page - 1) as i64 * limit,
                                )
                                .await?
                        } else {
                            (prices, total)
                        };
                        self.reload |= self
                            .prices
                            .iter()
                            .map(|p| p.id)
                            .ne(prices.iter().map(|p| p.id));
                        self.prices = prices;
                        self.total = total as u64;
                    }
                    Err(Error::Forbidden) => {
                        self.prices.clear();
                        self.total = 0;
                        self.company = None;
                        self.can_price = false;
                    }
                    Err(e) => return Err(e),
                }
                self.can_price = permitted(
                    bilibi_company::authorize_work_target(client, scope, company, PRICES_MANAGE)
                        .await,
                )?;
            }
            self.currency_choices = self
                .pages
                .products
                .currencies
                .choices(client, scope)
                .await?;
            let viewer = Viewer {
                capabilities: &held,
                operator: permitted(bilibi_tenancy::prove_operator(client, scope).await)?,
                owner: permitted(bilibi_tenancy::prove_owner(client, scope).await)?,
            };
            self.panels = self
                .pages
                .panels
                .iter()
                .filter(|p| p.access.admits(&viewer))
                .map(|p| PanelSpec {
                    id: p.id.clone(),
                    embed: p.embed.into(),
                    path: [
                        ("id".into(), id.to_string()),
                        ("product_id".into(), id.to_string()),
                        ("kinds".into(), "product".into()),
                    ]
                    .into(),
                    query: HashMap::new(),
                })
                .collect();
        } else {
            let (rows, total) = self
                .pages
                .products
                .list(
                    client,
                    scope,
                    &crate::ProductQuery {
                        search: &self.list.search,
                        status: &self.list.filters["status"],
                        category: &self.list.filters["category"],
                        limit,
                        offset,
                    },
                )
                .await?;
            let (rows, total) = if self
                .list
                .clamp_to_last_page((total as u64).div_ceil(self.list.page_size), true)
            {
                self.reload = true;
                self.pages
                    .products
                    .list(
                        client,
                        scope,
                        &crate::ProductQuery {
                            search: &self.list.search,
                            status: &self.list.filters["status"],
                            category: &self.list.filters["category"],
                            limit,
                            offset: (self.list.page - 1) as i64 * limit,
                        },
                    )
                    .await?
            } else {
                (rows, total)
            };
            self.reload |= self
                .products
                .iter()
                .map(|p| (p.id, p.version))
                .ne(rows.iter().map(|p| (p.id, p.version)));
            self.products = rows;
            self.total = total as u64;
        }
        drop(connection);
        if self.reload {
            let (columns, keys) = if self.id.is_some() {
                (
                    &mut self.price_columns,
                    self.prices.iter().map(|p| json!(p.id)).collect::<Vec<_>>(),
                )
            } else {
                (
                    &mut self.columns,
                    self.products
                        .iter()
                        .map(|p| json!(p.id))
                        .collect::<Vec<_>>(),
                )
            };
            columns.load(scope, &keys, &display).await?;
            self.reload = false;
        }
        Ok(())
    }
    async fn submit(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
        contract: &'static str,
        input: Value,
    ) -> Result<Option<Value>> {
        let result = self
            .pages
            .contracts
            .submit(
                &Call {
                    scope,
                    credential,
                    event,
                    contract,
                    service: PAGES,
                },
                input,
            )
            .await;
        match result {
            Ok(v) => {
                self.flash = "Product saved.".into();
                self.success = true;
                Ok(Some(v))
            }
            Err(bilibi_agent_api::Error::Backend(
                e @ (Error::Unauthorized | Error::StepUpRequired { .. }),
            )) => Err(e),
            Err(bilibi_agent_api::Error::InvalidInput(fields)) => {
                self.flash = fields
                    .iter()
                    .flat_map(|(k, vs)| {
                        vs.iter()
                            .map(move |v| error_sentence(&k.replace('_', " "), v))
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                self.success = false;
                if let Some(d) = &mut self.draft {
                    d.errors = fields;
                }
                Ok(None)
            }
            Err(
                bilibi_agent_api::Error::Forbidden
                | bilibi_agent_api::Error::Backend(Error::Forbidden),
            ) => {
                self.flash = "You do not have permission to make this change.".into();
                self.success = false;
                Ok(None)
            }
            Err(e) => {
                self.flash = e.to_string();
                self.success = false;
                Ok(None)
            }
        }
    }
    async fn event(&mut self, scope: &Scope, credential: &str, v: &Value) -> Result<()> {
        match text(v, "type") {
            "filters" => {
                self.list.apply_filters(&form_values(v, "filters")?);
                self.reload = true;
            }
            "page" => {
                self.list.put_page(v["page"].as_str());
                self.reload = true;
            }
            "grid" => {
                let params = params::string_values_checked(v, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let columns = if self.id.is_some() {
                    &mut self.price_columns
                } else {
                    &mut self.columns
                };
                if matches!(
                    columns.handle(scope, &params).await?,
                    bilibi_grid::Outcome::Patch
                ) {
                    self.reload = true;
                }
            }
            "clear-flash" => self.flash.clear(),
            "new" => {
                if !self.can_manage {
                    self.flash = "You do not have permission to create products.".into();
                    self.success = false;
                    return Ok(());
                }
                self.draft = Some(Draft {
                    values: [("kind".into(), "goods".into())].into(),
                    ..Draft::default()
                });
                self.flash.clear();
            }
            "close_new" => self.draft = None,
            "create" => {
                let values = form_values(v, "product")?;
                let get = |k: &str| values.get(k).cloned().unwrap_or_default();
                let input = json!({"code":get("code"),"name":get("name"),"kind":get("kind"),"base_unit":get("base_unit"),"category":get("category")});
                self.draft = Some(Draft {
                    values,
                    errors: BTreeMap::new(),
                });
                if let Some(result) = self
                    .submit(scope, credential, "create", "core.product.create", input)
                    .await?
                {
                    self.redirect = result["result"]["id"]
                        .as_i64()
                        .map(|id| format!("/products/{id}"));
                    self.draft = None;
                }
            }
            "save_field" => {
                self.reload = true;
                let field = text(v, "field");
                let value = text(v, "value");
                let version = text(v, "version")
                    .parse::<i64>()
                    .map_err(|_| Error::Invalid)?;
                let id = self.id.ok_or(Error::Invalid)?;
                let saved = self
                    .submit(
                        scope,
                        credential,
                        "save_field",
                        "core.product.update",
                        json!({"id":id,"version":version,"patch":{field:value}}),
                    )
                    .await?;
                self.commits.put(
                    field,
                    if saved.is_some() {
                        CommitStatus::Saved
                    } else {
                        CommitStatus::refusal(field, value, &[&self.flash])
                    },
                );
            }
            "add_price" => {
                self.reload = true;
                let values = form_values(v, "price")?;
                let get = |k: &str| values.get(k).cloned().unwrap_or_default();
                let id = self.id.ok_or(Error::Invalid)?;
                let company = get("selling_company")
                    .parse::<i64>()
                    .map_err(|_| Error::Invalid)?;
                self.draft = Some(Draft {
                    values: values.clone(),
                    errors: BTreeMap::new(),
                });
                if self.submit(scope,credential,"add_price","core.product.price.add",json!({"product":id,"company_id":company,"currency":get("currency"),"amount":get("amount"),"effective_on":get("effective_on")})).await?.is_some(){self.draft=None;self.flash="Selling price added.".into();}
            }
            _ => return Err(Error::Invalid),
        };
        Ok(())
    }
    fn create_form(&self, d: &Draft) -> Result<Html> {
        let field = |k: &str, label: &str| {
            Field::in_form("product", k)
                .id(format!("product-new-{k}"))
                .label(label)
                .value(d.values.get(k).cloned().unwrap_or_default())
                .errors(d.errors.get(k).cloned().unwrap_or_default())
        };
        let rows = vec![
            vec![render(
                field("name", "Name").required().attr("maxlength", "200"),
            )?],
            vec![
                render(
                    field("code", "Code")
                        .hint("Leave empty to generate a code.")
                        .attr("maxlength", "64"),
                )?,
                render(
                    field("kind", "Kind")
                        .kind(InputKind::Select)
                        .options(choices(&[("Goods", "goods"), ("Service", "service")]))
                        .required(),
                )?,
            ],
            vec![
                render(
                    field("base_unit", "Base unit")
                        .hint("For example: each, hour or year.")
                        .required()
                        .attr("maxlength", "80"),
                )?,
                render(field("category", "Category").attr("maxlength", "120"))?,
            ],
        ];
        let mut actions = button("Cancel", "close_new")?;
        actions.push(&render(Button {
            body: Html::text("Create product"),
            button_type: "submit",
            working: "Creating…",
            ..Button::default()
        })?);
        let mut body = summary("product-new", &d.errors)?;
        body.push(&render(Fields { rows })?);
        let form = render(Form {
            id: "product-create",
            event: "create",
            body,
            actions,
            ..Form::default()
        })?;
        render(Modal {
            id: "product-new-dialog",
            title: "New product",
            cancel_event: "close_new",
            body: form,
            ..Modal::default()
        })
    }
    fn table(&self, price: bool) -> Result<Html> {
        let columns = if price {
            &self.price_columns
        } else {
            &self.columns
        };
        let mut rows = Vec::new();
        if price {
            for (p, row) in self.prices.iter().zip(&columns.rows) {
                rows.push(
                    row.clone()
                        .slot(
                            "effective_on",
                            Html::text(
                                &bilibi_datetime::DisplayContext::anonymous()
                                    .render_date(p.effective_on),
                            ),
                        )
                        .slot("currency", Html::text(&p.currency))
                        .slot("amount", Html::text(&p.amount.to_string())),
                );
            }
        } else {
            for (p, row) in self.products.iter().zip(&columns.rows) {
                let href = format!("/products/{}", p.id);
                let id = p.id.to_string();
                let link = render(RecordLink {
                    workspace: self.framed,
                    kind: "core/product",
                    record_id: &id,
                    href: &href,
                    body: Html::text(&p.name),
                    class: "",
                })?;
                rows.push(
                    row.clone()
                        .slot("code", Html::text(&p.code))
                        .slot("name", link)
                        .slot(
                            "kind",
                            Html::text(if p.kind == "goods" {
                                "Goods"
                            } else {
                                "Service"
                            }),
                        )
                        .slot("base_unit", Html::text(&p.base_unit))
                        .slot("category", Html::text(p.category.as_deref().unwrap_or("—")))
                        .slot(
                            "status",
                            Html::text(if p.status == "active" {
                                "Active"
                            } else {
                                "Retired"
                            }),
                        ),
                );
            }
        }
        let mut table = columns.flex_table(if price { "product-prices" } else { "products" }, rows);
        table.caption = if price { "Selling prices" } else { "Products" };
        table.empty = Some(TableEmpty::Content(Html::text(if price {
            "No selling prices for this company. A missing price is not zero."
        } else {
            "No products match. Create a product or change the filters."
        })));
        let mut body = render(table)?;
        body.push(&render(Pagination::new(
            "product-pagination",
            PageCounts::new(self.list.page, self.list.page_size, self.total),
            &SIZES,
        ))?);
        render(Card {
            body,
            inner_class: "p-0",
            ..Card::default()
        })
    }
    fn facts(&self, p: &Product) -> Result<Html> {
        let mut items = vec![Fact {
            title: "Code",
            value: Html::text(&p.code),
            ..Fact::default()
        }];
        for (name, label, value, options) in [
            ("name", "Name", p.name.as_str(), vec![]),
            (
                "kind",
                "Kind",
                p.kind.as_str(),
                choices(&[("Goods", "goods"), ("Service", "service")]),
            ),
            ("base_unit", "Base unit", p.base_unit.as_str(), vec![]),
            (
                "category",
                "Category",
                p.category.as_deref().unwrap_or(""),
                vec![],
            ),
            (
                "status",
                "Status",
                p.status.as_str(),
                choices(&[("Active", "active"), ("Retired", "retired")]),
            ),
        ] {
            let mut field =
                InlineField::new(format!("product-{name}"), name, label, value).version(p.version);
            field.editable = self.can_manage;
            field.allow_empty = name == "category";
            field.status = self.commits.get(name);
            if options.is_empty() {
                items.push(Fact {
                    title: label,
                    value: render(InlineEdit { field })?,
                    ..Fact::default()
                });
            } else {
                let editor = render(
                    Field::named(name)
                        .id(format!("product-{name}-choice"))
                        .kind(InputKind::Select)
                        .options(options)
                        .value(value)
                        .label(label),
                )?;
                items.push(Fact {
                    title: label,
                    value: render(InlineChoice {
                        field,
                        display: Html::text(if value == "goods" {
                            "Goods"
                        } else if value == "service" {
                            "Service"
                        } else if value == "active" {
                            "Active"
                        } else {
                            "Retired"
                        }),
                        editor,
                    })?,
                    ..Fact::default()
                });
            }
        }
        render(Card {
            title: "Product",
            body: render(Facts {
                items,
                ..Facts::default()
            })?,
            ..Card::default()
        })
    }
    fn price_form(&self) -> Result<Html> {
        let d = self.draft.as_ref();
        let field = |k: &str, label: &str| {
            Field::in_form("price", k)
                .id(format!("product-price-{k}"))
                .label(label)
                .value(d.and_then(|d| d.values.get(k)).cloned().unwrap_or_default())
                .errors(d.and_then(|d| d.errors.get(k)).cloned().unwrap_or_default())
        };
        let rows = vec![
            vec![
                render(
                    field("amount", "Amount per base unit")
                        .required()
                        .attr("inputmode", "decimal"),
                )?,
                render(
                    field("currency", "Currency")
                        .kind(InputKind::Select)
                        .options(self.currency_choices.clone())
                        .required(),
                )?,
            ],
            vec![render(
                field("effective_on", "Applies from")
                    .kind(InputKind::Date)
                    .required(),
            )?],
        ];
        let mut body = summary(
            "product-price",
            &d.map(|d| d.errors.clone()).unwrap_or_default(),
        )?;
        body.push(&render(Fields { rows })?);
        body.push(&render(
            Field::in_form("price", "selling_company")
                .kind(InputKind::Hidden)
                .value(self.company.map(|c| c.to_string()).unwrap_or_default()),
        )?);
        let actions = render(Button {
            button_type: "submit",
            body: Html::text("Add selling price"),
            working: "Adding…",
            ..Button::default()
        })?;
        render(Card {
            title: "New selling price",
            body: render(Form {
                id: "product-price-form",
                event: "add_price",
                body,
                actions,
                ..Form::default()
            })?,
            ..Card::default()
        })
    }
    fn view(&self) -> Result<String> {
        let mut sections = Vec::new();
        let mut dialog = Html::EMPTY;
        let header = if let Some(p) = &self.product {
            sections.push(render(BackLink {
                href: "/products",
                title: "Products",
                ..BackLink::default()
            })?);
            sections.push(self.facts(p)?);
            sections.push(render(SectionHeading{title:"Selling prices",description:"Prices for the company selected above, per base unit. Earlier prices stay in the history.",..SectionHeading::default()})?);
            if self.company.is_some() {
                sections.push(self.table(true)?);
                if p.status == "retired" {
                    sections.push(render(EmptyState::missing(
                        "Retired product",
                        "Retired products keep earlier prices and cannot receive new ones.",
                    ))?);
                } else if !self.can_price {
                    sections.push(render(EmptyState::forbidden("add selling prices"))?);
                } else if self.currency_choices.is_empty() {
                    sections.push(render(EmptyState::missing(
                        "Selling currency",
                        "The currency list is not installed. Ask your administrator to install it.",
                    ))?);
                } else {
                    sections.push(self.price_form()?);
                }
            } else {
                sections.push(render(EmptyState::missing(
                    "Selling company",
                    "Choose a company you may work in to see selling prices.",
                ))?);
            }
            render(Header {
                title: Html::text(&p.name),
                ..Header::default()
            })?
        } else {
            let filters = self.list.filters_form();
            sections.push(render(FilterToolbar::new(
                "product-filters",
                vec![
                    ToolbarControl::search(
                        "product-search",
                        "search",
                        "Search products",
                        &filters["search"],
                        "Search by code or name…",
                    ),
                    ToolbarControl::select(
                        "product-status",
                        "status",
                        "Status",
                        &filters["status"],
                        choices(&[
                            ("All statuses", ""),
                            ("Active", "active"),
                            ("Retired", "retired"),
                        ]),
                    ),
                    ToolbarControl::search(
                        "product-category",
                        "category",
                        "Category",
                        &filters["category"],
                        "Category…",
                    ),
                ],
            ))?);
            sections.push(self.table(false)?);
            if let Some(d) = &self.draft {
                dialog = self.create_form(d)?;
            }
            let actions = if self.can_manage {
                render(CreateButton {
                    noun: "Product",
                    event: "new",
                    id: "product-new",
                    button_type: "button",
                    ..CreateButton::default()
                })?
            } else {
                Html::EMPTY
            };
            render(Header {
                title: Html::text("Products"),
                subtitle: "Goods and services shared across your work.",
                actions,
                ..Header::default()
            })?
        };
        let flash = if self.success {
            FlashMap {
                success: &self.flash,
                ..NO_FLASH
            }
        } else {
            FlashMap {
                error: &self.flash,
                ..NO_FLASH
            }
        };
        let id = self.id.map(|id| id.to_string()).unwrap_or_default();
        let panels = self.panels.iter().map(|p| p.id.clone()).collect();
        View {
            header,
            sections,
            dialog,
            flash,
            record_id: &id,
            variant: if self.id.is_some() {
                PageVariant::Detail
            } else {
                PageVariant::List
            },
            panels,
        }
        .render()
        .map_err(|_| Error::Internal)
    }
}
impl LiveState<Scope, Error> for State {
    fn panels(&self) -> Vec<PanelSpec> {
        self.panels.clone()
    }
    fn update<'a>(&'a mut self, r: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            // Refresh permissions before processing a stale page's events.
            self.load(r.scope).await?;
            match r.input {
                LiveInput::Refresh => {}
                LiveInput::Params(v) => {
                    self.list = ListState::parse(v, SPEC.clone()).map_err(|_| Error::Invalid)?;
                    self.initial_query = Some(v.clone());
                    self.reload = true;
                }
                LiveInput::Event(v) => self.event(r.scope, r.credential, v).await?,
            };
            if let Some(path) = self.redirect.take() {
                return Ok(LiveBody {
                    redirect: Some(path),
                    flash: Some((StatusKind::Success, "Product created.".into())),
                    ..LiveBody::default()
                });
            }
            self.load(r.scope).await?;
            Ok(LiveBody {
                html: self.view()?,
                query: {
                    let mut q = self.list.to_params();
                    q.extend(if self.id.is_some() {
                        self.price_columns.params()
                    } else {
                        self.columns.params()
                    });
                    q
                },
                ..LiveBody::default()
            })
        })
    }
}
#[derive(Template)]
#[template(path = "product_page.html")]
struct View<'a> {
    header: Html,
    sections: Vec<Html>,
    dialog: Html,
    flash: FlashMap<'a>,
    record_id: &'a str,
    variant: PageVariant,
    panels: Vec<String>,
}
#[derive(Template)]
#[template(path = "product_fields.html")]
struct Fields {
    rows: Vec<Vec<Html>>,
}

fn permitted<T>(result: Result<T>) -> Result<bool> {
    match result {
        Ok(_) => Ok(true),
        Err(Error::Forbidden | Error::NotFound) => Ok(false),
        Err(e) => Err(e),
    }
}

fn summary(prefix: &str, errors: &BTreeMap<String, Vec<String>>) -> Result<Html> {
    let mut summary = ErrorSummary::new(format!("{prefix}-errors"));
    for (field, messages) in errors {
        for message in messages {
            summary.push(
                format!("{prefix}-{field}"),
                error_sentence(&field.replace('_', " "), message),
            );
        }
    }
    render(summary)
}
