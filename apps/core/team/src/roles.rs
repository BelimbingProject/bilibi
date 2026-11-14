//! Team roles: a team carries permission roles and its members hold them
//! while they are in the team, "via team <name>". Core owns the team and
//! Authz owns the permission: the plan reads and checks here, and the write
//! goes through Authz's grant function inside the team's transaction, so
//! giving a role is always role-update permission, the subset rule and
//! step-up. A team with a company gives its roles in that company only.
use crate::access::{authorize_team, load, load_visible, reach};
use crate::facade::{invalid, Checked, FieldErrors, Team, Teams};
use crate::{GROUP_SOURCE, MANAGE, VIEW};
use bilibi_authz::authorize_key;
use bilibi_database::{Error, MutationTransaction, Result};
use bilibi_tenancy::Scope;
use serde::Serialize;
use tokio_postgres::GenericClient;

/// A role a team carries, or may be given.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TeamRole {
    pub id: i64,
    pub name: String,
}
/// A checked change to a team's roles.
#[derive(Clone, Debug, Serialize)]
pub struct RolePlan {
    pub team: Team,
    pub role: TeamRole,
    /// `true` gives the role, `false` takes it away.
    pub attach: bool,
    /// How many members hold it through the team, now or after the change.
    pub members: i64,
}
impl RolePlan {
    /// "Give KL Enterprise the role Sales reader", for reviews.
    pub fn summary(&self) -> String {
        if self.attach {
            format!("Give {} the role {}", self.team.name, self.role.name)
        } else {
            format!(
                "Take the role {} away from {}",
                self.role.name, self.team.name
            )
        }
    }
    /// The same, once it is done.
    pub fn done(&self) -> String {
        if self.attach {
            format!("Gave {} the role {}", self.team.name, self.role.name)
        } else {
            format!(
                "Took the role {} away from {}",
                self.role.name, self.team.name
            )
        }
    }
}
/// A grant refusal from Authz as a field error the person can act on:
/// `Forbidden` (no role-update permission, or a permission the actor does
/// not hold) says `forbidden`; a refusal keeps Authz's own words (a
/// full-access role, an engagement role, tenant-wide keys on a company
/// team). Any other error passes up, step-up included.
pub(crate) fn grant_refusal(error: Error, field: &str, forbidden: &str) -> Result<FieldErrors> {
    match error {
        Error::Forbidden => Ok(invalid(field, forbidden)),
        Error::Refused(message) => Ok(invalid(field, &message)),
        error => Err(error),
    }
}
/// The roles a team carries, by name, joined for a sentence ("Sales reader
/// and Line operator").
pub(crate) async fn names(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    team: i64,
) -> Result<String> {
    let names: Vec<String> = bilibi_authz::grants::group_roles(client, scope, GROUP_SOURCE, team)
        .await?
        .into_iter()
        .map(|role| role.name)
        .collect();
    Ok(match names.len() {
        0 => "no roles".into(),
        1 => names[0].clone(),
        n => format!("{} and {}", names[..n - 1].join(", "), names[n - 1]),
    })
}
async fn member_count(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    team: i64,
) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT count(*) FROM core_team_members WHERE tenant_id=$1 AND team_id=$2",
            &[&scope.tenant(), &team],
        )
        .await?
        .get(0))
}

impl Teams {
    /// The roles a team carries, under `teams.view`, for a team the reader
    /// may see: a hidden team's roles stay hidden with it.
    pub async fn roles(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: i64,
    ) -> Result<Vec<TeamRole>> {
        authorize_key(client, scope, VIEW).await?;
        let reach = reach(client, scope).await?;
        load_visible(client, scope, team, &reach).await?;
        Ok(
            bilibi_authz::grants::group_roles(client, scope, GROUP_SOURCE, team)
                .await?
                .into_iter()
                .map(|role| TeamRole {
                    id: role.id,
                    name: role.name,
                })
                .collect(),
        )
    }
    /// The roles the actor may give this team now, minus the ones it
    /// carries: none without `teams.manage`, role-update permission or the
    /// roles' permissions. For the "Add role" picker; the write checks again.
    pub async fn role_choices(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: &Team,
    ) -> Result<Vec<TeamRole>> {
        if !self.may_change(client, scope, team).await? {
            return Ok(Vec::new());
        }
        let carried = self.roles(client, scope, team.id).await?;
        Ok(self
            .grants
            .group_role_choices(client, scope, team.company_id)
            .await?
            .into_iter()
            .filter(|role| !carried.iter().any(|c| c.id == role.id))
            .map(|role| TeamRole {
                id: role.id,
                name: role.name,
            })
            .collect())
    }
    /// Checks giving (`attach`) or taking away one role, without writing.
    pub async fn plan_role(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: i64,
        role: i64,
        attach: bool,
        lock: bool,
    ) -> Result<Checked<RolePlan>> {
        authorize_key(client, scope, MANAGE).await?;
        let team = load(client, scope, team, lock).await?;
        if let Some(refusal) = authorize_team(client, scope, team.company_id, lock).await? {
            return Ok(Err(refusal));
        }
        let companies = self.grants.visible_companies(client, scope).await?;
        let Some(found) = (match role {
            role if role > 0 => bilibi_authz::roles::get(client, scope, &companies, role).await?,
            _ => None,
        }) else {
            return Ok(Err(invalid("role_id", "Choose a role from the list.")));
        };
        let role = TeamRole {
            id: found.role.id,
            name: found.role.name,
        };
        let carried = bilibi_authz::grants::group_roles(client, scope, GROUP_SOURCE, team.id)
            .await?
            .iter()
            .any(|r| r.id == role.id);
        if carried == attach {
            return Ok(Err(invalid(
                "role_id",
                &if attach {
                    format!("{} already gives {}.", team.name, role.name)
                } else {
                    format!("{} does not give {}.", team.name, role.name)
                },
            )));
        }
        if let Err(error) = self
            .grants
            .validate_group_role(client, scope, &team.group(), role.id, attach)
            .await
        {
            return Ok(Err(grant_refusal(
                error,
                "role_id",
                if attach {
                    "Giving a team a role needs permission to change roles and every permission the role gives."
                } else {
                    "Taking a role away from a team needs permission to change roles."
                },
            )?));
        }
        let members = member_count(client, scope, team.id).await?;
        Ok(Ok(RolePlan {
            team,
            role,
            attach,
            members,
        }))
    }
    /// Gives or takes away one role after checking it again on `tx`, with
    /// the team row locked; Authz asks for step-up and records the change
    /// under the team (`team_role`).
    pub async fn change_role(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        team: i64,
        role: i64,
        attach: bool,
    ) -> Result<Checked<RolePlan>> {
        let plan = match self
            .plan_role(tx.sql(), scope, team, role, attach, true)
            .await?
        {
            Ok(plan) => plan,
            Err(errors) => return Ok(Err(errors)),
        };
        match self
            .grants
            .set_group_role_in(tx, scope, &plan.team.group(), plan.role.id, attach)
            .await
        {
            Ok(()) => Ok(Ok(plan)),
            Err(error) => Ok(Err(grant_refusal(
                error,
                "role_id",
                "Giving a team a role needs permission to change roles and every permission the role gives.",
            )?)),
        }
    }
}
