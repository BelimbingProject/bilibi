//! Tenant provisioning and its page, composing Base Tenancy and
//! `Core.Company.PrimaryCompanyManager` above the operation registry.
mod operations;
mod oversight;
mod oversight_web;
mod web;

use bilibi_agent_api::{Error as ContractError, Result};
use bilibi_company::CompanyService;
use bilibi_database::{Db, Error, MutationTransaction};
use bilibi_session::Sessions;
use bilibi_tenancy::{
    CreateOutcome, Move, MoveRefusal, NewTenant, Scope, TenantErrors, TenantStatus, Tenants,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

pub const SERVICE: &str = "core/tenant.tenants";
pub const CREATE: &str = "core.tenant.tenant.create";
pub const LIST: &str = "core.tenant.tenant.list";
pub const GET: &str = "core.tenant.tenant.get";
pub const SUSPEND: &str = "core.tenant.tenant.suspend";
pub const REACTIVATE: &str = "core.tenant.tenant.reactivate";
const PAGES: &str = "core/tenant.pages";

/// The composition facade. No caller supplies a tenant or actor to act as.
pub struct TenantService {
    db: Db,
    tenants: Arc<Tenants>,
    sessions: Arc<Sessions>,
    companies: Arc<CompanyService>,
    step_up: Arc<bilibi_step_up::StepUp>,
    oversight: Arc<bilibi_tenancy::Oversight>,
}

fn tenant_errors(errors: TenantErrors) -> ContractError {
    ContractError::InvalidInput(
        BTreeMap::from([
            ("name".into(), errors.name),
            ("parent_id".into(), errors.parent_id),
        ])
        .into_iter()
        .filter(|(_, errors)| !errors.is_empty())
        .collect(),
    )
}
fn draft(input: &Value) -> Result<(NewTenant, Vec<String>)> {
    let mut companies = vec![input["primary_company"]
        .as_str()
        .ok_or(Error::Invalid)?
        .to_owned()];
    if let Some(additional) = input.get("additional_companies") {
        for name in additional.as_array().ok_or(Error::Invalid)? {
            companies.push(name.as_str().ok_or(Error::Invalid)?.to_owned());
        }
    }
    Ok((
        NewTenant {
            name: bilibi_agent_api::text_field(input, "name"),
            parent_id: input.get("parent_id").and_then(Value::as_i64),
        },
        companies,
    ))
}
fn tenant_json(tenant: &bilibi_tenancy::TenantIdentity) -> Value {
    json!({"id":tenant.id,"parent_id":tenant.parent_id,"name":tenant.name,"status":tenant.status.as_str(),"is_platform_operator":tenant.is_platform_operator,
        "suspension":tenant.suspension.as_ref().map(|suspension| json!({"at":suspension.at,"reason":suspension.reason}))})
}
/// The move named by a suspend or reactivate operation key.
fn move_of(key: &str) -> Move {
    if key == REACTIVATE {
        Move::Reactivate
    } else {
        Move::Suspend
    }
}
/// What a refused move says to the caller: `reason` problems sit on the reason
/// field, everything else on `tenant_id`.
fn move_refusal(refusal: MoveRefusal, change: Move) -> ContractError {
    let (field, message) = match refusal {
        MoveRefusal::NotFound => return ContractError::NotFound,
        MoveRefusal::Reason(message) => ("reason", message),
        MoveRefusal::Operator => (
            "tenant_id",
            "is the platform operator's own tenant and cannot be suspended".to_owned(),
        ),
        MoveRefusal::NotAllowed(status) => (
            "tenant_id",
            match (change, status) {
                (Move::Suspend, _) => "is already suspended".to_owned(),
                (Move::Reactivate, TenantStatus::Active) => "is not suspended".to_owned(),
                (Move::Reactivate, TenantStatus::Suspended) => "cannot be reactivated".to_owned(),
            },
        ),
    };
    ContractError::InvalidInput(BTreeMap::from([(field.into(), vec![message])]))
}
/// `tenant_id` and the reason a move's input names.
fn move_input(input: &Value) -> Result<(i64, String)> {
    Ok((
        input["tenant_id"].as_i64().ok_or(Error::Invalid)?,
        bilibi_agent_api::text_field(input, "reason"),
    ))
}

impl TenantService {
    /// What a suspend or reactivate would do, with nothing written.
    async fn review_move(&self, scope: &Scope, input: &Value, key: &str) -> Result<Value> {
        let change = move_of(key);
        let client = self.db.get().await.map_err(Error::from)?;
        let proof = bilibi_authz::authorize_operator(&**client, scope, change.capability()).await?;
        let (id, reason) = move_input(input)?;
        let plan = self
            .tenants
            .preview_move(&proof, id, change, &reason)
            .await?
            .map_err(|refusal| move_refusal(refusal, change))?;
        let (summary, effects) = match change {
            Move::Suspend => (
                format!("Suspend tenant “{}”", plan.tenant.name),
                vec![
                    "Everyone signed in to this tenant is signed out now.",
                    "Nobody can sign in, and agent connections and queued or scheduled jobs of this tenant are refused, until it is reactivated.",
                    "Its data is kept.",
                ],
            ),
            Move::Reactivate => (
                format!("Reactivate tenant “{}”", plan.tenant.name),
                vec!["People can sign in again and agent connections work again. Jobs that were cancelled while it was suspended are not restarted."],
            ),
        };
        Ok(
            json!({"summary":summary,"tenant":tenant_json(&plan.tenant),"to":plan.to.as_str(),"reason":plan.reason,"effects":effects}),
        )
    }
    /// Suspends or reactivates a tenant. Suspending also ends every session
    /// of the tenant in this same transaction, so a rolled-back move signs
    /// nobody out and a committed one leaves nobody signed in.
    async fn move_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &Value,
        key: &str,
    ) -> Result<Value> {
        let change = move_of(key);
        bilibi_authz::authorize_operator(tx.sql(), scope, change.capability()).await?;
        let (id, reason) = move_input(input)?;
        self.step_up.require(scope, key, "tenant status").await?;
        let moved = self
            .tenants
            .move_in(tx, scope, id, change, &reason)
            .await?
            .map_err(|refusal| move_refusal(refusal, change))?;
        let ended = match change {
            Move::Suspend => self.sessions.end_tenant(tx, id).await?,
            Move::Reactivate => 0,
        };
        Ok(json!({"tenant":tenant_json(&moved),"sessions_ended":ended}))
    }
    async fn review(&self, scope: &Scope, input: &Value) -> Result<Value> {
        let client = self.db.get().await.map_err(Error::from)?;
        let proof =
            bilibi_authz::authorize_operator(&**client, scope, bilibi_tenancy::CREATE).await?;
        let (tenant, names) = draft(input)?;
        let tenant = self
            .tenants
            .preview(&proof, &tenant)
            .await?
            .map_err(tenant_errors)?;
        let companies = self
            .companies
            .preview_tenant_companies(&**client, scope, &names)
            .await?
            .map_err(ContractError::InvalidInput)?;
        let parent = match tenant.parent_id {
            Some(id) => self
                .tenants
                .fetch(&proof, id)
                .await?
                .map(|tenant| tenant_json(&tenant)),
            None => None,
        };
        Ok(
            json!({"summary":"Create a tenant and its companies", "tenant":{"name":tenant.name,"status":TenantStatus::Active.as_str(),"parent":parent},
            "companies":companies.iter().enumerate().map(|(index,company)| json!({"name":company.name,"code":company.code,"status":company.status,"primary":index==0})).collect::<Vec<_>>() }),
        )
    }
    async fn create_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &Value,
    ) -> Result<Value> {
        bilibi_authz::authorize_operator(tx.sql(), scope, bilibi_tenancy::CREATE).await?;
        let (new, names) = draft(input)?;
        // Refuse invalid company input before inserting anything. Both owners
        // check again on this transaction; every failure rolls back the plan.
        self.companies
            .preview_tenant_companies(tx.sql(), scope, &names)
            .await?
            .map_err(ContractError::InvalidInput)?;
        self.step_up.require(scope, CREATE, "new tenant").await?;
        let receipt = match self.tenants.create_in(tx, scope, &new).await? {
            CreateOutcome::Created(receipt) => receipt,
            CreateOutcome::Refused(errors) => return Err(tenant_errors(errors)),
        };
        let companies = self
            .companies
            .provision_new_tenant_in(tx, scope, &receipt, &names)
            .await?;
        Ok(
            json!({"tenant":tenant_json(receipt.identity()), "companies":companies.iter().enumerate().map(|(index,company)| json!({"id":company.id,"tenant_id":company.tenant_id,"name":company.name,"code":company.code,"status":company.status,"primary":index==0})).collect::<Vec<_>>() }),
        )
    }
}

bilibi_step_up::declare!(
    "core/tenant",
    bilibi_step_up::Operation::new(CREATE, "Create a tenant and its companies").floor()
);
bilibi_step_up::declare!(
    "core/tenant",
    bilibi_step_up::Operation::new(SUSPEND, "Suspend a tenant").floor()
);
bilibi_step_up::declare!(
    "core/tenant",
    bilibi_step_up::Operation::new(REACTIVATE, "Reactivate a tenant").floor()
);

inventory::submit! {
    bilibi_module_registry::Module {
        id:"core/tenant", layer:bilibi_module_registry::Layer::Core,
        dependencies:&["base/agent_api","base/authz","base/audit","base/database","base/datetime","base/grid","base/locale","base/menu","base/module_registry","base/session","base/settings","base/step_up","base/system","base/tenancy","base/ui","core/company"], migrations:&[],
        services:&[
            bilibi_module_registry::Service { id:SERVICE, factory:|services| {
                let facade = Arc::new(TenantService {
                    db:services.get::<Db>("base/database.pool")?.as_ref().clone(),
                    tenants:services.get("base/tenancy.tenants")?, sessions:services.get("base/session.sessions")?, companies:services.get(bilibi_company::SERVICE)?,
                    step_up:services.get(bilibi_step_up::SERVICE)?,
                    oversight:services.get(bilibi_tenancy::OVERSIGHT)?,
                });
                operations::bind(facade.clone())?;
                Ok(facade)
            } },
            bilibi_module_registry::Service { id:PAGES, factory:|services| Ok(Arc::new(bilibi_ui::page::PageService::<Scope,Error>(Arc::new(web::Pages {
                db:services.get::<Db>("base/database.pool")?.as_ref().clone(),
                display:services.get("base/datetime.display")?,
                locales:services.get("base/locale.locales")?,
                contracts:services.get(bilibi_agent_api::page::SERVICE)?,
                authorization:services.get("base/tenancy.authorization")?,
            })))) },
            bilibi_module_registry::Service { id:oversight_web::PAGES, factory:oversight_web::service },
        ],
    }
}
inventory::submit! { bilibi_module_registry::Contribution::new(bilibi_module_registry::contributions::ROUTES, "core/tenant.tenants", bilibi_module_registry::Provenance { module:"core/tenant", source:file!(), line:line!() }, &bilibi_module_registry::contributions::Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method:bilibi_module_registry::contributions::Method::Get, path:"/tenancy/tenants", service:PAGES, operation:"tenants", access:bilibi_module_registry::contributions::Access::OperatorCapability(bilibi_tenancy::LIST), framed:true,
}) }
inventory::submit! { bilibi_module_registry::Contribution::new(bilibi_module_registry::contributions::PAGE_WRITES, PAGES, bilibi_module_registry::Provenance { module:"core/tenant", source:file!(), line:line!() }, &bilibi_module_registry::contributions::PageWrites {
    service:PAGES, contracts:&[("create",CREATE),("confirm_suspend",SUSPEND),("confirm_reactivate",REACTIVATE)], pending:bilibi_module_registry::contributions::Pending::None,
}) }

bilibi_menu::contribute!(
    "core/tenant",
    bilibi_menu::MenuItem::container("admin.tenancy", "Tenancy")
        .icon("building-library")
        .under("admin")
        .order(80)
);
bilibi_menu::contribute!(
    "core/tenant",
    bilibi_menu::MenuItem::page("admin.tenancy.tenant", "Tenants", "/tenancy/tenants")
        .under("admin.tenancy")
        .requires(bilibi_menu::Requirement::Capability(bilibi_tenancy::LIST))
        .order(10)
);
