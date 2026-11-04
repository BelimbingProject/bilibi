use crate::{MANAGE, PRICES_MANAGE, VIEW};
use bilibi_authz::authorize_key;
use bilibi_currency::Currencies;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_tenancy::Scope;
use jiff::civil::Date;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};
use tokio_postgres::{GenericClient, Row};

pub type FieldErrors = BTreeMap<String, Vec<String>>;
pub type Checked<T> = std::result::Result<T, FieldErrors>;
pub(crate) fn invalid(field: &str, message: &str) -> FieldErrors {
    [(field.into(), vec![message.into()])].into()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductInput {
    #[serde(default)]
    pub code: String,
    pub name: String,
    pub kind: String,
    pub base_unit: String,
    pub category: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Product {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub kind: String,
    pub base_unit: String,
    pub category: Option<String>,
    pub status: String,
    pub version: i64,
}
impl Product {
    fn row(row: &Row) -> Self {
        Self {
            id: row.get("id"),
            code: row.get("code"),
            name: row.get("name"),
            kind: row.get("kind"),
            base_unit: row.get("base_unit"),
            category: row.get("category"),
            status: row.get("status"),
            version: row.get("version"),
        }
    }
    pub fn to_json(&self) -> Value {
        json!(self)
    }
}
#[derive(Clone, Debug)]
pub struct PriceInput {
    pub product: i64,
    pub selling_company: i64,
    pub currency: String,
    pub effective_on: Date,
    pub amount: Decimal,
}
#[derive(Clone, Debug)]
pub struct Price {
    pub id: i64,
    pub product: i64,
    pub selling_company: i64,
    pub currency: String,
    pub effective_on: Date,
    pub amount: Decimal,
}
impl Price {
    fn row(row: &Row) -> Result<Self> {
        let scale = row.get::<_, i16>("minor_units") as u32;
        let mut amount: Decimal = row.get("amount");
        amount.rescale(scale);
        Ok(Self {
            id: row.get("id"),
            product: row.get("product_id"),
            selling_company: row.get("selling_company"),
            currency: row.get("currency"),
            effective_on: row
                .get::<_, String>("effective_on")
                .parse()
                .map_err(|_| Error::Internal)?,
            amount,
        })
    }
    pub fn to_json(&self) -> Value {
        json!({"id":self.id,"product":self.product,"selling_company":self.selling_company,"currency":self.currency,"effective_on":self.effective_on.to_string(),"amount":self.amount.to_string()})
    }
}
/// Tenant-wide product identity and company-authorized dated selling prices.
pub struct ProductQuery<'a> {
    pub search: &'a str,
    pub status: &'a str,
    pub category: &'a str,
    pub limit: i64,
    pub offset: i64,
}
pub struct Products {
    pub(crate) currencies: Arc<Currencies>,
}
const COLUMNS: &str = "id,code,name,kind,base_unit,category,status,version";
impl Products {
    pub async fn get(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        id: i64,
    ) -> Result<Product> {
        authorize_key(client, scope, VIEW).await?;
        self.load(client, scope, id, false).await
    }
    async fn load(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        id: i64,
        lock: bool,
    ) -> Result<Product> {
        client
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM core_products WHERE tenant_id=$1 AND id=$2{}",
                    if lock { " FOR UPDATE" } else { "" }
                ),
                &[&scope.tenant(), &id],
            )
            .await?
            .as_ref()
            .map(Product::row)
            .ok_or(Error::NotFound)
    }
    /// Bounded tenant-wide list; status and category are filters on this one list.
    pub async fn list(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        query: &ProductQuery<'_>,
    ) -> Result<(Vec<Product>, i64)> {
        let ProductQuery {
            search,
            status,
            category,
            limit,
            offset,
        } = *query;
        authorize_key(client, scope, VIEW).await?;
        let needle = format!(
            "%{}%",
            search
                .trim()
                .chars()
                .take(200)
                .collect::<String>()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let predicate = "FROM core_products WHERE tenant_id=$1 AND (name ILIKE $2 OR code ILIKE $2) AND ($3='' OR status=$3) AND ($4='' OR category=$4)";
        let total = client
            .query_one(
                &format!("SELECT count(*) {predicate}"),
                &[&scope.tenant(), &needle, &status, &category],
            )
            .await?
            .get(0);
        let rows = client
            .query(
                &format!("SELECT {COLUMNS} {predicate} ORDER BY name,id LIMIT $5 OFFSET $6"),
                &[
                    &scope.tenant(),
                    &needle,
                    &status,
                    &category,
                    &limit.clamp(1, 100),
                    &offset.max(0),
                ],
            )
            .await?;
        Ok((rows.iter().map(Product::row).collect(), total))
    }
    pub async fn preview_create(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        input: &ProductInput,
    ) -> Result<Checked<ProductInput>> {
        authorize_key(client, scope, MANAGE).await?;
        let mut after = input.clone();
        after.code = after.code.trim().to_owned();
        after.name = after.name.trim().to_owned();
        after.base_unit = after.base_unit.trim().to_owned();
        after.category = after
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let mut errors = FieldErrors::new();
        for (field, value, max, required) in [
            ("code", after.code.as_str(), 64, false),
            ("name", after.name.as_str(), 200, true),
            ("base_unit", after.base_unit.as_str(), 80, true),
            (
                "category",
                after.category.as_deref().unwrap_or(""),
                120,
                false,
            ),
        ] {
            if (required && value.is_empty())
                || value.chars().count() > max
                || value.chars().any(char::is_control)
            {
                errors.insert(
                    field.into(),
                    vec![format!(
                        "Use {} to {max} characters.",
                        if required { 1 } else { 0 }
                    )],
                );
            }
        }
        if !["goods", "service"].contains(&after.kind.as_str()) {
            errors.extend(invalid("kind", "Choose goods or service."));
        }
        if !after.code.is_empty()
            && client
                .query_opt(
                    "SELECT id FROM core_products WHERE tenant_id=$1 AND code=$2",
                    &[&scope.tenant(), &after.code],
                )
                .await?
                .is_some()
        {
            errors.extend(invalid("code", "This code is already in use."));
        }
        Ok(if errors.is_empty() {
            Ok(after)
        } else {
            Err(errors)
        })
    }
    pub async fn create(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &ProductInput,
    ) -> Result<Checked<Product>> {
        tx.sql()
            .query_one(
                "SELECT pg_advisory_xact_lock(hashtextextended('core/product.codes', $1))",
                &[&scope.tenant()],
            )
            .await?;
        let input = match self.preview_create(tx.sql(), scope, input).await? {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let tenant = scope.tenant();
        let saved=tx.mutate(move |sql| Box::pin(async move {
            let code = if input.code.is_empty() {
                loop {
                    let n:i64=sql.query_one("SELECT nextval('core_product_codes')",&[]).await?.get(0);
                    let candidate=format!("PRD-{n:06}");
                    if sql.query_opt("SELECT id FROM core_products WHERE tenant_id=$1 AND code=$2", &[&tenant,&candidate]).await?.is_none() { break candidate; }
                }
            } else { input.code };
            let row=sql.query_one(&format!("INSERT INTO core_products(tenant_id,code,name,kind,base_unit,category) VALUES($1,$2,$3,$4,$5,$6) RETURNING {COLUMNS}"),&[&tenant,&code,&input.name,&input.kind,&input.base_unit,&input.category]).await?;
            let saved=Product::row(&row);
            let change=Change {kind:"product".into(),id:saved.id,old:json!({}),new:saved.to_json()};
            Ok((saved,vec![change]))
        })).await?;
        Ok(Ok(saved))
    }
    /// Only shared facts may be patched; optimistic version fencing prevents lost edits.
    pub async fn preview_update(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Value,
    ) -> Result<Checked<Product>> {
        authorize_key(client, scope, MANAGE).await?;
        let mut after = self.load(client, scope, id, false).await?;
        if after.version != version {
            return Ok(Err(invalid(
                "version",
                "This product changed. Reload before saving.",
            )));
        }
        let Some(fields) = patch.as_object() else {
            return Ok(Err(invalid("patch", "Use an object of product fields.")));
        };
        for (field, value) in fields {
            let text = value.as_str().unwrap_or("").trim();
            match field.as_str() {
                "name" if !text.is_empty() && text.chars().count() <= 200 => {
                    after.name = text.into()
                }
                "kind" if ["goods", "service"].contains(&text) => after.kind = text.into(),
                "base_unit" if !text.is_empty() && text.chars().count() <= 80 => {
                    after.base_unit = text.into()
                }
                "category"
                    if text.chars().count() <= 120 && (value.is_string() || value.is_null()) =>
                {
                    after.category = (!text.is_empty()).then(|| text.into())
                }
                "status" if ["active", "retired"].contains(&text) => after.status = text.into(),
                _ => return Ok(Err(invalid(field, "Use a valid product value."))),
            }
            if text.chars().any(char::is_control) {
                return Ok(Err(invalid(field, "Use text without control characters.")));
            }
        }
        Ok(Ok(after))
    }
    pub async fn update(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Value,
    ) -> Result<Checked<Product>> {
        let before = self.load(tx.sql(), scope, id, true).await?;
        let mut after = match self
            .preview_update(tx.sql(), scope, id, version, patch)
            .await?
        {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        after.version += 1;
        let tenant = scope.tenant();
        let saved=tx.mutate(move |sql|Box::pin(async move {
            sql.execute("UPDATE core_products SET name=$3,kind=$4,base_unit=$5,category=$6,status=$7,version=$8 WHERE tenant_id=$1 AND id=$2",&[&tenant,&id,&after.name,&after.kind,&after.base_unit,&after.category,&after.status,&after.version]).await?;
            let change=Change{kind:"product".into(),id,old:before.to_json(),new:after.to_json()}; Ok((after,vec![change]))
        })).await?;
        Ok(Ok(saved))
    }
    pub async fn preview_price(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        input: &PriceInput,
    ) -> Result<Checked<PriceInput>> {
        bilibi_company::authorize_work_target(client, scope, input.selling_company, PRICES_MANAGE)
            .await?;
        let product = self.load(client, scope, input.product, false).await?;
        if product.status != "active" {
            return Ok(Err(invalid("product", "This product is retired.")));
        }
        let mut input = input.clone();
        input.currency = bilibi_currency::normalize(&input.currency).unwrap_or_default();
        let Some(currency) = self.currencies.get(client, scope, &input.currency).await? else {
            return Ok(Err(invalid("currency", "Choose an enabled currency.")));
        };
        if !currency.enabled || currency.withdrawn {
            return Ok(Err(invalid("currency", "Choose an enabled currency.")));
        }
        if input.amount < Decimal::ZERO
            || input.amount >= Decimal::from(1_000_000_000_000_000i64)
            || input.amount.normalize().scale() > currency.minor_units
        {
            return Ok(Err(invalid(
                "amount",
                &format!(
                    "Use a nonnegative amount with up to {} decimal places.",
                    currency.minor_units
                ),
            )));
        }
        if client.query_opt("SELECT id FROM core_product_prices WHERE tenant_id=$1 AND product_id=$2 AND selling_company=$3 AND currency=$4 AND effective_on=$5::text::date",&[&scope.tenant(),&input.product,&input.selling_company,&input.currency,&input.effective_on.to_string()]).await?.is_some() {return Ok(Err(invalid("effective_on","A price already starts on this date for this company and currency.")));}
        input.amount.rescale(currency.minor_units);
        Ok(Ok(input))
    }
    /// Add a dated amount. Earlier prices are never overwritten.
    pub async fn add_price(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &PriceInput,
    ) -> Result<Checked<Price>> {
        // Serialize against retirement and other price inserts for this product.
        self.load(tx.sql(), scope, input.product, true).await?;
        let input = match self.preview_price(tx.sql(), scope, input).await? {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        if let Err(refusal) = self.currencies.check(tx, scope, &input.currency).await? {
            return Ok(Err(invalid("currency", refusal.message())));
        }
        let tenant = scope.tenant();
        let saved=tx.mutate(move |sql|Box::pin(async move {
            let id=sql.query_one("INSERT INTO core_product_prices(tenant_id,product_id,selling_company,currency,effective_on,amount) VALUES($1,$2,$3,$4,$5::text::date,$6) RETURNING id",&[&tenant,&input.product,&input.selling_company,&input.currency,&input.effective_on.to_string(),&input.amount]).await?.get(0);
            let price=Price{id,product:input.product,selling_company:input.selling_company,currency:input.currency,effective_on:input.effective_on,amount:input.amount};
            let change=Change{kind:"product_price".into(),id,old:json!({}),new:price.to_json()}; Ok((price,vec![change]))
        })).await?;
        Ok(Ok(saved))
    }
    /// Latest price on or before `on`, or explicitly missing. No company/currency fallback.
    pub async fn price(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        product: i64,
        company: i64,
        currency: &str,
        on: Date,
    ) -> Result<Option<Price>> {
        bilibi_company::authorize_work_target(client, scope, company, VIEW).await?;
        self.load(client, scope, product, false).await?;
        let currency = bilibi_currency::normalize(currency).ok_or(Error::Invalid)?;
        client.query_opt("SELECT p.id,p.product_id,p.selling_company,p.currency,p.amount,p.effective_on::text AS effective_on,c.minor_units FROM core_product_prices p JOIN currencies c ON c.code=p.currency WHERE p.tenant_id=$1 AND p.product_id=$2 AND p.selling_company=$3 AND p.currency=$4 AND p.effective_on <= $5::text::date ORDER BY p.effective_on DESC LIMIT 1",&[&scope.tenant(),&product,&company,&currency,&on.to_string()]).await?.as_ref().map(Price::row).transpose()
    }
    pub async fn prices(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        product: i64,
        company: i64,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Price>, i64)> {
        bilibi_company::authorize_work_target(client, scope, company, VIEW).await?;
        self.load(client, scope, product, false).await?;
        let total=client.query_one("SELECT count(*) FROM core_product_prices WHERE tenant_id=$1 AND product_id=$2 AND selling_company=$3",&[&scope.tenant(),&product,&company]).await?.get(0);
        let rows=client.query("SELECT p.id,p.product_id,p.selling_company,p.currency,p.amount,p.effective_on::text AS effective_on,c.minor_units FROM core_product_prices p JOIN currencies c ON c.code=p.currency WHERE p.tenant_id=$1 AND p.product_id=$2 AND p.selling_company=$3 ORDER BY p.effective_on DESC,p.id DESC LIMIT $4 OFFSET $5",&[&scope.tenant(),&product,&company,&limit.clamp(1,100),&offset.max(0)]).await?;
        Ok((rows.iter().map(Price::row).collect::<Result<_>>()?, total))
    }
}
