use super::*;
use crate::{
    contributions::{RETIRE, UPDATE},
    model::kind_key,
    Person, PersonInput, PersonKind,
};
use bilibi_agent_api::Error as ContractError;
use bilibi_ui::{
    BackLink, Badge, BadgeKind, Button, ButtonVariant, Card, CommitStates, CommitStatus,
    ConfirmDialog, Fact, Facts, Field, Header, InlineChoice, InlineEdit, InlineField, InputKind,
    LivePanelStack, PageVariant, Restricted, SectionHeading,
};
use std::collections::BTreeMap;

/// The reveal dialog of one masked contact: the reason typed so far and its errors.
struct Reveal {
    field: String,
    reason: String,
    errors: Vec<String>,
}

#[derive(Template)]
#[template(path = "person_detail.html")]
struct View<'a> {
    person: &'a Person,
    header: Html,
    sections: Html,
    dialog: Html,
}
#[derive(Template)]
#[template(path = "person_actions.html")]
struct Actions {
    retire: Html,
    back: Html,
}
pub(super) struct Detail {
    pub pages: Pages,
    pub id: PersonId,
    active_panels: Vec<PanelSpec>,
    statuses: CommitStates,
    /// The person's name and reviewed input.
    pending_retire: Option<(String, Value)>,
    pub(super) notice: String,
    reveal: Option<Reveal>,
    /// Contacts revealed in this view (ADR 05), with the record version they
    /// were read at. Never stored; a new view masks again, and a value is
    /// shown only while the reader still may reveal it and the record has not
    /// changed.
    revealed: BTreeMap<String, (String, i64)>,
    /// The page is framed in a workspace tile.
    framed: bool,
    picking: BTreeMap<String, String>,
}
impl Detail {
    pub fn new(pages: Pages, id: PersonId, framed: bool) -> Self {
        Self {
            framed,
            picking: BTreeMap::new(),
            pages,
            id,
            active_panels: Vec::new(),
            statuses: CommitStates::default(),
            pending_retire: None,
            notice: String::new(),
            reveal: None,
            revealed: BTreeMap::new(),
        }
    }
    fn inline(
        &self,
        person: &Person,
        key: &str,
        label: &str,
        value: &str,
        editable: bool,
        prefix: &str,
    ) -> InlineField {
        let mut field = InlineField::new(format!("{prefix}-{key}"), key, label, value)
            .version(person.version)
            .status(self.statuses.get(key));
        field.id_value = self.id.get().to_string();
        if key != "name" {
            field = field.allow_empty();
        }
        if !editable {
            field = field.readonly();
        }
        field
    }
    /// The contact facts. `revealable` is the record page's own view: an
    /// embedded panel shows masked contacts without the reveal control.
    pub async fn contacts(
        &self,
        scope: &Scope,
        words: &bilibi_grid::RecordWords,
        person: &Person,
        prefix: &str,
        revealable: bool,
    ) -> Result<Html> {
        let editable = person.retired_at.is_none() && self.pages.allowed(scope, UPDATE).await?;
        let can_reveal = revealable
            && self
                .pages
                .allowed(scope, bilibi_privacy::FIELD_REVEAL)
                .await?;
        let revealed = |key: &str| {
            self.revealed
                .get(key)
                .filter(|(_, version)| can_reveal && *version == person.version)
                .map(|(value, _)| value.as_str())
        };
        let mut items = Vec::new();
        for (key, value) in [
            ("email", person.email.as_deref().unwrap_or("")),
            ("phone", person.phone.as_deref().unwrap_or("")),
        ] {
            let label = words.get(&super::fields::get(key));
            items.push(Fact {
                title: words.get(&super::fields::get(key)),
                value: if person.masked.iter().any(|m| m == key) {
                    // A partial value, or the full one revealed in this view.
                    let full = revealed(key);
                    let shown = full.unwrap_or(value);
                    let mut cell = Html::text(if shown.is_empty() { "—" } else { shown });
                    if can_reveal && !shown.is_empty() && full.is_none() {
                        cell.push(&render(bilibi_ui::IconButton {
                            id: &format!("{prefix}-{key}-reveal"),
                            kind: bilibi_ui::IconKind::Neutral,
                            event: "open_reveal",
                            name: "field",
                            value: key,
                            ..bilibi_ui::IconButton::new(
                                "hero-eye",
                                &format!("Reveal {}", label.to_lowercase()),
                            )
                        })?);
                    }
                    cell
                } else if person.restricted.iter().any(|r| r == key) {
                    render(Restricted::new(format!("{prefix}-{key}-restricted")))?
                } else {
                    render(InlineEdit {
                        field: self.inline(person, key, label, value, editable, prefix),
                    })?
                },
                ..Fact::default()
            });
        }
        let mut body = render(SectionHeading {
            title: "Personal contacts",
            ..SectionHeading::default()
        })?;
        body.push(&render(Facts {
            id: &format!("{prefix}-facts"),
            items,
        })?);
        render(Card {
            id: &format!("{prefix}-card"),
            role: "region",
            body,
            ..Card::default()
        })
    }
    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let client = self.pages.db.get().await?;
        let words = bilibi_grid::RecordWords::load(&**client, scope, "persons").await?;
        drop(client);
        let person = self.pages.persons.read(scope, self.id).await?;
        self.active_panels.clear();
        for panel in &self.pages.person_panels {
            if self.pages.embed_allowed(scope, panel.access).await? {
                let mut path = HashMap::from([
                    ("panel".into(), panel.anchor.clone()),
                    ("id".into(), self.id.get().to_string()),
                    ("kinds".into(), "person".into()),
                ]);
                // The record-history panel accepts exactly panel/kinds/id
                // (`bilibi_audit_views::history`); the Person panels also
                // read the person under their own name.
                if panel.anchor != "person-record-history" {
                    path.insert("person_id".into(), self.id.get().to_string());
                }
                self.active_panels.push(PanelSpec {
                    id: panel.anchor.clone(),
                    embed: panel.embed.clone(),
                    path,
                    query: HashMap::new(),
                });
            }
        }
        let editable = person.retired_at.is_none() && self.pages.allowed(scope, UPDATE).await?;
        let mut actions = Html::EMPTY;
        if person.retired_at.is_none()
            && person.system_key.is_none()
            && self.pages.allowed(scope, RETIRE).await?
        {
            actions.push(&render(Button {
                id: "person-retire",
                button_type: "button",
                event: "request_retire",
                variant: ButtonVariant::Danger,
                body: Html::text("Retire contact"),
                ..Button::default()
            })?);
        }
        let back = render(BackLink {
            href: "/contacts",
            id: "person-back",
            title: "Back to contacts",
            ..BackLink::default()
        })?;
        let actions = render(Actions {
            retire: actions,
            back,
        })?;
        let header = render(Header {
            title: Html::text(&person.name),
            actions,
            ..Header::default()
        })?;
        let kind = kind_key(person.kind);
        let kind_field = self.inline(
            &person,
            "kind",
            words.get(&super::fields::KIND),
            kind,
            editable && person.system_key.is_none(),
            "person-field",
        );
        let kind_editor = render(
            Field::named("kind")
                .id("person-kind-editor")
                .label_class("sr-only")
                .label(words.get(&super::fields::KIND))
                .kind(InputKind::Select)
                .value(kind)
                .options(bilibi_ui::choices(&[
                    ("Human", "human"),
                    ("Agent", "agent"),
                ])),
        )?;
        let mut items = vec![
            Fact {
                title: words.get(&super::fields::CODE),
                value: Html::text(&person.code),
                ..Fact::default()
            },
            Fact {
                title: words.get(&super::fields::NAME),
                value: render(InlineEdit {
                    field: self.inline(
                        &person,
                        "name",
                        words.get(&super::fields::NAME),
                        &person.name,
                        editable,
                        "person-field",
                    ),
                })?,
                ..Fact::default()
            },
            Fact {
                title: words.get(&super::fields::KIND),
                value: render(InlineChoice {
                    field: kind_field,
                    display: render(Badge {
                        body: Html::text(kind),
                        ..Badge::default()
                    })?,
                    editor: kind_editor,
                })?,
                ..Fact::default()
            },
            Fact {
                title: words.get(&super::fields::IDENTITY),
                value: render(Badge::status(
                    if person.retired_at.is_some() {
                        BadgeKind::Neutral
                    } else {
                        BadgeKind::Success
                    },
                    if person.retired_at.is_some() {
                        "retired"
                    } else {
                        "current"
                    },
                ))?,
                ..Fact::default()
            },
        ];
        // The tenant's own fields, edited in place through
        // `core.person.update` (`save_custom_field`).
        // A record field's pick saves through the same contract at once.
        let definitions = super::custom::active(&self.pages, scope).await?;
        // Resolved once for every date field, not once per field, and every
        // record field's record named in one call.
        let display = self.pages.display(scope).await?;
        let names = super::custom::Names::read(
            scope,
            &definitions.iter().collect::<Vec<_>>(),
            [&person.custom],
        )
        .await?;
        let shown = super::custom::Shown {
            pages: &self.pages,
            scope,
            display: &display,
            names: &names,
            framed: self.framed,
        };
        for definition in &definitions {
            let name = crate::custom::name(&definition.key);
            let stored = person.custom.get(&definition.key);
            let value = super::custom::inline_value(definition, stored);
            let mut field = self.inline(
                &person,
                &name,
                &definition.label,
                &value,
                editable,
                "person-field",
            );
            field.id = format!("person-field-{}", name.replace('_', "-"));
            field.save_event = "save_custom_field".into();
            field = field.allow_empty();
            let query = self.picking.get(&name).map_or("", String::as_str);
            let hits = if editable && definition.kind == bilibi_custom_fields::Kind::Reference {
                super::custom::hits(scope, definition, query).await?
            } else {
                Vec::new()
            };
            let picking = editable.then(|| super::custom::Picking {
                query,
                hits: &hits,
                errors: match self.statuses.get(&name) {
                    CommitStatus::Error(message) => vec![message],
                    _ => Vec::new(),
                },
            });
            items.push(Fact {
                title: &definition.label,
                value: shown.fact(definition, stored, field, picking).await?,
                ..Fact::default()
            });
        }
        let mut identity = render(SectionHeading {
            title: "Identity and contact details",
            ..SectionHeading::default()
        })?;
        identity.push(&render(Facts {
            id: "person-facts",
            items,
        })?);
        let identity = render(Card {
            id: "person-identity",
            role: "region",
            body: identity,
            ..Card::default()
        })?;
        let dialog = if let Some(reveal) = &self.reveal {
            let mut actions = render(Button {
                button_type: "button",
                event: "close_reveal",
                body: Html::text("Cancel"),
                ..Button::default()
            })?;
            actions.push(&render(Button {
                button_type: "submit",
                variant: ButtonVariant::Primary,
                body: Html::text("Reveal"),
                ..Button::default()
            })?);
            let field = Field::in_form("reveal", "reason")
                .id("person-reveal-reason")
                .label("Why you need the full value")
                .hint("A case or ticket reference. The reveal is recorded with it.")
                .value(reveal.reason.clone())
                .errors(reveal.errors.clone())
                .required();
            let title = format!(
                "Reveal {}",
                if reveal.field == "email" {
                    "personal email"
                } else {
                    "personal phone"
                }
            );
            render(bilibi_ui::Modal {
                id: "person-reveal-modal",
                title: &title,
                cancel_event: "close_reveal",
                description: "You see this contact masked. Revealing it shows the full value in this view only.",
                body: render(bilibi_ui::Form {
                    id: "person-reveal-form",
                    event: "reveal",
                    body: render(field)?,
                    actions,
                    ..bilibi_ui::Form::default()
                })?,
                ..bilibi_ui::Modal::default()
            })?
        } else if let Some((name, _)) = &self.pending_retire {
            render(ConfirmDialog {
                id: "person-retire-confirm",
                consequence: &format!("{name} will be retired."),
                detail: "Their details, history and affiliations are kept. The contact cannot be edited or linked again.",
                confirm: "Retire",
                working: "Retiring…",
                confirm_event: "retire",
                cancel_event: "cancel_retire",
            })?
        } else {
            Html::EMPTY
        };
        let panels = self.panels();
        let panel_ids: Vec<_> = panels
            .iter()
            .filter(|panel| panel.id != "person-record-history")
            .map(|panel| panel.id.as_str())
            .collect();
        let contacts = self
            .contacts(scope, &words, &person, "person-contact", true)
            .await?;
        let body = render(View {
            person: &person,
            header,
            sections: render(LivePanelStack {
                parts: &[identity, contacts],
                panel_ids: &panel_ids,
            })?,
            dialog,
        })?;
        Ok(LiveBody {
            html: render(bilibi_ui::Page {
                id: "person-detail",
                variant: PageVariant::Detail,
                body,
                ..bilibi_ui::Page::default()
            })?
            .to_string(),
            notice: self.notice.clone(),
            rows: 1,
            ..LiveBody::default()
        })
    }
    /// Saves one custom field (`field` is `cf_<key>`) through
    /// `core.person.update` with the identity as it stands and records the
    /// outcome on its fact; `true` when it was saved. A record field's
    /// refusal is the contract's own sentence (never the id); another
    /// field's repeats what was typed.
    async fn save_custom(
        &mut self,
        scope: &Scope,
        credential: &str,
        field: &str,
        definition: Option<&bilibi_custom_fields::Definition>,
        version: i64,
        pair: Value,
        submitted: &str,
    ) -> Result<bool> {
        let person = self.pages.persons.read(scope, self.id).await?;
        let identity = current_identity(person);
        // A refusal names the field as the page does.
        let label = definition.map_or("This field", |definition| definition.label.as_str());
        let record = definition
            .is_some_and(|definition| definition.kind == bilibi_custom_fields::Kind::Reference);
        let result = self
            .pages
            .contracts
            .submit(
                &self.pages.call(scope, credential, "save_custom_field", "core.person.update"),
                json!({"person_id": self.id.get(), "version": version, "identity": identity_json(&identity), "custom": [pair]}),
            )
            .await;
        let status = match result {
            Ok(_) => CommitStatus::Saved,
            Err(ContractError::Backend(
                error @ (Error::Unauthorized | Error::StepUpRequired { .. }),
            )) => return Err(error),
            Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                self.notice =
                    "You do not have permission to edit this contact, or it is retired.".into();
                self.statuses.clear_saved();
                CommitStatus::Error(self.notice.clone())
            }
            Err(ContractError::InvalidInput(errors)) => {
                let messages: Vec<&str> = errors
                    .values()
                    .flat_map(|messages| messages.iter().map(String::as_str))
                    .collect();
                if record {
                    CommitStatus::Error(if messages.is_empty() {
                        bilibi_ui::error_sentence(label, "is not valid")
                    } else {
                        messages
                            .iter()
                            .map(|message| bilibi_ui::error_sentence(label, message))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                } else {
                    CommitStatus::refusal(
                        label,
                        submitted,
                        if messages.is_empty() {
                            &["is not valid"]
                        } else {
                            &messages
                        },
                    )
                }
            }
            Err(ContractError::Backend(Error::Conflict)) if record => {
                CommitStatus::Error(bilibi_ui::error_sentence(
                    label,
                    "has changed since this edit began; reload before trying again",
                ))
            }
            Err(ContractError::Backend(Error::Conflict)) => CommitStatus::refusal(
                label,
                submitted,
                &["has changed since this edit began; reload before trying again"],
            ),
            Err(_) => CommitStatus::failure(),
        };
        let saved = status == CommitStatus::Saved;
        self.statuses.put(field, status);
        Ok(saved)
    }
    pub async fn handle(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        self.notice.clear();
        match event(value)? {
            "save_field" => {
                let field = value["field"].as_str().ok_or(Error::Invalid)?;
                if !["name", "kind", "email", "phone"].contains(&field)
                    || value["id"].as_str() != Some(&self.id.get().to_string())
                {
                    return Err(Error::Invalid);
                }
                let submitted = value["value"].as_str().ok_or(Error::Invalid)?;
                let version = value["version"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                let person = self.pages.persons.read(scope, self.id).await?;
                let mut identity = current_identity(person);
                match field {
                    "name" => identity.name = submitted.into(),
                    "email" => identity.email = Some(submitted.into()),
                    "phone" => identity.phone = Some(submitted.into()),
                    "kind" => {
                        identity.kind = match submitted {
                            "human" => PersonKind::Human,
                            "agent" => PersonKind::Agent,
                            _ => return Err(Error::Invalid),
                        }
                    }
                    _ => unreachable!(),
                }
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &self.pages.call(scope, credential, "save_field", "core.person.update"),
                        json!({"person_id": self.id.get(), "version": version, "identity": identity_json(&identity)}),
                    )
                    .await;
                // If visibility was revoked while this edit was in flight, the
                // refusal must never echo the now-hidden submitted contact.
                let masked = self
                    .pages
                    .persons
                    .read(scope, self.id)
                    .await?
                    .restricted
                    .iter()
                    .any(|r| r == field);
                let status = match result {
                    Ok(_) => CommitStatus::Saved,
                    Err(ContractError::Backend(
                        error @ (Error::Unauthorized | Error::StepUpRequired { .. }),
                    )) => return Err(error),
                    // The owner refuses a retired or protected identity as
                    // forbidden too; the record says which.
                    Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                        let person = self.pages.persons.read(scope, self.id).await?;
                        if person.retired_at.is_some() {
                            CommitStatus::Error(
                                "This identity is retired and cannot be edited.".into(),
                            )
                        } else if person.system_key.is_some() {
                            CommitStatus::Error("The platform system identity is protected.".into())
                        } else {
                            self.notice = "You do not have permission to edit this contact.".into();
                            self.statuses.clear_saved();
                            CommitStatus::Error(self.notice.clone())
                        }
                    }
                    Err(ContractError::InvalidInput(errors)) => {
                        let messages: Vec<&str> = errors
                            .iter()
                            .filter(|(path, _)| path.ends_with(field))
                            .flat_map(|(_, messages)| messages.iter().map(String::as_str))
                            .collect();
                        CommitStatus::refusal(
                            field,
                            if masked { "" } else { submitted },
                            if messages.is_empty() {
                                &["is not valid"]
                            } else {
                                &messages
                            },
                        )
                    }
                    Err(ContractError::Backend(Error::Conflict)) => CommitStatus::refusal(
                        field,
                        if masked { "" } else { submitted },
                        &["has changed since this edit began; reload before trying again"],
                    ),
                    Err(_) => CommitStatus::failure(),
                };
                self.statuses.put(field, status);
            }
            // One custom field (`cf_<key>`): the identity is sent as it is, so
            // only that value changes; the owner checks it against the
            // tenant's definitions. Several choices arrive as the ticked
            // codes joined by `,`.
            "save_custom_field" => {
                let field = value["field"].as_str().ok_or(Error::Invalid)?.to_owned();
                let Some(key) = field.strip_prefix("cf_").map(str::to_owned) else {
                    return Err(Error::Invalid);
                };
                if value["id"].as_str() != Some(&self.id.get().to_string()) {
                    return Err(Error::Invalid);
                }
                let submitted = value["value"].as_str().ok_or(Error::Invalid)?;
                let version = value["version"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                let definition = super::custom::active(&self.pages, scope)
                    .await?
                    .into_iter()
                    .find(|definition| definition.key == key);
                let pair = match definition.as_ref().map(|d| d.kind) {
                    Some(bilibi_custom_fields::Kind::Choices) => {
                        let codes = bilibi_custom_fields::codes_value([submitted]);
                        json!({"key": key, "values": if codes.is_null() { json!([]) } else { codes }})
                    }
                    _ => json!({"key": key, "value": submitted}),
                };
                self.save_custom(
                    scope,
                    credential,
                    &field,
                    definition.as_ref(),
                    version,
                    pair,
                    submitted,
                )
                .await?;
            }
            // A record field's picker: typing searches, a hit saves the field
            // at once with the record's version, and clearing saves none.
            super::custom::REFERENCE_SEARCH => {
                let form = bilibi_ui::params::form_values_checked(
                    value,
                    super::custom::REFERENCE_FORM,
                    bilibi_ui::params::FIELD_LIMIT,
                )
                .map_err(Error::refused)?;
                let definitions = super::custom::active(&self.pages, scope).await?;
                for (posted, query) in form {
                    let Some(name) = posted.strip_suffix("_search") else {
                        continue;
                    };
                    reference_field(&definitions, name)?;
                    let query: String = query.chars().take(100).collect();
                    self.picking.insert(name.to_owned(), query);
                }
            }
            name @ (super::custom::PICK_EVENT | super::custom::CLEAR_EVENT) => {
                let picked = name == super::custom::PICK_EVENT;
                let field = value["key"].as_str().ok_or(Error::Invalid)?;
                let definitions = super::custom::active(&self.pages, scope).await?;
                let definition = reference_field(&definitions, field)?;
                let id = if picked {
                    let raised = value["record"].as_str().ok_or(Error::Invalid)?;
                    Some(bilibi_custom_fields::picked(definition, raised).ok_or(Error::Invalid)?)
                } else {
                    None
                };
                let version = self.pages.persons.read(scope, self.id).await?.version;
                let key = definition.key.clone();
                let field = field.to_owned();
                if self
                    .save_custom(
                        scope,
                        credential,
                        &field,
                        Some(definition),
                        version,
                        json!({"key": key, "value": id}),
                        "",
                    )
                    .await?
                {
                    self.picking.remove(&field);
                }
            }
            // Retiring is the contract's dry run, then its rechecked commit
            // once the person confirms.
            "request_retire" => {
                if self.pages.allowed(scope, RETIRE).await? {
                    let person = self.pages.persons.read(scope, self.id).await?;
                    if person.retired_at.is_none() && person.system_key.is_none() {
                        let input = json!({"person_id": self.id.get(), "version": person.version});
                        let review = self
                            .pages
                            .contracts
                            .preview(
                                &self
                                    .pages
                                    .call(scope, credential, "retire", "core.person.retire"),
                                input.clone(),
                            )
                            .await;
                        match review {
                            Ok(review) if review["valid"] == true => {
                                self.pending_retire = Some((person.name, input))
                            }
                            Ok(_) => self.notice = "The contact cannot be retired. Reload and review it before trying again.".into(),
                            Err(ContractError::Backend(Error::Unauthorized)) => return Err(Error::Unauthorized),
                            Err(_) => self.notice = "The contact was not retired. Review your permission and reload before trying again.".into(),
                        }
                    }
                } else {
                    self.notice = "You do not have permission to retire this contact.".into();
                }
            }
            "cancel_retire" => self.pending_retire = None,
            "retire" => {
                if let Some((_, input)) = self.pending_retire.take() {
                    let result = self
                        .pages
                        .contracts
                        .commit(
                            &self
                                .pages
                                .call(scope, credential, "retire", "core.person.retire"),
                            input,
                        )
                        .await;
                    self.notice = match result {
                    Ok(_) => "Contact retired; history preserved.".into(),
                    Err(ContractError::Backend(error @ (Error::Unauthorized | Error::StepUpRequired { .. }))) => return Err(error),
                    Err(ContractError::Backend(Error::Conflict) | ContractError::PlanRejected(_)) => "The contact changed while confirmation was open. Review it before trying again.".into(),
                    Err(_) => "The contact was not retired. Review your permission and reload before trying again.".into(),
                };
                }
            }
            "open_reveal" => {
                let field = value["field"].as_str().unwrap_or_default();
                if ["email", "phone"].contains(&field) {
                    self.reveal = Some(Reveal {
                        field: field.into(),
                        reason: String::new(),
                        errors: Vec::new(),
                    });
                }
            }
            "close_reveal" => self.reveal = None,
            "reveal" => {
                let reason = bilibi_ui::params::form_values_checked(
                    value,
                    "reveal",
                    bilibi_ui::params::FIELD_LIMIT,
                )
                .map_err(Error::refused)?
                .get("reason")
                .cloned()
                .unwrap_or_default();
                let Some(open) = self.reveal.take() else {
                    return Ok(());
                };
                let version = self.pages.persons.read(scope, self.id).await?.version;
                // The privacy contract checks the capability, reason and Step-up
                // and records the reveal before it answers.
                match self
                    .pages
                    .contracts
                    .submit(
                        &self.pages.call(scope, credential, "reveal", "base.privacy.field.reveal"),
                        json!({"table": "persons", "field": open.field, "record": self.id.get(), "reason": reason}),
                    )
                    .await
                {
                    Ok(result) => {
                        let full = result["result"]["value"].as_str().unwrap_or_default().to_owned();
                        self.revealed.insert(open.field, (full, version));
                        self.notice =
                            "Revealed for this view. The reveal is recorded with your reason."
                                .into();
                    }
                    Err(ContractError::Backend(
                        error @ (Error::Unauthorized | Error::StepUpRequired { .. }),
                    )) => return Err(error),
                    Err(ContractError::InvalidInput(_) | ContractError::Backend(Error::Invalid)) => {
                        self.reveal = Some(Reveal {
                            errors: vec!["Give a reason of up to 255 characters.".into()],
                            reason,
                            ..open
                        });
                    }
                    Err(_) => {
                        self.notice = "The contact could not be revealed. Your access may have changed; reload and try again.".into();
                    }
                }
            }
            // The host's success flash (a created contact) dismisses with this event.
            "clear-flash" => {}
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
}
impl LiveState<Scope, Error> for Detail {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                self.handle(request.scope, request.credential, value)
                    .await?;
            }
            self.view(request.scope).await
        })
    }
    fn panels(&self) -> Vec<PanelSpec> {
        self.active_panels.clone()
    }
}

/// The active record field named `name` (`cf_<key>`); any other is a
/// tampered event.
fn reference_field<'a>(
    definitions: &'a [bilibi_custom_fields::Definition],
    name: &str,
) -> Result<&'a bilibi_custom_fields::Definition> {
    definitions
        .iter()
        .find(|definition| {
            definition.kind == bilibi_custom_fields::Kind::Reference
                && crate::custom::name(&definition.key) == name
        })
        .ok_or(Error::Invalid)
}

/// The person's identity as a replacement input. A restricted contact is
/// never carried: a masked one holds only its partial value, and left out the
/// owner keeps the stored value.
fn current_identity(person: Person) -> PersonInput {
    let masked = |key: &str| person.masked.iter().any(|m| m == key);
    PersonInput {
        email: person.email.clone().filter(|_| !masked("email")),
        phone: person.phone.clone().filter(|_| !masked("phone")),
        name: person.name,
        kind: person.kind,
    }
}

/// The contract's identity input: a hidden contact is omitted, so the owner
/// keeps it.
fn identity_json(identity: &PersonInput) -> Value {
    let mut value = json!({"name": identity.name, "kind": kind_key(identity.kind)});
    for (key, contact) in [("email", &identity.email), ("phone", &identity.phone)] {
        if let Some(contact) = contact {
            value[key] = json!(contact);
        }
    }
    value
}
