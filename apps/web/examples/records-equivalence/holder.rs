//! Disposable reference holder; no product routes or UI.
use bilibi_database::{Change, Db, MutationBoundary, Result};
use bilibi_grid::{Field, FieldType, GridContribution, Source, TableDecl};
use bilibi_module_registry::{Layer, Module};
use bilibi_records::{Holder, Records};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};

pub const TABLE: &str = "records_equivalence_notes";
pub const KIND: &str = "records_equivalence.note";
pub const VIEW: &str = "admin.company.list";

inventory::submit! {
    Module {
        id: "extensions/records_equivalence", layer: Layer::Extensions,
        dependencies: &["base/database", "base/grid", "base/records"],
        migrations: &[("extensions-records-equivalence-001-notes",
            "CREATE TABLE records_equivalence_notes (id bigint PRIMARY KEY, tenant_id bigint NOT NULL REFERENCES tenants(id), record_kind text NOT NULL, record_id text NOT NULL, owner_id bigint); CREATE INDEX records_equivalence_notes_source_idx ON records_equivalence_notes(tenant_id,record_kind,record_id,id);")],
        services: &[],
    }
}
static GRID: GridContribution = GridContribution {
    record_fields: &[],
    tables: &[TableDecl::new(
        TABLE, "Notes", VIEW,
        Source::tenant("SELECT id,record_kind,record_id,owner_id FROM records_equivalence_notes WHERE tenant_id=$1"),
        "id",
        &[
            Field::new("id", "ID", FieldType::Integer),
            Field::new("record_kind", "Kind", FieldType::String),
            Field::new("record_id", "Record", FieldType::String),
            Field::new("owner_id", "Owner", FieldType::Integer),
        ],
    ).record_types(&[KIND])],
    links: &[],
};
bilibi_grid::contribute!(
    "extensions/records_equivalence",
    "records_equivalence.grid",
    &GRID
);
bilibi_records::holder!(
    "extensions/records_equivalence",
    Holder {
        table: TABLE,
        kind_field: "record_kind",
        id_field: "record_id"
    }
);

pub async fn insert(
    db: &Db,
    boundary: &MutationBoundary,
    records: &Records,
    scope: &Scope,
    notes: &[(i64, &str, &str, Option<i64>)],
) -> Result<()> {
    let mut client = db.get().await?;
    let mut tx = boundary
        .begin(
            &mut client,
            scope.mutation_context("records_equivalence.create"),
        )
        .await?;
    for &(id, kind, source, owner) in notes {
        let checked = records
            .check(tx.sql(), scope, kind, source)
            .await?
            .map_err(|_| bilibi_database::Error::Invalid)?;
        let tenant = scope.tenant();
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute("INSERT INTO records_equivalence_notes(id,tenant_id,record_kind,record_id,owner_id) VALUES($1,$2,$3,$4,$5)", &[&id,&tenant,&checked.kind,&checked.id,&owner]).await?;
            Ok(((), vec![Change { kind: KIND.into(), id, old: json!({}), new: json!({"record_kind": checked.kind, "record_id": checked.id, "owner_id": owner}) }]))
        })).await?;
    }
    tx.commit().await
}

/// Compare the complete holder rows and their complete mutation evidence.
pub async fn snapshot(db: &Db, scope: &Scope) -> Result<(Value, Value)> {
    let client = db.get().await?;
    let rows = client.query_one("SELECT coalesce(jsonb_agg(to_jsonb(n) ORDER BY id),'[]') FROM records_equivalence_notes n WHERE tenant_id=$1", &[&scope.tenant()]).await?.get(0);
    let audit = client.query_one("SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY id),'[]') FROM base_audit_mutations a WHERE tenant_id=$1 AND auditable_type=$2", &[&scope.tenant(),&KIND]).await?.get(0);
    Ok((rows, audit))
}
