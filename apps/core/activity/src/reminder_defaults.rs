//! A person's own reminder defaults: the offsets and the email choice an
//! activity they create starts with, between the tenant setting
//! (`core.activity.reminder_minutes`, `core.activity.reminder_email`) and the
//! activity's own value. They are copied onto the activity when it is
//! created; changing them later changes no existing activity. README,
//! Reminders.
use crate::model::{offsets, MAX_REMINDERS, MAX_REMINDER_MINUTES};
use crate::{FieldError, WriteError, WriteResult, MANAGE, VIEW};
use bilibi_authz::authorize_proof;
use bilibi_database::{MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{Definition, ScopeKind, Setting, Settings, Target, ValueType, DEFINITIONS};
use bilibi_tenancy::{ReadClient, Scope};
use serde_json::{json, Value};

/// The person's default offsets, in minutes before: a list of up to five;
/// null (unset) follows the tenant setting.
pub const DEFAULT_MINUTES: &str = "core.activity.default_reminder_minutes";
/// Whether the person's activities email their reminders; null follows the
/// tenant setting.
pub const DEFAULT_EMAIL: &str = "core.activity.default_reminder_email";

/// What a person's new activities start with. `None` follows the tenant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReminderDefaults {
    pub minutes: Option<Vec<i32>>,
    pub email: Option<bool>,
}

fn accepts(value: &Value) -> bool {
    value.is_null()
        || value.as_array().is_some_and(|items| {
            items.len() <= MAX_REMINDERS
                && items.iter().all(|item| {
                    item.as_i64()
                        .is_some_and(|m| (0..=i64::from(MAX_REMINDER_MINUTES)).contains(&m))
                })
        })
}

inventory::submit! { Contribution::new(DEFINITIONS, DEFAULT_MINUTES, Provenance { module: crate::MODULE, source: file!(), line: line!() }, &Setting::Definition(Definition {
    value_type: ValueType::Array, scopes: &[ScopeKind::User], default: || Value::Null,
    nullable: true, encrypted: false, minimum: None, maximum: None,
    validator: Some(bilibi_settings::Validator { accepts, message: "must list up to five whole numbers of minutes, each 0 to 10080" }),
    editor: None,
})) }
inventory::submit! { Contribution::new(DEFINITIONS, DEFAULT_EMAIL, Provenance { module: crate::MODULE, source: file!(), line: line!() }, &Setting::Definition(Definition {
    value_type: ValueType::Boolean, scopes: &[ScopeKind::User], default: || Value::Null,
    nullable: true, encrypted: false, minimum: None, maximum: None, validator: None,
    editor: None,
})) }

/// The reader's own defaults. A named system principal has none.
pub(crate) async fn read(
    settings: &Settings,
    client: &dyn ReadClient,
    scope: &Scope,
) -> Result<ReminderDefaults> {
    if scope.principal().is_some() {
        return Ok(ReminderDefaults::default());
    }
    let own = Target::User { company: None };
    let minutes = settings
        .get(client, scope, own, DEFAULT_MINUTES)
        .await?
        .value;
    let email = settings.get(client, scope, own, DEFAULT_EMAIL).await?.value;
    Ok(ReminderDefaults {
        minutes: minutes.is_array().then(|| {
            offsets(
                minutes
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m.as_i64().and_then(|m| i32::try_from(m).ok()))
                    .collect(),
            )
        }),
        email: email.as_bool(),
    })
}

/// The rules on the values alone.
pub(crate) fn check(defaults: &ReminderDefaults) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if let Some(minutes) = &defaults.minutes {
        if minutes.len() > MAX_REMINDERS {
            errors.push(FieldError::new(
                "reminder_minutes",
                format!("must name at most {MAX_REMINDERS} reminders"),
            ));
        } else if minutes
            .iter()
            .any(|m| !(0..=MAX_REMINDER_MINUTES).contains(m))
        {
            errors.push(FieldError::new(
                "reminder_minutes",
                format!("must be between 0 and {MAX_REMINDER_MINUTES} minutes (one week) before"),
            ));
        }
    }
    errors
}

/// The checks `write` makes, with no write: the person may create
/// activities, and is not someone else's session.
pub(crate) async fn prepare(
    client: &dyn ReadClient,
    scope: &Scope,
    defaults: &ReminderDefaults,
) -> WriteResult<ReminderDefaults> {
    authorize_proof(client, scope, MANAGE).await?;
    if scope.impersonator().is_some() || scope.principal().is_some() {
        return Err(WriteError::Forbidden);
    }
    let errors = check(defaults);
    if !errors.is_empty() {
        return Err(WriteError::Invalid(errors));
    }
    Ok(ReminderDefaults {
        minutes: defaults.minutes.clone().map(offsets),
        email: defaults.email,
    })
}

/// Stores the person's defaults; an unset one goes back to following the
/// tenant.
pub(crate) async fn write(
    settings: &Settings,
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    defaults: &ReminderDefaults,
) -> WriteResult<ReminderDefaults> {
    let defaults = prepare(tx.sql(), scope, defaults).await?;
    let own = Target::User { company: None };
    match &defaults.minutes {
        Some(minutes) => {
            settings
                .put(tx, scope, own, DEFAULT_MINUTES, json!(minutes))
                .await?
        }
        None => settings.delete(tx, scope, own, DEFAULT_MINUTES).await?,
    }
    match defaults.email {
        Some(email) => {
            settings
                .put(tx, scope, own, DEFAULT_EMAIL, json!(email))
                .await?
        }
        None => settings.delete(tx, scope, own, DEFAULT_EMAIL).await?,
    }
    Ok(defaults)
}

/// May the reader read their own defaults.
pub(crate) async fn may_read(client: &dyn ReadClient, scope: &Scope) -> Result<()> {
    authorize_proof(client, scope, VIEW).await
}
