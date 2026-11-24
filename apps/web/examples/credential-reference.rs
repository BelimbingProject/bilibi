//! Running proof for contributed bearer routes. The reference module owns a
//! failing public `/survey/{token}` page so Web's log, debug report and
//! response policy can be inspected without a real credential or a mounted
//! Domain.
//!
//! `--refusals` adds a staff token route without its `CredentialPath` and
//! exits non-zero unless catalog discovery refuses it. The same rule for a
//! public page or link is in `public-site-reference --refusals`.
use bilibi_base::public::{PublicPage, PublicScope, SenderBudget, PUBLIC_PAGES};
use bilibi_database::Error;
use bilibi_module_registry::{
    contributions::{
        Access, Catalog, CompanyScope, CredentialPath, Method, Route, CREDENTIAL_PATHS, ROUTES,
    },
    Consumer, Contribution, Layer, Module, Provenance, Service,
};
use bilibi_ui::page::{Page, PageFuture, PageRequest};
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
impl Page<PublicScope, Error> for Reference {
    fn render<'a>(&'a self, _request: PageRequest<'a, PublicScope>) -> PageFuture<'a, Error> {
        Box::pin(async { Err(Error::Internal) })
    }
}

inventory::submit! {
    Module {
        id: MODULE,
        layer: Layer::Extensions,
        dependencies: &["base/ui", "base/public"],
        migrations: &[],
        services: &[Service {
            id: PAGES,
            factory: |_| Ok(Arc::new(bilibi_ui::page::PageService::<PublicScope, Error>(Arc::new(Reference)))),
        }],
    }
}
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "credential_reference.survey", provenance!(), &CredentialPath { path: "/survey/{token}" }) }
inventory::submit! { Contribution::new(PUBLIC_PAGES, "credential_reference.survey", provenance!(), &PublicPage {
    path: "/survey/{token}",
    service: PAGES,
    operation: "answer",
    budget: SenderBudget { per_minute: 60, per_hour: 600 },
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
        access: Access::SignedIn,
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
    let app = bilibi_web::App::new(db, &origin)?.at_public_site(&origin, 1);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| Error::Internal)?;
    println!("Credential reference listening at {origin}");
    axum::serve(listener, bilibi_web::public_router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|_| Error::Internal)?;
    Ok(std::process::ExitCode::SUCCESS)
}
