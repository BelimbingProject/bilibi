use super::Result;
use bilibi_agent_api::job::JobContracts;
use bilibi_database::{Db, MutationBoundary};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance, Service};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_privacy::{NewRequest, Privacy, RequestKind, REQUEST_VIEW};
use bilibi_queue::{
    Config, Execution, Executor, Handler, HandlerBinding, HandlerFuture, HandlerService, Outcome,
    Queue, State, Worker,
};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

const MODULE: &str = "extensions/privacy_job_read_walkthrough";
const WORKER: &str = "privacy-job-read-walkthrough/locate";
const SERVICE: &str = "extensions/privacy_job_read_walkthrough.worker";
const SOURCE: Provenance = Provenance {
    module: MODULE,
    source: file!(),
    line: line!(),
};

struct LocateWorker {
    db: Arc<Db>,
    contracts: Arc<JobContracts>,
}

impl Handler for LocateWorker {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let Ok(mut connection) = self.db.get().await else {
                return Outcome::Cancel("no_connection");
            };
            assert_eq!(self.db.status().max_size, 1);
            assert_eq!(self.db.status().available, 0);
            let result = self
                .contracts
                .read(
                    &mut **connection,
                    execution,
                    "base.privacy.request.locate",
                    &args,
                )
                .await;
            match result {
                Ok(answer)
                    if answer["result"]["stores"].as_array().is_some_and(|stores| {
                        stores.iter().any(|store| {
                            store["store"] == "core/person.people"
                                && store["kinds"].as_array().is_some_and(|kinds| {
                                    kinds.iter().any(|kind| {
                                        kind["kind"] == "person"
                                            && kind["by_person"] == 1
                                            && kind["by_identifier"] == 0
                                    })
                                })
                        })
                    }) =>
                {
                    Outcome::Completed
                }
                Err(error) => {
                    eprintln!("Privacy locate job failed: {error}");
                    Outcome::Cancel("locate_failed")
                }
                _ => Outcome::Cancel("incorrect_locate"),
            }
        })
    }
}

inventory::submit! { Module {
    id: MODULE, layer: Layer::Extensions,
    dependencies: &["base/agent_api", "base/database", "base/privacy", "base/queue"],
    migrations: &[], services: &[Service { id: SERVICE, factory: |services| {
        Ok(Arc::new(HandlerService(Arc::new(LocateWorker {
            db: services.get("base/database.pool")?,
            contracts: services.get(bilibi_agent_api::job::SERVICE)?,
        }))))
    }}],
} }

inventory::submit! { Contribution::new(bilibi_queue::WORKERS, WORKER, SOURCE, &Worker {
    max_attempts: 1,
    validate: |args| if args["id"].as_i64().is_some_and(|id| id > 0) { Ok(args) } else { Err(bilibi_database::Error::Invalid) },
}) }

inventory::submit! { Contribution::new(bilibi_queue::HANDLERS, WORKER, SOURCE, &HandlerBinding {
    service: SERVICE, capability: REQUEST_VIEW,
}) }

pub(super) async fn run(
    app: &bilibi_web::App,
    db: &Db,
    scope: &Scope,
    privacy: &Privacy,
    boundary: &MutationBoundary,
) -> Result<()> {
    let queue = app.services.get::<Queue>(bilibi_queue::SERVICE)?;
    let mut client = db.get().await?;
    super::case_files::install(&client, app.services.get("base/media.store")?).await?;
    let request = privacy
        .receive(
            &mut client,
            scope,
            NewRequest {
                kind: RequestKind::Access,
                person: PersonId::new(1)?,
                company: OrganisationId::new(1)?,
                identifiers: Vec::new(),
            },
        )
        .await?;
    let mut tx = boundary
        .begin(
            &mut client,
            scope.mutation_context("privacy.walkthrough.locate"),
        )
        .await?;
    let id = queue
        .enqueue_for(&mut tx, scope, WORKER, json!({"id": request.id}))
        .await?
        .id();
    tx.commit().await?;
    drop(client);

    let executor = Executor::from_services(
        &app.services,
        Config {
            concurrency: 1,
            ..Config::default()
        },
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(executor.run(stopped));
    let status = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = {
                let client = db.get().await?;
                bilibi_queue::lookup(&**client, scope.tenant(), id).await?
            };
            if status.state.terminal() {
                break Ok::<_, bilibi_database::Error>(status);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    let _ = stop.send(true);
    worker.await??;
    let status = status??;
    assert_eq!(
        status.state,
        State::Completed,
        "the job must locate one directly linked person with its only pool slot: {:?}",
        status.failure_code
    );
    println!("PASS Privacy job located the request's person on the only pooled connection");
    Ok(())
}
