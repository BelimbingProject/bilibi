//! Headless adapters call the owner; E2 owns review, replay and outer commit.
use crate::{
    contributions::*, custom, identity, Custom, DuplicateQuery, Person, PersonId, PersonInput,
    Search, Update, WriteError,
};
use bilibi_agent_api::{
    json::{Data, Output, Page},
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::Scope;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, OnceLock},
};

static PERSONS: OnceLock<Arc<crate::PersonService>> = OnceLock::new();
pub fn bind(persons: Arc<crate::PersonService>) -> bilibi_module_registry::Result<()> {
    PERSONS.set(persons).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate person agent binding".into())
    })
}
fn persons() -> bilibi_agent_api::Result<&'static crate::PersonService> {
    PERSONS.get().map(Arc::as_ref).ok_or(Error::Internal.into())
}
fn parse<T: for<'a> Deserialize<'a>>(value: &Value) -> bilibi_agent_api::Result<T> {
    serde_json::from_value(value.clone()).map_err(|_| Error::Invalid.into())
}
fn create_input(input: &Value) -> bilibi_agent_api::Result<(PersonInput, Option<String>, Custom)> {
    let mut identity = input.clone();
    let object = identity.as_object_mut().ok_or(Error::Invalid)?;
    let code = object
        .remove("code")
        .map(|value| value.as_str().map(str::to_owned).ok_or(Error::Invalid))
        .transpose()?;
    let values = custom::patch(&object.remove("custom").unwrap_or(Value::Null))?;
    Ok((parse(&identity)?, code, values))
}
/// The update an input asks for. A contact its identity leaves out is kept as
/// stored, and only `""` clears one: a plan built while a contact was hidden
/// omits it, and must not clear it when the restriction is lifted before the
/// plan commits (#603). A contact still hidden is kept by the owner itself.
async fn update_input(
    client: &dyn bilibi_tenancy::ReadClient,
    scope: &Scope,
    input: &Value,
    lock: bool,
) -> bilibi_agent_api::Result<Update> {
    let mut update = Update {
        version: version(input)?,
        identity: parse(&input["identity"])?,
        custom: custom::patch(&input["custom"])?,
    };
    let given = input["identity"].as_object().ok_or(Error::Invalid)?;
    let keep_email = !given.contains_key("email");
    let keep_phone = !given.contains_key("phone");
    if keep_email || keep_phone {
        bilibi_authz::authorize_proof(client, scope, UPDATE).await?;
        let masked = identity::read_fields(client, scope).await?;
        let stored = identity::raw(client, scope, id(input)?, lock).await?;
        if keep_email && !masked.contains("email") {
            update.identity.email = stored.email;
        }
        if keep_phone && !masked.contains("phone") {
            update.identity.phone = stored.phone;
        }
    }
    Ok(update)
}
fn id(input: &Value) -> bilibi_agent_api::Result<PersonId> {
    PersonId::new(input["person_id"].as_i64().ok_or(Error::Invalid)?)
        .map_err(|_| Error::Invalid.into())
}
fn version(input: &Value) -> bilibi_agent_api::Result<i64> {
    input["version"]
        .as_i64()
        .filter(|v| *v > 0)
        .ok_or(Error::Invalid.into())
}
fn run_id(input: &Value) -> bilibi_agent_api::Result<i64> {
    input["run_id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or(Error::Invalid.into())
}
fn invalid(error: WriteError) -> bilibi_agent_api::Error {
    match error {
        WriteError::Backend(Error::NotFound) => bilibi_agent_api::Error::NotFound,
        WriteError::Backend(error) => error.into(),
        WriteError::Validation(errors) => {
            let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for error in errors {
                fields
                    .entry(error.field.into())
                    .or_default()
                    .push(error.message.into());
            }
            bilibi_agent_api::Error::InvalidInput(fields)
        }
        WriteError::CustomFields(errors) => {
            let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for error in errors {
                fields.entry(error.field).or_default().push(error.message);
            }
            bilibi_agent_api::Error::InvalidInput(fields)
        }
        WriteError::Retired | WriteError::SystemIdentity => Error::Forbidden.into(),
    }
}
/// A person for agents: hidden contacts marked restricted, and `custom` as
/// the tenant's active fields with this person's values.
fn person_data(active: &[bilibi_custom_fields::Definition], person: Person) -> Data {
    let masked: HashSet<_> = person.restricted.iter().cloned().collect();
    let mut value = person.json();
    value["custom"] = custom::listed(active, &person.custom);
    value["path"] = json!(format!("/contacts/{}", person.id.get()));
    Data::restricted(value, &masked)
}
async fn record(
    client: &dyn bilibi_tenancy::ReadClient,
    scope: &Scope,
    person: Person,
) -> bilibi_agent_api::Result<Data> {
    Ok(person_data(&custom::active(client, scope).await?, person))
}
/// The tenant's field definitions are read once for the whole list.
async fn records(
    client: &dyn bilibi_tenancy::ReadClient,
    scope: &Scope,
    people: Vec<Person>,
) -> bilibi_agent_api::Result<Data> {
    let active = custom::active(client, scope).await?;
    Ok(Data::Array(
        people
            .into_iter()
            .map(|person| person_data(&active, person))
            .collect(),
    ))
}
/// What an update would do, on the plan's connection: the person before (with
/// its custom fields), the identity it would store and the custom fields after.
async fn update_preview(
    client: &dyn bilibi_tenancy::ReadClient,
    scope: &Scope,
    id: PersonId,
    update: &Update,
) -> bilibi_agent_api::Result<Value> {
    let (old, identity, after) = identity::preview_update(client, scope, id, update)
        .await
        .map_err(invalid)?;
    let active = custom::active(client, scope).await?;
    let mut before = old.json();
    before["custom"] = custom::listed(&active, &old.custom);
    let after = custom::listed(&active, &after);
    // Record JSON already lacks masked values. An omitted contact is kept;
    // the guide explains this to clients.
    Ok(
        json!({"summary":"Update person", "person_id":old.id, "version":old.version, "before":before, "identity":identity, "custom":after}),
    )
}
enum Read {
    List,
    Get,
    Duplicates,
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "core/person"
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            match self {
                Self::List => {
                    let client = persons()?.db.get().await.map_err(Error::from)?;
                    let page = identity::search(&**client, scope, &parse::<Search>(input)?).await?;
                    Ok(Output::Page(Page {
                        entries: records(&**client, scope, page.entries).await?,
                        page: page.page as u64,
                        page_size: page.page_size as u64,
                        total: page.total as u64,
                    }))
                }
                Self::Get => {
                    let client = persons()?.db.get().await.map_err(Error::from)?;
                    let person = identity::read(&**client, scope, id(input)?).await?;
                    Ok(Output::Data(record(&**client, scope, person).await?))
                }
                Self::Duplicates => {
                    let people = persons()?
                        .duplicates_on(client, scope, &parse::<DuplicateQuery>(input)?)
                        .await?;
                    Ok(Output::Data(records(client, scope, people).await?))
                }
            }
        })
    }
}
enum Write {
    Create,
    Update,
    Retire,
    ImportUndo,
}
impl WriteHandler for Write {
    fn creates(&self) -> Option<bilibi_agent_api::CreatedRecord> {
        matches!(self, Self::Create).then_some(bilibi_agent_api::CreatedRecord {
            kind: "person",
            id_pointer: "/id",
        })
    }
    fn owner(&self) -> &'static str {
        "core/person"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let value = match self {
                Self::Create => {
                    let (identity, mut code, values) = create_input(input)?;
                    if let Some(tx) = client.transaction() {
                        identity::lock_code_allocation(tx, scope.tenant()).await?;
                    }
                    let generated = code.as_deref().is_none_or(|code| code.trim().is_empty());
                    if generated {
                        code = Some(
                            identity::available_code_for_preview(
                                client,
                                scope.tenant(),
                                &identity.name,
                            )
                            .await?,
                        );
                    }
                    let person = identity::preview_with_code(
                        client,
                        scope,
                        &identity,
                        code.as_deref(),
                        &values,
                    )
                    .await
                    .map_err(invalid)?;
                    if !generated && !client.reserve_preview_value("core.person.code", &person.code)
                    {
                        return Err(invalid(WriteError::Validation(vec![crate::model::error(
                            "code",
                            "has already been taken",
                        )])));
                    }
                    json!({"summary":"Create person without an account or access grants", "identity":person})
                }
                Self::Update => {
                    // On the plan's own connection: a second pooled
                    // connection per preview would exhaust the pool under
                    // concurrent plans.
                    update_preview(
                        client,
                        scope,
                        id(input)?,
                        &update_input(client, scope, input, false).await?,
                    )
                    .await?
                }
                Self::Retire => {
                    let old = match client.transaction() {
                        Some(tx) => {
                            identity::preview_retire(tx, scope, id(input)?, version(input)?).await
                        }
                        None => {
                            let pooled = persons()?.db.get().await.map_err(Error::from)?;
                            identity::preview_retire(&**pooled, scope, id(input)?, version(input)?)
                                .await
                        }
                    }
                    .map_err(invalid)?;
                    json!({"summary":"Retire person; retain identity and history", "person_id":old.id, "version":old.version})
                }
                Self::ImportUndo => {
                    let (run, person, version) = (run_id(input)?, id(input)?, version(input)?);
                    let service = persons()?;
                    let old = if let Some(tx) = client.transaction() {
                        service
                            .bulk
                            .preview_undo(
                                tx,
                                scope,
                                "core/person.contacts",
                                run,
                                &person.get().to_string(),
                                version,
                            )
                            .await?;
                        identity::preview_import_undo(tx, scope, person, version).await
                    } else {
                        let pooled = service.db.get().await.map_err(Error::from)?;
                        service
                            .bulk
                            .preview_undo(
                                &**pooled,
                                scope,
                                "core/person.contacts",
                                run,
                                &person.get().to_string(),
                                version,
                            )
                            .await?;
                        identity::preview_import_undo(&**pooled, scope, person, version).await
                    }
                    .map_err(invalid)?;
                    json!({"summary":"Remove the unchanged contact created by this import", "person_id":old.id, "version":old.version})
                }
            };
            Ok(value.into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let person = match self {
                Self::Create => {
                    let (identity, code, values) = create_input(input)?;
                    identity::create_with_code(tx, scope, &identity, code.as_deref(), &values).await
                }
                Self::Update => {
                    let update = update_input(tx.sql(), scope, input, true).await?;
                    identity::update(tx, scope, id(input)?, &update).await
                }
                Self::Retire => {
                    crate::step_up::retire(scope, id(input)?).await?;
                    identity::retire(tx, scope, id(input)?, version(input)?).await
                }
                Self::ImportUndo => {
                    let (run, person, version) = (run_id(input)?, id(input)?, version(input)?);
                    persons()?
                        .bulk
                        .claim_undo(
                            tx,
                            scope,
                            "core/person.contacts",
                            run,
                            &person.get().to_string(),
                            version,
                        )
                        .await?;
                    identity::undo_import(tx, scope, person, version)
                        .await
                        .map_err(invalid)?;
                    return Ok(Output::Data(
                        json!({"person_id":person.get(),"removed":true}).into(),
                    ));
                }
            }
            .map_err(invalid)?;
            Ok(Output::Data(record(tx.sql(), scope, person).await?))
        })
    }
}
fn identity_schema() -> Value {
    json!({"type":"object","required":["name","kind"],"properties":{
        "name":{"type":"string","maxLength":160},"kind":{"type":"string","enum":["human","agent"]},
        "email":{"type":"string","maxLength":254},"phone":{"type":"string","maxLength":64}
    }})
}
fn create_schema() -> Value {
    let mut schema = identity_schema();
    schema["properties"]["custom"] = custom::schema();
    schema["properties"]["code"] = json!({"type":"string","maxLength":255,"description":"Propose a code of at most 12 characters from whole name words, except truncate the first word to fit the limit. Omit source-spelled connectors bin, binti, a/l, a/p, s/o and d/o; add _2, _3 and so on for collisions within 12 characters. The person may choose another code, including a longer one."});
    schema
}
fn list_schema() -> Value {
    json!({"type":"object","properties":{
        "search":{"type":"string","maxLength":255},"kind":{"type":"string","enum":["human","agent"]},
        "sort":{"type":"string","maxLength":43,"description":"name, code, kind, state or created_at, or cf_<key> to sort by an active custom field (number as a number, date as a date; empty last)."},"descending":{"type":"boolean"},
        "custom":{"type":"array","description":"Conditions on custom field values, all of which hold: op contains (text), eq (text or choice code), gte or lte (number or date). A key that is not an active field, or a value the field cannot hold, is refused.","items":{"type":"object","required":["key","op","value"],"properties":{"key":{"type":"string","maxLength":40},"op":{"type":"string","enum":["contains","eq","gte","lte"]},"value":{"type":"string","maxLength":256}}}},
        "include_retired":{"type":"boolean"},"page":{"type":"integer","minimum":1,"maximum":1000000},"page_size":{"type":"integer","minimum":1,"maximum":300}
    }})
}
fn id_schema() -> Value {
    json!({"type":"integer","minimum":1})
}
fn get_schema() -> Value {
    json!({"type":"object","required":["person_id"],"properties":{"person_id":id_schema()}})
}
fn duplicate_schema() -> Value {
    json!({"type":"object","properties":{
        "name":{"type":"string","maxLength":160},"email":{"type":"string","maxLength":254},"phone":{"type":"string","maxLength":64},
        "except_id":id_schema(),"limit":{"type":"integer","minimum":1,"maximum":100}
    }})
}
fn update_schema() -> Value {
    json!({"type":"object","required":["person_id","version","identity"],"properties":{"person_id":id_schema(),"version":id_schema(),"identity":identity_schema(),"custom":custom::schema()}})
}
fn retire_schema() -> Value {
    json!({"type":"object","required":["person_id","version"],"properties":{"person_id":id_schema(),"version":id_schema()}})
}
fn import_undo_schema() -> Value {
    json!({"type":"object","required":["run_id","person_id","version"],"properties":{"run_id":id_schema(),"person_id":id_schema(),"version":id_schema()}})
}
fn one_output() -> Value {
    json!({"type":"object","description":"Person fixed Core code and identity/contact facts; hidden contacts read as {\"restricted\":true}; custom lists the tenant's active custom fields as {key, label, type, value}. No credentials or grants."})
}
fn import_undo_output() -> Value {
    json!({"type":"object","description":"The removed contact id and removed=true."})
}
fn many_output() -> Value {
    json!({"type":"array","description":"Bounded Person identities with contacts redacted under your current role restrictions."})
}
inventory::submit! { Contribution::new(DEFINITIONS, "core/person.agent", Provenance { module: "core/person", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key:"core.person.list", title:"List persons", summary:"Search identities by fixed Core code, name and visible contacts in your tenant.", keywords:&["people","contact","find"], capability:LIST, input:list_schema, output:many_output, handler:Handler::Read(&Read::List),channels:Channels::Every,step_up:&[] },
        Operation { key:"core.person.get", title:"Read a person", summary:"Read one identity and its visible personal contacts.", keywords:&["identity","contact"], capability:VIEW, input:get_schema, output:one_output, handler:Handler::Read(&Read::Get),channels:Channels::Every,step_up:&[] },
        Operation { key:"core.person.duplicates", title:"Find duplicate candidates", summary:"Find exact name, email or phone candidates, including retired identities. Does not merge or infer identity.", keywords:&["contact","reuse","lookup"], capability:LIST, input:duplicate_schema, output:many_output, handler:Handler::Read(&Read::Duplicates),channels:Channels::Every,step_up:&[] },
        Operation { key:"core.person.create", title:"Create a person", summary:"Generate a fixed tenant-unique Core code and record a human or agent contact without creating an account, credentials, employment or grants.", keywords:&["contact","add","human","agent"], capability:CREATE, input:create_schema, output:one_output, handler:Handler::SafeWrite(&Write::Create),channels:Channels::Every,step_up:&[] },
        Operation { key:"core.person.update", title:"Update a person", summary:"Replace identity and visible contact facts using the current version.", keywords:&["contact","edit"], capability:UPDATE, input:update_schema, output:one_output, handler:Handler::SafeWrite(&Write::Update),channels:Channels::Every,step_up:&[] },
        Operation { key:"core.person.retire", title:"Retire a person", summary:"Retain the contact and history while refusing new identity bindings. Protected system identities cannot retire.", keywords:&["contact","archive"], capability:RETIRE, input:retire_schema, output:one_output, handler:Handler::SafeWrite(&Write::Retire),channels:Channels::Every,step_up:&[crate::step_up::RETIRE] },
        Operation { key:"core.person.import.undo", title:"Remove an imported person", summary:"Remove an unchanged contact created by an import that is currently being undone. Referenced contacts are left in place.", keywords:&["contact","import","undo"], capability:IMPORT, input:import_undo_schema, output:import_undo_output, handler:Handler::SafeWrite(&Write::ImportUndo),channels:Channels::Every,step_up:&[] },
    ],
    guides: &[Guide { key:"core.person", title:"Contacts and personal details", summary:"Identity, duplicate candidates, agents and optional account binding without People.", keywords:&["contact","human","agent","account"], capability:LIST,
        body:"Postal addresses are saved and linked through core.address, not a scalar field here. Search address or read guide core.address. Use core.address.company.list or core.address.person.list to find existing addresses; create_and_attach or attach links an office, billing or other address to its company or contact. Use one ordered plan for new companies, contacts, affiliations and addresses, with {\"$ref\":\"1.id\"} for the record created by step 1; review_entries accepts operations for linked steps.\n\nEach Person has one fixed Core code. Propose a short code of at most 12 characters from whole name words, except truncate the first word to fit the limit. Omit source-spelled connectors bin, binti, a/l, a/p, s/o and d/o; append _2, _3 and so on for collisions within 12 characters. The person may choose another, longer code. The create preview and every read return code; core.person.list searches and sorts by code. Renaming or retiring never changes or frees it. Do not send code in update inputs.\n\nA Person is a human or agent identity with optional personal email and phone, independent of any company, employment or login account. Search with core.person.list and core.person.duplicates before creating; duplicate email and name are allowed and never imply the same person. Duplicate lookup matches exact case-insensitive trimmed name/email or exact trimmed phone and includes retired records. It never probes a contact field you may not see.\n\nCreate through a dry run and explicit review before the write. Employee numbers, employment lifecycle, account IDs, grants and reserved system keys are refused. Agent kind grants no login or capability. The operator explicitly provisions the protected platform-orchestrator identity through its owner API, never by a magic numeric ID.\n\nUpdate sends person_id, current version and identity (name, kind, optional email/phone). An empty string clears a visible contact; an omitted contact, visible or hidden, is kept as stored. A hidden contact must be omitted. Read results mark hidden contacts as restricted. Retirement retains identity/history and prevents new account bindings.\n\nCustom fields are the tenant's own person fields (read them with base.custom_fields.list, record type person). Reads return custom as {key, label, type, value} for the active fields. core.person.create and core.person.update take custom as a list of {key, value}: text, a number as a decimal string, a date as YYYY-MM-DD, a choice as its pick-list code, or null to clear. Update changes only the keys you send; the others are kept. A key that is not an active field is refused on cf_<key>. Optional account links belong to User and require a live Person; Person creation never creates a link. Desks separately grant staff workplace access. Work contacts and dated company affiliations belong to Affiliation: record a namecard contact (a new person and their company position at once) with core.affiliation.contact.create, and affiliate an existing person with core.affiliation.create. Business roles toward a company (customer, prospect, vendor) are core.company.relationship.assign with target_kind person. Never invent an employee number, employment status or login from a namecard; HR belongs to People, which is not needed for contacts." }],
}) }
