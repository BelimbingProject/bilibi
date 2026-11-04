//! Shared product identity and dated selling prices. Contract: README.md; ADR 22.
mod facade;
mod operations;
mod records;
mod web;
use bilibi_authz::{CapabilityDefinition as C, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{
    contributions::{
        Access, CompanyScope, Method, PageWrites, Pending, Route, PAGE_WRITES, ROUTES,
    },
    Contribution, Layer, Module, Provenance, Service,
};
pub use facade::{
    Checked, FieldErrors, Price, PriceInput, Product, ProductInput, ProductQuery, Products,
};
pub use jiff::civil::Date;
pub use rust_decimal::Decimal;
use std::sync::Arc;
pub const SERVICE: &str = "core/product.products";
pub const VIEW: &str = "products.view";
pub const MANAGE: &str = "products.manage";
pub const PRICES_MANAGE: &str = "products.prices.manage";
pub(crate) const PAGES: &str = "core/product.pages";
macro_rules! provenance {
    () => {
        Provenance {
            module: "core/product",
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! {Module{
    id:"core/product",layer:Layer::Core,
    dependencies:&["base/agent_api","base/authz","base/database","base/datetime","base/locale","base/grid","base/menu","base/module_registry","base/records","base/tenancy","base/ui","core/company","core/currency"],
    migrations:&[("core-product-001-product",include_str!("../migrations/001-product.sql"))],
    services:&[
        Service{id:SERVICE,factory:|s|{let products=Arc::new(Products{currencies:s.get(bilibi_currency::SERVICE)?});operations::bind(s.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),products.clone())?;Ok(products)}},
        Service{id:PAGES,factory:|s|Ok(Arc::new(bilibi_ui::page::PageService::<bilibi_tenancy::Scope,bilibi_database::Error>(Arc::new(web::Pages{db:s.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),products:s.get(SERVICE)?,contracts:s.get(bilibi_agent_api::page::SERVICE)?,panels:web::record_panels()?,views:s.get("base/grid.page_views")?,display:s.get("base/datetime.display")?,locales:s.get("base/locale.locales")?}))))},
    ],
}}
inventory::submit! {Contribution::new(VOCABULARY,"core/product",provenance!(),&VocabularyContribution{
    domains:&["products"],verbs:&["view","manage"],capabilities:&[C::tenant(VIEW),C::tenant(MANAGE),C::company(PRICES_MANAGE)],
    roles:&[RoleDefinition{code:"tenant_owner",name:"Tenant Owner",description:"Contributed system role; reconciled explicitly",grant_all:false,capabilities:&[VIEW,MANAGE,PRICES_MANAGE]}],system_principals:&[],
})}
macro_rules! route{($id:literal,$path:literal,$operation:literal,$scope:expr)=>{inventory::submit!{Contribution::new(ROUTES,$id,provenance!(),&Route{company_scope:$scope,method:Method::Get,path:$path,service:PAGES,operation:$operation,access:Access::Capability(VIEW),framed:true})}};}
route!(
    "core/product.list",
    "/products",
    "list",
    CompanyScope::Tenant
);
route!(
    "core/product.record",
    "/products/{id}",
    "record",
    CompanyScope::Company
);
inventory::submit! {Contribution::new(PAGE_WRITES,PAGES,provenance!(),&PageWrites{service:PAGES,contracts:&[("create","core.product.create"),("save_field","core.product.update"),("add_price","core.product.price.add")],pending:Pending::None})}
bilibi_menu::contribute!(
    "core/product",
    bilibi_menu::MenuItem::page("products", "Products", "/products")
        .under("admin")
        .icon("cube")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(65)
);
