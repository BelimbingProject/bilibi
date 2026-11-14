//! The Teams operation contracts. The team page, the "Add to team" action
//! and agents all call these; writes are reviewed safe writes.
use crate::{
    facade::invalid, Checked, MemberChange, MemberPlan, TeamInput, TeamQuery, Teams, MANAGE, VIEW,
};
use bilibi_agent_api::{
    json::{Data, Output},
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, ReadHandler,
    WriteHandler, DEFINITIONS,
};
use bilibi_database::{Db, Error, MutationTransaction};
use bilibi_module_registry::Contribution;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
};
pub(crate) const CREATE: &str = "core.team.create";
pub(crate) const UPDATE: &str = "core.team.update";
pub(crate) const ADD_MEMBERS: &str = "core.team.add_members";
pub(crate) const REMOVE_MEMBERS: &str = "core.team.remove_members";
pub(crate) const SET_LEAD: &str = "core.team.set_lead";
struct Bound {
    db: Db,
    teams: Arc<Teams>,
}
static BOUND: OnceLock<Bound> = OnceLock::new();
pub(crate) fn bind(db: Db, teams: Arc<Teams>) -> bilibi_module_registry::Result<()> {
    BOUND.set(Bound { db, teams }).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate team binding".into())
    })
}
fn bound() -> bilibi_agent_api::Result<&'static Bound> {
    BOUND.get().ok_or(Error::Internal.into())
}
pub(crate) fn teams() -> bilibi_agent_api::Result<&'static Teams> {
    Ok(&bound()?.teams)
}
fn checked<T>(v: Checked<T>) -> bilibi_agent_api::Result<T> {
    v.map_err(bilibi_agent_api::Error::InvalidInput)
}
fn id(v: &Value, key: &str) -> bilibi_agent_api::Result<i64> {
    v[key].as_i64().filter(|v| *v > 0).ok_or_else(|| {
        bilibi_agent_api::Error::InvalidInput(invalid(key, "Choose a valid record."))
    })
}
fn optional_id(v: &Value, key: &str) -> bilibi_agent_api::Result<Option<i64>> {
    match &v[key] {
        Value::Null => Ok(None),
        _ => id(v, key).map(Some),
    }
}
fn ids(v: &Value, key: &str) -> bilibi_agent_api::Result<Vec<i64>> {
    let refuse =
        || bilibi_agent_api::Error::InvalidInput(invalid(key, "Choose people from the list."));
    v[key]
        .as_array()
        .ok_or_else(refuse)?
        .iter()
        .map(|v| v.as_i64().filter(|v| *v > 0).ok_or_else(refuse))
        .collect()
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}
fn team_input(v: &Value) -> bilibi_agent_api::Result<TeamInput> {
    Ok(TeamInput {
        name: text(v, "name").into(),
        kind: text(v, "kind").into(),
        parent_id: optional_id(v, "parent_id")?,
        company_id: optional_id(v, "company_id")?,
        leads: ids(v, "leads")?,
    })
}
fn member_change(op: &Write, v: &Value) -> bilibi_agent_api::Result<MemberChange> {
    Ok(match op {
        Write::Add => MemberChange::Add(ids(v, "user_ids")?),
        Write::Remove => MemberChange::Remove(ids(v, "user_ids")?),
        _ => MemberChange::Lead {
            user: id(v, "user_id")?,
            lead: v["lead"].as_bool().ok_or_else(|| {
                bilibi_agent_api::Error::InvalidInput(invalid("lead", "Say true or false."))
            })?,
        },
    })
}
fn plan_json(plan: &MemberPlan) -> Value {
    json!({"team_id":plan.team.id,"team":plan.team.name,"changes":plan.steps,"unchanged":plan.unchanged})
}
fn output(v: Value) -> Output {
    Output::Data(Data::restricted(v, &HashSet::new()))
}
enum Read {
    List,
    Get,
    Members,
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "core/team"
    }
    fn call<'a>(
        &'a self,
        _client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let b = bound()?;
            let client = b.db.get().await.map_err(Error::from)?;
            let client = &**client;
            let page = input["page"].as_i64().unwrap_or(1).clamp(1, 1_000_000);
            let size = input["page_size"].as_i64().unwrap_or(25).clamp(1, 100);
            let value = match self {
                Self::List => {
                    let (rows, total) = b
                        .teams
                        .list(
                            client,
                            scope,
                            &TeamQuery {
                                search: text(input, "search"),
                                kind: text(input, "kind"),
                                parent: optional_id(input, "parent_id")?,
                                limit: size,
                                offset: (page - 1) * size,
                            },
                        )
                        .await?;
                    json!({"teams":rows,"total":total,"page":page,"page_size":size})
                }
                Self::Get => {
                    let team_id = id(input, "id")?;
                    let team = b.teams.get(client, scope, team_id).await?;
                    let leads = b.teams.leads(client, scope, team_id).await?;
                    let above = b.teams.leads_above(client, scope, team_id).await?;
                    let (subs, sub_total) = b
                        .teams
                        .list(
                            client,
                            scope,
                            &TeamQuery {
                                search: "",
                                kind: "",
                                parent: Some(team_id),
                                limit: 100,
                                offset: 0,
                            },
                        )
                        .await?;
                    let (_, members) = b.teams.members(client, scope, team_id, 1, 0).await?;
                    json!({"team":team,"leads":leads,"leads_above":above,"members":members,
                        "sub_teams":subs.iter().map(|s|json!({"id":s.team.id,"name":s.team.name})).collect::<Vec<_>>(),"sub_team_total":sub_total})
                }
                Self::Members => {
                    let team_id = id(input, "id")?;
                    let (rows, total) = b
                        .teams
                        .members(client, scope, team_id, size, (page - 1) * size)
                        .await?;
                    json!({"members":rows,"total":total,"page":page,"page_size":size})
                }
            };
            Ok(output(value))
        })
    }
}
enum Write {
    Create,
    Update,
    Add,
    Remove,
    Lead,
}
impl WriteHandler for Write {
    fn creates(&self) -> Option<bilibi_agent_api::CreatedRecord> {
        matches!(self, Self::Create).then_some(bilibi_agent_api::CreatedRecord {
            kind: "team",
            id_pointer: "/id",
        })
    }
    fn owner(&self) -> &'static str {
        "core/team"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let t = teams()?;
            // The review's own read-only transaction; never another pool connection.
            let client = client.transaction().ok_or(Error::Internal)?;
            let value = match self {
                Self::Create => {
                    let after =
                        checked(t.preview_create(client, scope, &team_input(input)?).await?)?;
                    json!({"summary":format!("Create team {} with {} lead{}",after.name,after.leads.len(),if after.leads.len()==1 {""} else {"s"}),
                        "after":{"name":after.name,"kind":after.kind,"parent_id":after.parent_id,"company_id":after.company_id,"leads":after.leads}})
                }
                Self::Update => {
                    let after = checked(
                        t.preview_update(
                            client,
                            scope,
                            id(input, "id")?,
                            id(input, "version")?,
                            &input["patch"],
                        )
                        .await?,
                    )?;
                    json!({"summary":format!("Update team {}",after.name),"after":after})
                }
                Self::Add | Self::Remove | Self::Lead => {
                    let plan = checked(
                        t.plan_members(
                            client,
                            scope,
                            id(input, "team_id")?,
                            &member_change(self, input)?,
                            false,
                        )
                        .await?,
                    )?;
                    json!({"summary":plan.summary(),"after":plan_json(&plan)})
                }
            };
            Ok(value.into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let t = teams()?;
            let value = match self {
                Self::Create => checked(t.create(tx, scope, &team_input(input)?).await?)?.to_json(),
                Self::Update => checked(
                    t.update(
                        tx,
                        scope,
                        id(input, "id")?,
                        id(input, "version")?,
                        &input["patch"],
                    )
                    .await?,
                )?
                .to_json(),
                Self::Add | Self::Remove | Self::Lead => {
                    let plan = checked(
                        t.change_members(
                            tx,
                            scope,
                            id(input, "team_id")?,
                            &member_change(self, input)?,
                        )
                        .await?,
                    )?;
                    let mut value = plan_json(&plan);
                    value["summary"] = json!(plan.done());
                    value
                }
            };
            Ok(output(value))
        })
    }
}
fn list_schema() -> Value {
    json!({"type":"object","properties":{"search":{"type":"string","maxLength":200},"kind":{"type":"string","maxLength":64,"description":"A code of the tenant's team kind pick-list (core.team.kind)."},"parent_id":{"type":"integer","minimum":1,"description":"Only the direct sub-teams of this team."},"page":{"type":"integer","minimum":1,"maximum":1000000},"page_size":{"type":"integer","minimum":1,"maximum":100}}})
}
fn get_schema() -> Value {
    json!({"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1}}})
}
fn members_schema() -> Value {
    json!({"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1},"page":{"type":"integer","minimum":1,"maximum":1000000},"page_size":{"type":"integer","minimum":1,"maximum":100}}})
}
fn create_schema() -> Value {
    json!({"type":"object","required":["name","leads"],"properties":{"name":{"type":"string","maxLength":200},"kind":{"type":"string","maxLength":64,"x-bilibi-picklist":"core.team.kind","description":"A code of the core.team.kind pick-list; leave empty for the tenant's default."},"parent_id":{"type":"integer","minimum":1,"nullable":true},"company_id":{"type":"integer","minimum":1,"nullable":true,"description":"A company you work in, or null."},"leads":{"type":"array","items":{"type":"integer","minimum":1},"description":"Staff account ids; they become the first members."}}})
}
fn update_schema() -> Value {
    json!({"type":"object","required":["id","version","patch"],"properties":{"id":{"type":"integer","minimum":1},"version":{"type":"integer","minimum":1},"patch":{"type":"object","properties":{"name":{"type":"string","maxLength":200},"kind":{"type":"string","maxLength":64,"x-bilibi-picklist":"core.team.kind"},"parent_id":{"type":"integer","minimum":1,"nullable":true},"company_id":{"type":"integer","minimum":1,"nullable":true}}}}})
}
fn members_change_schema() -> Value {
    json!({"type":"object","required":["team_id","user_ids"],"properties":{"team_id":{"type":"integer","minimum":1},"user_ids":{"type":"array","items":{"type":"integer","minimum":1},"description":"Staff account ids."}}})
}
fn lead_schema() -> Value {
    json!({"type":"object","required":["team_id","user_id","lead"],"properties":{"team_id":{"type":"integer","minimum":1},"user_id":{"type":"integer","minimum":1},"lead":{"type":"boolean"}}})
}
fn result_schema() -> Value {
    json!({"type":"object","description":"The team, its members or leads, a bounded list, or the membership changes made."})
}
macro_rules! op {
    ($key:expr,$title:literal,$summary:literal,$cap:expr,$schema:ident,$handler:expr) => {
        Operation {
            key: $key,
            title: $title,
            summary: $summary,
            keywords: &["team", "member", "lead", "crew"],
            capability: $cap,
            input: $schema,
            output: result_schema,
            handler: $handler,
            channels: Channels::Every,
            step_up: &[],
        }
    };
}
inventory::submit! {Contribution::new(DEFINITIONS,"core/team.agent",crate::provenance!(),&Definitions{
    operations:&[
        op!("core.team.list","List teams","Find teams by name or kind, or the sub-teams of a team.",VIEW,list_schema,Handler::Read(&Read::List)),
        op!("core.team.get","Read a team","A team's facts, its leads, the leads of the teams above it, its sub-teams and how many members it has.",VIEW,get_schema,Handler::Read(&Read::Get)),
        op!("core.team.members","List a team's members","One page of a team's members, leads first.",VIEW,members_schema,Handler::Read(&Read::Members)),
        op!(CREATE,"Create a team","Create a team with one or more leads, an optional parent team and an optional company.",MANAGE,create_schema,Handler::SafeWrite(&Write::Create)),
        op!(UPDATE,"Update a team","Rename a team or change its kind, parent team or company, using the version read.",MANAGE,update_schema,Handler::SafeWrite(&Write::Update)),
        op!(ADD_MEMBERS,"Add people to a team","Add staff to a team. People already in it are left as they are.",MANAGE,members_change_schema,Handler::SafeWrite(&Write::Add)),
        op!(REMOVE_MEMBERS,"Remove people from a team","Remove members from a team. A team keeps at least one lead.",MANAGE,members_change_schema,Handler::SafeWrite(&Write::Remove)),
        op!(SET_LEAD,"Make a member a lead, or not","Turn a member's lead role on or off. A team keeps at least one lead.",MANAGE,lead_schema,Handler::SafeWrite(&Write::Lead)),
    ],guides:&[],
})}
