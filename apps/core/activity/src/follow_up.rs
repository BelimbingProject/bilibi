//! A source owner's reviewed follow-up operation. The owner proves and locks
//! its record; Activities owns recipient validation, privacy and task creation.
use crate::{Activities, Common, Reference, SystemTaskInput, TaskInput};
use bilibi_agent_api::{OperationClient, OperationFuture, WriteHandler};
use bilibi_database::{Error, MutationTransaction};
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{ProofFuture, Scope};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};

/// Prove the source record's live capability, status and expected revision,
/// holding its writable company and record locks on the supplied connection.
/// Returns its company; never takes company or actor identity from the input.
pub type FollowUpCheck =
    for<'a> fn(&'a dyn OperationClient, &'a Scope, &'a Value) -> ProofFuture<'a, OrganisationId>;

/// A narrow handler an owner registers as its own operation. The record kind
/// is fixed code and `check` is the owner's single source proof for every door.
pub struct FollowUp {
    pub module: &'static str,
    pub kind: &'static str,
    pub check: FollowUpCheck,
}
static ACTIVITIES: OnceLock<Arc<Activities>> = OnceLock::new();
pub(crate) fn bind(activities: Arc<Activities>) -> bilibi_module_registry::Result<()> {
    ACTIVITIES.set(activities).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate follow-up binding".into())
    })
}
fn activities() -> bilibi_agent_api::Result<&'static Activities> {
    ACTIVITIES
        .get()
        .map(Arc::as_ref)
        .ok_or(Error::Internal.into())
}
impl FollowUp {
    /// The shared closed schema. The runner binds the source id and revision;
    /// the approved version fixes the recipient and task text.
    pub fn input() -> Value {
        json!({"type":"object","required":["record_id","expected_revision","assignee_user_id","title"],"properties":{
            "record_id":{"type":"integer","minimum":1},
            "expected_revision":{"type":"integer","minimum":1},
            "assignee_user_id":{"type":"integer","minimum":1},
            "title":{"type":"string","maxLength":160}
        }})
    }
    async fn prepare(
        &self,
        client: &dyn OperationClient,
        scope: &Scope,
        input: &Value,
    ) -> bilibi_agent_api::Result<(OrganisationId, i64, TaskInput)> {
        let company = (self.check)(client, scope, input).await?;
        if scope.actor().principal().is_none() && scope.current_company() != Some(company) {
            return Err(bilibi_agent_api::invalid(
                "record_id",
                "Switch to the source record’s workplace before creating its task.",
            ));
        }
        let permission = if scope.actor().principal().is_some() {
            crate::SYSTEM_CREATE
        } else {
            crate::MANAGE
        };
        bilibi_authz::authorize_proof(client, scope, permission).await?;
        let assignee = input["assignee_user_id"].as_i64().ok_or(Error::Invalid)?;
        bilibi_user::UserService::company_account_on(client, scope, company.get(), assignee)
            .await?;
        let title = bilibi_agent_api::text_field(input, "title");
        if title.is_empty() || title.chars().count() > 160 {
            return Err(bilibi_agent_api::invalid(
                "title",
                "Enter a task title of up to 160 characters.",
            ));
        }
        let id = input["record_id"].as_i64().ok_or(Error::Invalid)?;
        Ok((
            company,
            assignee,
            TaskInput {
                common: Common {
                    title,
                    related: Some(Reference::new(self.kind, id.to_string())),
                    assignee: Some(assignee),
                    // Private, no invitations or email: source read authority
                    // must not become authority to disclose it to a third party.
                    reminder_email: Some(false),
                    ..Common::default()
                },
                ..TaskInput::default()
            },
        ))
    }
}
impl WriteHandler for FollowUp {
    fn owner(&self) -> &'static str {
        self.module
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (company, assignee, task) = self.prepare(client, scope, input).await?;
            Ok(json!({"summary":"Create a private follow-up task","company_id":company.get(),"assignee_user_id":assignee,"title":task.common.title,"related":{"kind":self.kind,"id":input["record_id"]}}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (company, assignee, task) = self.prepare(tx.sql(), scope, input).await?;
            bilibi_company::require_writable(tx.sql(), scope, company.get(), true)
                .await
                .map_err(|error| bilibi_agent_api::invalid("record_id", error.to_string()))?;
            let tenant = scope.tenant();
            let kind = self.kind.to_owned();
            let source = input["record_id"].as_i64().ok_or(Error::Invalid)?;
            let key = bilibi_watchers::Watchers::action_identity(tx.sql(), scope)
                .await?
                .map(|action| {
                    let binding = json!([action, kind, source]);
                    Sha256::digest(binding.to_string().as_bytes())
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                });
            if let Some(key) = &key {
                if let Some(row) = tx
                    .sql()
                    .query_opt(
                        "SELECT id FROM core_activities WHERE tenant_id=$1 AND followup_watcher_key=$2",
                        &[&tenant, key],
                    )
                    .await
                    .map_err(Error::from)?
                {
                    return Ok(json!({"activity_id":row.get::<_,i64>(0)}).into());
                }
            }
            let activity = if scope.actor().principal().is_some() {
                activities()?
                    .create_system_task(
                        tx,
                        scope,
                        SystemTaskInput {
                            company,
                            originating_user: assignee,
                            task,
                        },
                    )
                    .await
            } else {
                activities()?.create_task(tx, scope, &task).await
            }
            .map_err(|error| crate::agent_operations::invalid(error, ""))?;
            let activity_id = activity.id;
            if let Some(key) = key {
                tx.mutate(move |sql| {
                Box::pin(async move {
                    sql.execute(
                        "UPDATE core_activities SET followup_watcher_key=$3 WHERE tenant_id=$1 AND id=$2",
                        &[&tenant, &activity_id, &key],
                    )
                    .await?;
                    Ok((
                        (),
                        vec![bilibi_database::Change {
                            kind: crate::RECORD.into(),
                            id: activity_id,
                            old: json!({"followup_watcher_key":null}),
                            new: json!({"followup_watcher_key":key}),
                        }],
                    ))
                })
            })
            .await?;
            }
            Ok(json!({"activity_id":activity_id}).into())
        })
    }
}
