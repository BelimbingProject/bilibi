//! The custom-field operations agents may call (`base.custom_fields.*`): thin
//! adapters over the `CustomFields` facade with the sealed scope the registry
//! proved. Reads list the record types and the definitions; every write is a
//! reviewed safe write whose preview shares the facade's checks without
//! locking or writing and whose commit writes through the plan's audited
//! transaction (`base/agent_api` owns review, replay and the outer commit).
//! The administration page calls these same contracts.
use crate::{
    declaration::{LABEL_MAX, RECORD_TYPE_MAX},
    definitions::{Checked, CustomFields, FieldError, FieldUpdate, NewField, HELP_MAX, KEY_MAX},
    Kind, MANAGE, VIEW,
};
use bilibi_agent_api::{
    json::{Data, Output},
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Db, Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, OnceLock},
};

struct Bound {
    db: Db,
    fields: Arc<CustomFields>,
}
static BOUND: OnceLock<Bound> = OnceLock::new();
pub(crate) fn bind(db: Db, fields: Arc<CustomFields>) -> bilibi_module_registry::Result<()> {
    BOUND.set(Bound { db, fields }).map_err(|_| {
        bilibi_module_registry::RegistryError::Service(
            "duplicate custom-field agent binding".into(),
        )
    })
}
fn bound() -> bilibi_agent_api::Result<&'static Bound> {
    BOUND.get().ok_or(Error::Internal.into())
}
fn invalid(errors: Vec<FieldError>) -> bilibi_agent_api::Error {
    let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for error in errors {
        fields.entry(error.field).or_default().push(error.message);
    }
    bilibi_agent_api::Error::InvalidInput(fields)
}
fn checked<T>(checked: Checked<T>) -> bilibi_agent_api::Result<T> {
    checked.map_err(invalid)
}
fn found(error: Error) -> bilibi_agent_api::Error {
    match error {
        Error::NotFound => bilibi_agent_api::Error::NotFound,
        other => other.into(),
    }
}
fn version(input: &Value) -> bilibi_agent_api::Result<i64> {
    input["version"]
        .as_i64()
        .filter(|value| *value > 0)
        .ok_or(Error::Invalid.into())
}
fn new_field(input: &Value) -> NewField {
    NewField {
        record_type: bilibi_agent_api::text_field(input, "record_type"),
        key: bilibi_agent_api::text_field(input, "key"),
        label: bilibi_agent_api::text_field(input, "label"),
        help: input["help"].as_str().map(str::to_owned),
        kind: bilibi_agent_api::text_field(input, "kind"),
        picklist: input["picklist"].as_str().map(str::to_owned),
        target: input["target"].as_str().map(str::to_owned),
        rule_condition: input["rule_condition"].as_bool().unwrap_or(false),
    }
}
fn update(input: &Value) -> bilibi_agent_api::Result<FieldUpdate> {
    Ok(FieldUpdate {
        label: bilibi_agent_api::text_field(input, "label"),
        help: input["help"].as_str().map(str::to_owned),
        rule_condition: input["rule_condition"].as_bool(),
        version: version(input)?,
    })
}
fn record(value: Value) -> Data {
    Data::restricted(value, &HashSet::new())
}
fn to_json(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("custom field output serializes")
}

enum Read {
    RecordTypes,
    List,
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "base/custom_fields"
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let bound = bound()?;
            // The connection the caller handed over, never a second one.
            let client = client.sql_client().await?;
            let value = match self {
                Self::RecordTypes => {
                    bilibi_authz::authorize_key(client, scope, VIEW).await?;
                    let installed = bilibi_grid::installed().map_err(|_| Error::Internal)?;
                    let mut record_types = Vec::new();
                    for record in bound.fields.record_types() {
                        let table = bound
                            .fields
                            .sources
                            .iter()
                            .find(|source| source.record_type == record.id)
                            .and_then(|source| installed.table(source.table));
                        let path = table.and_then(bilibi_grid::Table::list_address);
                        let mut kinds = vec![record.id];
                        if let Some(table) = table {
                            kinds.extend(table.record_kinds().filter(|kind| *kind != record.id));
                        }
                        record_types.push(json!({"id": record.id, "label": record.label, "path": path, "record_kinds": kinds}));
                    }
                    json!({
                        "record_types": record_types,
                        "max_fields": bound.fields.limit(client, scope).await?,
                        "pick_lists": bound.fields.choice_lists().into_iter().map(|(key, label)| json!({"key": key, "label": label})).collect::<Vec<_>>(),
                        "record_kinds": bound.fields.reference_kinds().into_iter().map(|(key, label)| json!({"key": key, "label": label})).collect::<Vec<_>>(),
                    })
                }
                Self::List => to_json(
                    bound
                        .fields
                        .definitions(
                            client,
                            scope,
                            input["record_type"].as_str(),
                            input["include_archived"].as_bool().unwrap_or(false),
                        )
                        .await
                        .map_err(found)?,
                ),
            };
            Ok(Output::Data(record(value)))
        })
    }
}

enum Write {
    Define,
    Update,
    Archive,
    Restore,
}
impl Write {
    async fn preview_on(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        input: &Value,
    ) -> bilibi_agent_api::Result<bilibi_agent_api::json::Output> {
        let bound = bound()?;
        let fields = &bound.fields;
        let record_type = input["record_type"].as_str().unwrap_or_default();
        let key = input["key"].as_str();
        let value = match self {
            Self::Define => {
                let field = checked(
                    fields
                        .preview_define(client, scope, &new_field(input))
                        .await
                        .map_err(found)?,
                )?;
                json!({"summary":format!("Add the {} field “{}” ({}) to {}", field.kind, field.label, field.key, field.record_type),"field":field})
            }
            Self::Update => {
                let key = key.ok_or(Error::Invalid)?;
                let (before, after) = checked(
                    fields
                        .preview_update(client, scope, record_type, key, &update(input)?)
                        .await
                        .map_err(found)?,
                )?;
                json!({"summary":format!("Change {key} on {record_type}; its key and type stay"),"before":before,"after":after})
            }
            Self::Archive => {
                let key = key.ok_or(Error::Invalid)?;
                let before = checked(
                    fields
                        .preview_archive(client, scope, record_type, key, version(input)?)
                        .await
                        .map_err(found)?,
                )?;
                json!({"summary":format!("Archive {key} on {record_type}; records keep its values"),"before":before})
            }
            Self::Restore => {
                let key = key.ok_or(Error::Invalid)?;
                let before = checked(
                    fields
                        .preview_restore(client, scope, record_type, key, version(input)?)
                        .await
                        .map_err(found)?,
                )?;
                json!({"summary":format!("Restore {key} on {record_type}"),"before":before})
            }
        };
        Ok(value.into())
    }
}
impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        "base/custom_fields"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            // The door's own transaction when it gives one: a preview never
            // asks the pool for a second connection.
            match client.transaction() {
                Some(tx) => self.preview_on(tx, scope, input).await,
                None => {
                    let pooled = bound()?.db.get().await.map_err(Error::from)?;
                    self.preview_on(&**pooled, scope, input).await
                }
            }
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let fields = &bound()?.fields;
            let record_type = input["record_type"].as_str().unwrap_or_default();
            let key = input["key"].as_str();
            let value = match self {
                Self::Define => to_json(checked(
                    fields
                        .define(tx, scope, &new_field(input))
                        .await
                        .map_err(found)?,
                )?),
                Self::Update => to_json(checked(
                    fields
                        .update(
                            tx,
                            scope,
                            record_type,
                            key.ok_or(Error::Invalid)?,
                            &update(input)?,
                        )
                        .await
                        .map_err(found)?,
                )?),
                Self::Archive => to_json(checked(
                    fields
                        .archive(
                            tx,
                            scope,
                            record_type,
                            key.ok_or(Error::Invalid)?,
                            version(input)?,
                        )
                        .await
                        .map_err(found)?,
                )?),
                Self::Restore => to_json(checked(
                    fields
                        .restore(
                            tx,
                            scope,
                            record_type,
                            key.ok_or(Error::Invalid)?,
                            version(input)?,
                        )
                        .await
                        .map_err(found)?,
                )?),
            };
            Ok(Output::Data(record(value)))
        })
    }
}

fn record_type_schema() -> Value {
    json!({"type":"string","maxLength":RECORD_TYPE_MAX,"description":"The kind of record, from base.custom_fields.record_types (for example company)."})
}
fn key_schema() -> Value {
    json!({"type":"string","maxLength":KEY_MAX,"description":"The field's stable key: a lower-case letter, then letters, digits and underscores. It never changes and is never reused."})
}
fn version_schema() -> Value {
    json!({"type":"integer","minimum":1,"description":"The version you read; a changed field is refused."})
}
fn text_properties() -> Value {
    json!({
        "label":{"type":"string","maxLength":LABEL_MAX,"description":"What people see beside the value."},
        "help":{"type":"string","maxLength":HELP_MAX,"description":"Optional help text; blank removes it."},
        "rule_condition":{"type":"boolean","description":"Offer the field to Base Rules conditions (text, number, date, choice and boolean fields only). A rule never reads a field that is not offered; withdrawing it, or archiving the field, stops every rule that reads it until a new version is approved. Omit on update to keep it."},
    })
}
fn record_types_schema() -> Value {
    json!({"type":"object","properties":{}})
}
fn list_schema() -> Value {
    json!({"type":"object","properties":{"record_type":record_type_schema(),"include_archived":{"type":"boolean"}}})
}
fn define_schema() -> Value {
    let mut properties = text_properties();
    properties["record_type"] = record_type_schema();
    properties["key"] = key_schema();
    properties["kind"] = json!({"type":"string","enum":Kind::ALL.map(Kind::as_str),"description":"What the field holds; it never changes. A number is kept exactly as written."});
    properties["picklist"] = json!({"type":"string","maxLength":128,"description":"For a choice or choices field, the pick-list key from base.custom_fields.record_types; omit for any other kind."});
    properties["target"] = json!({"type":"string","maxLength":128,"description":"For a reference field, the kind of record it points at, a record_kinds key from base.custom_fields.record_types; omit for any other kind."});
    json!({"type":"object","required":["record_type","key","label","kind"],"properties":properties})
}
fn update_schema() -> Value {
    let mut properties = text_properties();
    properties["record_type"] = record_type_schema();
    properties["key"] = key_schema();
    properties["version"] = version_schema();
    json!({"type":"object","required":["record_type","key","label","version"],"properties":properties})
}
fn status_schema() -> Value {
    json!({"type":"object","required":["record_type","key","version"],"properties":{"record_type":record_type_schema(),"key":key_schema(),"version":version_schema()}})
}
fn record_types_output() -> Value {
    json!({"type":"object","description":"record_types (id, label, path if registered and record_kinds) that offer custom fields, max_fields (active fields allowed per record type), pick_lists (key and label) a choice or choices field may use, and record_kinds (key and label) a reference field may point at."})
}
fn list_output() -> Value {
    json!({"type":"array","description":"The tenant's fields: id, record_type, key, label, help, kind, picklist, target, position, archived_on and version."})
}
fn field_output() -> Value {
    json!({"type":"object","description":"A field: id, record_type, key, label, help, kind, picklist, target, position, archived_on and version."})
}

const MODULE: &str = "base/custom_fields";
inventory::submit! { Contribution::new(DEFINITIONS, "base/custom_fields.agent", Provenance { module: MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "base.custom_fields.record_types", title: "List the records that take custom fields", summary: "The kinds of record whose organisation can add its own fields, the pick-lists a choice field may use and the kinds of record a reference field may point at.", keywords: &["custom", "field", "attribute", "extra"], capability: VIEW, input: record_types_schema, output: record_types_output, handler: Handler::Read(&Read::RecordTypes), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.custom_fields.list", title: "List custom fields", summary: "The organisation's own fields on a kind of record, with their key, type, label and status; archived ones on request.", keywords: &["custom", "field", "attribute", "extra"], capability: VIEW, input: list_schema, output: list_output, handler: Handler::Read(&Read::List), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.custom_fields.define", title: "Add a custom field", summary: "Add a typed custom field to a kind of record. Its key and type never change.", keywords: &["custom", "field", "attribute", "add", "create"], capability: MANAGE, input: define_schema, output: field_output, handler: Handler::SafeWrite(&Write::Define), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.custom_fields.update", title: "Change a custom field", summary: "Rename a field, rewrite its help text or offer it to rule conditions using its current version; its key and type never change.", keywords: &["custom", "field", "rename", "help"], capability: MANAGE, input: update_schema, output: field_output, handler: Handler::SafeWrite(&Write::Update), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.custom_fields.archive", title: "Archive a custom field", summary: "Hide a field from forms, columns and agents; records keep the values already stored under it.", keywords: &["custom", "field", "archive", "hide", "retire"], capability: MANAGE, input: status_schema, output: field_output, handler: Handler::SafeWrite(&Write::Archive), channels: Channels::Every, step_up: &[] },
        Operation { key: "base.custom_fields.restore", title: "Restore a custom field", summary: "Bring an archived field back with the values its records kept.", keywords: &["custom", "field", "restore", "unarchive"], capability: MANAGE, input: status_schema, output: field_output, handler: Handler::SafeWrite(&Write::Restore), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[Guide { key: "base.custom_fields", title: "Custom fields", summary: "How an organisation adds its own fields to records: types, stable keys, pick-lists and archiving.", keywords: &["custom", "field", "attribute", "extra"], capability: VIEW,
        body: "A custom field is a typed value an organisation adds to a kind of record, such as a Segment on a company, without a software release. Find the kinds of record that take them, and the pick-lists a choice field may use, with base.custom_fields.record_types; read the fields with base.custom_fields.list.\n\nA field has a stable key (a lower-case letter, then letters, digits and underscores), a label, optional help text and a type: text, number, date, pick-list (choice), several choices (choices), yes/no, long text, link, phone, email or record (reference). The key and the type never change and a key is never reused, even after the field is archived, so decide them with care. A number is kept exactly as written. A pick-list field stores the code of a value from one of the organisation's pick-lists; a several-choices field stores a list of such codes. Yes/no stores true or false as text. A record field points at one record of a fixed kind (its target, such as company) and stores that record's id; a record the person cannot see is refused when chosen and shown as unavailable when held. Long text uses a multiline editor. Links must start with http:// or https://. Phone and email fields check their values before saving.\n\nArchive a field with base.custom_fields.archive rather than deleting it: forms, columns and agents stop showing it and records keep their values, and base.custom_fields.restore brings it back. Updates, archives and restorations need the version you read. A record type may have a limited number of active fields, set by the organisation's administrators. Defining and changing fields needs base.custom-fields.manage; reading them needs base.custom-fields.view. For each write, run the dry-run check, show the complete preview, wait for explicit approval, then commit the unchanged operations." }],
}) }
