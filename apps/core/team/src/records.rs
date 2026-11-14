//! The `team` record kind and the `teams` Grid table, so other modules can
//! point at a team and lists can pick its columns.
use bilibi_database::Error;
use bilibi_records::{Access, RecordKind, RecordOwner};
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
struct TeamRecords;
static RECORDS: TeamRecords = TeamRecords;
impl RecordOwner for TeamRecords {
    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        id: &'a str,
    ) -> ProofFuture<'a, Access> {
        Box::pin(async move {
            match bilibi_authz::authorize_proof(client, scope, crate::VIEW).await {
                Ok(()) => {}
                Err(Error::Forbidden) => return Ok(Access::Forbidden),
                Err(e) => return Err(e),
            }
            let Ok(id) = id.parse::<i64>() else {
                return Ok(Access::NotFound);
            };
            let reach = crate::access::reach(client, scope).await?;
            Ok(
                if client
                    .query_opt(
                        &format!(
                            "SELECT t.id FROM core_teams t WHERE t.tenant_id=$1 AND t.id=$2 AND {}",
                            crate::access::seen("t", "$3")
                        ),
                        &[&scope.tenant(), &id, &reach],
                    )
                    .await?
                    .is_some()
                {
                    Access::Readable
                } else {
                    Access::NotFound
                },
            )
        })
    }
}
bilibi_records::declare!(
    "core/team",
    RecordKind::new("team", "teams", &RECORDS).page("/teams/{id}")
);
use bilibi_grid::{Field, FieldType, GridContribution, Source, TableDecl};
static GRID: GridContribution = GridContribution {
    record_fields: &[],
    tables: &[TableDecl::new(
        "teams",
        "Teams",
        crate::VIEW,
        Source::tenant(
            "SELECT t.id,t.name,t.kind,t.parent_id,t.company_id,t.version FROM core_teams t WHERE t.tenant_id=$1 AND (t.company_id IS NULL OR EXISTS(SELECT 1 FROM user_desks d JOIN users u ON u.tenant_id=d.tenant_id AND u.person_id=d.person_id WHERE d.tenant_id=t.tenant_id AND d.company_id=t.company_id AND u.id=$2 AND u.kind='staff'))",
        ),
        "id",
        &[
            Field::new("id", "ID", FieldType::Integer).protected(),
            Field::new("name", "Name", FieldType::String).protected(),
            Field::new("kind", "Kind", FieldType::String).protected(),
            Field::new("parent_id", "Parent team ID", FieldType::Integer).protected(),
            Field::new("company_id", "Company ID", FieldType::Integer).protected(),
            Field::new("version", "Version", FieldType::Integer).protected(),
        ],
    )
    .label_field("name")
    .record_types(&["team"])],
    links: &[],
};
bilibi_grid::contribute!("core/team", "team.grid", &GRID);
