use bilibi_database::Error;
use bilibi_records::{Access, RecordKind, RecordOwner};
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
struct ProductRecords;
static RECORDS: ProductRecords = ProductRecords;
impl ProductRecords {
    async fn check(
        &self,
        client: &dyn ProofClient,
        scope: &Scope,
        id: &str,
        select: bool,
    ) -> bilibi_database::Result<Access> {
        match bilibi_authz::authorize_proof(client, scope, crate::VIEW).await {
            Ok(()) => {}
            Err(Error::Forbidden) => return Ok(Access::Forbidden),
            Err(e) => return Err(e),
        }
        let Ok(id) = id.parse::<i64>() else {
            return Ok(Access::NotFound);
        };
        let row = client
            .query_opt(
                "SELECT status FROM core_products WHERE tenant_id=$1 AND id=$2",
                &[&scope.tenant(), &id],
            )
            .await?;
        Ok(match row {
            None => Access::NotFound,
            Some(row) if select && row.get::<_, String>(0) != "active" => Access::Forbidden,
            Some(_) => Access::Readable,
        })
    }
}
impl RecordOwner for ProductRecords {
    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        id: &'a str,
    ) -> ProofFuture<'a, Access> {
        Box::pin(self.check(client, scope, id, false))
    }
    fn select<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        id: &'a str,
    ) -> ProofFuture<'a, Access> {
        Box::pin(self.check(client, scope, id, true))
    }
}
bilibi_records::declare!(
    "core/product",
    RecordKind::new("product", "products", &RECORDS).page("/products/{id}")
);
use bilibi_grid::{Field, FieldType, GridContribution, Source, TableDecl};
static GRID:GridContribution=GridContribution{
    record_fields:&[],tables:&[TableDecl::new("products","Products",crate::VIEW,
        Source::tenant("SELECT id,code,name,kind,base_unit,category,status,version FROM core_products WHERE tenant_id=$1"),"id",&[
            Field::new("id","ID",FieldType::Integer).protected(),Field::new("code","Code",FieldType::String).protected(),Field::new("name","Name",FieldType::String).protected(),Field::choice("kind","Kind",&["goods","service"]).protected(),Field::new("base_unit","Base unit",FieldType::String).protected(),Field::new("category","Category",FieldType::String).protected(),Field::choice("status","Status",&["active","retired"]).protected(),Field::new("version","Version",FieldType::Integer).protected(),
        ]).label_field("name").record_types(&["product"])],links:&[],
};
bilibi_grid::contribute!("core/product", "product.grid", &GRID);
