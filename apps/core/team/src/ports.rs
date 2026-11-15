//! Read ports for Domains that keep their own facts beside a team (a
//! crew's shift pattern, a queue's team) without depending on Core Teams.
//! Shapes: README.md, "Ports".
use crate::Teams;
use bilibi_module_registry::Contribution;
use bilibi_ports::{
    answer, on_client, request, Port, PortClient, PortFuture, PortProvider, Value, PORTS,
};
use bilibi_tenancy::Scope;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_postgres::GenericClient;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamRequest {
    team_id: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamsOfRequest {
    user_id: i64,
}
#[derive(Serialize)]
struct TeamAnswer {
    id: i64,
    name: String,
    kind: String,
    parent_id: Option<i64>,
    company_id: Option<i64>,
    leads: Vec<i64>,
    leads_above: Vec<i64>,
    members: Vec<i64>,
}
fn teams() -> bilibi_database::Result<&'static Teams> {
    crate::operations::teams().map_err(|_| bilibi_database::Error::Internal)
}
async fn team_answer(
    teams: &Teams,
    c: &(impl GenericClient + Sync),
    scope: &Scope,
    team_id: i64,
) -> bilibi_database::Result<TeamAnswer> {
    let team = teams.get(c, scope, team_id).await?;
    let members = teams.member_ids(c, scope, team_id).await?;
    let mut above: Vec<i64> = teams
        .leads_above(c, scope, team_id)
        .await?
        .into_iter()
        .map(|l| l.user_id)
        .collect();
    above.sort_unstable();
    above.dedup();
    Ok(TeamAnswer {
        id: team.id,
        name: team.name,
        kind: team.kind,
        parent_id: team.parent_id,
        company_id: team.company_id,
        leads: members.iter().filter(|m| m.1).map(|m| m.0).collect(),
        leads_above: above,
        members: members.iter().map(|m| m.0).collect(),
    })
}
struct TeamPort;
impl PortProvider for TeamPort {
    fn call<'a>(&'a self, client: PortClient<'a>, scope: &'a Scope, body: Value) -> PortFuture<'a> {
        Box::pin(async move {
            let TeamRequest { team_id } = request(body)?;
            let teams = teams()?;
            let answer_of = on_client!(client, |c| team_answer(teams, c, scope, team_id).await)?;
            answer(&answer_of)
        })
    }
}
struct TeamsOfPort;
impl PortProvider for TeamsOfPort {
    fn call<'a>(&'a self, client: PortClient<'a>, scope: &'a Scope, body: Value) -> PortFuture<'a> {
        Box::pin(async move {
            let TeamsOfRequest { user_id } = request(body)?;
            let teams = teams()?;
            let rows = on_client!(client, |c| teams.teams_of(c, scope, user_id).await)?;
            answer(&json!({ "teams": rows }))
        })
    }
}
inventory::submit! { Contribution::new(PORTS, "core/team.team", crate::provenance!(), &Port { provider: &TeamPort }) }
inventory::submit! { Contribution::new(PORTS, "core/team.teams_of", crate::provenance!(), &Port { provider: &TeamsOfPort }) }
