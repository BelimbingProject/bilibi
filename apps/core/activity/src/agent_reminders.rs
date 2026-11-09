//! Operation contracts for the guest panel's "create a person" and a
//! person's own reminder defaults. The pages call the write contracts too.
use crate::agent_operations::{guest_id, guest_record, id, invalid, owner};
use crate::{ReminderDefaults, MANAGE, MAX_REMINDERS, MAX_REMINDER_MINUTES, VIEW};
use bilibi_agent_api::{
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, ReadHandler,
    WriteHandler, DEFINITIONS,
};
use bilibi_database::MutationTransaction;
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};

const KEYWORDS: &[&str] = &[
    "activity", "guest", "person", "reminder", "default", "email",
];

fn name(input: &Value) -> bilibi_agent_api::Result<&str> {
    Ok(input["name"].as_str().unwrap_or_default())
}

struct CreatePersonGuest;
impl WriteHandler for CreatePersonGuest {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (activity, row, person) = owner()?
                .preview_create_person_guest(
                    client,
                    scope,
                    id(input)?,
                    guest_id(input)?,
                    name(input)?,
                )
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({
                "activity": crate::agent_operations::record(&activity, None),
                "guest": guest_record(&row),
                "creates_person": {"name": person.name, "email": person.email},
            })
            .into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let row = owner()?
                .create_person_guest(tx, scope, id(input)?, guest_id(input)?, name(input)?)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(guest_record(&row).into())
        })
    }
}

fn defaults_record(defaults: &ReminderDefaults) -> Value {
    json!({"reminder_minutes": defaults.minutes, "reminder_email": defaults.email})
}

struct GetDefaults;
impl ReadHandler for GetDefaults {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn call<'a>(
        &'a self,
        _: &'a dyn OperationClient,
        scope: &'a Scope,
        _: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let defaults = owner()?.reminder_defaults(scope).await?;
            Ok(defaults_record(&defaults).into())
        })
    }
}

/// `reminder_minutes` and `reminder_email`, each a value or null (follow the
/// organisation's setting).
fn defaults_input(input: &Value) -> bilibi_agent_api::Result<ReminderDefaults> {
    let mut fields = std::collections::BTreeMap::new();
    let mut refuse = |path: &str, message: &str| {
        fields
            .entry(path.to_owned())
            .or_insert_with(Vec::new)
            .push(message.to_owned())
    };
    let minutes = match &input["reminder_minutes"] {
        Value::Null => None,
        Value::Array(items) => {
            let parsed: Option<Vec<i32>> = items
                .iter()
                .map(|item| item.as_i64().and_then(|m| i32::try_from(m).ok()))
                .collect();
            if parsed.is_none() {
                refuse(
                    "reminder_minutes",
                    "must be a list of whole numbers of minutes",
                );
            }
            parsed
        }
        _ => {
            refuse(
                "reminder_minutes",
                "must be a list of whole numbers of minutes or null",
            );
            None
        }
    };
    let email = match &input["reminder_email"] {
        Value::Null => None,
        Value::Bool(email) => Some(*email),
        _ => {
            refuse("reminder_email", "must be true, false or null");
            None
        }
    };
    if fields.is_empty() {
        Ok(ReminderDefaults { minutes, email })
    } else {
        Err(bilibi_agent_api::Error::InvalidInput(fields))
    }
}

struct SetDefaults;
impl WriteHandler for SetDefaults {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let defaults = owner()?
                .preview_set_reminder_defaults(client, scope, &defaults_input(input)?)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(json!({"stores": defaults_record(&defaults)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let defaults = owner()?
                .set_reminder_defaults(tx, scope, &defaults_input(input)?)
                .await
                .map_err(|e| invalid(e, ""))?;
            Ok(defaults_record(&defaults).into())
        })
    }
}

fn person_guest_schema() -> Value {
    json!({"type":"object","required":["id","guest_id","name"],"properties":{
        "id":{"type":"integer","minimum":1},
        "guest_id":{"type":"integer","minimum":1,"description":"The email-address guest's id, from core.activity.get."},
        "name":{"type":"string","maxLength":200,"description":"The new person's name; their email is the guest's address."}
    }})
}
fn defaults_schema() -> Value {
    json!({"type":"object","properties":{
        "reminder_minutes":{"type":"array","items":{"type":"integer","minimum":0,"maximum":MAX_REMINDER_MINUTES},"description":format!("Minutes before an activity you create starts or is due to remind at, up to {MAX_REMINDERS}. An empty list is no reminder; omitted follows the organisation's setting.")},
        "reminder_email":{"type":"boolean","nullable":true,"description":"Whether reminders of activities you create are also emailed; omitted or null follows the organisation's setting."}
    }})
}
fn no_input() -> Value {
    json!({"type":"object","properties":{}})
}
fn defaults_output() -> Value {
    json!({"type":"object","description":"reminder_minutes (the offsets, null to follow the organisation's setting) and reminder_email (null likewise)."})
}
fn review_output() -> Value {
    json!({"type":"object","description":"The review shows what will be stored; the commit returns the result."})
}

static CREATE_PERSON_GUEST: CreatePersonGuest = CreatePersonGuest;
static GET_DEFAULTS: GetDefaults = GetDefaults;
static SET_DEFAULTS: SetDefaults = SetDefaults;
inventory::submit! { Contribution::new(DEFINITIONS, "core/activity.reminder_operations", Provenance { module: crate::MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "core.activity.create_person_guest", title: "Create a person from a guest", summary: "Create a Core person from an email-address guest of an activity you may change, using the address and the name you give, and make the guest that person. Refused when a person already has that address.", keywords: KEYWORDS, capability: MANAGE, input: person_guest_schema, output: review_output, handler: Handler::SafeWrite(&CREATE_PERSON_GUEST), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.reminder_defaults", title: "Read your reminder defaults", summary: "The reminder offsets and email choice your new activities start with; null follows the organisation's setting.", keywords: KEYWORDS, capability: VIEW, input: no_input, output: defaults_output, handler: Handler::Read(&GET_DEFAULTS), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.activity.set_reminder_defaults", title: "Set your reminder defaults", summary: "Choose the reminder offsets and email choice your new activities start with. Existing activities keep their own.", keywords: KEYWORDS, capability: MANAGE, input: defaults_schema, output: review_output, handler: Handler::SafeWrite(&SET_DEFAULTS), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[],
}) }
