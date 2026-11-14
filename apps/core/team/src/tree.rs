//! The tree of teams: one parent each, no loops, at most [`MAX_DEPTH`]
//! levels, and distinct names among siblings.
use crate::access::{load_visible, reach};
use crate::facade::{invalid, FieldErrors, MAX_DEPTH};
use bilibi_database::{MutationTransaction, Result};
use bilibi_tenancy::Scope;
use tokio_postgres::GenericClient;

/// Serializes changes to the tree of one tenant's teams.
pub(crate) async fn tree_lock(tx: &MutationTransaction<'_>, scope: &Scope) -> Result<()> {
    tx.sql()
        .query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended('core/team.tree', $1))",
            &[&scope.tenant()],
        )
        .await?;
    Ok(())
}
/// Why `parent` cannot hold `team` (a new team when `None`): missing, the
/// team itself or one below it, or too deep.
pub(crate) async fn parent_refusal(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    team: Option<i64>,
    parent: i64,
) -> Result<Option<FieldErrors>> {
    let reach = reach(client, scope).await?;
    if load_visible(client, scope, parent, &reach).await.is_err() {
        return Ok(Some(invalid("parent_id", "Choose a team from the list.")));
    }
    // How many levels the parent sits at (1 for a top team), and whether the
    // moving team is among its ancestors.
    let row = client
        .query_opt(
            "WITH RECURSIVE up(id,parent_id,depth) AS (SELECT id,parent_id,1 FROM core_teams WHERE tenant_id=$1 AND id=$2 \
               UNION ALL SELECT t.id,t.parent_id,up.depth+1 FROM up JOIN core_teams t ON t.tenant_id=$1 AND t.id=up.parent_id WHERE up.depth<=$4) \
             SELECT max(depth), bool_or(id=$3) FROM up HAVING count(*)>0",
            &[&scope.tenant(), &parent, &team.unwrap_or(0), &(MAX_DEPTH + 1)],
        )
        .await?;
    let Some(row) = row else {
        return Ok(Some(invalid("parent_id", "Choose a team from the list.")));
    };
    let depth: i32 = row.get(0);
    let cycle: bool = row.get::<_, Option<bool>>(1).unwrap_or(false);
    if cycle {
        return Ok(Some(invalid(
            "parent_id",
            "A team cannot sit under itself or one of its own sub-teams.",
        )));
    }
    // The moving team's own levels below it count too.
    let height: i32 = match team {
        None => 1,
        Some(team) => client
            .query_one(
                "WITH RECURSIVE down(id,depth) AS (SELECT id,1 FROM core_teams WHERE tenant_id=$1 AND id=$2 \
                   UNION ALL SELECT t.id,down.depth+1 FROM down JOIN core_teams t ON t.tenant_id=$1 AND t.parent_id=down.id WHERE down.depth<=$3) \
                 SELECT max(depth) FROM down",
                &[&scope.tenant(), &team, &(MAX_DEPTH + 1)],
            )
            .await?
            .get::<_, Option<i32>>(0)
            .unwrap_or(1),
    };
    Ok((depth + height > MAX_DEPTH).then(|| {
        invalid(
            "parent_id",
            &format!("Teams nest at most {MAX_DEPTH} levels deep."),
        )
    }))
}
pub(crate) async fn sibling_taken(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    team: Option<i64>,
    parent: Option<i64>,
    name: &str,
) -> Result<bool> {
    Ok(client
        .query_opt(
            "SELECT 1 FROM core_teams WHERE tenant_id=$1 AND coalesce(parent_id,0)=coalesce($2::bigint,0) AND lower(name)=lower($3) AND id<>$4",
            &[&scope.tenant(), &parent, &name, &team.unwrap_or(0)],
        )
        .await?
        .is_some())
}
