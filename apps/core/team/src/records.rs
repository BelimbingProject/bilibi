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
            Ok(
                if client
                    .query_opt(
                        "SELECT id FROM core_teams WHERE tenant_id=$1 AND id=$2",
                        &[&scope.tenant(), &id],
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
            "SELECT id,name,kind,parent_id,company_id,version FROM core_teams WHERE tenant_id=$1",
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
