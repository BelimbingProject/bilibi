//! Contribution data and the validated catalog it becomes. Contributors declare
//! static tables, fields and links; nothing here queries a database, and no part
//! of a path or SQL identifier ever comes from a client.
use bilibi_module_registry::{Entry, Provenance, RegistryError, Result};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_TABLES: usize = 512;
pub const MAX_FIELDS: usize = 64;
pub const MAX_LINKS_PER_TABLE: usize = 64;
const MAX_ID: usize = 64;
const MAX_LABEL: usize = 120;
const MAX_SQL: usize = 20_000;

use crate::{Field, FieldType};

/// The owner's statement for the rows a scope may read. It is static SQL that the
/// grid wraps in a subquery, so every hop carries its owner's boundary and the
/// grid adds no tenant predicate of its own. A tenant-scoped source takes the
/// tenant id as `$1` (bigint) and may take the actor's user id as `$2`. A
/// company-scoped source also takes the selected company as `$3` (bigint); it
/// must reference all three parameters, even when it needs `$2` only as a
/// proof that an actor was supplied. A global source (reference data with no
/// tenant) takes none. It must expose every declared field under its `column`
/// key, and the keys it selects must not depend on the scope.
#[derive(Clone, Copy, Debug)]
pub struct Source {
    pub sql: &'static str,
    pub tenant_scoped: bool,
    pub company_scoped: bool,
    /// Other Grid tables whose fields supply projected values. Consumers
    /// that cache results use these table ids to pin their field restrictions.
    pub extra_tables: &'static [&'static str],
}
impl Source {
    pub const fn tenant(sql: &'static str) -> Self {
        Self {
            sql,
            tenant_scoped: true,
            company_scoped: false,
            extra_tables: &[],
        }
    }
    /// Rows of the selected company. `$1`, `$2` and `$3` are the tenant,
    /// actor and sealed current company respectively.
    pub const fn company(sql: &'static str) -> Self {
        Self {
            sql,
            tenant_scoped: true,
            company_scoped: true,
            extra_tables: &[],
        }
    }
    pub const fn global(sql: &'static str) -> Self {
        Self {
            sql,
            tenant_scoped: false,
            company_scoped: false,
            extra_tables: &[],
        }
    }
    /// Declares other Grid tables supplying projected values, so cached output is
    /// invalidated when any of their field restrictions change.
    pub const fn reads(self, tables: &'static [&'static str]) -> Self {
        Self {
            extra_tables: tables,
            ..self
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TableDecl {
    pub id: &'static str,
    pub label: &'static str,
    list_address: Option<&'static str>,
    /// What an account needs to read any row; checked before a path may touch it.
    pub capability: &'static str,
    pub source: Source,
    /// The field that identifies a row.
    pub key: &'static str,
    /// The field that names a row to a person.
    pub label_field: Option<&'static str>,
    /// The date field a rollup orders by for "latest" and buckets by for a trend.
    pub time_field: Option<&'static str>,
    pub fields: &'static [Field],
    /// The audit `auditable_type` values rows are recorded under, so a field
    /// restriction reaches the audit views of those rows. One table claims a type.
    pub record_types: &'static [&'static str],
    /// The field holding the account that owns a row (a deal's owner, a
    /// case's assignee), for record reach (#683). Required exactly when the
    /// table's capability supports reach (`CapabilityDefinition::with_reach`).
    pub owner: Option<&'static str>,
    /// The field holding the Core team a row is filed under (a case's queue
    /// team), which "my team's" reach also admits. Needs `owner`.
    pub team: Option<&'static str>,
    /// The record each row is about; see [`Subject`].
    pub subject: Option<Subject>,
}

/// The record a table's rows are about (an escalation's customer): the field
/// holding its id and its Base Records kind (#877). The field then reads as
/// the record that id names now, so rows about records merged since group and
/// filter under their survivor, and the report builder's reads
/// ([`crate::report_summary`], [`crate::report_page`]) keep rows whose
/// subject passes the Base Records owner's read rule. A row
/// without a subject stays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Subject {
    pub field: &'static str,
    pub kind: &'static str,
}
impl TableDecl {
    pub const fn new(
        id: &'static str,
        label: &'static str,
        capability: &'static str,
        source: Source,
        key: &'static str,
        fields: &'static [Field],
    ) -> Self {
        Self {
            id,
            label,
            list_address: None,
            capability,
            source,
            key,
            label_field: None,
            time_field: None,
            fields,
            record_types: &[],
            owner: None,
            team: None,
            subject: None,
        }
    }
    /// Names the integer or text field holding the id of the record each
    /// row is about, and that record's Base Records kind. See [`Subject`].
    pub const fn subject(self, field: &'static str, kind: &'static str) -> Self {
        Self {
            subject: Some(Subject { field, kind }),
            ..self
        }
    }
    pub const fn list_address(self, address: &'static str) -> Self {
        Self {
            list_address: Some(address),
            ..self
        }
    }
    pub const fn label_field(self, field: &'static str) -> Self {
        Self {
            label_field: Some(field),
            ..self
        }
    }
    pub const fn time_field(self, field: &'static str) -> Self {
        Self {
            time_field: Some(field),
            ..self
        }
    }
    pub const fn record_types(self, types: &'static [&'static str]) -> Self {
        Self {
            record_types: types,
            ..self
        }
    }
    /// Names the integer field holding the account that owns a row. Grid
    /// then keeps a reader to the rows their reach on the table's capability
    /// admits (#683).
    pub const fn owner(self, field: &'static str) -> Self {
        Self {
            owner: Some(field),
            ..self
        }
    }
    /// Names the integer field holding the Core team a row is filed under.
    pub const fn team(self, field: &'static str) -> Self {
        Self {
            team: Some(field),
            ..self
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkKind {
    /// At most one row of `to` per row of `from`; its fields become plain columns.
    One,
    /// Several rows, rolled up into one cell.
    Many,
}
/// How a link joins: field ids on the two tables, or an edge query the declaring
/// module owns, selecting `from_key` and `to_key`. A one-link's edge must yield at
/// most one `to_key` per `from_key`, or joined rows multiply.
#[derive(Clone, Copy, Debug)]
pub enum Join {
    On {
        from_field: &'static str,
        to_field: &'static str,
    },
    Edge(Source),
}
/// A walkable relation. The declaring module need not own `from`: the module that
/// owns the edge is the one that knows it. It must own one end.
#[derive(Clone, Copy, Debug)]
pub struct LinkDecl {
    pub id: &'static str,
    pub label: &'static str,
    pub from: &'static str,
    pub to: &'static str,
    pub kind: LinkKind,
    pub join: Join,
}
impl LinkDecl {
    pub const fn on(
        id: &'static str,
        label: &'static str,
        kind: LinkKind,
        from: &'static str,
        from_field: &'static str,
        to: &'static str,
        to_field: &'static str,
    ) -> Self {
        Self {
            id,
            label,
            from,
            to,
            kind,
            join: Join::On {
                from_field,
                to_field,
            },
        }
    }
    pub const fn edge(
        id: &'static str,
        label: &'static str,
        kind: LinkKind,
        from: &'static str,
        to: &'static str,
        source: Source,
    ) -> Self {
        Self {
            id,
            label,
            from,
            to,
            kind,
            join: Join::Edge(source),
        }
    }
}

/// The payload a module contributes to `base/grid.tables`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RecordFields {
    pub table: &'static str,
    pub fields: &'static [bilibi_ui::RecordField],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GridContribution {
    /// The same schema used to label the owning detail page, including panels.
    pub record_fields: &'static [RecordFields],
    pub tables: &'static [TableDecl],
    pub links: &'static [LinkDecl],
}

/// A validated link with the module that declared it.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub decl: &'static LinkDecl,
    pub owner: &'static str,
}
impl Link {
    pub fn id(&self) -> &'static str {
        self.decl.id
    }
    pub fn label(&self) -> &'static str {
        self.decl.label
    }
    pub fn kind(&self) -> LinkKind {
        self.decl.kind
    }
    pub fn to(&self) -> &'static str {
        self.decl.to
    }
}

/// A validated table. A scoped catalog holds copies with restricted fields removed.
#[derive(Clone, Debug)]
pub struct Table {
    pub decl: &'static TableDecl,
    pub owner: &'static str,
    pub provenance: Provenance,
    pub(crate) record_fields: Vec<bilibi_ui::RecordField>,
    fields: Vec<Field>,
    links: Vec<Link>,
    protected: BTreeSet<&'static str>,
    /// In a scoped catalog: the reader's reach on an owner-declaring table
    /// when it is narrower than all (#683).
    reach: Option<bilibi_authz::Reach>,
    pub(crate) report_subjects: bool,
    /// In a scoped catalog: the tenant's custom fields among `fields`, by id,
    /// with their current label and value SQL over the row alias `{alias}`.
    custom: BTreeMap<&'static str, CustomField>,
}
#[derive(Clone, Debug)]
struct CustomField {
    label: String,
    sql: String,
    searchable: bool,
    reference: Option<&'static str>,
    watcher_condition: bool,
}
impl Table {
    pub fn list_address(&self) -> Option<&'static str> {
        self.decl.list_address
    }

    pub fn id(&self) -> &'static str {
        self.decl.id
    }
    pub fn label(&self) -> &'static str {
        self.decl.label
    }
    /// The field that names a row to a person, when the owner declares one.
    pub fn label_field(&self) -> Option<&'static str> {
        self.decl.label_field
    }
    pub fn key(&self) -> &'static str {
        self.decl.key
    }
    pub fn time_field(&self) -> Option<&'static str> {
        self.decl.time_field.filter(|id| self.field(id).is_some())
    }
    pub fn capability(&self) -> &'static str {
        self.decl.capability
    }
    pub fn source(&self) -> Source {
        self.decl.source
    }
    /// Declared fields in declaration order, hidden ones included.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }
    /// Check the whole declaration, not just this reader's visible subset:
    /// removing an unknown field cannot attest that its source was reviewed.
    pub fn oversight_classified(&self) -> bool {
        self.decl.fields.iter().all(|field| field.classified)
    }
    /// Fields marked personal, hidden ones included.
    pub fn personal_fields(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|field| field.personal.is_some())
    }
    /// Static detail fields contributed for records in this table.
    pub fn record_fields(&self) -> &[bilibi_ui::RecordField] {
        &self.record_fields
    }
    pub fn field(&self, id: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.id == id)
    }
    /// What a field is called: its declared label, or a tenant custom field's
    /// current label.
    pub fn field_label<'a>(&'a self, field: &'a Field) -> &'a str {
        self.custom
            .get(field.id)
            .map_or(field.label, |custom| custom.label.as_str())
    }
    /// Whether `id` is one of the tenant's custom fields in this catalog.
    pub fn is_custom(&self, id: &str) -> bool {
        self.custom.contains_key(id)
    }
    /// Whether `id` is a custom field the tenant offered to watcher conditions.
    pub fn custom_watcher_condition(&self, id: &str) -> bool {
        self.custom
            .get(id)
            .is_some_and(|custom| custom.watcher_condition)
    }
    pub fn custom_reference(&self, id: &str) -> Option<&'static str> {
        self.custom.get(id).and_then(|custom| custom.reference)
    }
    /// The record each row is about, when the owner declares one.
    pub fn subject(&self) -> Option<Subject> {
        self.decl.subject
    }
    /// The Base Records kind a field's value names: a tenant's custom record
    /// field, or the table's subject field.
    pub fn reference(&self, id: &str) -> Option<&'static str> {
        self.custom_reference(id)
            .or_else(|| self.report_subject(id).map(|subject| subject.kind))
    }
    pub(crate) fn report_subject(&self, id: &str) -> Option<Subject> {
        self.subject()
            .filter(|subject| self.report_subjects && subject.field == id)
    }
    pub(crate) fn remove_custom_references(&mut self) {
        self.fields.retain(|field| {
            !self
                .custom
                .get(field.id)
                .is_some_and(|custom| custom.reference.is_some())
        });
        self.custom.retain(|_, custom| custom.reference.is_none());
    }
    pub(crate) fn custom_sql(&self, id: &str, alias: &str) -> Option<String> {
        self.custom
            .get(id)
            .map(|custom| custom.sql.replace("{alias}", alias))
    }
    /// The values of the tenant's searchable custom fields (text, links,
    /// phone numbers, email) on the row `alias`, as SQL text expressions.
    pub fn custom_search_values(&self, alias: &str) -> Vec<String> {
        self.custom
            .values()
            .filter(|custom| custom.searchable)
            .map(|custom| custom.sql.replace("{alias}", alias))
            .collect()
    }
    pub(crate) fn add_custom(
        &mut self,
        field: Field,
        label: String,
        sql: String,
        searchable: bool,
        reference: Option<&'static str>,
        watcher_condition: bool,
    ) {
        self.custom.insert(
            field.id,
            CustomField {
                label,
                sql,
                searchable,
                reference,
                watcher_condition,
            },
        );
        self.fields.push(field);
    }
    /// The fields a column may show: declaration order, without hidden ones.
    pub fn visible_fields(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|field| !field.hidden)
    }
    /// Links starting at this table, in declaration order.
    pub fn links(&self) -> &[Link] {
        &self.links
    }
    pub fn link(&self, id: &str) -> Option<&Link> {
        self.links.iter().find(|link| link.decl.id == id)
    }
    pub fn key_field(&self) -> &Field {
        self.field(self.decl.key).expect("validated key")
    }
    /// Whether an operator may never restrict the field.
    pub fn is_protected(&self, field: &str) -> bool {
        self.protected.contains(field)
    }
    /// In a scoped catalog, how far the reader reaches into this table's
    /// rows when it is narrower than all; `None` is every row the source
    /// returns.
    pub fn reach(&self) -> Option<bilibi_authz::Reach> {
        self.reach
    }
    pub(crate) fn with_reach(mut self, reach: bilibi_authz::Reach) -> Self {
        self.reach = (reach != bilibi_authz::Reach::All).then_some(reach);
        self
    }
    /// The owner's statement for the rows this reader may read, as a
    /// parenthesized subquery: the source itself, or, when the reader's reach
    /// is narrower than all, the source kept to the rows the reach admits.
    /// It binds the tenant as `$1` and the reader as `$2`, as every Grid
    /// statement does. Put this, never `source().sql`, into a statement that
    /// reads rows.
    pub fn scoped_source(&self) -> String {
        let sql = self.decl.source.sql;
        match (self.reach, self.decl.owner) {
            (Some(reach), Some(owner)) => {
                let column = |id: &'static str| {
                    let key = self
                        .decl
                        .fields
                        .iter()
                        .find(|field| field.id == id)
                        .map_or(id, Field::source_key);
                    format!("grid_reach.\"{}\"", key.replace('"', "\"\""))
                };
                let team = self.decl.team.map(column);
                format!(
                    "(SELECT * FROM ({sql}) grid_reach WHERE {})",
                    reach.predicate(&column(owner), team.as_deref())
                )
            }
            _ => format!("({sql})"),
        }
    }
    /// A link to a table the actor cannot read does not exist for them: a path
    /// through it is refused exactly as an unknown one is, revealing nothing.
    pub(crate) fn retain_links(&mut self, visible: impl Fn(&Link) -> bool) {
        self.links.retain(visible);
    }
    pub(crate) fn without(&self, restricted: &BTreeSet<&str>) -> Self {
        let mut copy = self.clone();
        copy.fields.retain(|field| !restricted.contains(field.id));
        copy
    }
}

fn invalid(message: String) -> RegistryError {
    RegistryError::Contribution(message)
}
fn where_(provenance: &Provenance) -> String {
    format!(
        "{} ({}:{})",
        provenance.module, provenance.source, provenance.line
    )
}
pub fn id_valid(id: &str) -> bool {
    let mut chars = id.bytes();
    id.len() <= MAX_ID
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}
fn label_valid(label: &str) -> bool {
    !label.trim().is_empty() && label.len() <= MAX_LABEL
}
fn source_valid(source: &Source) -> bool {
    let sql = source.sql.trim();
    let head = sql
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    !sql.is_empty()
        && sql.len() <= MAX_SQL
        && (head == "select" || head == "with")
        && !sql.contains(';')
        && source.sql.contains("$1") == source.tenant_scoped
        && source.sql.contains("$3") == source.company_scoped
        && (!source.company_scoped || source.sql.contains("$2"))
}

/// Every installed table, validated into one catalog.
#[derive(Debug, Default)]
pub struct Installed {
    tables: BTreeMap<&'static str, Table>,
}
impl Installed {
    /// Validates the complete sorted set. Any defect fails the snapshot build, so a
    /// broken catalog never reaches a page. Nothing here runs a source.
    pub fn from_entries(entries: &[Entry]) -> Result<Self> {
        let mut tables: BTreeMap<&'static str, Table> = BTreeMap::new();
        let mut pending = Vec::new();
        let mut types: BTreeMap<&str, &str> = BTreeMap::new();
        for entry in entries {
            let contribution = entry.get::<GridContribution>()?;
            let owner = entry.provenance.module;
            let at = where_(&entry.provenance);
            for decl in contribution.tables {
                let table = Table::new(decl, owner, entry.provenance, &at)?;
                for kind in decl.record_types {
                    if let Some(first) = types.insert(kind, decl.id) {
                        return Err(invalid(format!(
                            "grid record type {kind} claimed by {first} and {} from {at}",
                            decl.id
                        )));
                    }
                }
                if tables.insert(decl.id, table).is_some() {
                    return Err(invalid(format!(
                        "duplicate grid table {} from {at}",
                        decl.id
                    )));
                }
                if tables.len() > MAX_TABLES {
                    return Err(invalid(format!("more than {MAX_TABLES} grid tables")));
                }
            }
            pending.extend(
                contribution
                    .links
                    .iter()
                    .map(|link| (link, entry.provenance)),
            );
        }
        for (link, provenance) in pending {
            link_into(&mut tables, link, &provenance)?;
        }
        for table in tables.values() {
            for extra in table.source().extra_tables {
                if !tables.contains_key(extra) {
                    return Err(invalid(format!(
                        "grid table {} names unknown extra source table {extra}",
                        table.id()
                    )));
                }
            }
        }
        for entry in entries {
            let contribution = entry.get::<GridContribution>()?;
            for schema in contribution.record_fields {
                let table = tables.get_mut(schema.table).ok_or_else(|| {
                    invalid(format!("record fields name unknown table {}", schema.table))
                })?;
                if table.owner != entry.provenance.module
                    && !contribution
                        .links
                        .iter()
                        .any(|link| link.from == schema.table)
                {
                    return Err(invalid(format!(
                        "record fields for {} must own the table or a panel link",
                        schema.table
                    )));
                }
                for field in schema.fields {
                    if field.id.is_empty()
                        || field.label.is_empty()
                        || field.column.is_none() && field.exclusion.is_none()
                    {
                        return Err(invalid(format!(
                            "invalid record field for {}",
                            schema.table
                        )));
                    }
                    if let Some(column) = field.column {
                        if table
                            .record_fields
                            .iter()
                            .any(|previous| previous.column == Some(column))
                        {
                            return Err(invalid(format!(
                                "duplicate record column {column} on {}",
                                schema.table
                            )));
                        }
                    }
                    table.record_fields.push(*field);
                }
            }
        }
        for table in tables.values() {
            for field in table.fields() {
                if let Some(vocabulary) = field.vocabulary {
                    let canonical =
                        bilibi_ui::record_field(vocabulary.vocabulary).ok_or_else(|| {
                            invalid(format!("unknown vocabulary {}", vocabulary.vocabulary))
                        })?;
                    let expected = Field::record(*canonical);
                    if field.id != expected.id
                        || field.label != expected.label
                        || field.ty != expected.ty
                        || field.values != expected.values
                        || field.picklist != expected.picklist
                        || field.personal != expected.personal
                        || field.classified != expected.classified
                    {
                        return Err(invalid(format!(
                            "field {} on {} disagrees with {}",
                            field.id,
                            table.id(),
                            vocabulary.vocabulary
                        )));
                    }
                }
            }
            for field in &table.record_fields {
                if let Some(path) = field.column {
                    if path == field.id
                        && table.field(path).is_some_and(|column| {
                            column
                                .vocabulary
                                .is_none_or(|reference| reference.vocabulary != field.vocabulary)
                        })
                    {
                        return Err(invalid(format!(
                            "record field {} on {} must reference its vocabulary",
                            field.id,
                            table.id()
                        )));
                    }
                    crate::column::resolve(path, table, &|id| tables.get(id)).map_err(|error| {
                        invalid(format!(
                            "record field {} on {}: {error}",
                            field.id,
                            table.id()
                        ))
                    })?;
                }
            }
        }
        Ok(Self { tables })
    }
    pub fn from_inventory() -> Result<Self> {
        let catalog =
            bilibi_module_registry::Catalog::discover(&bilibi_module_registry::discover()?)?;
        Self::from_entries(catalog.entries::<GridContribution>(crate::TABLES)?)
    }
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.tables.values()
    }
    pub fn table(&self, id: &str) -> Option<&Table> {
        self.tables.get(id)
    }
    pub(crate) fn map(&self) -> &BTreeMap<&'static str, Table> {
        &self.tables
    }
}

impl Table {
    fn new(
        decl: &'static TableDecl,
        owner: &'static str,
        provenance: Provenance,
        at: &str,
    ) -> Result<Self> {
        let bad = |why: String| invalid(format!("grid table {} from {at}: {why}", decl.id));
        if !id_valid(decl.id) || !label_valid(decl.label) {
            return Err(bad("invalid id or label".into()));
        }
        if !bilibi_authz::vocabulary::valid_key(decl.capability) {
            return Err(bad(format!("invalid capability key {}", decl.capability)));
        }
        if !source_valid(&decl.source) {
            return Err(bad(
                "source must be one SELECT/WITH statement that takes $1 exactly when tenant-scoped and all of $1/$2/$3 when company-scoped"
                    .into(),
            ));
        }
        if decl.fields.is_empty() || decl.fields.len() > MAX_FIELDS {
            return Err(bad(format!("declare 1 to {MAX_FIELDS} fields")));
        }
        let mut seen = BTreeSet::new();
        for field in decl.fields {
            if !id_valid(field.id)
                || !label_valid(field.label)
                || !seen.insert(field.id)
                || field.column.is_some_and(|column| !id_valid(column))
            {
                return Err(bad(format!("invalid or duplicate field {}", field.id)));
            }
            let mut values = BTreeSet::new();
            let closed = field.ty == FieldType::Enum;
            if closed != !field.values.is_empty()
                || !field
                    .values
                    .iter()
                    .all(|v| label_valid(v) && values.insert(*v))
            {
                return Err(bad(format!(
                    "field {} needs distinct enum values exactly when it is an enum",
                    field.id
                )));
            }
        }
        for (name, id) in [
            ("key", Some(decl.key)),
            ("label field", decl.label_field),
            ("time field", decl.time_field),
        ] {
            if id.is_some_and(|id| !seen.contains(id)) {
                return Err(bad(format!("{name} is not a declared field")));
            }
        }
        if decl
            .time_field
            .and_then(|id| decl.fields.iter().find(|f| f.id == id))
            .is_some_and(|field| !field.ty.dated())
        {
            return Err(bad("time field must be a date or datetime".into()));
        }
        for (name, id) in [("owner", decl.owner), ("team", decl.team)] {
            if id.is_some_and(|id| {
                decl.fields
                    .iter()
                    .find(|f| f.id == id)
                    .is_none_or(|field| field.ty != FieldType::Integer)
            }) {
                return Err(bad(format!("{name} must be a declared integer field")));
            }
        }
        if decl.team.is_some() && decl.owner.is_none() {
            return Err(bad("a team field needs an owner field".into()));
        }
        if let Some(subject) = decl.subject {
            let field_valid = subject.field != decl.key
                && decl
                    .fields
                    .iter()
                    .find(|f| f.id == subject.field)
                    .is_some_and(|f| matches!(f.ty, FieldType::Integer | FieldType::String));
            if !field_valid || !decl.source.tenant_scoped {
                return Err(bad(
                    "a subject names a declared integer or text field other than the key and needs a tenant source"
                        .into(),
                ));
            }
        }
        let mut kinds = BTreeSet::new();
        if !decl
            .record_types
            .iter()
            .all(|kind| !kind.is_empty() && kind.len() <= 200 && kinds.insert(*kind))
        {
            return Err(bad("record types must be distinct and non-empty".into()));
        }
        let mut protected: BTreeSet<&'static str> =
            [Some(decl.key), decl.label_field, decl.time_field]
                .into_iter()
                .flatten()
                .collect();
        for field in decl.fields {
            if field.hidden || field.protected {
                protected.insert(field.id);
            }
        }
        Ok(Self {
            decl,
            record_fields: Vec::new(),
            owner,
            provenance,
            fields: decl.fields.to_vec(),
            links: Vec::new(),
            protected,
            reach: None,
            report_subjects: false,
            custom: BTreeMap::new(),
        })
    }
}

fn link_into(
    tables: &mut BTreeMap<&'static str, Table>,
    decl: &'static LinkDecl,
    provenance: &Provenance,
) -> Result<()> {
    let at = where_(provenance);
    let bad = |why: String| invalid(format!("grid link {} from {at}: {why}", decl.id));
    if !id_valid(decl.id) || !label_valid(decl.label) {
        return Err(bad("invalid id or label".into()));
    }
    let from = tables.get(decl.from).ok_or_else(|| {
        bad(format!(
            "names table {}, which no module declares",
            decl.from
        ))
    })?;
    let to = tables
        .get(decl.to)
        .ok_or_else(|| bad(format!("names table {}, which no module declares", decl.to)))?;
    // The module that owns the edge knows it; one that owns neither end has no
    // standing to put a link between two other modules' tables.
    if from.owner != provenance.module && to.owner != provenance.module {
        return Err(bad(format!(
            "{} owns neither {} nor {}",
            provenance.module, decl.from, decl.to
        )));
    }
    if from.link(decl.id).is_some() || from.field(decl.id).is_some() {
        return Err(bad(format!(
            "id is already a field or link of {}",
            decl.from
        )));
    }
    if from.links.len() >= MAX_LINKS_PER_TABLE {
        return Err(bad(format!("{} has too many links", decl.from)));
    }
    let mut joined = Vec::new();
    match decl.join {
        Join::On {
            from_field,
            to_field,
        } => {
            if from.field(from_field).is_none() || to.field(to_field).is_none() {
                return Err(bad(format!(
                    "joins on {}.{from_field} and {}.{to_field}, which must both be declared",
                    decl.from, decl.to
                )));
            }
            if decl.kind == LinkKind::One && to_field != to.decl.key {
                return Err(bad(format!(
                    "a one-link joins on the target key {}, not {to_field}",
                    to.decl.key
                )));
            }
            joined.push((decl.from, from_field));
            joined.push((decl.to, to_field));
        }
        Join::Edge(source) => {
            if !source_valid(&source) || !source.tenant_scoped || source.company_scoped {
                return Err(bad(
                    "an edge must be one tenant-wide SELECT/WITH statement".into()
                ));
            }
        }
    }
    let link = Link {
        decl,
        owner: provenance.module,
    };
    tables.get_mut(decl.from).expect("checked").links.push(link);
    for (table, field) in joined {
        tables
            .get_mut(table)
            .expect("checked")
            .protected
            .insert(field);
    }
    Ok(())
}
