//! Bounded record facts owned and authorized by Company.
use bilibi_database::{Error, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_ports::{
    Port, PortClient, PortFuture, PortProvider, RecordFact, RecordFactValue, PORTS, RECORD_FACTS,
};
use bilibi_tenancy::Scope;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Request {
    id: String,
}
struct Count {
    departments: bool,
}
impl PortProvider for Count {
    fn call<'a>(&'a self, client: PortClient<'a>, scope: &'a Scope, body: Value) -> PortFuture<'a> {
        Box::pin(async move {
            let request: Request = bilibi_ports::request(body)?;
            let id: i64 = request.id.parse().map_err(|_| Error::Invalid)?;
            bilibi_ports::on_client!(client, |c| count(c, scope, id, self.departments).await)
        })
    }
}
async fn count(
    client: &(impl tokio_postgres::GenericClient + Sync),
    scope: &Scope,
    id: i64,
    departments: bool,
) -> Result<Value> {
    if !bilibi_authz::holds_proof(client, scope, crate::VIEW).await? {
        return Err(Error::Forbidden);
    }
    if client
        .query_opt(
            "SELECT 1 FROM companies WHERE tenant_id=$1 AND id=$2 AND deleted_at IS NULL",
            &[&scope.tenant(), &id],
        )
        .await?
        .is_none()
    {
        return Err(Error::NotFound);
    }
    let (label, sql) = if departments {
        ("Departments", "SELECT count(*) FROM (SELECT 1 FROM company_departments WHERE tenant_id=$1 AND company_id=$2 LIMIT 1001) bounded")
    } else {
        ("Sub-companies", "SELECT count(*) FROM (SELECT 1 FROM companies WHERE tenant_id=$1 AND parent_id=$2 AND deleted_at IS NULL LIMIT 1001) bounded")
    };
    let count: i64 = client.query_one(sql, &[&scope.tenant(), &id]).await?.get(0);
    bilibi_ports::answer(&RecordFactValue {
        label: label.into(),
        value: if count > 1000 {
            "1000+".into()
        } else {
            count.to_string()
        },
    })
}
macro_rules! provenance {
    () => {
        Provenance {
            module: "core/company",
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! { Contribution::new(PORTS, "core/company.department_count", provenance!(), &Port { provider: &Count { departments: true } }) }
inventory::submit! { Contribution::new(PORTS, "core/company.child_count", provenance!(), &Port { provider: &Count { departments: false } }) }
inventory::submit! { Contribution::new(RECORD_FACTS, "core/company.department_count", provenance!(), &RecordFact { record_kind: "core/company", port: "core/company.department_count" }) }
inventory::submit! { Contribution::new(RECORD_FACTS, "core/company.child_count", provenance!(), &RecordFact { record_kind: "core/company", port: "core/company.child_count" }) }
