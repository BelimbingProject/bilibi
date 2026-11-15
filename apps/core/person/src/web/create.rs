//! New-contact dialog on the Contacts list, using
//! the same shared modal as legal entity types (`company/web/reference_types`).
//! `/contacts/new` only sends the browser to the list with that dialog open.
use super::*;
use crate::{DuplicateQuery, Person, PersonInput, PersonKind, contributions::CREATE, model};
use bilibi_agent_api::Error as ContractError;
use bilibi_custom_fields::Kind;
use bilibi_ui::{
    Alert, Button, ButtonVariant, ErrorSummary, Field, InputKind, RestrictedField, StatusKind,
    error_sentence,
};

const OPEN: &str = "/contacts?new=1";
/// The most ticks one several-choices field posts that the dialog reads.
const CODES_MAX: usize = bilibi_picklists::VALUES_MAX;

/// HTTP and live visits to `/contacts/new` land on the list with the dialog open.
pub(super) struct Create;

impl LiveState<Scope, Error> for Create {
    fn update<'a>(&'a mut self, _request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            Ok(LiveBody {
                redirect: Some(OPEN.into()),
                ..LiveBody::default()
            })
        })
    }
}

pub(super) fn open_query() -> &'static str {
    OPEN
}

/// The dialog's draft. Field errors stay here until the dialog closes.
pub(super) struct Draft {
    input: PersonInput,
    /// Custom field values as typed, by control name (`cf_<key>`).
    custom: std::collections::BTreeMap<String, String>,
    /// Several-choices fields' ticked codes, by control name.
    codes: std::collections::BTreeMap<String, Vec<String>>,
    /// Record fields' pickers by control name: the text typed so far, its
    /// hits, and the record chosen.
    picking: std::collections::BTreeMap<String, Picking>,
    /// The custom fields' labels by control name, for the error summary.
    custom_labels: std::collections::BTreeMap<String, String>,
    /// The custom fields the form last drew, by control name: how each
    /// posted value is read.
    custom_kinds: std::collections::BTreeMap<String, bilibi_custom_fields::Kind>,
    code: String,
    code_edited: bool,
    errors: Vec<(String, String)>,
    notice: String,
    validated: bool,
    submitted: bool,
}

impl Draft {
    pub fn new() -> Self {
        Self {
            input: PersonInput {
                name: String::new(),
                kind: PersonKind::Human,
                email: None,
                phone: None,
            },
            custom: std::collections::BTreeMap::new(),
            codes: std::collections::BTreeMap::new(),
            picking: std::collections::BTreeMap::new(),
            custom_labels: std::collections::BTreeMap::new(),
            custom_kinds: std::collections::BTreeMap::new(),
            code: String::new(),
            code_edited: false,
            errors: Vec::new(),
            notice: String::new(),
            validated: false,
            submitted: false,
        }
    }

    /// Reads the posted form. An unknown kind is a tampered event.
    pub fn read(&mut self, value: &Value) -> Result<()> {
        let fields =
            bilibi_ui::params::form_values_checked(value, "person", bilibi_ui::params::FIELD_LIMIT)
                .map_err(bilibi_database::Error::refused)?;
        self.code = fields.get("code").cloned().unwrap_or_default();
        self.custom.clear();
        for (name, text) in fields.iter().filter(|(name, _)| name.starts_with("cf_")) {
            match self.custom_kinds.get(name) {
                Some(Kind::Reference | Kind::Choices) => {}
                Some(_) | None => {
                    self.custom.insert(name.clone(), text.clone());
                }
            }
        }
        let searches = bilibi_ui::params::form_values_checked(
            value,
            "person_search",
            bilibi_ui::params::FIELD_LIMIT,
        )
        .map_err(bilibi_database::Error::refused)?;
        for (name, text) in searches {
            if self.custom_kinds.get(&name) == Some(&Kind::Reference) {
                self.picking.entry(name).or_default().typed = text.chars().take(100).collect();
            }
        }
        self.codes = self
            .custom_kinds
            .iter()
            .filter(|(_, kind)| **kind == Kind::Choices)
            .map(|(name, _)| {
                let ticked = value
                    .get(format!("person[{name}][]"))
                    .and_then(Value::as_array)
                    .map(|codes| {
                        codes
                            .iter()
                            .filter_map(Value::as_str)
                            .filter(|code| !code.is_empty())
                            .take(CODES_MAX)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                (name.clone(), ticked)
            })
            .collect();
        self.input = PersonInput {
            name: fields.get("name").cloned().unwrap_or_default(),
            kind: match fields.get("kind").map(String::as_str) {
                Some("human") => PersonKind::Human,
                Some("agent") => PersonKind::Agent,
                _ => return Err(Error::Invalid),
            },
            email: fields
                .get("email")
                .cloned()
                .filter(|v| !v.trim().is_empty()),
            phone: fields
                .get("phone")
                .cloned()
                .filter(|v| !v.trim().is_empty()),
        };
        self.validated = true;
        self.errors = model::validate(&self.input)
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|error| (error.field.to_owned(), error.message.to_owned()))
            .collect();
        self.notice.clear();
        Ok(())
    }

    /// Validates, or creates when the draft is clean. `Some` is the new contact.
    pub async fn submit(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        credential: &str,
        create: bool,
        value: &Value,
    ) -> Result<Option<PersonId>> {
        let previous_name = self.input.name.clone();
        self.code_edited |= value
            .get("code_edited")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.read(value)?;
        self.search(pages, scope).await?;
        if !create && self.input.name != previous_name && !self.code_edited {
            let client = pages.db.get().await?;
            let suggestion = if self.input.name.trim().is_empty() {
                String::new()
            } else {
                crate::identity::available_code(&**client, scope.tenant(), &self.input.name).await?
            };
            self.code = suggestion;
        }
        if create {
            self.submitted = true;
        }
        if !create || !self.errors.is_empty() {
            return Ok(None);
        }
        let call = pages.call(scope, credential, "create", "core.person.create");
        let mut input = json!({
            "name": self.input.name,
            "kind": model::kind_key(self.input.kind)
        });
        if self.code_edited && !self.code.trim().is_empty() {
            input["code"] = json!(self.code);
        }
        for (key, contact) in [("email", &self.input.email), ("phone", &self.input.phone)] {
            if let Some(contact) = contact {
                input[key] = json!(contact);
            }
        }
        // Filled custom fields only; the owner checks each against the
        // tenant's definitions and refuses on `cf_<key>`.
        let mut custom: Vec<Value> = self
            .custom
            .iter()
            .filter(|(_, value)| !value.trim().is_empty())
            .filter_map(|(name, value)| {
                Some(json!({"key": name.strip_prefix("cf_")?, "value": value.trim()}))
            })
            .collect();
        for (name, codes) in &self.codes {
            let codes = bilibi_custom_fields::codes_value(codes.iter().map(String::as_str));
            if let (Some(key), false) = (name.strip_prefix("cf_"), codes.is_null()) {
                custom.push(json!({"key": key, "values": codes}));
            }
        }
        for (name, picking) in &self.picking {
            if let (Some(key), Some(chosen)) = (name.strip_prefix("cf_"), &picking.chosen) {
                custom.push(json!({"key": key, "value": chosen.id}));
            }
        }
        if !custom.is_empty() {
            input["custom"] = json!(custom);
        }
        match pages.contracts.submit(&call, input).await {
            Ok(result) => Ok(result["result"]["id"]
                .as_i64()
                .and_then(|id| PersonId::new(id).ok())),
            Err(ContractError::InvalidInput(fields)) => {
                self.attach_input_errors(fields);
                Ok(None)
            }
            Err(ContractError::Backend(
                error @ (Error::Unauthorized | Error::StepUpRequired { .. }),
            )) => Err(error),
            Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                self.notice = "You do not have permission to create this contact or to fill in these contact details.".into();
                Ok(None)
            }
            Err(_) => {
                self.notice = "The contact was not created. Try again.".into();
                Ok(None)
            }
        }
    }

    fn attach_input_errors(&mut self, fields: std::collections::BTreeMap<String, Vec<String>>) {
        let mut attached = false;
        let mut leftover = Vec::new();
        for (path, messages) in fields {
            let key = ["name", "code", "kind", "email", "phone"]
                .into_iter()
                .find(|key| path == *key || path.ends_with(&format!(".{key}")))
                .map(str::to_owned)
                .or_else(|| path.starts_with("cf_").then(|| path.clone()));
            match key {
                Some(key) => {
                    for message in messages {
                        self.errors.push((key.clone(), message));
                    }
                    attached = true;
                }
                None => leftover.extend(messages),
            }
        }
        if !attached {
            self.notice = if leftover.is_empty() {
                "The contact was not created. Check the fields and try again.".into()
            } else {
                leftover.join(" ")
            };
        }
    }

    /// Finds the records each record field's typed text names (only when
    /// it changed), among those the reader may select.
    async fn search(&mut self, pages: &Pages, scope: &Scope) -> Result<()> {
        let changed: Vec<String> = self
            .picking
            .iter()
            .filter(|(_, picking)| picking.typed != picking.searched)
            .map(|(name, _)| name.clone())
            .collect();
        if changed.is_empty() {
            return Ok(());
        }
        let definitions = super::custom::active(pages, scope).await?;
        for name in changed {
            let Some(definition) = definitions
                .iter()
                .find(|d| d.kind == Kind::Reference && crate::custom::name(&d.key) == name)
            else {
                self.picking.remove(&name);
                continue;
            };
            let Some(picking) = self.picking.get_mut(&name) else {
                continue;
            };
            picking.hits = super::custom::hits(scope, definition, &picking.typed).await?;
            picking.searched = picking.typed.clone();
        }
        Ok(())
    }

    /// A record field's picker raised its pick (with `record=kind:id`) or its
    /// clear, naming the field as `key`; `false` when the event is neither.
    pub async fn pick(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        event: &str,
        value: &Value,
    ) -> Result<bool> {
        let picked = match event {
            super::custom::PICK_EVENT => true,
            super::custom::CLEAR_EVENT => false,
            _ => return Ok(false),
        };
        let name = value["key"].as_str().ok_or(Error::Invalid)?;
        if !name.starts_with("cf_") {
            return Ok(false);
        }
        let definitions = super::custom::active(pages, scope).await?;
        let definition = definitions
            .iter()
            .find(|d| d.kind == Kind::Reference && crate::custom::name(&d.key) == name)
            .ok_or(Error::Invalid)?;
        let chosen = if picked {
            let raised = value["record"].as_str().ok_or(Error::Invalid)?;
            Some(super::custom::chosen(scope, definition, raised).await?)
        } else {
            None
        };
        self.errors.retain(|(field, _)| field != name);
        self.picking.insert(
            name.to_owned(),
            Picking {
                chosen,
                ..Picking::default()
            },
        );
        Ok(true)
    }

    pub async fn form(&mut self, pages: &Pages, scope: &Scope) -> Result<Html> {
        let client = pages.db.get().await?;
        let restricted = bilibi_authz::restrictions_for(&**client, scope, "persons").await?;
        drop(client);
        // A newly restricted field discards its server-side draft as well as
        // rendering no input; hidden personal data never returns in a patch.
        if restricted.contains("email") {
            self.input.email = None;
        }
        if restricted.contains("phone") {
            self.input.phone = None;
        }
        let can_create = pages.allowed(scope, CREATE).await?;
        let mut fields = Vec::new();
        let mut masked = Vec::new();
        let input = &self.input;
        for (key, label, value) in [
            ("name", "Name", input.name.as_str()),
            ("code", "Code", self.code.as_str()),
            ("kind", "Kind", model::kind_key(input.kind)),
            (
                "email",
                "Personal email",
                input.email.as_deref().unwrap_or(""),
            ),
            (
                "phone",
                "Personal phone",
                input.phone.as_deref().unwrap_or(""),
            ),
        ] {
            if restricted.contains(key) {
                masked.push(RestrictedField::new(format!("person-{key}"), label));
                continue;
            }
            let mut field = Field::in_form("person", key)
                .id(format!("person-{key}"))
                .label(label)
                .value(value)
                .errors(
                    self.errors
                        .iter()
                        .filter(|(field, _)| field == key)
                        .map(|(_, message)| sentence(label, message))
                        .collect(),
                )
                .attr("data-debounce", "300");
            if ["name", "kind"].contains(&key) {
                field = field.required();
            }
            if !can_create {
                field = field.disabled();
            }
            match key {
                "kind" => {
                    field = field.kind(InputKind::Select).options(bilibi_ui::choices(&[
                        ("Human", "human"),
                        ("Agent", "agent"),
                    ]))
                }
                "code" => {
                    field = field
                        .hint("Clear the code to let Bilibi choose one.")
                        .attr("maxlength", "255")
                        .attr("data-edited-marker", "code_edited")
                }
                "email" => {
                    field = field
                        .kind(InputKind::Email)
                        .autocomplete("email")
                        .placeholder("ada@example.com")
                }
                "phone" => {
                    field = field
                        .kind(InputKind::Tel)
                        .autocomplete("tel")
                        .placeholder("+60 12-345 6789")
                }
                _ => field = field.autocomplete("name").placeholder("Ada Lovelace"),
            }
            fields.push(field);
        }
        let mut fields = fields.into_iter().map(render).collect::<Result<Vec<_>>>()?;
        // The tenant's own fields, posted as `cf_<key>`.
        self.custom_labels.clear();
        self.custom_kinds.clear();
        for definition in super::custom::active(pages, scope).await? {
            let name = crate::custom::name(&definition.key);
            let label = definition.label.clone();
            let errors = self
                .errors
                .iter()
                .filter(|(field, _)| *field == name)
                .map(|(_, message)| sentence(&label, message))
                .collect();
            let picking = self.picking.get(&name);
            let entered = super::custom::Entered {
                text: self.custom.get(&name).map_or("", String::as_str),
                codes: self.codes.get(&name).map_or(&[], Vec::as_slice),
                query: picking.map_or("", |p| p.typed.as_str()),
                hits: picking.map_or(&[], |p| p.hits.as_slice()),
                chosen: picking.and_then(|p| p.chosen.as_ref()),
            };
            fields.push(
                super::custom::control(pages, scope, &definition, entered, errors, can_create)
                    .await?,
            );
            self.custom_labels.insert(name.clone(), label);
            self.custom_kinds.insert(name, definition.kind);
        }
        let duplicates = self.duplicates(pages, scope).await?;
        let summary = if self.submitted && !self.errors.is_empty() {
            let mut summary = ErrorSummary::new("person-create-errors");
            for (field, message) in &self.errors {
                let label = self
                    .custom_labels
                    .get(field)
                    .map_or_else(|| label_for(field), String::as_str);
                summary.push(
                    format!("person-{}", field.replace('_', "-")),
                    sentence(label, message),
                );
            }
            render(summary)?
        } else {
            Html::EMPTY
        };
        let notice = if self.notice.is_empty() {
            Html::EMPTY
        } else {
            render(Alert {
                kind: StatusKind::Error,
                id: "person-create-notice",
                body: Html::text(&self.notice),
                ..Alert::default()
            })?
        };
        render(Form {
            fields,
            restricted: masked,
            duplicates,
            summary,
            notice,
            cancel: render(Button {
                id: "person-cancel",
                button_type: "button",
                event: "close_modal",
                body: Html::text("Cancel"),
                ..Button::default()
            })?,
            submit: render(Button {
                id: "person-create",
                button_type: "submit",
                variant: ButtonVariant::Primary,
                disabled: !can_create,
                working: "Creating…",
                body: Html::text("Create contact"),
                ..Button::default()
            })?,
        })
    }

    async fn duplicates(&self, pages: &Pages, scope: &Scope) -> Result<Vec<Person>> {
        if !self.validated
            || model::validate(&self.input).is_err()
            || !pages.allowed(scope, crate::contributions::LIST).await?
        {
            return Ok(Vec::new());
        }
        let query = DuplicateQuery {
            name: (!self.input.name.trim().is_empty()).then(|| self.input.name.clone()),
            email: self.input.email.clone(),
            phone: self.input.phone.clone(),
            limit: 5,
            ..DuplicateQuery::default()
        };
        pages.persons.duplicates(scope, &query).await
    }
}

fn label_for(field: &str) -> &'static str {
    match field {
        "email" => "Personal email",
        "phone" => "Personal phone",
        "kind" => "Kind",
        "code" => "Code",
        _ => "Name",
    }
}

/// The owner's rule, as one sentence a person can act on.
fn sentence(label: &str, message: &str) -> String {
    let message = match message {
        "must be a valid email address of at most 254 bytes" => {
            "must be an email address, like ada@example.com"
        }
        "must contain at most 64 characters" => "must be 64 characters or fewer",
        "contains an invalid character" => "contains a character that cannot be stored",
        other => other,
    };
    error_sentence(label, message)
}

/// One record field's picker in the dialog.
#[derive(Default)]
struct Picking {
    /// The text typed so far, and the text the hits were found for.
    typed: String,
    searched: String,
    hits: Vec<bilibi_ui::PickerHit>,
    /// The record chosen, named with the reader's access.
    chosen: Option<bilibi_records::Resolved>,
}

#[derive(Template)]
#[template(path = "person_form.html")]
struct Form {
    fields: Vec<Html>,
    restricted: Vec<RestrictedField>,
    duplicates: Vec<Person>,
    summary: Html,
    notice: Html,
    cancel: Html,
    submit: Html,
}
