//! A table's subject (#877): the record each row is about, read as the
//! record its stored id names now. Base Records owns merges, so it binds the
//! one [`Survivors`] relation here and Grid never reads its storage by name;
//! who may read a subject is the reader's scoped catalog, as for any row.
use crate::{FieldType, catalog::Catalog, model::Table, query::ident};
use bilibi_module_registry::RegistryError;
use std::sync::OnceLock;

/// What the module owning record merges states: `relation` is one static
/// `SELECT` of `tenant_id` (bigint), `kind`, `from_id` and `to_id` (text),
/// a row per merged-away record with the survivor it reads as now. A survivor
/// is never itself merged, so one lookup reaches it.
#[derive(Clone, Copy, Debug)]
pub struct Survivors {
    pub relation: &'static str,
}

static SURVIVORS: OnceLock<Survivors> = OnceLock::new();

/// Binds the merge relation once at boot (`base/records` does).
pub fn bind_survivors(survivors: Survivors) -> bilibi_module_registry::Result<()> {
    SURVIVORS
        .set(survivors)
        .map_err(|_| RegistryError::Service("duplicate Grid survivor binding".into()))
}

pub(crate) fn survivor(subject: crate::Subject, ty: FieldType, raw: &str) -> String {
    let Some(survivors) = SURVIVORS.get() else {
        return raw.into();
    };
    let id = format!(
        "COALESCE((SELECT grid_merged.to_id FROM ({}) grid_merged WHERE grid_merged.tenant_id = $1 AND grid_merged.kind = '{}' AND grid_merged.from_id = ({raw})::text), ({raw})::text)",
        survivors.relation, subject.kind
    );
    if ty == FieldType::Integer {
        format!(
            "(SELECT CASE WHEN pg_input_is_valid(grid_id.id, 'bigint') THEN grid_id.id::bigint END FROM (SELECT {id} AS id) grid_id)"
        )
    } else {
        id
    }
}

/// Keeps the rows of `table`, aliased `alias`, whose subject the reader may
/// read: a row of the kind's own table in this catalog, through its scoped
/// source. A kind no readable table claims (not granted, or its module not
/// mounted) admits only rows without a subject. `None` when the table
/// declares no subject.
pub(crate) fn readable(catalog: &Catalog, table: &Table, alias: &str) -> Option<String> {
    let subject = table.subject().filter(|_| table.report_subjects)?;
    let field = table.decl.fields.iter().find(|f| f.id == subject.field)?;
    let raw = format!("{alias}.{}", ident(field.source_key()));
    let Some(target) = catalog.record_table(subject.kind) else {
        return Some(format!("{raw} IS NULL"));
    };
    let survivor = survivor(subject, field.ty, &raw);
    let key = format!("grid_subject.{}", ident(target.key_field().source_key()));
    let native = field.ty == FieldType::Integer && target.key_field().ty == FieldType::Integer;
    let (survivor, key) = if native {
        (survivor, key)
    } else {
        (format!("({survivor})::text"), format!("({key})::text"))
    };
    Some(format!(
        "({raw} IS NULL OR {survivor} IN (SELECT {key} FROM {} grid_subject))",
        target.scoped_source()
    ))
}
