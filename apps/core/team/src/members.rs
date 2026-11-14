//! Membership: every change (a create's leads, add, remove, lead on or
//! off) is planned by [`Teams::plan_members`] and written by [`apply`], the
//! one place a membership row changes, so a later rule (team roles) has one
//! place to check.
use crate::access::{authorize_team, load};
use crate::facade::{distinct, invalid, Checked, Team, Teams, MAX_BATCH, MAX_MEMBERS};
use crate::MANAGE;
use bilibi_authz::authorize_key;
use bilibi_database::{Change, MutationTransaction, Result};
use bilibi_tenancy::Scope;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use tokio_postgres::GenericClient;

/// A membership change, as the page, the "Add to team" action and agents ask.
#[derive(Clone, Debug)]
pub enum MemberChange {
    Add(Vec<i64>),
    Remove(Vec<i64>),
    Lead { user: i64, lead: bool },
}
/// One membership row before and after (`None`: not a member; `Some(lead)`).
#[derive(Clone, Debug, Serialize)]
pub struct MemberStep {
    pub user_id: i64,
    pub name: String,
    pub before: Option<bool>,
    pub after: Option<bool>,
}
/// A checked membership change: the rows it writes, and the people it left
/// alone because the change already holds for them.
#[derive(Clone, Debug, Serialize)]
pub struct MemberPlan {
    pub team: Team,
    pub steps: Vec<MemberStep>,
    pub unchanged: Vec<String>,
}
impl MemberPlan {
    /// "Add Aisyah and Ben to KL Enterprise", for reviews and flashes.
    pub fn summary(&self) -> String {
        self.say(false)
    }
    /// The same, once it is done: "Added Aisyah and Ben to KL Enterprise".
    pub fn done(&self) -> String {
        self.say(true)
    }
    fn say(&self, done: bool) -> String {
        let verb = |now: &'static str, then: &'static str| if done { then } else { now };
        let names = |f: fn(&MemberStep) -> bool| {
            let names: Vec<&str> = self
                .steps
                .iter()
                .filter(|s| f(s))
                .map(|s| s.name.as_str())
                .collect();
            match names.len() {
                0 => String::new(),
                1 => names[0].into(),
                n if n <= 3 => format!("{} and {}", names[..n - 1].join(", "), names[n - 1]),
                n => format!("{} people", n),
            }
        };
        let team = &self.team.name;
        let added = names(|s| s.before.is_none() && s.after.is_some());
        let removed = names(|s| s.before.is_some() && s.after.is_none());
        let led = names(|s| s.before == Some(false) && s.after == Some(true));
        let unled = names(|s| s.before == Some(true) && s.after == Some(false));
        let mut parts = Vec::new();
        if !added.is_empty() {
            parts.push(format!("{} {added} to {team}", verb("Add", "Added")));
        }
        if !removed.is_empty() {
            parts.push(format!(
                "{} {removed} from {team}",
                verb("Remove", "Removed")
            ));
        }
        if !led.is_empty() {
            parts.push(format!("{} {led} a lead of {team}", verb("Make", "Made")));
        }
        if !unled.is_empty() {
            parts.push(format!("{unled} no longer leads {team}"));
        }
        parts.join("; ")
    }
}

impl Teams {
    pub async fn plan_members(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: i64,
        change: &MemberChange,
        lock: bool,
    ) -> Result<Checked<MemberPlan>> {
        authorize_key(client, scope, MANAGE).await?;
        let team = load(client, scope, team, lock).await?;
        if let Some(refusal) = authorize_team(client, scope, team.company_id, lock).await? {
            return Ok(Err(refusal));
        }
        let users = match change {
            MemberChange::Add(users) | MemberChange::Remove(users) => distinct(users),
            MemberChange::Lead { user, .. } => vec![*user],
        };
        if users.is_empty() || users.iter().any(|u| *u <= 0) {
            return Ok(Err(invalid("user_ids", "Choose at least one person.")));
        }
        if users.len() > MAX_BATCH {
            return Ok(Err(invalid(
                "user_ids",
                &format!("Choose at most {MAX_BATCH} people at a time."),
            )));
        }
        let current: BTreeMap<i64, bool> = client
            .query(
                "SELECT user_id,lead FROM core_team_members WHERE tenant_id=$1 AND team_id=$2 AND user_id=ANY($3)",
                &[&scope.tenant(), &team.id, &users],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        let names = self.names_of(client, scope, &users).await?;
        let name = |u: &i64| names.get(u).cloned().unwrap_or_else(|| format!("User {u}"));
        let mut steps = Vec::new();
        let mut unchanged = Vec::new();
        match change {
            MemberChange::Add(_) => {
                let new: Vec<i64> = users
                    .iter()
                    .copied()
                    .filter(|u| !current.contains_key(u))
                    .collect();
                // A person is re-proved on save: the picker's choice is no proof.
                if self.live_staff(client, scope, &new).await? != new {
                    return Ok(Err(invalid(
                        "user_ids",
                        "Choose people from your active staff.",
                    )));
                }
                let count: i64 = client
                    .query_one(
                        "SELECT count(*) FROM core_team_members WHERE tenant_id=$1 AND team_id=$2",
                        &[&scope.tenant(), &team.id],
                    )
                    .await?
                    .get(0);
                if count + new.len() as i64 > MAX_MEMBERS {
                    return Ok(Err(invalid(
                        "user_ids",
                        &format!("A team holds at most {MAX_MEMBERS} members."),
                    )));
                }
                for user in &users {
                    if current.contains_key(user) {
                        unchanged.push(name(user));
                    } else {
                        steps.push(MemberStep {
                            user_id: *user,
                            name: name(user),
                            before: None,
                            after: Some(false),
                        });
                    }
                }
                if steps.is_empty() {
                    return Ok(Err(invalid(
                        "user_ids",
                        "Everyone you chose is already in this team.",
                    )));
                }
            }
            MemberChange::Remove(_) => {
                if let Some(missing) = users.iter().find(|u| !current.contains_key(u)) {
                    return Ok(Err(invalid(
                        "user_ids",
                        &format!("{} is not in this team.", name(missing)),
                    )));
                }
                for user in &users {
                    steps.push(MemberStep {
                        user_id: *user,
                        name: name(user),
                        before: current.get(user).copied(),
                        after: None,
                    });
                }
            }
            MemberChange::Lead { user, lead } => {
                let Some(was) = current.get(user).copied() else {
                    return Ok(Err(invalid(
                        "user_ids",
                        &format!("{} is not in this team.", name(user)),
                    )));
                };
                if was == *lead {
                    return Ok(Err(invalid(
                        "lead",
                        if *lead {
                            "This person already leads the team."
                        } else {
                            "This person is not a lead."
                        },
                    )));
                }
                steps.push(MemberStep {
                    user_id: *user,
                    name: name(user),
                    before: Some(was),
                    after: Some(*lead),
                });
            }
        }
        // A team keeps at least one lead.
        let losing = steps
            .iter()
            .filter(|s| s.before == Some(true) && s.after != Some(true))
            .count() as i64;
        if losing > 0 {
            let leads: i64 = client
                .query_one(
                    "SELECT count(*) FROM core_team_members WHERE tenant_id=$1 AND team_id=$2 AND lead",
                    &[&scope.tenant(), &team.id],
                )
                .await?
                .get(0);
            if leads - losing < 1 {
                return Ok(Err(invalid(
                    "lead",
                    "A team needs at least one lead. Make someone else a lead first.",
                )));
            }
        }
        Ok(Ok(MemberPlan {
            team,
            steps,
            unchanged,
        }))
    }
    /// Writes a membership change after checking it again on `tx`, with the
    /// team row locked so concurrent changes cannot leave it without a lead.
    pub async fn change_members(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        team: i64,
        change: &MemberChange,
    ) -> Result<Checked<MemberPlan>> {
        let plan = match self
            .plan_members(tx.sql(), scope, team, change, true)
            .await?
        {
            Ok(plan) => plan,
            Err(e) => return Ok(Err(e)),
        };
        apply(tx, scope, &plan).await?;
        Ok(Ok(plan))
    }
}
/// The one place a membership row is written. Each person's row is its own
/// audit entry under the team (`team_member`, the team's id), so the team's
/// history shows who joined, left and led, and when.
pub(crate) async fn apply(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    plan: &MemberPlan,
) -> Result<()> {
    let tenant = scope.tenant();
    let team = plan.team.id;
    let steps = plan.steps.clone();
    tx.mutate(move |sql| {
        Box::pin(async move {
            let mut changes = Vec::with_capacity(steps.len());
            for step in &steps {
                match (step.before, step.after) {
                    (None, Some(lead)) => {
                        sql.execute(
                            "INSERT INTO core_team_members(tenant_id,team_id,user_id,lead) VALUES($1,$2,$3,$4)",
                            &[&tenant, &team, &step.user_id, &lead],
                        )
                        .await?;
                    }
                    (Some(_), Some(lead)) => {
                        sql.execute(
                            "UPDATE core_team_members SET lead=$4 WHERE tenant_id=$1 AND team_id=$2 AND user_id=$3",
                            &[&tenant, &team, &step.user_id, &lead],
                        )
                        .await?;
                    }
                    (Some(_), None) => {
                        sql.execute(
                            "DELETE FROM core_team_members WHERE tenant_id=$1 AND team_id=$2 AND user_id=$3",
                            &[&tenant, &team, &step.user_id],
                        )
                        .await?;
                    }
                    (None, None) => continue,
                }
                let side = |lead: Option<bool>| match lead {
                    Some(lead) => json!({"user_id":step.user_id,"lead":lead}),
                    None => json!({}),
                };
                changes.push(Change {
                    kind: "team_member".into(),
                    id: team,
                    old: side(step.before),
                    new: side(step.after),
                });
            }
            Ok(((), changes))
        })
    })
    .await
}
