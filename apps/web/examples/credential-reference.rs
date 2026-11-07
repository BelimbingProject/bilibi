//! Running proof for contributed bearer routes. The reference module owns a
//! failing `/survey/{token}` page so Web's log, debug report and response
//! policy can be inspected without a real credential or a mounted Domain.
//!
//! `--refusals` adds a second token route without its `CredentialPath` and
//! exits non-zero unless catalog discovery refuses it.
use bilibi_database::Error;
use bilibi_module_registry::{
    contributions::{
        Access, Catalog, CompanyScope, CredentialPath, Method, Route, CREDENTIAL_PATHS, ROUTES,
    },
    Consumer, Contribution, Layer, Module, Provenance, Service,
};
use bilibi_security::Scope;
use bilibi_ui::page::{Page, PageFuture, PageRequest, PageService};
use std::sync::Arc;

const MODULE: &str = "extensions/credential_reference";
const PAGES: &str = "extensions/credential_reference.pages";

macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

struct Reference;
impl Page<Scope, Error> for Reference {
    fn render<'a>(&'a self, _request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async { Err(Error::Internal) })
    }
}

inventory::submit! {
    Module {
        id: MODULE,
        layer: Layer::Extensions,
        dependencies: &["base/ui", "base/security"],
        migrations: &[],
        services: &[Service {
            id: PAGES,
            factory: |_| Ok(Arc::new(PageService::<Scope, Error>(Arc::new(Reference)))),
        }],
    }
}
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "credential_reference.survey", provenance!(), &CredentialPath { path: "/survey/{token}" }) }
inventory::submit! { Contribution::new(ROUTES, "credential_reference.survey", provenance!(), &Route {
    company_scope: CompanyScope::Tenant,
    method: Method::Get,
    path: "/survey/{token}",
    service: PAGES,
    operation: "answer",
    access: Access::Public,
    framed: false,
}) }

static FORGOTTEN: Contribution = Contribution::new(
    ROUTES,
    "credential_reference.forgotten",
    provenance!(),
    &Route {
        company_scope: CompanyScope::Tenant,
        method: Method::Get,
        path: "/forgotten/{token}",
        service: PAGES,
        operation: "forgotten",
        access: Access::Public,
        framed: false,
    },
);

fn refusal() -> std::process::ExitCode {
    let modules = match bilibi_web::modules() {
        Ok(modules) => modules,
        Err(error) => {
            eprintln!("UNEXPECTED refusal of valid catalog: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let consumers = inventory::iter::<Consumer>.into_iter().collect();
    let mut contributions: Vec<&Contribution> =
        inventory::iter::<Contribution>.into_iter().collect();
    contributions.push(&FORGOTTEN);
    match Catalog::build(&modules, consumers, contributions) {
        Err(error) => {
            println!("refused token route without credential path: {error}");
            std::process::ExitCode::SUCCESS
        }
        Ok(_) => {
            eprintln!("UNEXPECTEDLY accepted token route without credential path");
            std::process::ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn main() -> Result<std::process::ExitCode, Error> {
    if std::env::args().any(|arg| arg == "--refusals") {
        return Ok(refusal());
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let bind = std::env::var("BIND_ADDR").map_err(|_| Error::Invalid)?;
    let app = bilibi_web::App::new(db, &origin)?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| Error::Internal)?;
    println!("Credential reference listening at {origin}");
    axum::serve(listener, bilibi_web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|_| Error::Internal)?;
    Ok(std::process::ExitCode::SUCCESS)
}
