//! Disposable scheduled owner proof, including the real Schedule board.
//! See base/schedule's README. No specimen is linked into the product.
use bilibi_authz::{RoleDefinition, SystemPrincipalDefinition, VocabularyContribution, VOCABULARY};
use bilibi_base::schedule::{
    self, Definition, ExecutionPrincipal, Misfire, Overlap, Reach, Schedule,
};
use bilibi_company::{CompanyService, Edit};
use bilibi_database::{Db, Error, Result};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance, Service};
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, HandlerService, Outcome, Worker,
};
use bilibi_tenancy::{Authentication, PartyProofs};
use serde_json::{json, Value};
use std::sync::Arc;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const MODULE: &str = "extensions/scheduled_owner_reference";
const WORKER: &str = "scheduled-owner-reference/update";
const KEY: &str = "scheduled-owner-reference/update";
const LOCKED_KEY: &str = "scheduled-owner-reference/locked";
const PRINCIPAL: &str = "scheduled-owner-reference";
const HANDLER: &str = "extensions/scheduled_owner_reference.worker";
macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

#[path = "scheduled-owner-reference/catalog.rs"]
mod catalog;
#[path = "scheduled-owner-reference/notifications.rs"]
mod notifications;
#[path = "scheduled-owner-reference/owner.rs"]
mod owner;

inventory::submit! { Contribution::new(VOCABULARY, "scheduled-owner-reference.vocabulary", provenance!(), &VocabularyContribution {
    domains: &[], verbs: &[], capabilities: &[],
    system_principals: &[SystemPrincipalDefinition {
        name: PRINCIPAL,
        capabilities: &[schedule::EXECUTE, schedule::TENANT_WORK, bilibi_company::VIEW, bilibi_company::UPDATE],
    }],
    // The fixture's tenant-2 manager administers this specimen's consent and
    // permissions through the real Grants facade, with no cross-tenant actor.
    roles: &[RoleDefinition {
        code: "manager", name: "manager", description: "Disposable schedule administrator", grant_all: false,
        capabilities: &["admin.authz.system-principal.grant", "admin.authz.system-principal.revoke", schedule::TENANT_WORK, schedule::TENANT_VIEW],
    }],
}) }

fn business_arguments(args: Value) -> Result<Value> {
    if args.as_object().is_some_and(|object| {
        object.is_empty()
            || (object.len() == 1
                && object
                    .get("company_id")
                    .and_then(Value::as_i64)
                    .is_some_and(|id| id > 0))
    }) {
        Ok(args)
    } else {
        Err(Error::Invalid)
    }
}
inventory::submit! { Contribution::new(bilibi_queue::WORKERS, WORKER, provenance!(), &Worker {
    max_attempts: 3, validate: |args| schedule::worker::validate(args, business_arguments),
}) }
inventory::submit! { Contribution::new(bilibi_queue::HANDLERS, WORKER, provenance!(), &HandlerBinding {
    service: HANDLER, capability: schedule::EXECUTE,
}) }
inventory::submit! { Contribution::new(bilibi_queue::CAPABILITY_POLICIES, WORKER, provenance!(), &schedule::TENANT_POLICY) }
inventory::submit! { Contribution::new(schedule::DEFINITIONS, KEY, provenance!(), &Definition {
    name: "Scheduled owner reference", expression: "* * * * *", timezone: "Etc/UTC",
    task_name: "Update the primary company", worker: WORKER, args: || json!({}),
    overlap: Overlap::Forbid, misfire: Misfire::Coalesce, reach: Reach::EachTenant, owner_route: Some("/companies"),
}) }
inventory::submit! { Contribution::new(schedule::EXECUTION_PRINCIPALS, KEY, provenance!(), &ExecutionPrincipal { principal: PRINCIPAL }) }
// A fresh walkthrough database's first newly created company is 101. Create
// and archive it through the real Company UI before enabling this proof.
inventory::submit! { Contribution::new(schedule::DEFINITIONS, LOCKED_KEY, provenance!(), &Definition {
    name: "Scheduled locked owner reference", expression: "* * * * *", timezone: "Etc/UTC",
    task_name: "Attempt an archived company update", worker: WORKER, args: || json!({"company_id": 101}),
    overlap: Overlap::Forbid, misfire: Misfire::Coalesce, reach: Reach::EachTenant, owner_route: Some("/companies"),
}) }
inventory::submit! { Contribution::new(schedule::EXECUTION_PRINCIPALS, LOCKED_KEY, provenance!(), &ExecutionPrincipal { principal: PRINCIPAL }) }

struct OwnerWorker {
    db: Db,
    schedule: Arc<Schedule>,
    companies: Arc<CompanyService>,
    parties: Arc<PartyProofs>,
}
impl OwnerWorker {
    async fn effect(&self, execution: &Execution, company_id: Option<i64>) -> Result<()> {
        let scope = execution.scope().ok_or(Error::Forbidden)?;
        // Keep the real lease fence while the ordinary Company facade opens
        // its own short transaction and checks its business permission.
        let mut client = self.db.get().await?;
        let fence = execution
            .begin(&mut client, "scheduled-reference.fence")
            .await?;
        let company = match company_id {
            Some(id) => id,
            None => self
                .parties
                .primary_organisation(fence.sql(), scope)
                .await?
                .ok_or(Error::NotFound)?
                .get(),
        };
        let (current, _) = bilibi_company::detail(fence.sql(), scope, company).await?;
        let value = format!("Scheduled reference job {}", execution.job_id);
        if current.legal_name.as_deref() != Some(value.as_str()) {
            self.companies
                .save_as(
                    scope,
                    &Edit {
                        id: company,
                        field: "legal_name".into(),
                        value,
                        version: current.version,
                    },
                )
                .await?;
        } else {
            // A replay of a committed effect is a no-op, but must still hold
            // the business permission. New effects always reach the facade.
            bilibi_authz::authorize_key(fence.sql(), scope, bilibi_company::UPDATE).await?;
        }
        println!(
            "scheduled owner mutation tenant={} job={} user={} principal={}",
            scope.tenant(),
            execution.job_id,
            scope.user(),
            scope.principal().unwrap_or("person")
        );
        fence.rollback().await
    }
}
impl Handler for OwnerWorker {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(self.schedule.run(execution, args, move |args| async move {
            match self
                .effect(execution, args.get("company_id").and_then(Value::as_i64))
                .await
            {
                Ok(()) => Outcome::Completed,
                Err(Error::Forbidden | Error::Unauthorized) => Outcome::Cancel("owner_refused"),
                Err(error) => Outcome::Retry(match error {
                    Error::Invalid => "owner_invalid",
                    Error::Conflict => "owner_conflict",
                    Error::NotFound => "owner_missing",
                    Error::Database(_) => "owner_database",
                    Error::Pool(_) => "owner_pool",
                    Error::Internal => "owner_internal",
                    _ => "owner_unavailable",
                }),
            }
        }))
    }
}
inventory::submit! { Module {
    id: MODULE, layer: Layer::Extensions,
    dependencies: &["base/authz", "base/database", "base/queue", "base/schedule", "base/tenancy", "core/company"],
    migrations: &[], services: &[Service { id: HANDLER, factory: |services| {
        Ok(Arc::new(HandlerService(Arc::new(OwnerWorker {
            db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
            schedule: services.get("base/schedule.store")?,
            companies: services.get(bilibi_company::SERVICE)?,
            parties: services.get("base/tenancy.parties")?,
        }))))
    }}],
} }

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("catalog") {
        return catalog::prove();
    }
    if std::env::var("BILIBI_DISPOSABLE_SCHEDULE_DEMO").as_deref() != Ok("yes") {
        return Err(Error::Invalid);
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_web::migrate(&db).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    let command = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    if command != "serve" {
        let tenant: i64 = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "1".into())
            .parse()
            .map_err(|_| Error::Invalid)?;
        let user = match tenant {
            1 => 4,
            2 => 5,
            _ => return Err(Error::Invalid),
        };
        let grants = app
            .services
            .get::<bilibi_authz::grants::Grants>("base/authz.grants")?;
        grants.reconcile().await?;
        let client = db.get().await?;
        let admin = app
            .services
            .get::<Authentication>("base/tenancy.authentication")?
            .authenticate(&**client, tenant, user, String::new())
            .await?;
        drop(client);
        match command.as_str() {
            "notifications" => notifications::prove(&app, &db, &admin).await?,
            "locked-owner" => {
                let id: i64 = std::env::args()
                    .nth(3)
                    .ok_or(Error::Invalid)?
                    .parse()
                    .map_err(|_| Error::Invalid)?;
                owner::locked(&app, &db, &admin, id).await?;
            }
            "grant-fan-out" if tenant == 1 => {
                for key in [schedule::EXECUTE, bilibi_tenancy::LIST] {
                    grants
                        .set_system(&admin, schedule::PRINCIPAL, key, Some(true))
                        .await?;
                }
            }
            "grant" => {
                for key in [
                    if tenant == 1 {
                        schedule::EXECUTE
                    } else {
                        schedule::TENANT_WORK
                    },
                    bilibi_company::VIEW,
                    bilibi_company::UPDATE,
                ] {
                    grants
                        .set_system(&admin, PRINCIPAL, key, Some(true))
                        .await?;
                }
                if tenant == 1 {
                    for key in [schedule::EXECUTE, bilibi_tenancy::LIST] {
                        grants
                            .set_system(&admin, schedule::PRINCIPAL, key, Some(true))
                            .await?;
                    }
                }
            }
            "revoke-business" => {
                grants
                    .set_system(&admin, PRINCIPAL, bilibi_company::UPDATE, None)
                    .await?
            }
            "deny-business" => {
                grants
                    .set_system(&admin, PRINCIPAL, bilibi_company::UPDATE, Some(false))
                    .await?
            }
            "revoke-consent" => {
                grants
                    .set_system(
                        &admin,
                        PRINCIPAL,
                        if tenant == 1 {
                            schedule::EXECUTE
                        } else {
                            schedule::TENANT_WORK
                        },
                        None,
                    )
                    .await?
            }
            "retry" => {
                let id: i64 = std::env::args()
                    .nth(3)
                    .ok_or(Error::Invalid)?
                    .parse()
                    .map_err(|_| Error::Invalid)?;
                let schedule = app.services.get::<Schedule>("base/schedule.store")?;
                let mut client = db.get().await?;
                let owned = schedule
                    .runs(&**client, &admin, Some(KEY), 100)
                    .await?
                    .iter()
                    .any(|run| run.job_id == Some(id) && run.tenant_id == Some(tenant));
                if !owned {
                    return Err(Error::NotFound);
                }
                let boundary = app
                    .services
                    .get::<bilibi_database::MutationBoundary>("base/database.mutations")?;
                let mut tx = boundary
                    .begin(
                        &mut client,
                        admin.mutation_context("scheduled-reference.retry"),
                    )
                    .await?;
                bilibi_authz::authorize_key(
                    tx.sql(),
                    &admin,
                    if tenant == 1 {
                        schedule::EXECUTE
                    } else {
                        schedule::TENANT_RUN
                    },
                )
                .await?;
                bilibi_queue::retry_for(&mut tx, &admin, id).await?;
                tx.commit().await?;
            }
            "invalid-grant" => {
                assert!(matches!(
                    grants
                        .set_system(&admin, PRINCIPAL, bilibi_company::DELETE, Some(true))
                        .await,
                    Err(Error::Invalid)
                ));
            }
            _ => return Err(Error::Invalid),
        }
        println!("{command} tenant={tenant}");
        return Ok(());
    }
    let listener =
        tokio::net::TcpListener::bind(std::env::var("BIND_ADDR").map_err(|_| Error::Invalid)?)
            .await
            .map_err(|_| Error::Internal)?;
    let executor =
        bilibi_queue::Executor::from_services(&app.services, bilibi_queue::Config::default())?;
    let scheduler =
        schedule::Scheduler::from_services(&app.services, schedule::scheduler::Config::default())?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(executor.run(stopped.clone()));
    let ticker = tokio::spawn(scheduler.run(stopped));
    let shutdown = stop.clone();
    println!("Scheduled owner reference at {origin}/system/schedule");
    let result = axum::serve(listener, bilibi_web::router(app))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = shutdown.send(true);
        })
        .await
        .map_err(|_| Error::Internal);
    let _ = stop.send(true);
    worker.await.map_err(|_| Error::Internal)??;
    ticker.await.map_err(|_| Error::Internal)??;
    result
}
