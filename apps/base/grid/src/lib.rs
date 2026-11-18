//! Grid table/field/link catalog, scoped paths, bounded page projections,
//! grouped summaries and filtered row pages (the report builder's reads), the
//! lenses that turn them into FlexTable cells, and what each account arranged
//! on each list page. There is no Grid page: a list hosts the walked columns
//! through `PageColumns`; see `README.md`.
pub mod catalog;
pub mod column;
mod custom;
mod field;
pub mod lens;
mod model;
pub use field::{Field, FieldType, PersonalKind};
mod oversight;
pub mod page_columns;
pub mod page_views;
pub mod query;
pub mod reference;
mod shared_views;
mod summary;
pub mod view;
mod view_operations;
pub use lens::{Lens, Range};
pub use oversight::{
    ChildResult, OversightData, OversightQueries, OversightResult, OversightSpec, OVERSIGHT_QUERIES,
};
pub use page_columns::{Outcome, PageColumns, ViewCaller};
pub use page_views::{PageViews, PAGE_VIEWS};
pub use query::{
    project, project_lenses, project_on, project_within, scalar_access, scalar_expression,
    scalar_join, stats, Lenses, Projection, QueryError, Row, ScalarJoin, Stats, BOUNDS,
    MAX_ESTIMATED_COST,
};
pub use shared_views::{Audience, SharedView, SHARE as SHARE_VIEWS};
pub use summary::{
    page_keys, page_keys_on, summarize, summarize_on, Bounds, Bucket, Filter, Grouping, KeyPage,
    Line, Measure, PageSpec, Period, Sort, Summary, SummarySpec, Test, MAX_CHOICES, MAX_COST_BOUND,
    MAX_FILTERS, MAX_GROUPINGS, MAX_MEASURES, MAX_OFFSET, MAX_RESULT_GROUPS, MAX_SORTS,
    MAX_STATEMENT_MS, MAX_TEXT,
};
pub use view::View;

use bilibi_module_registry::{Consumer, Layer, Module, RegistryError, Result, Service};
use std::sync::OnceLock;

pub use catalog::{
    restrictable, restriction_catalog, Catalog, CatalogError, Restrictable, MAX_COLUMNS,
};
pub use column::{Agg, Column, ColumnError, ColumnKind};
pub use custom::{
    bind_custom_columns, custom_field_id, custom_key, custom_tables, key_shaped, CustomColumn,
    CustomColumns, CustomFuture, CustomTable,
};
pub use model::{
    id_valid, GridContribution, Installed, Join, Link, LinkDecl, LinkKind, RecordFields, Source,
    Table, TableDecl, MAX_FIELDS, MAX_LINKS_PER_TABLE, MAX_TABLES,
};

#[doc(hidden)]
pub use bilibi_module_registry::{Contribution, Provenance};
#[doc(hidden)]
pub use inventory;

/// The registry consumer every module's `GridContribution` is submitted to.
pub const TABLES: &str = "base/grid.tables";

/// Contribute tables and links from the owning module:
/// `bilibi_grid::contribute!("core/x", "x.grid", &GRID)` with
/// `static GRID: GridContribution`.
#[macro_export]
macro_rules! contribute {
    ($module:literal, $id:literal, $payload:expr) => {
        $crate::inventory::submit! {
            $crate::Contribution::new(
                $crate::TABLES,
                $id,
                $crate::Provenance { module: $module, source: file!(), line: line!() },
                $payload,
            )
        }
    };
}

/// The immutable deployment snapshot, validated once. Grants and restrictions are
/// never part of it.
pub fn installed() -> Result<&'static Installed> {
    static SNAPSHOT: OnceLock<Result<Installed>> = OnceLock::new();
    SNAPSHOT
        .get_or_init(|| {
            let installed = Installed::from_inventory()?;
            validate_capabilities(
                &installed,
                bilibi_authz::vocabulary().map_err(|_| {
                    RegistryError::Contribution("authz vocabulary unavailable".into())
                })?,
            )?;
            Ok(installed)
        })
        .as_ref()
        .map_err(|error| RegistryError::Contribution(error.to_string()))
}

/// A typo in a table's capability must fail at boot, not hide the table forever.
pub fn validate_capabilities(
    installed: &Installed,
    vocabulary: &bilibi_authz::Vocabulary,
) -> Result<()> {
    for table in installed.tables() {
        let Some(capability) = vocabulary.capabilities.get(table.capability()) else {
            return Err(RegistryError::Contribution(format!(
                "grid table {} from {} ({}:{}) names undeclared capability {}",
                table.id(),
                table.owner,
                table.provenance.source,
                table.provenance.line,
                table.capability()
            )));
        };
        // Reach is applied through the owner field: a table read under a key
        // that supports reach without one would show every row (#683).
        if capability.definition.reach != table.decl.owner.is_some() {
            return Err(RegistryError::Contribution(format!(
                "grid table {} from {} ({}:{}): {} {}, so the table {} an owner field",
                table.id(),
                table.owner,
                table.provenance.source,
                table.provenance.line,
                table.capability(),
                if capability.definition.reach {
                    "supports record reach"
                } else {
                    "does not support record reach"
                },
                if capability.definition.reach {
                    "must declare"
                } else {
                    "cannot declare"
                },
            )));
        }
    }
    Ok(())
}

/// Grid supplies Authz's protected-field catalog (`bilibi_authz::field_catalog`)
/// as data: the restrictable fields of every installed table.
fn restriction_snapshot(
) -> std::result::Result<Vec<bilibi_authz::field_catalog::CatalogTable>, String> {
    use bilibi_authz::field_catalog::{CatalogField, CatalogTable};
    Ok(
        restriction_catalog(installed().map_err(|error| error.to_string())?)
            .into_iter()
            .map(|entry| CatalogTable {
                id: entry.table,
                label: entry.label,
                record_types: entry.record_types,
                fields: entry
                    .fields
                    .into_iter()
                    .map(|(id, label, personal)| CatalogField {
                        id: id.into(),
                        label: label.into(),
                        personal,
                        restriction_aliases: Vec::new(),
                    })
                    .collect(),
            })
            .collect(),
    )
}
inventory::submit! {
    Contribution::new(bilibi_authz::field_catalog::FIELD_CATALOG, "base/grid.restrictable",
        Provenance { module: "base/grid", source: file!(), line: line!() },
        &bilibi_authz::field_catalog::CatalogSource { snapshot: restriction_snapshot })
}

/// Grid supplies Labels' renamable record types and fields as data: every
/// installed table, and each table's non-hidden fields by `table.field`.
fn label_subjects() -> std::result::Result<Vec<bilibi_labels::Subject>, String> {
    use bilibi_labels::{subject_id, Kind, Subject};
    let installed = installed().map_err(|error| error.to_string())?;
    let mut subjects = Vec::new();
    for table in installed.tables() {
        subjects.push(Subject {
            kind: Kind::Record,
            id: table.id().to_owned(),
            default: table.label().to_owned(),
            trail: String::new(),
        });
        subjects.extend(table.visible_fields().map(|field| Subject {
            kind: Kind::Field,
            id: subject_id(table.id(), field.id),
            default: field.label.to_owned(),
            trail: table.label().to_owned(),
        }));
    }
    Ok(subjects)
}
inventory::submit! {
    Contribution::new(bilibi_labels::SUBJECTS, "base/grid.label_subjects",
        Provenance { module: "base/grid", source: file!(), line: line!() },
        &bilibi_labels::SubjectSource { snapshot: label_subjects })
}

inventory::submit! { Consumer::new::<GridContribution>(TABLES, "base/grid", |entries| Installed::from_entries(entries).map(|_| ())) }
inventory::submit! {
    Module {
        id: "base/grid", layer: Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/tenancy", "base/authz", "base/audit", "base/ui", "base/datetime", "base/settings", "base/personal_data", "base/agent_api", "base/labels"],
        migrations: &[
            ("base-grid-001", include_str!("../migrations/001-reference.sql")),
            ("base-grid-002", include_str!("../migrations/002-reference-recorded-at.sql")),
            ("base-grid-003-reference-tasks", include_str!("../migrations/003-reference-tasks.sql")),
            ("base-grid-004-shared-views", include_str!("../migrations/004-shared-views.sql")),
        ],
        services: &[Service { id: PAGE_VIEWS, factory: page_views::service }, Service { id: OVERSIGHT_QUERIES, factory: oversight::service }],
    }
}
inventory::submit! {
    bilibi_module_registry::Ready { module: "base/grid", check: |_| installed().map(|_| ()) }
}

/// A record field's word: the tenant's label for the Grid field it reads, or
/// the owner's own.
pub(crate) fn record_word(
    catalog: &Catalog,
    root: &Table,
    field: &bilibi_ui::RecordField,
) -> String {
    field
        .column
        .and_then(|path| catalog.resolve(root, path).ok())
        .and_then(|column| {
            let id = column.field.as_ref()?.id;
            catalog.field_name(column.table, id)
        })
        .unwrap_or_else(|| field.label.to_owned())
}

/// The words a record page shows for its table's Grid and record fields under one
/// tenant's field labels (`base/labels`). A detail page, form or inline
/// editor draws its labels with [`RecordWords::get`] instead of
/// `field.label`, so a rename shows there as it does in lists and exports.
/// Load it once per render; never keep it across requests or tenants.
#[derive(Clone, Debug, Default)]
pub struct RecordWords(std::collections::BTreeMap<&'static str, String>);
impl RecordWords {
    /// One read of the tenant's field labels, applied to visible Grid fields
    /// and registered record-field paths of `table`. An uninstalled table has no words.
    pub async fn load(
        client: &impl tokio_postgres::GenericClient,
        scope: &bilibi_tenancy::Scope,
        table: &str,
    ) -> std::result::Result<Self, bilibi_database::Error> {
        let names = bilibi_labels::field_labels(client, scope).await?;
        let installed = installed().map_err(|_| bilibi_database::Error::Internal)?;
        let Some(root) = installed.table(table) else {
            return Ok(Self::default());
        };
        let mut words: std::collections::BTreeMap<_, _> = root
            .visible_fields()
            .filter_map(|field| {
                names
                    .get(root.id(), field.id)
                    .map(|word| (field.id, word.to_owned()))
            })
            .collect();
        for field in &root.record_fields {
            let Some(column) = field
                .column
                .and_then(|path| column::resolve(path, root, &|id| installed.table(id)).ok())
            else {
                continue;
            };
            let Some(word) = column
                .field
                .as_ref()
                .and_then(|field| names.get(column.table, field.id))
            else {
                continue;
            };
            words.insert(field.vocabulary, word.to_owned());
            words.insert(field.id, word.to_owned());
        }
        Ok(Self(words))
    }
    /// The tenant's word for a Grid field without record vocabulary.
    pub fn field<'a>(&'a self, id: &str, default: &'a str) -> &'a str {
        self.0.get(id).map_or(default, String::as_str)
    }
    /// The tenant's word for `field`, or the owner's own.
    pub fn get<'a>(&'a self, field: &bilibi_ui::RecordField) -> &'a str {
        self.0
            .get(field.vocabulary)
            .map_or_else(|| self.field(field.id, field.label), String::as_str)
    }
}

pub async fn render_labels(
    client: &impl tokio_postgres::GenericClient,
    scope: &bilibi_tenancy::Scope,
) -> std::result::Result<bilibi_ui::FieldLabels, bilibi_database::Error> {
    let names = bilibi_labels::field_labels(client, scope).await?;
    let installed = installed().map_err(|_| bilibi_database::Error::Internal)?;
    let mut words = std::collections::BTreeMap::new();
    for root in installed.tables() {
        for field in root.visible_fields() {
            if let Some(word) = names.get(root.id(), field.id) {
                words.insert(format!("{}.{}", root.id(), field.id), word.to_owned());
            }
        }
        for field in &root.record_fields {
            if let Some(word) = field.column.and_then(|path| {
                let column = column::resolve(path, root, &|id| installed.table(id)).ok()?;
                names.get(column.table, column.field.as_ref()?.id)
            }) {
                words.insert(field.vocabulary.to_owned(), word.to_owned());
            }
        }
    }
    Ok(bilibi_ui::FieldLabels(words))
}
