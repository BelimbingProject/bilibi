//! The customer's own contact details on the portal (ADR 16, issue 474). A
//! Domain's portal facade reads and changes the Person a `PortalScope` signs
//! in as through `PersonService::portal_read` and `portal_update_in`, never
//! with SQL on `persons`. The Person is always the proof's, never a parameter,
//! and the staff capabilities (`admin.person.*`) play no part: the customer
//! needs the portal capability `PORTAL`, which only the portal role holds.
use crate::{identity, model::*, PersonService, WriteError};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{
    Definition, Editor, ScopeKind, Setting, Target, Validator, ValueType, DEFINITIONS,
};
use bilibi_tenancy::{PortalScope, ReadClient, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_postgres::GenericClient;

/// A customer reads, and changes what the tenant allows of, its own contact
/// details on the portal. A portal capability: only portal roles hold it.
pub const PORTAL: &str = "core.person.portal.manage";
/// The contact facts a customer may change on the portal: `name`, `phone`
/// and `cf_<key>` for a custom field. Empty by default; email never.
pub const PORTAL_EDITABLE_KEY: &str = "person.portal_editable";
/// The most entries the editable list takes.
const EDITABLE_MAX: usize = 64;

/// The signed-in customer's own contact details, as a portal page shows them.
#[derive(Clone, Debug, Serialize)]
pub struct PortalPerson {
    pub id: PersonId,
    pub name: String,
    /// The address the customer signs in with is the account's; this is the
    /// contact's personal email, never editable here.
    pub email: Option<String>,
    pub phone: Option<String>,
    /// The version a `PortalUpdate` must name.
    pub version: i64,
    /// The tenant's active contact custom fields with this person's values,
    /// in the fields' order. An archived field is left out.
    pub custom: Vec<PortalField>,
    /// What the customer may change now: `name`, `phone` and `cf_<key>` of
    /// active custom fields, as the tenant's `person.portal_editable` lists.
    pub editable: Vec<String>,
    /// Contacts a field restriction on the customer's role withholds.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub restricted: Vec<String>,
    /// Contacts it shows only partly.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub masked: Vec<String>,
}
impl PortalPerson {
    /// Whether the customer may change `field` (`name`, `phone` or `cf_<key>`).
    pub fn can_edit(&self, field: &str) -> bool {
        self.editable.iter().any(|item| item == field)
    }
}

/// One custom field: `{key, label, type, value}`, plus how a choice reads and
/// what a customer may pick.
#[derive(Clone, Debug, Serialize)]
pub struct PortalField {
    pub key: String,
    pub label: String,
    /// `text`, `number`, `date` or `choice`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The stored value: text, a decimal string, `YYYY-MM-DD` or a choice
    /// code; `None` when empty.
    pub value: Option<String>,
    /// A choice's label in the customer's language (and whether that choice
    /// is retired); `None` for other kinds or no value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub retired: bool,
    pub editable: bool,
    /// For an editable choice field, the active choices in the operator's
    /// order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<PortalChoice>,
}
#[derive(Clone, Debug, Serialize)]
pub struct PortalChoice {
    pub code: String,
    pub label: String,
}

/// A customer's change to its own contact details. A fact left out is kept;
/// a fact the tenant does not let customers change is refused unless it
/// equals what is stored. There is no email: it is the address the customer
/// signs in with and the duplicate key, so an `email` key is refused.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PortalUpdate {
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A blank phone clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// Custom field values by key (a `null` or blank clears); the others are
    /// kept.
    #[serde(default, skip_serializing_if = "Custom::is_empty")]
    pub custom: Custom,
}

const NOT_EDITABLE: &str = "cannot be changed on the portal";

/// The tenant's editable list, as stored: only known shapes count.
fn editable_valid(value: &Value) -> bool {
    let Some(items) = value.as_array() else {
        return false;
    };
    let mut seen = std::collections::HashSet::new();
    items.len() <= EDITABLE_MAX
        && items.iter().all(|item| {
            item.as_str().is_some_and(|text| {
                (matches!(text, "name" | "phone")
                    || text.strip_prefix("cf_").is_some_and(|key| {
                        let mut bytes = key.bytes();
                        key.len() <= 40
                            && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
                            && bytes
                                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                    }))
                    && seen.insert(text)
            })
        })
}

inventory::submit! { Contribution::new(DEFINITIONS, PORTAL_EDITABLE_KEY, Provenance { module: "core/person", source: file!(), line: line!() }, &Setting::Definition(Definition {
    value_type: ValueType::Array, scopes: &[ScopeKind::Tenant], default: || json!([]), nullable: false, encrypted: false,
    minimum: None, maximum: None,
    validator: Some(Validator { accepts: editable_valid, message: "must list name, phone or cf_ and a custom field key, each once, such as [\"name\", \"phone\", \"cf_segment\"]; email is never one of them" }),
    editor: Some(Editor { group: "Contacts", label: "What customers may change on the portal", help: "The contact facts a signed-in customer may change: name, phone, or cf_ and a custom field's key, for example [\"name\", \"phone\", \"cf_segment\"]. Email is never one of them: it is the address they sign in with. Empty: none.", capability: Some(crate::UPDATE), reveal_duration_ms: 5_000 }),
})) }

/// The re-proved portal scope: the customer still stands and still signs in
/// as the same Person, and holds `PORTAL`.
async fn live(
    service: &PersonService,
    client: &(impl GenericClient + Sync),
    portal: &PortalScope,
) -> Result<PortalScope> {
    let live = PortalScope::seal(service.fresh(client, portal.scope()).await?)?;
    if live.person() != portal.person() {
        return Err(Error::Unauthorized);
    }
    bilibi_authz::authorize_portal(client, &live, PORTAL).await?;
    Ok(live)
}

/// What the tenant lets customers change, as stored.
async fn editable(
    service: &PersonService,
    client: &dyn ReadClient,
    scope: &Scope,
) -> Result<Vec<String>> {
    let value = service
        .settings
        .get(client, scope, Target::Tenant, PORTAL_EDITABLE_KEY)
        .await?
        .value;
    Ok(value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

/// The portal read model of `person`, already redacted for the customer.
async fn view(
    service: &PersonService,
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    person: Person,
    allowed: &[String],
) -> Result<PortalPerson> {
    let active = crate::custom::fields()?
        .active(client, scope, crate::custom::RECORD_TYPE)
        .await?;
    let mut custom = Vec::with_capacity(active.len());
    for definition in &active {
        let value = person
            .custom
            .get(&definition.key)
            .and_then(Value::as_str)
            .map(str::to_owned);
        let editable = allowed.contains(&crate::custom::name(&definition.key));
        let (mut text, mut retired, mut choices) = (None, false, Vec::new());
        if let Some(list) = definition.picklist.as_deref() {
            if let Some(code) = &value {
                if let Some(label) = service.picklists.label(client, scope, list, code).await? {
                    text = Some(label.text);
                    retired = label.retired;
                }
            }
            if editable {
                choices = service
                    .picklists
                    .choices(client, scope, list, None)
                    .await?
                    .into_iter()
                    .map(|choice| PortalChoice {
                        code: choice.value,
                        label: choice.label,
                    })
                    .collect();
            }
        }
        custom.push(PortalField {
            key: definition.key.clone(),
            label: definition.label.clone(),
            kind: definition.kind.as_str(),
            value,
            text,
            retired,
            editable,
            choices,
        });
    }
    // Only what can be changed now: an editable contact the customer's role
    // does not withhold, and active custom fields.
    let editable = allowed
        .iter()
        .filter(|item| match item.as_str() {
            "name" => true,
            "phone" => !person
                .restricted
                .iter()
                .chain(&person.masked)
                .any(|f| f == "phone"),
            other => custom
                .iter()
                .any(|field| field.editable && crate::custom::name(&field.key) == other),
        })
        .cloned()
        .collect();
    Ok(PortalPerson {
        id: person.id,
        name: person.name,
        email: person.email,
        phone: person.phone,
        version: person.version,
        custom,
        editable,
        restricted: person.restricted,
        masked: person.masked,
    })
}

/// A submitted custom value as it would be stored: trimmed text, a number as
/// written; `None` clears. `Err` for anything else (Custom Fields names it).
fn submitted(value: &Value) -> std::result::Result<Option<String>, ()> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty())),
        Value::Number(number) => Ok(Some(number.to_string())),
        _ => Err(()),
    }
}

impl PersonService {
    /// The signed-in customer's own name, phone, email and active custom
    /// fields, for a Domain's portal page. Re-proves the customer, asks for
    /// `core.person.portal.manage` and applies the field restrictions of the
    /// customer's role. The Person is the proof's.
    pub async fn portal_read(&self, portal: &PortalScope) -> Result<PortalPerson> {
        let client = self.db.get().await?;
        let portal = live(self, &**client, portal).await?;
        let scope = portal.scope();
        let row = client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM persons WHERE tenant_id=$1 AND id=$2"),
                &[&scope.tenant(), &portal.person().get()],
            )
            .await?
            .ok_or(Error::NotFound)?;
        let restrictions = identity::read_fields(&**client, scope).await?;
        let person = Person::from_row(&row)?.redact(&restrictions);
        let allowed = editable(self, &**client, scope).await?;
        view(self, &**client, scope, person, &allowed).await
    }

    /// Changes the signed-in customer's own contact details in the caller's
    /// transaction, which the caller opened with
    /// `PortalScope::mutation_context`. Only the facts the tenant lists in
    /// `person.portal_editable` may change; email never does. Core's own
    /// validation, the customer role's field restrictions, custom-field
    /// checks, the version check and one audited `person` change by the
    /// customer apply. A change that alters nothing writes nothing.
    pub async fn portal_update_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        portal: &PortalScope,
        update: &PortalUpdate,
    ) -> std::result::Result<PortalPerson, WriteError> {
        let portal = live(self, tx.sql(), portal).await?;
        let scope = portal.scope().clone();
        identity::check_context(tx, &scope)?;
        let id = portal.person();
        let old = identity::raw(tx.sql(), &scope, id, true).await?;
        if old.version != update.version {
            return Err(Error::Conflict.into());
        }
        if old.retired_at.is_some() {
            return Err(WriteError::Retired);
        }
        if old.system_key.is_some() || old.kind != PersonKind::Human {
            return Err(WriteError::SystemIdentity);
        }
        let allowed = editable(self, tx.sql(), &scope).await?;
        let may = |field: &str| allowed.iter().any(|item| item == field);
        let restrictions = identity::read_fields(tx.sql(), &scope).await?;
        // A withheld or masked phone is neither shown nor changed.
        if update.phone.is_some() && restrictions.contains("phone") {
            return Err(Error::Forbidden.into());
        }
        let input = validate(&PersonInput {
            name: update.name.clone().unwrap_or_else(|| old.name.clone()),
            kind: old.kind,
            email: old.email.clone(),
            phone: match &update.phone {
                Some(phone) => Some(phone.clone()),
                None => old.phone.clone(),
            },
        })
        .map_err(WriteError::Validation)?;
        let mut refused = Vec::new();
        if input.name != old.name && !may("name") {
            refused.push(error("name", NOT_EDITABLE));
        }
        if input.phone != old.phone && !may("phone") {
            refused.push(error("phone", NOT_EDITABLE));
        }
        if !refused.is_empty() {
            return Err(WriteError::Validation(refused));
        }
        let mut patch = Custom::new();
        let mut custom_refused = Vec::new();
        for (key, value) in &update.custom {
            let stored = old
                .custom
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned);
            if submitted(value).is_ok_and(|value| value == stored) {
                continue;
            }
            if !may(&crate::custom::name(key)) {
                custom_refused.push(CustomFieldError {
                    field: crate::custom::name(key),
                    message: NOT_EDITABLE.into(),
                });
                continue;
            }
            patch.insert(key.clone(), value.clone());
        }
        if !custom_refused.is_empty() {
            return Err(WriteError::CustomFields(custom_refused));
        }
        let checked = crate::custom::check(tx.sql(), Some(tx.sql()), &scope, &patch, &old.custom)
            .await?
            .map_err(WriteError::CustomFields)?;
        let merged = crate::custom::merge(&old.custom, &checked);
        if input.name == old.name && input.phone == old.phone && merged == old.custom {
            let person = old.redact(&restrictions);
            return Ok(view(self, tx.sql(), &scope, person, &allowed).await?);
        }
        let tenant = scope.tenant();
        let custom = Value::Object(merged);
        let person = tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_one(&format!("UPDATE persons SET name=$3,phone=$4,custom_fields=$5,version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 RETURNING {COLUMNS}"), &[&tenant, &id.get(), &input.name, &input.phone, &custom]).await?;
            let person = Person::from_row(&row)?;
            let change = Change { kind: "person".into(), id: id.get(), old: old.json(), new: person.json() };
            Ok((person, vec![change]))
        })).await?;
        crate::publish(tx, tenant, id);
        Ok(view(
            self,
            tx.sql(),
            &scope,
            person.redact(&restrictions),
            &allowed,
        )
        .await?)
    }
}
