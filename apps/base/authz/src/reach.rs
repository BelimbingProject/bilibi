//! Record reach (#683): how far a grant of a capability reaches into the
//! records it guards. A role gives a capability with reach `own` (records
//! the account owns), `team` (also records owned by someone in one of its
//! Core teams, or filed under one of its teams) or `all`. Only a capability
//! whose owner opted in with `CapabilityDefinition::with_reach` carries less
//! than `all`. The owner names the field that holds a record's owner (and
//! optionally its team) and applies [`Reach::predicate`] in every read; Grid
//! does it for every declared table, so lists, reports and search share it.
//!
//! Reach only narrows: it is applied on top of the owner's own tenant and
//! company filters (ADR 19), never instead of them. It is read live with the
//! decision; nothing is cached.
use bilibi_database::{Error, Result};
use bilibi_tenancy::{ProofClient, Scope};

/// The group source whose members share "my team's" records: Core teams,
/// whose members Authz keeps in `authz_group_members` (Group roles).
pub const TEAM_SOURCE: &str = "team";

/// How far a grant reaches. Ordered: `Own < Team < All`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Reach {
    /// Records the account owns.
    Own,
    /// Records the account owns, records owned by a member of one of its
    /// teams, and records filed under one of its teams.
    Team,
    /// Every record the capability opens.
    All,
}
impl Reach {
    pub const EVERY: [Reach; 3] = [Reach::Own, Reach::Team, Reach::All];
    /// The stored and agent-facing code: `own`, `team`, `all`.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Team => "team",
            Self::All => "all",
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        Self::EVERY.into_iter().find(|reach| reach.code() == code)
    }
    /// How a person reads it beside a permission.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Own => "Mine",
            Self::Team => "My team's",
            Self::All => "All",
        }
    }
    pub(crate) fn from_rank(rank: i32) -> Self {
        match rank {
            2.. => Self::All,
            1 => Self::Team,
            _ => Self::Own,
        }
    }
    /// The records this reach admits, as a SQL condition for the owner's
    /// own statement. `owner` (and `team`) are column expressions from the
    /// owner's static declaration, never request input. The statement binds
    /// the tenant as `$1` and the reader's account as `$2`, as Grid sources
    /// and every owner read do. `All` is `true`. It is never NULL: a row
    /// with no owner or no team is simply not admitted by that part.
    pub fn predicate(self, owner: &str, team: Option<&str>) -> String {
        match self {
            Self::All => "true".to_owned(),
            Self::Own => format!("COALESCE({owner} = $2, false)"),
            Self::Team => {
                let mut sql = format!(
                    "COALESCE({owner} = $2 OR {owner} IN (SELECT o.user_id FROM authz_group_members m JOIN authz_group_members o ON o.tenant_id = m.tenant_id AND o.source = m.source AND o.group_id = m.group_id WHERE m.tenant_id = $1 AND m.user_id = $2 AND m.source = '{TEAM_SOURCE}')"
                );
                if let Some(team) = team {
                    sql.push_str(&format!(
                        " OR {team} IN (SELECT m.group_id FROM authz_group_members m WHERE m.tenant_id = $1 AND m.user_id = $2 AND m.source = '{TEAM_SOURCE}')"
                    ));
                }
                sql.push_str(", false)");
                sql
            }
        }
    }
}
impl std::fmt::Display for Reach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The stored reach of a role grant as a number, so SQL can take the widest.
pub(crate) const RANK_SQL: &str = "CASE r.reach WHEN 'all' THEN 2 WHEN 'team' THEN 1 ELSE 0 END";

/// How far the reader holds `key` now, unlogged: `None` when the key is not
/// held (as [`crate::holds_proof`] answers it), the widest reach of the
/// grants that give it otherwise. A direct allow and grant-all reach all; a
/// key that does not support reach is `All` when held. A named principal's
/// scope holds nothing here, as for `holds_proof`. Read it in the same
/// transaction as the records it filters.
pub async fn reach_proof(
    client: &dyn ProofClient,
    scope: &Scope,
    key: &str,
) -> Result<Option<Reach>> {
    if key.is_empty() || key.len() > 200 {
        return Err(Error::Invalid);
    }
    if scope.principal().is_some() {
        return Ok(None);
    }
    let judged = crate::user_judgement(
        &crate::Proof(client),
        scope,
        key,
        scope.current_company().map(|id| id.get()),
    )
    .await?;
    Ok(judged.decision.allowed.then_some(judged.reach))
}

/// Whether the reader's reach on `key` admits a record owned by `owner`
/// and filed under `team`, for an owner's write recheck inside its
/// transaction. A record with no owner is admitted only by `All` reach.
pub async fn reach_admits(
    client: &dyn ProofClient,
    scope: &Scope,
    key: &str,
    owner: Option<i64>,
    team: Option<i64>,
) -> Result<bool> {
    let Some(reach) = reach_proof(client, scope, key).await? else {
        return Ok(false);
    };
    if reach == Reach::All {
        return Ok(true);
    }
    let Some(owner) = owner else {
        return Ok(false);
    };
    let sql = format!(
        "SELECT {} FROM (SELECT $1::bigint AS tenant, $2::bigint AS usr, $3::bigint AS owner, $4::bigint AS team) r",
        reach.predicate("r.owner", Some("r.team"))
    );
    Ok(client
        .query_opt(&sql, &[&scope.tenant(), &scope.user(), &owner, &team])
        .await?
        .is_some_and(|row| row.get::<_, bool>(0)))
}
