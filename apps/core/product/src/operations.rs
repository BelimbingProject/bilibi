use crate::{
    facade::invalid, Checked, PriceInput, ProductInput, Products, MANAGE, PRICES_MANAGE, VIEW,
};
use bilibi_agent_api::{
    json::{Data, Output},
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, ReadHandler,
    WriteHandler, DEFINITIONS,
};
use bilibi_database::{Db, Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
};
struct Bound {
    db: Db,
    products: Arc<Products>,
}
static BOUND: OnceLock<Bound> = OnceLock::new();
pub(crate) fn bind(db: Db, products: Arc<Products>) -> bilibi_module_registry::Result<()> {
    BOUND.set(Bound { db, products }).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate product binding".into())
    })
}
fn bound() -> bilibi_agent_api::Result<&'static Bound> {
    BOUND.get().ok_or(Error::Internal.into())
}
fn checked<T>(v: Checked<T>) -> bilibi_agent_api::Result<T> {
    v.map_err(bilibi_agent_api::Error::InvalidInput)
}
fn id(v: &Value, key: &str) -> bilibi_agent_api::Result<i64> {
    v[key].as_i64().filter(|v| *v > 0).ok_or_else(|| {
        bilibi_agent_api::Error::InvalidInput(invalid(key, "Choose a valid record."))
    })
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}
fn price_input(v: &Value) -> bilibi_agent_api::Result<PriceInput> {
    Ok(PriceInput {
        product: id(v, "product")?,
        selling_company: id(v, "company_id")?,
        currency: text(v, "currency").into(),
        effective_on: text(v, "effective_on").parse().map_err(|_| {
            bilibi_agent_api::Error::InvalidInput(invalid(
                "effective_on",
                "Use a date like 2026-11-04.",
            ))
        })?,
        amount: text(v, "amount").parse().map_err(|_| {
            bilibi_agent_api::Error::InvalidInput(invalid("amount", "Use an amount like 47.60."))
        })?,
    })
}
fn product_input(v: &Value) -> bilibi_agent_api::Result<ProductInput> {
    serde_json::from_value(v.clone()).map_err(|_| {
        bilibi_agent_api::Error::InvalidInput(invalid(
            "input",
            "Enter a product name, goods or service, and base unit.",
        ))
    })
}
fn output(v: Value) -> Output {
    Output::Data(Data::restricted(v, &HashSet::new()))
}
enum Read {
    List,
    Get,
    Price,
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "core/product"
    }
    fn call<'a>(
        &'a self,
        _client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let b = bound()?;
            let client = b.db.get().await.map_err(Error::from)?;
            let client = &**client;
            let value = match self {
                Self::List => {
                    let page = input["page"].as_i64().unwrap_or(1).clamp(1, 1_000_000);
                    let size = input["page_size"].as_i64().unwrap_or(25).clamp(1, 100);
                    let (rows, total) = b
                        .products
                        .list(
                            client,
                            scope,
                            &crate::ProductQuery {
                                search: text(input, "search"),
                                status: text(input, "status"),
                                category: text(input, "category"),
                                limit: size,
                                offset: (page - 1) * size,
                            },
                        )
                        .await?;
                    json!({"products":rows,"total":total,"page":page,"page_size":size})
                }
                Self::Get => b
                    .products
                    .get(client, scope, id(input, "id")?)
                    .await?
                    .to_json(),
                Self::Price => {
                    let on = text(input, "on").parse().map_err(|_| Error::Invalid)?;
                    let price = b
                        .products
                        .price(
                            client,
                            scope,
                            id(input, "product")?,
                            id(input, "company_id")?,
                            text(input, "currency"),
                            on,
                        )
                        .await?;
                    json!({"missing":price.is_none(),"price":price.map(|p|p.to_json())})
                }
            };
            Ok(output(value))
        })
    }
}
enum Write {
    Create,
    Update,
    Price,
}
impl WriteHandler for Write {
    fn creates(&self) -> Option<bilibi_agent_api::CreatedRecord> {
        matches!(self, Self::Create).then_some(bilibi_agent_api::CreatedRecord {
            kind: "product",
            id_pointer: "/id",
        })
    }

    fn owner(&self) -> &'static str {
        "core/product"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let p = &bound()?.products;
            // Use the review's own read-only transaction; never take another pool connection.
            let client = client.transaction().ok_or(Error::Internal)?;
            let value = match self {
                Self::Create => {
                    let after = checked(
                        p.preview_create(client, scope, &product_input(input)?)
                            .await?,
                    )?;
                    json!({"summary":format!("Create product {}",after.name),"after":after})
                }
                Self::Update => {
                    let after = checked(
                        p.preview_update(
                            client,
                            scope,
                            id(input, "id")?,
                            id(input, "version")?,
                            &input["patch"],
                        )
                        .await?,
                    )?;
                    json!({"summary":format!("Update product {}",after.name),"after":after})
                }
                Self::Price => {
                    let after =
                        checked(p.preview_price(client, scope, &price_input(input)?).await?)?;
                    json!({"summary":format!("Add {} {} per base unit from {}",after.currency,after.amount,after.effective_on),"after":{"product":after.product,"selling_company":after.selling_company,"currency":after.currency,"amount":after.amount.to_string(),"effective_on":after.effective_on.to_string()}})
                }
            };
            Ok(value.into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let p = &bound()?.products;
            let value = match self {
                Self::Create => {
                    checked(p.create(tx, scope, &product_input(input)?).await?)?.to_json()
                }
                Self::Update => checked(
                    p.update(
                        tx,
                        scope,
                        id(input, "id")?,
                        id(input, "version")?,
                        &input["patch"],
                    )
                    .await?,
                )?
                .to_json(),
                Self::Price => {
                    checked(p.add_price(tx, scope, &price_input(input)?).await?)?.to_json()
                }
            };
            Ok(output(value))
        })
    }
}
fn list_schema() -> Value {
    json!({"type":"object","properties":{"search":{"type":"string","maxLength":200},"status":{"type":"string","enum":["active","retired",""]},"category":{"type":"string","maxLength":120},"page":{"type":"integer","minimum":1,"maximum":1000000},"page_size":{"type":"integer","minimum":1,"maximum":100}}})
}
fn get_schema() -> Value {
    json!({"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1}}})
}
fn create_schema() -> Value {
    json!({"type":"object","required":["name","kind","base_unit"],"properties":{"code":{"type":"string","maxLength":64,"description":"Leave empty to generate a code."},"name":{"type":"string","maxLength":200},"kind":{"type":"string","enum":["goods","service"]},"base_unit":{"type":"string","maxLength":80},"category":{"type":"string","nullable":true,"maxLength":120}}})
}
fn update_schema() -> Value {
    json!({"type":"object","required":["id","version","patch"],"properties":{"id":{"type":"integer","minimum":1},"version":{"type":"integer","minimum":1},"patch":{"type":"object","properties":{"name":{"type":"string","maxLength":200},"kind":{"type":"string","enum":["goods","service"]},"base_unit":{"type":"string","maxLength":80},"category":{"type":"string","nullable":true,"maxLength":120},"status":{"type":"string","enum":["active","retired"]}}}}})
}
fn price_schema() -> Value {
    json!({"type":"object","required":["product","company_id","currency","effective_on","amount"],"properties":{"product":{"type":"integer","minimum":1},"company_id":{"type":"integer","minimum":1},"currency":{"type":"string","maxLength":3},"effective_on":{"type":"string","format":"date"},"amount":{"type":"string","maxLength":40,"x-bilibi-money":true}}})
}
fn lookup_schema() -> Value {
    json!({"type":"object","required":["product","company_id","currency","on"],"properties":{"product":{"type":"integer","minimum":1},"company_id":{"type":"integer","minimum":1},"currency":{"type":"string","maxLength":3},"on":{"type":"string","format":"date"}}})
}
fn result_schema() -> Value {
    json!({"type":"object","description":"The product, dated selling price, or bounded list. A missing price is null with missing true; amounts are decimal strings."})
}
macro_rules! op {
    ($key:literal,$title:literal,$summary:literal,$cap:expr,$schema:ident,$handler:expr) => {
        Operation {
            key: $key,
            title: $title,
            summary: $summary,
            keywords: &["product", "selling", "price"],
            capability: $cap,
            input: $schema,
            output: result_schema,
            handler: $handler,
            channels: Channels::Every,
            step_up: &[],
        }
    };
}
inventory::submit! {Contribution::new(DEFINITIONS,"core/product.agent",Provenance{module:"core/product",source:file!(),line:line!()},&Definitions{
    operations:&[
        op!("core.product.list","List products","Find shared goods and services, including retired products.",VIEW,list_schema,Handler::Read(&Read::List)),
        op!("core.product.get","Read a product","Shared product facts; earlier references remain readable after retirement.",VIEW,get_schema,Handler::Read(&Read::Get)),
        op!("core.product.price","Find a selling price","Latest price effective on the requested date for exactly this company and currency. Missing is never zero.",VIEW,lookup_schema,Handler::Read(&Read::Price)),
        op!("core.product.create","Create a product","Create shared product identity. Leave code empty to generate one.",MANAGE,create_schema,Handler::SafeWrite(&Write::Create)),
        op!("core.product.update","Update a product","Change shared facts or retire a product using the version read. Earlier transaction snapshots stay unchanged.",MANAGE,update_schema,Handler::SafeWrite(&Write::Update)),
        op!("core.product.price.add","Add a selling price","Add an amount per base unit from a date for an authorized selling company. Duplicate dates are refused.",PRICES_MANAGE,price_schema,Handler::SafeWrite(&Write::Price)),
    ],guides:&[],
})}
