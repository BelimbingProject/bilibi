//! Desk operation contract.
use crate::{desks, UserService};
use bilibi_agent_api::{
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, WriteHandler,
    DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
static USERS: OnceLock<Arc<UserService>> = OnceLock::new();
pub(crate) fn bind(users: Arc<UserService>) -> bilibi_module_registry::Result<()> {
    USERS.set(users).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate desk operation binding".into())
    })
}
fn args(input: &Value) -> bilibi_agent_api::Result<(i64, i64, bool)> {
    Ok((
        input["company_id"]
            .as_i64()
            .filter(|v| *v > 0)
            .ok_or(Error::Invalid)?,
        input["person_id"]
            .as_i64()
            .filter(|v| *v > 0)
            .ok_or(Error::Invalid)?,
        input["assigned"].as_bool().ok_or(Error::Invalid)?,
    ))
}
struct Set;
impl Set {
    async fn preview_on(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        input: &Value,
    ) -> bilibi_agent_api::Result<bilibi_agent_api::json::Output> {
        let (company, person, assigned) = args(input)?;
        desks::check(client, scope, company, person, false, assigned).await?;
        Ok(json!({"summary":if assigned {"Give this person a desk in the company"} else {"Remove this person's desk in the company"},"person_id":person,"company_id":company,"assigned":assigned}).into())
    }
}
impl WriteHandler for Set {
    fn owner(&self) -> &'static str {
        "core/user"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            // The door's own transaction when it gives one: a preview never
            // asks the pool for a second connection.
            match client.transaction() {
                Some(tx) => self.preview_on(tx, scope, input).await,
                None => {
                    let pooled = USERS
                        .get()
                        .ok_or(Error::Internal)?
                        .db
                        .get()
                        .await
                        .map_err(Error::from)?;
                    self.preview_on(&**pooled, scope, input).await
                }
            }
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (company, person, assigned) = args(input)?;
            Ok(USERS
                .get()
                .ok_or(Error::Internal)?
                .set_desk_in(tx, scope, company, person, assigned)
                .await?
                .into())
        })
    }
}
fn schema() -> Value {
    json!({"type":"object","required":["company_id","person_id","assigned"],"properties":{"company_id":{"type":"integer","minimum":1},"person_id":crate::record_fields::PERSON_ID.input(json!({"minimum":1})),"assigned":{"type":"boolean"}}})
}
fn output() -> Value {
    json!({"type":"object"})
}
inventory::submit! { Contribution::new(DEFINITIONS,"core/user.desks",Provenance{module:"core/user",source:file!(),line:line!()},&Definitions {
    operations:&[Operation {key:"core.user.desk.set",title:"Assign or remove a desk",summary:"Give a staff person a desk in a company or remove it. Company managers may assign themselves.",keywords:&["desk","work","company","access"],capability:bilibi_company::UPDATE,input:schema,output,handler:Handler::SafeWrite(&Set),channels:Channels::Every,step_up:&[]}],guides:&[],
}) }
