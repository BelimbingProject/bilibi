//! Reference owner for the import-results walkthrough; linked only by this example.
use bilibi_agent_api::{
    json::{Data, Output, Page},
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, ReadHandler,
    WriteHandler, DEFINITIONS,
};
use bilibi_authz::{CapabilityDefinition as C, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_base::bulk_data::{
    Export, Import, ImportOutcome, ImportResult, ImportResults, RecordType, ResultLink,
    IMPORT_RESULTS, RECORD_TYPES,
};
use bilibi_database::{Change, Error, MutationTransaction};
use bilibi_grid::{Field, FieldType, GridContribution, Source, TableDecl};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

const MODULE: &str = "reference/import_results";
pub const TYPE: &str = "reference/import_results.records";
const CAPABILITY: &str = "reference.import-results.import";
const VIEW_CAPABILITY: &str = "reference.import-results.view";
pub static RETRY: AtomicBool = AtomicBool::new(false);
macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! { Module {
    id: MODULE, layer: Layer::Extensions,
    dependencies: &["base/agent_api", "base/authz", "base/bulk_data", "base/database", "base/grid", "base/module_registry", "base/tenancy", "base/ui"],
    migrations: &[("reference-import-results-001-records", "CREATE TABLE reference_import_records(id bigserial PRIMARY KEY, tenant_id bigint NOT NULL, company_id bigint NOT NULL, code text NOT NULL, UNIQUE(tenant_id,company_id,code)); CREATE TABLE reference_import_tasks(id bigserial PRIMARY KEY,tenant_id bigint NOT NULL,record_id bigint NOT NULL REFERENCES reference_import_records(id));")], services: &[bilibi_module_registry::Service { id: "reference/import_results.pages", factory: |services| Ok(std::sync::Arc::new(bilibi_ui::page::PageService::<Scope, Error>(std::sync::Arc::new(Pages(services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone()))))) }],
} }
inventory::submit! { Contribution::new(VOCABULARY, MODULE, provenance!(), &VocabularyContribution {
    domains: &["reference"], verbs: &[], capabilities: &[C::company(CAPABILITY), C::company(VIEW_CAPABILITY)],
    roles: &[RoleDefinition { code: "manager", name: "manager", description: "Import result walkthrough", grant_all: false, capabilities: &[CAPABILITY, VIEW_CAPABILITY] }], system_principals: &[],
}) }
static GRID: GridContribution = GridContribution {
    record_fields: &[],
    tables: &[TableDecl::new(
        "reference_import_records",
        "Result reference records",
        VIEW_CAPABILITY,
        Source::company("SELECT id,company_id,code FROM reference_import_records WHERE tenant_id=$1 AND company_id=$3 AND $2::bigint IS NOT NULL"),
        "id",
        &[
            Field::new("id", "ID", FieldType::Integer).non_personal(),
            Field::new("company_id", "Company", FieldType::Integer)
                .hidden()
                .non_personal(),
            Field::new("code", "Code", FieldType::String).non_personal(),
        ],
    )
    .label_field("code")],
    links: &[],
};
bilibi_grid::contribute!(
    "reference/import_results",
    "reference.import_results.grid",
    &GRID
);
inventory::submit! { Contribution::new(RECORD_TYPES, TYPE, provenance!(), &RecordType {
    label: "Result reference records", table: "reference_import_records", path: Some("/reference/import-records/{id}"),
    import: Import { capability: CAPABILITY, create: "reference.import_results.create", id: "id", duplicates: None, references: &[] },
    export: Some(Export { capability: CAPABILITY, list: "reference.import_results.list" }), bulk_update: None,
}) }

fn interpret(value: &Value) -> bilibi_database::Result<ImportResult> {
    let outcome = match value["outcome"].as_str() {
        Some("created") => ImportOutcome::Create,
        Some("enquiry") => ImportOutcome::Action,
        _ => return Err(Error::Internal),
    };
    let record_id = value["id"].as_i64().map(|id| id.to_string());
    let mut links = Vec::new();
    if let Some(task) = value["task"].as_i64() {
        links.push(ResultLink {
            label: format!("Enquiry task {task}"),
            path: format!("/reference/import-tasks/{task}"),
        });
    }
    if value["bad_link"] == true {
        links.push(ResultLink {
            label: "Unsafe".into(),
            path: "//example.test".into(),
        });
    }
    Ok(ImportResult {
        outcome,
        record_id,
        message: Some(
            if value["outcome"] == "enquiry" {
                "Another enquiry on the existing record."
            } else {
                "New record."
            }
            .into(),
        ),
        links,
    })
}
inventory::submit! { Contribution::new(IMPORT_RESULTS, TYPE, provenance!(), &ImportResults { interpret }) }

fn input() -> Value {
    json!({"type":"object","properties":{"code":{"type":"string"},"mode":{"type":"string","enum":["normal","refuse","poison","retry","bad-result","bad-link"]}},"required":["code"]})
}
fn output() -> Value {
    json!({"type":"object"})
}
fn list_input() -> Value {
    json!({"type":"object","properties":{"page":{"type":"integer"},"page_size":{"type":"integer"}}})
}
fn list_output() -> Value {
    json!({"type":"array"})
}
struct List;
impl ReadHandler for List {
    fn owner(&self) -> &'static str {
        MODULE
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_proof(client, scope, VIEW_CAPABILITY).await?;
            let company = scope.current_company().ok_or(Error::Forbidden)?.get();
            let page = input["page"].as_i64().unwrap_or(1).max(1);
            let page_size = input["page_size"].as_i64().unwrap_or(100).clamp(1, 100);
            let offset = (page - 1) * page_size;
            let rows = client
                .query(
                    "SELECT id,code,count(*) OVER() AS total FROM reference_import_records WHERE tenant_id=$1 AND company_id=$2 ORDER BY id LIMIT $3 OFFSET $4",
                    &[&scope.tenant(), &company, &page_size, &offset],
                )
                .await?;
            let total = rows.first().map_or(0, |row| row.get::<_, i64>(2));
            Ok(Output::Page(Page {
                entries: Data::Json(Value::Array(
                    rows.iter()
                        .map(|row| json!({"id": row.get::<_, i64>(0), "code": row.get::<_, String>(1)}))
                        .collect(),
                )),
                page: page as u64,
                page_size: page_size as u64,
                total: total as u64,
            }))
        })
    }
}
struct Owner;
impl WriteHandler for Owner {
    fn owner(&self) -> &'static str {
        MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_proof(client, scope, CAPABILITY).await?;
            if input["mode"] == "refuse" {
                return Err(Error::refused("Supply a usable contact before importing.").into());
            }
            let company = scope.current_company().ok_or(Error::Forbidden)?.get();
            let code = input["code"].as_str().ok_or(Error::Invalid)?;
            let row = client.query_opt("SELECT id FROM reference_import_records WHERE tenant_id=$1 AND company_id=$2 AND code=$3", &[&scope.tenant(), &company, &code]).await?;
            Ok(json!({"outcome": if row.is_some() { "enquiry" } else { "created" }, "id": row.map(|row| row.get::<_,i64>(0))}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_key(tx.sql(), scope, CAPABILITY).await?;
            let tenant = scope.tenant();
            let company = scope.current_company().ok_or(Error::Forbidden)?.get();
            let code = input["code"].as_str().ok_or(Error::Invalid)?.to_owned();
            let existing = tx.sql().query_opt("SELECT id FROM reference_import_records WHERE tenant_id=$1 AND company_id=$2 AND code=$3 FOR UPDATE", &[&tenant, &company, &code]).await.map_err(Error::from)?;
            let answer = tx.mutate(move |sql| Box::pin(async move {
                let (id, task, kind) = if let Some(row) = existing {
                    let id: i64 = row.get(0);
                    let task: i64 = sql.query_one("INSERT INTO reference_import_tasks(tenant_id,record_id) VALUES($1,$2) RETURNING id", &[&tenant,&id]).await?.get(0);
                    (id, Some(task), "reference/import_results.task")
                } else {
                    let id: i64 = sql.query_one("INSERT INTO reference_import_records(tenant_id,company_id,code) VALUES($1,$2,$3) RETURNING id", &[&tenant,&company,&code]).await?.get(0);
                    (id, None, "reference/import_results.record")
                };
                Ok((json!({"outcome": if task.is_some() { "enquiry" } else { "created" }, "id":id, "task":task}), vec![Change { kind: kind.into(), id: task.unwrap_or(id), old: Value::Null, new: json!({"record":id}) }]))
            })).await?;
            if input["mode"] == "poison" {
                tx.poison();
                return Err(
                    Error::refused("Owner refused after its mutation; fix this row.").into(),
                );
            }
            if input["mode"] == "retry" && RETRY.swap(false, Ordering::SeqCst) {
                // A real PostgreSQL error after the owner mutation, once.
                tx.sql()
                    .query_one("SELECT 1/0", &[])
                    .await
                    .map_err(Error::from)?;
            }
            if input["mode"] == "bad-result" {
                return Ok(json!({"outcome":"unknown"}).into());
            }
            let mut answer = answer;
            answer["bad_link"] = json!(input["mode"] == "bad-link");
            Ok(Output::from(answer))
        })
    }
}
const OPERATIONS: &[Operation] = &[
    Operation {
        key: "reference.import_results.list",
        title: "List reference records",
        summary: "List records in the selected company.",
        keywords: &["list"],
        capability: VIEW_CAPABILITY,
        input: list_input,
        output: list_output,
        handler: Handler::Read(&List),
        channels: Channels::Every,
        step_up: &[],
    },
    Operation {
        key: "reference.import_results.create",
        title: "Capture reference enquiry",
        summary: "Create a record or record another enquiry.",
        keywords: &["import"],
        capability: CAPABILITY,
        input,
        output,
        handler: Handler::SafeWrite(&Owner),
        channels: Channels::Every,
        step_up: &[],
    },
];
inventory::submit! { Contribution::new(DEFINITIONS, MODULE, provenance!(), &Definitions { operations: OPERATIONS, guides: &[] }) }

struct Pages(bilibi_database::Db);
impl bilibi_ui::page::Page<Scope, Error> for Pages {
    fn render<'a>(
        &'a self,
        request: bilibi_ui::page::PageRequest<'a, Scope>,
    ) -> bilibi_ui::page::PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let client = self.0.get().await?;
            bilibi_authz::authorize_key(&**client, scope, CAPABILITY).await?;
            let id = request
                .path
                .get("id")
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or(Error::Invalid)?;
            let company = scope.current_company().ok_or(Error::Forbidden)?.get();
            let sql = if request.operation == "task" {
                "SELECT r.id FROM reference_import_tasks t JOIN reference_import_records r ON r.tenant_id=t.tenant_id AND r.id=t.record_id WHERE t.tenant_id=$1 AND t.id=$2 AND r.company_id=$3"
            } else {
                "SELECT id FROM reference_import_records WHERE tenant_id=$1 AND id=$2 AND company_id=$3"
            };
            let row = client
                .query_opt(sql, &[&scope.tenant(), &id, &company])
                .await?
                .ok_or(Error::NotFound)?;
            let record: i64 = row.get(0);
            let mut html = bilibi_ui::Html::render(&bilibi_ui::Header {
                title: bilibi_ui::Html::text(if request.operation == "task" {
                    "Enquiry task"
                } else {
                    "Reference record"
                }),
                ..Default::default()
            })
            .map_err(|_| Error::Internal)?;
            html.push(
                &bilibi_ui::Html::render(&bilibi_ui::Facts {
                    id: "reference-facts",
                    items: vec![bilibi_ui::Fact {
                        title: "Record",
                        id: "reference-record",
                        value: bilibi_ui::Html::text(&record.to_string()),
                    }],
                })
                .map_err(|_| Error::Internal)?,
            );
            Ok(bilibi_ui::page::PageBody {
                html: html.as_str().to_owned(),
                live: false,
                title: None,
            })
        })
    }
}
use bilibi_module_registry::contributions::{Access, CompanyScope, Method, Route, ROUTES};
inventory::submit! { Contribution::new(ROUTES,"reference.record",provenance!(),&Route { method:Method::Get,path:"/reference/import-records/{id}",service:"reference/import_results.pages",operation:"record",access:Access::Capability(CAPABILITY),framed:true,company_scope:CompanyScope::Company }) }
inventory::submit! { Contribution::new(ROUTES,"reference.task",provenance!(),&Route { method:Method::Get,path:"/reference/import-tasks/{id}",service:"reference/import_results.pages",operation:"task",access:Access::Capability(CAPABILITY),framed:true,company_scope:CompanyScope::Company }) }
