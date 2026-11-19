//! The `CustomFields` facade: the tenant's field definitions for each record
//! type that opts in. Reads serve the page, agents and owners; every write is
//! one audited `Change` on `custom_field`. A definition's key and type never
//! change, and it is archived, never deleted, so the values records hold under
//! it stay readable and its key is never given to another field.
use crate::{
    declaration::{shaped, RecordType, LABEL_MAX},
    MANAGE, MAX_FIELDS, VIEW,
};
use bilibi_authz::authorize_key;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_picklists::{PickLists, Reach};
use bilibi_records::Records;
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{ReadClient, Scope};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use tokio_postgres::{GenericClient, Row};

pub const KEY_MAX: usize = 40;
pub const HELP_MAX: usize = 255;

const COLUMNS: &str =
    "id,record_type,key,label,help,kind,picklist,target,position,archived_on::text AS archived_on,version,rule_condition";

/// What a field holds. Fixed when the field is defined.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Free text, kept as a JSON string.
    Text,
    /// A decimal number, kept as an exact JSON string (`"12.50"`), never a
    /// float, so no digit is lost.
    Number,
    /// A calendar date, kept as `YYYY-MM-DD`.
    Date,
    /// A code of a standalone tenant pick-list (`Definition::picklist`).
    Choice,
    /// Yes or no, stored as the stable text true or false.
    Boolean,
    LongText,
    Link,
    Phone,
    Email,
    /// Several codes of one standalone tenant pick-list, kept as a JSON
    /// array of codes in the order chosen, each at most once.
    Choices,
    /// One record of a Base Records kind (`Definition::target`), kept as the
    /// record's id text; it is named with the reader's own access.
    Reference,
}
impl Kind {
    pub const ALL: [Kind; 11] = [
        Kind::Text,
        Kind::Number,
        Kind::Date,
        Kind::Choice,
        Kind::Boolean,
        Kind::LongText,
        Kind::Link,
        Kind::Phone,
        Kind::Email,
        Kind::Choices,
        Kind::Reference,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Number => "number",
            Self::Date => "date",
            Self::Choice => "choice",
            Self::Boolean => "boolean",
            Self::LongText => "long_text",
            Self::Link => "link",
            Self::Phone => "phone",
            Self::Email => "email",
            Self::Choices => "choices",
            Self::Reference => "reference",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Number => "Number",
            Self::Date => "Date",
            Self::Choice => "Pick-list",
            Self::Boolean => "Yes/no",
            Self::LongText => "Long text",
            Self::Link => "Link",
            Self::Phone => "Phone",
            Self::Email => "Email",
            Self::Choices => "Several choices",
            Self::Reference => "Record",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
    /// Whether the field names a pick-list its values come from.
    pub fn takes_picklist(self) -> bool {
        matches!(self, Self::Choice | Self::Choices)
    }
    /// Whether the field may be a Base Rules condition: one typed value a
    /// condition compares exactly (#730).
    pub fn rule_condition_kind(self) -> bool {
        matches!(
            self,
            Self::Text | Self::Number | Self::Date | Self::Choice | Self::Boolean
        )
    }
}

/// One tenant-defined field as stored.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Definition {
    pub id: i64,
    pub record_type: String,
    /// The name the value is stored under; never changes, never reused.
    pub key: String,
    pub label: String,
    pub help: Option<String>,
    pub kind: Kind,
    /// For a choice or several-choices field, the pick-list its values come from.
    pub picklist: Option<String>,
    /// For a record field, the Base Records kind it points at (`company`).
    pub target: Option<String>,
    pub position: i32,
    pub archived_on: Option<String>,
    pub version: i64,
    /// Whether the tenant opted it into Base Rules conditions; a rule never
    /// reads a field that is not.
    pub rule_condition: bool,
}
impl Definition {
    fn from_row(row: &Row) -> Result<Self> {
        Ok(Self {
            id: row.get("id"),
            record_type: row.get("record_type"),
            key: row.get("key"),
            label: row.get("label"),
            help: row.get("help"),
            kind: Kind::parse(row.get::<_, &str>("kind")).ok_or(Error::Internal)?,
            picklist: row.get("picklist"),
            target: row.get("target"),
            position: row.get("position"),
            archived_on: row.get("archived_on"),
            version: row.get("version"),
            rule_condition: row.get("rule_condition"),
        })
    }
    pub fn archived(&self) -> bool {
        self.archived_on.is_some()
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("custom field definition serializes")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FieldError {
    /// A form field of this module, or a custom field's key.
    pub field: String,
    pub message: String,
}
impl FieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}
/// A write's answer: the saved definition, or what the administrator must
/// fix. A stale version is a refusal on `version`.
pub type Checked<T> = std::result::Result<T, Vec<FieldError>>;

/// A new field. Its key and kind are fixed from here on.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct NewField {
    pub record_type: String,
    pub key: String,
    pub label: String,
    pub help: Option<String>,
    pub kind: String,
    pub picklist: Option<String>,
    /// For a record field, the kind of record it points at.
    pub target: Option<String>,
    /// Offer it to Base Rules conditions (text, number, date, pick-list and
    /// yes/no fields only).
    pub rule_condition: bool,
}
/// Everything but the key and the kind.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct FieldUpdate {
    pub label: String,
    pub help: Option<String>,
    /// Offer it to Base Rules conditions, or withdraw it; `None` keeps it.
    pub rule_condition: Option<bool>,
    pub version: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Archive,
    Restore,
}

pub struct CustomFields {
    pub(crate) record_types: BTreeMap<&'static str, &'static RecordType>,
    /// Where Grid reads each record type's values (`GridSource`), validated
    /// at boot; an operator restricts a field there as `cf_<key>`.
    pub(crate) sources: Vec<&'static crate::GridSource>,
    pub(crate) picklists: Arc<PickLists>,
    pub(crate) records: Arc<Records>,
    pub(crate) settings: Arc<Settings>,
}

fn change(old: serde_json::Value, saved: &Definition) -> Change {
    Change {
        kind: "custom_field".into(),
        id: saved.id,
        old,
        new: saved.to_json(),
    }
}

/// Serializes the writes of one tenant's record type (define, restore), so the
/// count and the key read after it cannot change underneath the write.
async fn lock(client: &impl GenericClient, tenant: i64, record_type: &str) -> Result<()> {
    client
        .execute(
            "SELECT pg_advisory_xact_lock(hashtextextended('base-custom-fields-' || $1::text || ':' || $2, 0))",
            &[&tenant.to_string(), &record_type],
        )
        .await?;
    Ok(())
}

fn text(
    errors: &mut Vec<FieldError>,
    label: &str,
    help: &Option<String>,
) -> (String, Option<String>) {
    let label = label.trim().to_owned();
    if label.is_empty() || label.chars().count() > LABEL_MAX || label.chars().any(char::is_control)
    {
        errors.push(FieldError::new(
            "label",
            format!("use 1 to {LABEL_MAX} characters"),
        ));
    }
    let help = help
        .as_deref()
        .map(str::trim)
        .filter(|help| !help.is_empty())
        .map(str::to_owned);
    if help
        .as_deref()
        .is_some_and(|help| help.chars().count() > HELP_MAX || help.chars().any(char::is_control))
    {
        errors.push(FieldError::new(
            "help",
            format!("use at most {HELP_MAX} characters"),
        ));
    }
    (label, help)
}

impl CustomFields {
    /// The record types mounted modules offer.
    pub fn record_types(&self) -> impl Iterator<Item = &'static RecordType> + '_ {
        self.record_types.values().copied()
    }
    pub fn record_type(&self, id: &str) -> Result<&'static RecordType> {
        self.record_types.get(id).copied().ok_or(Error::NotFound)
    }

    /// The pick-lists a choice field may use: the tenant's own, standalone,
    /// with no capability of their own beyond the pick-list page's.
    pub fn choice_lists(&self) -> Vec<(&'static str, &'static str)> {
        self.picklists
            .lists()
            .filter(|list| {
                list.reach == Reach::Tenant && list.parent.is_none() && list.capability.is_none()
            })
            .map(|list| (list.key, list.label))
            .collect()
    }

    /// The kinds of record a record field may point at: every mounted Base
    /// Records kind, by key, with its name for people ("Companies").
    pub fn reference_kinds(&self) -> Vec<(&'static str, &'static str)> {
        let mut kinds: Vec<_> = self
            .records
            .kinds()
            .map(|kind| (kind.key, self.records.kind_label(kind.key)))
            .collect();
        kinds.sort_by_key(|(key, label)| (*label, *key));
        kinds
    }

    /// The Base Records facade a record field's values are checked and named
    /// through, for owners that show them.
    pub fn records(&self) -> &Records {
        &self.records
    }

    /// The most active fields one record type may have, a tenant setting.
    async fn limit(&self, client: &dyn ReadClient, scope: &Scope) -> Result<i64> {
        self.settings
            .get(client, scope, Target::Tenant, MAX_FIELDS)
            .await?
            .value
            .as_i64()
            .ok_or(Error::Internal)
    }

    /// Every definition of the tenant for `record_type` (all types when
    /// `None`), in the order they were defined; archived ones on request.
    /// Needs `base.custom-fields.view`.
    pub async fn definitions(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        record_type: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<Definition>> {
        authorize_key(client, scope, VIEW).await?;
        if let Some(record_type) = record_type {
            self.record_type(record_type)?;
        }
        let rows = client
            .query(
                &format!("SELECT {COLUMNS} FROM base_custom_fields WHERE tenant_id=$1 AND ($2::text IS NULL OR record_type=$2) AND ($3 OR archived_on IS NULL) ORDER BY record_type,position,id"),
                &[&scope.tenant(), &record_type, &include_archived],
            )
            .await?;
        rows.iter()
            .filter(|row| {
                self.record_types
                    .contains_key(row.get::<_, &str>("record_type"))
            })
            .map(Definition::from_row)
            .collect()
    }

    /// The fields a record of `record_type` currently offers, for an owner
    /// that has already authorized its own page or write. Needs no
    /// custom-field capability: a person who may open a company may see the
    /// company's fields. A field an operator withholds from this reader
    /// (Field Restrictions, `cf_<key>` on the record type's Grid table) is
    /// left out, so pages do not show it and writes refuse it.
    pub async fn active(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        record_type: &str,
    ) -> Result<Vec<Definition>> {
        let mut fields = self.defined(client, scope, record_type).await?;
        if let Some(source) = self.sources.iter().find(|s| s.record_type == record_type) {
            let restricted = bilibi_authz::fields::restricted_read(client, scope).await?;
            if let Some(withheld) = restricted.get(source.table) {
                fields.retain(|field| !withheld.contains(bilibi_grid::custom_field_id(&field.key)));
            }
        }
        Ok(fields)
    }

    /// The tenant's active fields of `record_type` whatever the reader's
    /// restrictions: for Grid and Authz, which apply restrictions themselves.
    pub(crate) async fn defined(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        record_type: &str,
    ) -> Result<Vec<Definition>> {
        self.record_type(record_type)?;
        let tenant = scope.tenant();
        let rows = client
            .query(
                &format!("SELECT {COLUMNS} FROM base_custom_fields WHERE tenant_id=$1 AND record_type=$2 AND archived_on IS NULL ORDER BY position,id"),
                &[&tenant, &record_type],
            )
            .await?;
        // A choice field whose pick-list is no longer offered, or a record
        // field whose kind is no longer mounted, leaves forms, lists and
        // agents with its module; its definition and the values stay.
        rows.iter()
            .map(Definition::from_row)
            .filter(|definition| {
                definition.as_ref().map_or(true, |definition| {
                    definition
                        .picklist
                        .as_deref()
                        .is_none_or(|list| self.picklists.list(list).is_ok())
                        && definition
                            .target
                            .as_deref()
                            .is_none_or(|kind| self.records.kind(kind).is_some())
                })
            })
            .collect()
    }

    async fn one(
        &self,
        client: &impl GenericClient,
        tenant: i64,
        record_type: &str,
        key: &str,
        suffix: &str,
    ) -> Result<Option<Definition>> {
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM base_custom_fields WHERE tenant_id=$1 AND record_type=$2 AND key=$3{suffix}"),
                &[&tenant, &record_type, &key],
            )
            .await?
            .map(|row| Definition::from_row(&row))
            .transpose()
    }

    /// Read one definition, including an archived key. The caller authorizes
    /// its operation before reading. A mutation may hold the definition and
    /// the record type's creation lock while applying a group of changes.
    pub async fn stored_definition(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        record_type: &str,
        key: &str,
        write: bool,
    ) -> Result<Option<Definition>> {
        self.record_type(record_type)?;
        let tenant = scope.tenant();
        if write {
            lock(client, tenant, record_type).await?;
        }
        self.one(
            client,
            tenant,
            record_type,
            key,
            if write { " FOR UPDATE" } else { "" },
        )
        .await
    }

    /// The definition as it stands, locked `FOR UPDATE` by a write; a stale
    /// `version` is a refusal.
    async fn current(
        &self,
        client: &impl GenericClient,
        tenant: i64,
        record_type: &str,
        key: &str,
        version: i64,
        lock: bool,
    ) -> Result<Checked<Definition>> {
        self.record_type(record_type)?;
        let definition = self
            .one(
                client,
                tenant,
                record_type,
                key,
                if lock { " FOR UPDATE" } else { "" },
            )
            .await?
            .ok_or(Error::NotFound)?;
        if definition.version != version {
            return Ok(Err(vec![FieldError::new(
                "version",
                "the field changed; reload before saving",
            )]));
        }
        Ok(Ok(definition))
    }

    async fn check_define(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        input: &NewField,
        write: bool,
        pending: i64,
    ) -> Result<Checked<NewField>> {
        authorize_key(client, scope, MANAGE).await?;
        let tenant = scope.tenant();
        let record_type = self.record_type(input.record_type.trim())?.id;
        let mut errors = Vec::new();
        let key = input.key.trim().to_owned();
        if !shaped(&key, KEY_MAX) {
            errors.push(FieldError::new(
                "key",
                format!(
                    "use a lower-case letter, then letters, digits and '_' (at most {KEY_MAX})"
                ),
            ));
        }
        let (label, help) = text(&mut errors, &input.label, &input.help);
        let kind = Kind::parse(input.kind.trim());
        if kind.is_none() {
            errors.push(FieldError::new("kind", "choose a supported field type"));
        }
        let picklist = input
            .picklist
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty());
        match (kind, picklist) {
            (Some(kind), None) if kind.takes_picklist() => {
                errors.push(FieldError::new("picklist", "choose the pick-list to use"))
            }
            (Some(kind), Some(key)) if kind.takes_picklist() => {
                if !self.choice_lists().iter().any(|(list, _)| *list == key) {
                    errors.push(FieldError::new(
                        "picklist",
                        "is not a pick-list this organisation can use",
                    ));
                }
            }
            (Some(_), Some(_)) => errors.push(FieldError::new(
                "picklist",
                "only a pick-list or several-choices field takes a pick-list",
            )),
            _ => {}
        }
        let target = input
            .target
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty());
        match (kind, target) {
            (Some(Kind::Reference), None) => errors.push(FieldError::new(
                "target",
                "choose the kind of record it points at",
            )),
            (Some(Kind::Reference), Some(key)) => {
                if self.records.kind(key).is_none() {
                    errors.push(FieldError::new(
                        "target",
                        "is not a kind of record that can be linked",
                    ));
                }
            }
            (Some(_), Some(_)) => errors.push(FieldError::new(
                "target",
                "only a record field points at a kind of record",
            )),
            _ => {}
        }
        if input.rule_condition && kind.is_some_and(|kind| !kind.rule_condition_kind()) {
            errors.push(FieldError::new(
                "rule_condition",
                "only a text, number, date, pick-list or yes/no field can be a rule condition",
            ));
        }
        if write {
            lock(client, tenant, record_type).await?;
        }
        if errors.is_empty() {
            if self
                .one(client, tenant, record_type, &key, "")
                .await?
                .is_some()
            {
                errors.push(FieldError::new(
                    "key",
                    "has already been used for this record type, even by an archived field",
                ));
            }
            let active: i64 = client
                .query_one(
                    "SELECT count(*) FROM base_custom_fields WHERE tenant_id=$1 AND record_type=$2 AND archived_on IS NULL",
                    &[&tenant, &record_type],
                )
                .await?
                .get(0);
            let limit = self.limit(client, scope).await?;
            if active + pending >= limit {
                errors.push(FieldError::new(
                    "key",
                    if pending == 0 {
                        format!("this record type already has {limit} active fields, the most this organisation allows; archive one first")
                    } else {
                        format!("the proposed fields exceed this record type's limit of {limit} active fields")
                    },
                ));
            }
        }
        if !errors.is_empty() {
            return Ok(Err(errors));
        }
        Ok(Ok(NewField {
            record_type: record_type.into(),
            key,
            label,
            help,
            kind: kind
                .map(|kind| kind.as_str().to_owned())
                .unwrap_or_default(),
            picklist: picklist.map(str::to_owned),
            target: target.map(str::to_owned),
            rule_condition: input.rule_condition,
        }))
    }
    /// The field a define would add, without adding it (the agent preview).
    pub async fn preview_define(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        input: &NewField,
    ) -> Result<Checked<NewField>> {
        self.preview_define_after(client, scope, input, 0).await
    }
    /// Preview a definition with the number of earlier, still unsaved
    /// additions for the same record type included in the capacity check.
    pub async fn preview_define_after(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        input: &NewField,
        pending: u32,
    ) -> Result<Checked<NewField>> {
        self.check_define(client, scope, input, false, pending.into())
            .await
    }
    /// Adds a field under `base.custom-fields.manage`, after the record type's
    /// other fields.
    pub async fn define(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &NewField,
    ) -> Result<Checked<Definition>> {
        let field = match self.check_define(tx.sql(), scope, input, true, 0).await? {
            Ok(field) => field,
            Err(errors) => return Ok(Err(errors)),
        };
        let tenant = scope.tenant();
        let created = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let row = sql
                        .query_one(
                            &format!("INSERT INTO base_custom_fields(tenant_id,record_type,key,label,help,kind,picklist,target,position,rule_condition) VALUES($1,$2,$3,$4,$5,$6,$7,$8,(SELECT coalesce(max(position),0)+10 FROM base_custom_fields WHERE tenant_id=$1 AND record_type=$2),$9) RETURNING {COLUMNS}"),
                            &[&tenant, &field.record_type, &field.key, &field.label, &field.help, &field.kind, &field.picklist, &field.target, &field.rule_condition],
                        )
                        .await?;
                    let created = Definition::from_row(&row)?;
                    // An empty `old` records a create.
                    let change = change(json!({}), &created);
                    Ok((created, vec![change]))
                })
            })
            .await?;
        Ok(Ok(created))
    }

    async fn check_update(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        record_type: &str,
        key: &str,
        input: &FieldUpdate,
        write: bool,
    ) -> Result<Checked<(Definition, FieldUpdate)>> {
        authorize_key(client, scope, MANAGE).await?;
        let before = match self
            .current(
                client,
                scope.tenant(),
                record_type,
                key,
                input.version,
                write,
            )
            .await?
        {
            Ok(before) => before,
            Err(errors) => return Ok(Err(errors)),
        };
        let mut errors = Vec::new();
        let (label, help) = text(&mut errors, &input.label, &input.help);
        let rule_condition = input.rule_condition.unwrap_or(before.rule_condition);
        if rule_condition && !before.kind.rule_condition_kind() {
            errors.push(FieldError::new(
                "rule_condition",
                "only a text, number, date, pick-list or yes/no field can be a rule condition",
            ));
        }
        if !errors.is_empty() {
            return Ok(Err(errors));
        }
        Ok(Ok((
            before,
            FieldUpdate {
                label,
                help,
                rule_condition: Some(rule_condition),
                version: input.version,
            },
        )))
    }
    /// The field before and the update as it would apply (the agent preview).
    pub async fn preview_update(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        record_type: &str,
        key: &str,
        input: &FieldUpdate,
    ) -> Result<Checked<(Definition, FieldUpdate)>> {
        self.check_update(client, scope, record_type, key, input, false)
            .await
    }
    /// Renames a field or rewrites its help text; the key and the kind stay.
    /// An archived field may still be edited.
    pub async fn update(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        record_type: &str,
        key: &str,
        input: &FieldUpdate,
    ) -> Result<Checked<Definition>> {
        let (before, update) = match self
            .check_update(tx.sql(), scope, record_type, key, input, true)
            .await?
        {
            Ok(checked) => checked,
            Err(errors) => return Ok(Err(errors)),
        };
        let tenant = scope.tenant();
        let saved = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let row = sql
                        .query_one(
                            &format!("UPDATE base_custom_fields SET label=$3,help=$4,rule_condition=$5,version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 RETURNING {COLUMNS}"),
                            &[&tenant, &before.id, &update.label, &update.help, &update.rule_condition.unwrap_or(false)],
                        )
                        .await?;
                    let saved = Definition::from_row(&row)?;
                    let change = change(before.to_json(), &saved);
                    Ok((saved, vec![change]))
                })
            })
            .await?;
        Ok(Ok(saved))
    }

    async fn check_status(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        (record_type, key, version): (&str, &str, i64),
        step: Step,
        write: bool,
    ) -> Result<Checked<Definition>> {
        authorize_key(client, scope, MANAGE).await?;
        let tenant = scope.tenant();
        if write && step == Step::Restore {
            lock(client, tenant, record_type).await?;
        }
        let before = match self
            .current(client, tenant, record_type, key, version, write)
            .await?
        {
            Ok(before) => before,
            Err(errors) => return Ok(Err(errors)),
        };
        match step {
            Step::Archive if before.archived() => {
                Ok(Err(vec![FieldError::new("key", "is already archived")]))
            }
            Step::Restore if !before.archived() => {
                Ok(Err(vec![FieldError::new("key", "is not archived")]))
            }
            Step::Restore => {
                let active: i64 = client
                    .query_one(
                        "SELECT count(*) FROM base_custom_fields WHERE tenant_id=$1 AND record_type=$2 AND archived_on IS NULL",
                        &[&tenant, &record_type],
                    )
                    .await?
                    .get(0);
                let limit = self.limit(client, scope).await?;
                if active >= limit {
                    return Ok(Err(vec![FieldError::new(
                        "key",
                        format!("this record type already has {limit} active fields, the most this organisation allows; archive one first"),
                    )]));
                }
                Ok(Ok(before))
            }
            Step::Archive => Ok(Ok(before)),
        }
    }
    async fn set_status(
        tx: &mut MutationTransaction<'_>,
        tenant: i64,
        before: Definition,
        step: Step,
    ) -> Result<Definition> {
        tx.mutate(move |sql| {
            Box::pin(async move {
                let set = match step {
                    Step::Archive => "archived_on=current_date",
                    Step::Restore => "archived_on=NULL",
                };
                let row = sql
                    .query_one(
                        &format!("UPDATE base_custom_fields SET {set},version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 RETURNING {COLUMNS}"),
                        &[&tenant, &before.id],
                    )
                    .await?;
                let saved = Definition::from_row(&row)?;
                let change = change(before.to_json(), &saved);
                Ok((saved, vec![change]))
            })
        })
        .await
    }
    /// The field an archive would hide, without hiding it (the agent preview).
    pub async fn preview_archive(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        record_type: &str,
        key: &str,
        version: i64,
    ) -> Result<Checked<Definition>> {
        self.check_status(
            client,
            scope,
            (record_type, key, version),
            Step::Archive,
            false,
        )
        .await
    }
    /// Hides a field from forms, columns and agents; records keep its values.
    pub async fn archive(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        record_type: &str,
        key: &str,
        version: i64,
    ) -> Result<Checked<Definition>> {
        let before = match self
            .check_status(
                tx.sql(),
                scope,
                (record_type, key, version),
                Step::Archive,
                true,
            )
            .await?
        {
            Ok(before) => before,
            Err(errors) => return Ok(Err(errors)),
        };
        Ok(Ok(Self::set_status(
            tx,
            scope.tenant(),
            before,
            Step::Archive,
        )
        .await?))
    }
    pub async fn preview_restore(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        record_type: &str,
        key: &str,
        version: i64,
    ) -> Result<Checked<Definition>> {
        self.check_status(
            client,
            scope,
            (record_type, key, version),
            Step::Restore,
            false,
        )
        .await
    }
    /// Brings an archived field back, with the values its records kept.
    pub async fn restore(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        record_type: &str,
        key: &str,
        version: i64,
    ) -> Result<Checked<Definition>> {
        let before = match self
            .check_status(
                tx.sql(),
                scope,
                (record_type, key, version),
                Step::Restore,
                true,
            )
            .await?
        {
            Ok(before) => before,
            Err(errors) => return Ok(Err(errors)),
        };
        Ok(Ok(Self::set_status(
            tx,
            scope.tenant(),
            before,
            Step::Restore,
        )
        .await?))
    }
}
