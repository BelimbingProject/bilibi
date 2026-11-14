//! Who may see and change a team: the reader's company reach, the visibility
//! test every team read shares, and the manager's work-access proof.
use crate::facade::{invalid, FieldErrors, Team, COLUMNS};
use crate::MANAGE;
use bilibi_authz::authorize_key;
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use tokio_postgres::GenericClient;

pub(crate) async fn load(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    id: i64,
    lock: bool,
) -> Result<Team> {
    client
        .query_opt(
            &format!(
                "SELECT {COLUMNS} FROM core_teams WHERE tenant_id=$1 AND id=$2{}",
                if lock { " FOR UPDATE" } else { "" }
            ),
            &[&scope.tenant(), &id],
        )
        .await?
        .as_ref()
        .map(Team::row)
        .ok_or(Error::NotFound)
}
/// The companies a reader works in; `None` reaches every company (a system
/// principal). A team of a company is visible only through them.
pub(crate) async fn reach(
    client: &dyn bilibi_tenancy::ProofClient,
    scope: &Scope,
) -> Result<Option<Vec<i64>>> {
    use bilibi_tenancy::OrganisationDirectory;
    if scope.principal().is_some() {
        return Ok(None);
    }
    Ok(Some(
        bilibi_company::Directory
            .reachable(client, scope)
            .await?
            .into_iter()
            .map(|company| company.id.get())
            .collect(),
    ))
}
/// The SQL test that team alias `t` is visible to a reader whose reach is
/// the bigint[] parameter `reach`: a team without a company, or one of a
/// company the reader works in.
pub(crate) fn seen(t: &str, reach: &str) -> String {
    format!("({t}.company_id IS NULL OR {reach}::bigint[] IS NULL OR {t}.company_id=ANY({reach}::bigint[]))")
}
/// A team the reader may see; one they may not is `NotFound`, which reveals
/// nothing.
pub(crate) async fn load_visible(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    id: i64,
    reach: &Option<Vec<i64>>,
) -> Result<Team> {
    client
        .query_opt(
            &format!(
                "SELECT t.id,t.name,t.kind,{} AS parent_id,t.company_id,t.version FROM core_teams t WHERE t.tenant_id=$1 AND t.id=$2 AND {}",
                seen_parent("t", "$3"),
                seen("t", "$3")
            ),
            &[&scope.tenant(), &id, reach],
        )
        .await?
        .as_ref()
        .map(Team::row)
        .ok_or(Error::NotFound)
}
/// The SQL value of team alias `t`'s parent id, or null when the parent is
/// hidden from the reader (see [`seen`]), so no hidden id leaks.
pub(crate) fn seen_parent(t: &str, reach: &str) -> String {
    format!(
        "(SELECT p.id FROM core_teams p WHERE p.tenant_id={t}.tenant_id AND p.id={t}.parent_id AND {})",
        seen("p", reach)
    )
}
/// `teams.manage`, and for a team of a company, work access there and a
/// company that is not archived (`Some` refusal). A team never reaches a
/// company its manager cannot already work in.
pub(crate) async fn authorize_team(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    company: Option<i64>,
    lock: bool,
) -> Result<Option<FieldErrors>> {
    authorize_key(client, scope, MANAGE).await?;
    let Some(company) = company else {
        return Ok(None);
    };
    bilibi_company::authorize_work_target(client, scope, company, MANAGE).await?;
    match bilibi_company::require_writable(client, scope, company, lock).await {
        Ok(_) => Ok(None),
        Err(bilibi_company::WriteError::Archived) => Ok(Some(invalid(
            "company_id",
            "This team's company is archived, so the team cannot change.",
        ))),
        Err(bilibi_company::WriteError::Access(error)) => Err(error),
        Err(_) => Err(Error::NotFound),
    }
}
