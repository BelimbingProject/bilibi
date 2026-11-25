//! Native import-result proof on an explicitly disposable database.
//! The reference owner is fixture-only; product composition has no reference routes.
#[path = "import-results/owner.rs"]
mod owner;
use bilibi_agent_api::{
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
    Registry,
};
use bilibi_base::{
    bulk_data::{BulkData, ExportState, Run, SERVICE},
    party_ids::OrganisationId,
};
use bilibi_database::{pool, Db};
use bilibi_queue::{Config, Executor};
use bilibi_tenancy::{Authentication, ProofFuture, Scope};
use serde_json::{json, Value};
use std::{os::unix::fs::DirBuilderExt, time::Duration};
use tokio::{sync::watch, task::JoinHandle};
use tokio_postgres::Transaction;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Failure = Box<dyn std::error::Error>;

struct Browser<'a> {
    app: &'a bilibi_web::App,
    token: &'a str,
    scope: Option<Scope>,
}
impl PlanAuthority for Browser<'_> {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            Ok(PlanIdentity {
                scope: match &self.scope {
                    Some(scope) => scope.clone(),
                    None => self.app.security.sessions.hold(tx, self.token).await?,
                },
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}

struct Person<'a> {
    plans: &'a Plans,
    registry: &'a Registry,
    bulk: &'a BulkData,
    browser: Browser<'a>,
    scope: Scope,
    db: &'a Db,
}
impl Person<'_> {
    /// Preview, then commit, one reviewed write.
    async fn write(&self, key: &str, input: Value) -> Result<Value, String> {
        let ops = [PlannedOperation {
            key: key.into(),
            input,
        }];
        let plan = self
            .plans
            .preview(&self.browser, &ops)
            .await
            .map_err(|error| error.to_string())?;
        if plan["valid"] != json!(true) {
            return Err(plan["results"][0]["error"].to_string());
        }
        self.plans
            .commit(&self.scope, &self.browser, &ops)
            .await
            .map(|result| result["results"][0]["result"]["result"].clone())
            .map_err(|error| error.to_string())
    }
    async fn read(&self, key: &str, input: Value) -> Result<Value, String> {
        let mut client = self.db.get().await.map_err(|error| error.to_string())?;
        self.registry
            .call(&mut **client, &self.scope, key, &input)
            .await
            .map(|out| out["result"].clone())
            .map_err(|error| error.to_string())
    }
    async fn upload(&self, kind: &str, name: &str, bytes: &[u8]) -> Result<i64, String> {
        match self.bulk.upload(&self.scope, kind, name, bytes).await {
            Ok(Ok(id)) => Ok(id),
            Ok(Err(why)) => Err(why),
            Err(error) => Err(error.to_string()),
        }
    }
    async fn upload_case(&self, cases: &Value, name: &str, kind: &str) -> Result<i64, String> {
        let case = &cases["files"][name];
        self.upload(
            kind,
            case["file_name"].as_str().ok_or("missing case filename")?,
            case["csv"].as_str().ok_or("missing case CSV")?.as_bytes(),
        )
        .await
    }
    async fn run(&self, id: i64) -> Run {
        let client = self.db.get().await.expect("database");
        self.bulk
            .run(&**client, &self.scope, id)
            .await
            .expect("run")
    }
    /// Waits until no job works on the run.
    async fn settle(&self, id: i64) -> Run {
        for _ in 0..1200 {
            let run = self.run(id).await;
            if !run.state.working() || run.stopped {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("run {id} did not settle");
    }
    async fn pass(&self, key: &str, id: i64, mapping: &Value) -> Result<Run, String> {
        let version = self.run(id).await.version;
        self.write(
            key,
            json!({"run_id": id, "version": version, "mapping": mapping}),
        )
        .await?;
        Ok(self.settle(id).await)
    }
}
fn start(
    app: &bilibi_web::App,
    lease: i32,
) -> Result<(watch::Sender<bool>, JoinHandle<()>), Failure> {
    let executor = Executor::from_services(
        &app.services,
        Config {
            concurrency: 4,
            poll_interval: Duration::from_millis(100),
            lease_seconds: lease,
            max_runtime: Duration::from_secs(600),
        },
    )?;
    let (stop, receiver) = watch::channel(false);
    let handle = tokio::spawn(async move {
        let _ = executor.run(receiver).await;
    });
    Ok((stop, handle))
}

fn require(ok: bool, label: &str) -> Result<(), Failure> {
    if !ok {
        return Err(format!("FAILED: {label}").into());
    }
    println!("PASS: {label}");
    Ok(())
}

async fn settle_export(db: &Db, bulk: &BulkData, scope: &Scope, id: i64) -> Result<(), Failure> {
    for _ in 0..600 {
        let client = db.get().await?;
        let export = bulk.export(&**client, scope, id).await?;
        if export.state != ExportState::Running {
            return require(
                export.state == ExportState::Completed,
                "the export completed",
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("export did not settle".into())
}

async fn company_scope_proof(
    app: &bilibi_web::App,
    db: &Db,
    bulk: &BulkData,
    plans: &Plans,
    registry: &Registry,
) -> Result<(), Failure> {
    let seed_ids = {
        let sql = db.get().await?;
        sql.batch_execute(
            "INSERT INTO persons(tenant_id,code,name,kind) VALUES(1,'grid_company_staff','Grid company staff','human') ON CONFLICT (tenant_id,code) DO NOTHING;
         UPDATE users SET person_id=(SELECT id FROM persons WHERE tenant_id=1 AND code='grid_company_staff') WHERE tenant_id=1 AND id=3;
         INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,company FROM users CROSS JOIN (VALUES(1),(2)) c(company) WHERE tenant_id=1 AND id=3 ON CONFLICT DO NOTHING;
         INSERT INTO role_capabilities(tenant_id,role_id,capability) VALUES
           (1,3,'admin.import.view'),(1,3,'admin.import.create'),
           (1,3,'admin.person.import'),(1,3,'admin.person.export') ON CONFLICT DO NOTHING;
         INSERT INTO reference_import_records(tenant_id,company_id,code) VALUES
           (1,1,'Company A seed'),(1,2,'Company B seed') ON CONFLICT DO NOTHING;",
        )
        .await?;
        let role: i64 = sql
            .query_one(
                "INSERT INTO roles(tenant_id,name,code,description,company_id) VALUES(1,'Company A records','company_a_records','Walkthrough company grant',1) RETURNING id",
                &[],
            )
            .await?
            .get(0);
        sql.execute(
            "INSERT INTO role_capabilities(tenant_id,role_id,capability) VALUES(1,$1,'reference.import-results.import'),(1,$1,'reference.import-results.view') ON CONFLICT DO NOTHING",
            &[&role],
        )
        .await?;
        sql.execute(
            "INSERT INTO principal_roles(tenant_id,user_id,role_id,company_id) VALUES(1,3,$1,1) ON CONFLICT DO NOTHING",
            &[&role],
        )
        .await?;
        sql.query(
            "SELECT id::text FROM reference_import_records WHERE tenant_id=1 AND ((company_id=1 AND code='Company A seed') OR (company_id=2 AND code='Company B seed'))",
            &[],
        )
        .await?
        .into_iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>()
    };
    require(
        seed_ids.len() == 2,
        "both companies have their seeded record",
    )?;

    let login = app
        .user
        .login("staff@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let company_a = app.security.guard(&login.token).await?;
    let auth = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let company_b = {
        let sql = db.get().await?;
        auth.reprove_work_company(&**sql, &company_a, OrganisationId::new(2)?)
            .await?
    };
    let person = |scope: Scope| Person {
        plans,
        registry,
        bulk,
        browser: Browser {
            app,
            token: &login.token,
            scope: Some(scope.clone()),
        },
        scope,
        db,
    };
    let a = person(company_a);
    let b = person(company_b);

    let a_rows = a
        .read(
            "reference.import_results.list",
            json!({"page":1,"page_size":100}),
        )
        .await?;
    require(
        a_rows.to_string().contains("Company A seed")
            && !a_rows.to_string().contains("Company B seed"),
        "the owner list reads company A and excludes company B",
    )?;
    require(
        b.read(
            "reference.import_results.list",
            json!({"page":1,"page_size":100}),
        )
        .await
        .is_err(),
        "the same agent read is refused in company B without its company grant",
    )?;
    let b_catalog = {
        let sql = db.get().await?;
        bilibi_grid::Catalog::for_scope(&**sql, &b.scope).await?
    };
    require(
        b_catalog.table("reference_import_records").is_none(),
        "the company B Grid catalog omits a table granted only in company A",
    )?;

    let run = a
        .upload(
            owner::TYPE,
            "company-a.csv",
            b"Code,company_id\r\nImported through A,2\r\n",
        )
        .await?;
    let imported = a
        .pass(
            "base.bulk_data.import.start",
            run,
            &json!({"fields":[{"field":"code","column":"Code"}]}),
        )
        .await?;
    let imported_into_a: bool = {
        let sql = db.get().await?;
        sql.query_one(
            "SELECT company_id=1 FROM reference_import_records WHERE tenant_id=1 AND code='Imported through A'",
            &[],
        )
        .await?
        .get(0)
    };
    require(
        imported.run.create == 1 && imported_into_a,
        "an import payload cannot replace the selected company",
    )?;
    require(
        b.upload(owner::TYPE, "company-b.csv", b"Code\r\nForbidden B\r\n")
            .await
            .is_err(),
        "Bulk Data import is refused in company B without its company grant",
    )?;

    require(
        a.write(
            "base.bulk_data.export.start",
            json!({"record_type":owner::TYPE,"filter":"{}","columns":["code"],"company_id":2}),
        )
        .await
        .is_err(),
        "an export input cannot select another company",
    )?;
    let export = a
        .write(
            "base.bulk_data.export.start",
            json!({"record_type":owner::TYPE,"filter":"{}","columns":["code"]}),
        )
        .await?;
    let export_id = export["id"].as_i64().ok_or("missing export id")?;
    settle_export(db, bulk, &a.scope, export_id).await?;
    let (_, file) = bulk.export_file(&a.scope, export_id).await?;
    let file = String::from_utf8(file)?;
    require(
        file.contains("Company A seed")
            && file.contains("Imported through A")
            && !file.contains("Company B seed"),
        "an export reads only the selected company",
    )?;
    require(
        b.write(
            "base.bulk_data.export.start",
            json!({"record_type":owner::TYPE,"filter":"{}","columns":["code"]}),
        )
        .await
        .is_err(),
        "Bulk Data export is refused in company B without its company grant",
    )?;

    let selected = a
        .write(
            "base.bulk_data.export.start",
            json!({"record_type":owner::TYPE,"ids":seed_ids,"columns":["code"]}),
        )
        .await?;
    let selected_id = selected["id"]
        .as_i64()
        .ok_or("missing selected export id")?;
    settle_export(db, bulk, &a.scope, selected_id).await?;
    let (_, file) = bulk.export_file(&a.scope, selected_id).await?;
    let file = String::from_utf8(file)?;
    require(
        file.contains("Company A seed") && !file.contains("Company B seed"),
        "Grid projects only the company A record from the two selected IDs",
    )?;

    for (person, name) in [(&a, "Tenant wide from A"), (&b, "Tenant wide from B")] {
        let tenant_run = person
            .upload(
                "core/person.contacts",
                "tenant-wide.csv",
                format!("Name,Kind,company_id\r\n{name},human,1\r\n").as_bytes(),
            )
            .await?;
        let tenant_import = person
            .pass(
                "base.bulk_data.import.start",
                tenant_run,
                &json!({"fields":[{"field":"name","column":"Name"},{"field":"kind","column":"Kind"}]}),
            )
            .await?;
        require(
            tenant_import.run.create == 1,
            "a tenant-wide Bulk Data import still works unchanged",
        )?;
        let tenant_export = person
            .write(
                "base.bulk_data.export.start",
                json!({"record_type":"core/person.contacts","filter":json!({"search":name}).to_string(),"columns":["name"]}),
            )
            .await?;
        let tenant_export_id = tenant_export["id"]
            .as_i64()
            .ok_or("missing tenant export id")?;
        settle_export(db, bulk, &person.scope, tenant_export_id).await?;
        let (_, file) = bulk.export_file(&person.scope, tenant_export_id).await?;
        require(
            String::from_utf8(file)?.contains(name),
            "a tenant-wide Grid source and export still work unchanged",
        )?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_BULK_DATA_DEMO").as_deref() != Ok("disposable") {
        return Err("explicit disposable database required".into());
    }
    bilibi_base::link();
    bilibi_core::link();
    println!("{}", bilibi_web::status());
    println!("REFERENCE_OWNER=reference/import_results (fixture-only, same source commit)");
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_web::migrate(&db).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let origin = std::env::var("APP_ORIGIN")?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let bulk = app.services.get::<BulkData>(SERVICE)?;
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let sql = db.get().await?;
    let root = std::env::current_dir()?.join("data/artifacts");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    sql.execute("INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES('artifacts.storage_root',0,'global',0,$1,false) ON CONFLICT (key,tenant_id,scope_type,scope_id) DO NOTHING", &[&json!(root)]).await?;
    let (stop, worker) = start(&app, 3)?;
    if std::env::args().any(|arg| arg == "--serve") {
        let listener = tokio::net::TcpListener::bind(std::env::var("BIND_ADDR")?).await?;
        println!("reference application ready");
        axum::serve(listener, bilibi_web::staff_router(app))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        stop.send(true)?;
        worker.await?;
        return Ok(());
    }
    if std::env::args().any(|arg| arg == "--company-scope") {
        drop(sql);
        company_scope_proof(&app, &db, &bulk, &plans, &registry).await?;
        stop.send(true)?;
        worker.await?;
        return Ok(());
    }
    // Case inputs stay outside source and publication evidence. The oracle
    // below fixes each scenario's expected business effects, not its data.
    let cases: Value = serde_json::from_slice(&std::fs::read(std::env::var(
        "BILIBI_IMPORT_RESULTS_CASES",
    )?)?)?;
    let race_code = cases["codes"]["race"].as_str().ok_or("missing race code")?;
    let poison_code = cases["codes"]["poisoned"]
        .as_str()
        .ok_or("missing poison code")?;
    let lease_code = cases["codes"]["lease"]
        .as_str()
        .ok_or("missing lease code")?;
    let retained_code = cases["codes"]["retained"]
        .as_str()
        .ok_or("missing retained code")?;
    let cancelled_code = cases["codes"]["cancelled"]
        .as_str()
        .ok_or("missing cancelled code")?;
    let login = app
        .user
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let scope = app.security.guard(&login.token).await?;
    let admin = Person {
        plans: &plans,
        registry: &registry,
        bulk: &bulk,
        browser: Browser {
            app: &app,
            token: &login.token,
            scope: None,
        },
        scope,
        db: &db,
    };
    let mapping = json!({"fields":[{"field":"code","column":"Code"},{"field":"mode","column":"Mode"}],"source_key":"Code"});
    let run = admin.upload_case(&cases, "outcomes", owner::TYPE).await?;
    let checked = admin
        .pass("base.bulk_data.import.preview", run, &mapping)
        .await?;
    require(
        checked.preview.create == 4 && checked.preview.error == 1,
        "preview interprets owner results and actionable refusal",
    )?;
    require(
        sql.query_one("SELECT count(*) FROM reference_import_records", &[])
            .await?
            .get::<_, i64>(0)
            == 0,
        "preview writes no owner records",
    )?;
    admin
        .write("reference.import_results.create", json!({"code":race_code}))
        .await?;
    let duplicate_check = admin
        .upload_case(&cases, "duplicate_preview", owner::TYPE)
        .await?;
    let duplicate_check = admin
        .pass("base.bulk_data.import.preview", duplicate_check, &mapping)
        .await?;
    require(
        duplicate_check.preview.action == 1 && duplicate_check.preview.create == 0,
        "preview preserves the owner's non-create business outcome",
    )?;
    let imported = admin
        .pass("base.bulk_data.import.start", run, &mapping)
        .await?;
    require(
        imported.run.create == 2
            && imported.run.action == 1
            && imported.run.error == 2
            && imported.progress() == 5,
        "commit result wins over checked create; refusal rows advance",
    )?;
    let rows = admin
        .read(
            "base.bulk_data.import.rows",
            json!({"run_id":run,"pass":"run"}),
        )
        .await?;
    require(
        rows[0]["outcome"] == "action"
            && rows[0]["links"][0]["label"]
                .as_str()
                .is_some_and(|s| s.starts_with("Enquiry task ")),
        "duplicate business action preserves task link without false create",
    )?;
    require(
        rows[2]["message"] == "Supply a usable contact before importing."
            && rows[3]["message"] == "Owner refused after its mutation; fix this row.",
        "owner refusal messages survive",
    )?;
    require(
        sql.query_one(
            "SELECT count(*) FROM reference_import_records WHERE code=$1",
            &[&poison_code],
        )
        .await?
        .get::<_, i64>(0)
            == 0,
        "poisoned owner mutation rolled back before row failure was recorded",
    )?;
    require(sql.query_one("SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type='reference/import_results.record' AND trace_id=$1 AND new_values->>'record' NOT IN (SELECT id::text FROM reference_import_records)",&[&imported.trace_id]).await?.get::<_,i64>(0)==0,"refusal leaves no orphan owner audit evidence")?;
    require(sql.query_one("SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type IN ('reference/import_results.record','reference/import_results.task') AND trace_id=$1",&[&imported.trace_id]).await?.get::<_,i64>(0)==3,"only three successful owner mutations share the row-report trace")?;
    println!("RESULT_RUN={run}");
    println!("RESULT_ROWS={rows}");

    owner::RETRY.store(true, std::sync::atomic::Ordering::SeqCst);
    let retried = admin.upload_case(&cases, "retry", owner::TYPE).await?;
    let result = admin
        .pass("base.bulk_data.import.start", retried, &mapping)
        .await?;
    require(
        result.run.action == 1 && result.run.create == 2 && result.run.error == 0,
        "genuine backend failure retries rather than becoming a failed row",
    )?;
    let attempts: i32 = sql
        .query_one(
            "SELECT attempt FROM base_queue_jobs WHERE tenant_id=1 AND id=$1",
            &[&result.job_id],
        )
        .await?
        .get(0);
    require(attempts >= 2, "backend failure retained Queue retry")?;
    let tasks: i64 = sql
        .query_one("SELECT count(*) FROM reference_import_tasks", &[])
        .await?
        .get(0);
    require(
        tasks == 2,
        "next_row fence prevents repeating an already committed enquiry task",
    )?;

    // Block current() after Execution::begin has fenced the lease. Queue's
    // heartbeat cannot renew the locked job, so the attempt is dropped.
    stop.send(true)?;
    worker.await?;
    let leased = admin.upload_case(&cases, "lease", owner::TYPE).await?;
    admin
        .write(
            "base.bulk_data.import.start",
            json!({"run_id":leased,"version":admin.run(leased).await.version,"mapping":mapping}),
        )
        .await?;
    let mut lock = db.get().await?;
    let held = lock.transaction().await?;
    held.query_one(
        "SELECT id FROM base_bulk_data_runs WHERE tenant_id=1 AND id=$1 FOR UPDATE",
        &[&leased],
    )
    .await?;
    let (stop, worker) = start(&app, 3)?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    require(
        sql.query_one(
            "SELECT count(*) FROM reference_import_records WHERE code=$1",
            &[&lease_code],
        )
        .await?
        .get::<_, i64>(0)
            == 0,
        "lease contention commits no owner mutation",
    )?;
    held.rollback().await?;
    let result = admin.settle(leased).await;
    let attempts: i32 = sql
        .query_one(
            "SELECT attempt FROM base_queue_jobs WHERE tenant_id=1 AND id=$1",
            &[&result.job_id],
        )
        .await?
        .get(0);
    require(
        attempts >= 2 && result.run.create == 1 && result.run.error == 0,
        "lost lease retries through the original fenced runner",
    )?;

    for case in ["invalid_result", "invalid_link"] {
        let name = cases["files"][case]["code"]
            .as_str()
            .ok_or("missing rejected case code")?;
        let id = admin.upload_case(&cases, case, owner::TYPE).await?;
        admin
            .write(
                "base.bulk_data.import.start",
                json!({"run_id":id,"version":admin.run(id).await.version,"mapping":mapping}),
            )
            .await?;
        // Observe a retry, then cancel through Base's normal operation.
        for _ in 0..600 {
            let r = admin.run(id).await;
            let failed:bool=sql.query_one("SELECT EXISTS(SELECT 1 FROM base_queue_jobs WHERE tenant_id=1 AND id=$1 AND state='retryable')",&[&r.job_id]).await?.get(0);
            if failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let r = admin.run(id).await;
        require(
            r.progress() == 0 && r.run.done() == 0,
            "invalid projection cannot commit owner mutation or report success",
        )?;
        require(
            sql.query_one(
                "SELECT count(*) FROM reference_import_records WHERE code=$1",
                &[&name],
            )
            .await?
            .get::<_, i64>(0)
                == 0,
            "projection rejection rolls back owner row",
        )?;
        admin
            .write(
                "base.bulk_data.import.cancel",
                json!({"run_id":id,"version":r.version}),
            )
            .await?;
    }
    owner::RETRY.store(true, std::sync::atomic::Ordering::SeqCst);
    let cancelled = admin.upload_case(&cases, "cancel", owner::TYPE).await?;
    admin.write("base.bulk_data.import.start",json!({"run_id":cancelled,"version":admin.run(cancelled).await.version,"mapping":mapping})).await?;
    for _ in 0..600 {
        let r = admin.run(cancelled).await;
        let failed:bool=sql.query_one("SELECT EXISTS(SELECT 1 FROM base_queue_jobs WHERE tenant_id=1 AND id=$1 AND state='retryable')",&[&r.job_id]).await?.get(0);
        if failed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = admin.run(cancelled).await;
    require(
        r.progress() == 1 && r.run.create == 1,
        "backend retry leaves earlier committed row and next_row intact",
    )?;
    admin
        .write(
            "base.bulk_data.import.cancel",
            json!({"run_id":cancelled,"version":r.version}),
        )
        .await?;
    require(
        sql.query_one(
            "SELECT count(*) FROM reference_import_records WHERE code=$1",
            &[&retained_code],
        )
        .await?
        .get::<_, i64>(0)
            == 1
            && sql
                .query_one(
                    "SELECT count(*) FROM reference_import_records WHERE code=$1",
                    &[&cancelled_code],
                )
                .await?
                .get::<_, i64>(0)
                == 0,
        "cancel retains earlier committed rows without committing pending row",
    )?;

    // An ordinary contributor still uses its original top-level id/outcome.
    let ordinary = admin
        .upload_case(&cases, "ordinary", "core/person.contacts")
        .await?;
    let result=admin.pass("base.bulk_data.import.start",ordinary,&json!({"fields":[{"field":"name","column":"Name"},{"field":"kind","column":"Kind"},{"field":"email","column":"Email"}]})).await?;
    let rows = admin
        .read("base.bulk_data.import.rows", json!({"run_id":ordinary}))
        .await?;
    require(
        result.run.create == 1
            && result.run.action == 0
            && rows[0]["record_id"].is_string()
            && rows[0]["links"] == json!([]),
        "ordinary contributor preserves create/id result",
    )?;
    stop.send(true)?;
    worker.await?;
    Ok(())
}
