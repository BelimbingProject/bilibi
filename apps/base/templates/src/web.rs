mod index;

use crate::{
    catalogue, operations,
    store::{Edit, Selection},
    Origin, Problem, Slot, Template, Templates, Text, MANAGE, PAGES,
};
use askama::Template as Askama;
use bilibi_agent_api::page::{Call, PageContracts};
use bilibi_database::{Db, Error, Result};
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{NamedOrganisation, Scope};
use bilibi_ui::forms::TextInsertions;
use bilibi_ui::{
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, Alert, Attrs, BackLink, Button, ButtonVariant, Card, Choice, Combobox, ConfirmDialog,
    ErrorSummary, Fact, Facts, Form, Header, Html, InlineField, InlineLongText, InputKind,
    PageVariant, StatusKind,
};
use index::Index;
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

pub(crate) const INDEX: &str = "/templates";

#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub templates: Arc<Templates>,
    pub contracts: Arc<PageContracts>,
}

fn render(component: impl Askama) -> Result<Html> {
    Html::render(&component).map_err(|_| Error::Internal)
}

#[derive(Askama)]
#[template(
    source = r#"{% import "components.html" as ui %}{% call ui::stack(parts=parts, wide=true) %}{% endcall %}"#,
    ext = "html"
)]
struct Stack {
    parts: Vec<Html>,
}

#[derive(Askama)]
#[template(
    source = r#"{% import "components.html" as ui %}{% call ui::cluster(parts=parts) %}{% endcall %}"#,
    ext = "html"
)]
struct Cluster {
    parts: Vec<Html>,
}

fn page(id: &str, variant: PageVariant, body: Html) -> Result<String> {
    bilibi_ui::Page {
        id,
        variant,
        body,
        ..Default::default()
    }
    .render()
    .map_err(|_| Error::Internal)
}

struct Record {
    pages: Pages,
    template: &'static Template,
    companies: Vec<NamedOrganisation>,
    company: Option<OrganisationId>,
    language: String,
    wording: Edit,
    can_manage: bool,
    draft: Option<Text>,
    confirm: bool,
    show_errors: bool,
    status: Option<(StatusKind, String)>,
}

impl Record {
    async fn load(
        pages: &Pages,
        scope: &Scope,
        template: &'static Template,
        input: &HashMap<String, String>,
    ) -> Result<Self> {
        let company = match input.get("company").map(String::as_str) {
            None | Some("" | "workspace") => None,
            Some(value) => Some(
                OrganisationId::try_from(params::positive_id(Some(value)).ok_or(Error::Invalid)?)
                    .map_err(|_| Error::Invalid)?,
            ),
        };
        let language = catalogue::language(
            input
                .get("language")
                .filter(|value| !value.is_empty())
                .map_or(template.base_language, String::as_str),
        )
        .ok_or_else(|| Error::refused("Choose a valid language."))?;
        let client = pages.db.get().await?;
        let companies = pages.templates.companies(&**client, scope).await?;
        let wording = pages
            .templates
            .edit(
                &**client,
                scope,
                &Selection {
                    key: template.key,
                    company,
                    language: &language,
                },
                false,
            )
            .await?;
        let can_manage = !companies
            .iter()
            .any(|c| Some(c.id) == company && c.archived)
            && bilibi_authz::authorize_key(&**client, scope, MANAGE)
                .await
                .is_ok();
        Ok(Self {
            pages: pages.clone(),
            template,
            companies,
            company,
            language,
            wording,
            can_manage,
            draft: None,
            confirm: false,
            show_errors: false,
            status: None,
        })
    }

    fn selectors(&self) -> Result<Html> {
        let mut companies = vec![Choice::new("Workspace", "workspace")];
        companies.extend(
            self.companies
                .iter()
                .map(|company| Choice::new(&company.name, company.id.to_string())),
        );
        let mut language = BTreeMap::from([(
            self.language.as_str(),
            bilibi_locale::label(&self.language).to_owned(),
        )]);
        for default in self.template.defaults {
            language.insert(default.language, language_label(default.language));
        }
        for locale in bilibi_locale::SUPPORTED {
            language.insert(locale.code, language_label(locale.code));
            language
                .entry(locale.language)
                .or_insert_with(|| language_label(locale.language));
        }
        let disabled = Attrs {
            disabled: self.draft.is_some() || self.confirm,
            ..Default::default()
        };
        let companies = render(
            Combobox::new("template-company", "template[company]")
                .label("Applies to")
                .hint(if self.draft.is_some() {
                    "Publish or cancel to change the company or language."
                } else {
                    ""
                })
                .options(companies)
                .attrs(disabled.clone())
                .value(
                    self.company
                        .map_or_else(|| "workspace".into(), |id| id.to_string()),
                ),
        )?;
        let language = render(
            Combobox::new("template-language", "template[language]")
                .label("Language")
                .value(&self.language)
                .attrs(disabled)
                .options(
                    language
                        .into_iter()
                        .map(|(code, label)| Choice::new(label, code))
                        .collect(),
                ),
        )?;
        render(Form {
            id: "template-selectors",
            event: "select_template",
            change: true,
            class: "",
            body: [companies, language].into_iter().collect(),
            actions: Html::EMPTY,
        })
    }

    fn fields(&self, values: &Text, problem: Option<&Problem>) -> Result<Html> {
        self.template
            .slots
            .iter()
            .map(|slot| {
                let field = bilibi_ui::Field::in_form("draft", slot.id)
                    .label(slot.label)
                    .value(values.get(slot.id).map_or("", String::as_str))
                    .kind(match slot.context {
                        crate::Context::Subject => InputKind::Text,
                        crate::Context::PlainText => InputKind::Textarea,
                    })
                    .attrs(field_attrs(slot))
                    .errors(
                        problem
                            .filter(|p| p.field == slot.id)
                            .map(|p| vec![p.message.clone()])
                            .unwrap_or_default(),
                    );
                let choices = self
                    .template
                    .variables
                    .iter()
                    .filter(|v| v.slots.contains(&slot.id))
                    .map(|v| Choice::new(v.label, format!("{{{{{}}}}}", v.id)))
                    .collect();
                render(TextInsertions { field, choices })
            })
            .collect()
    }

    fn facts(&self, values: &Text, group: &str) -> Result<Html> {
        let mut facts = Vec::new();
        let ids: Vec<_> = self
            .template
            .slots
            .iter()
            .map(|slot| format!("{group}_{}", slot.id))
            .collect();
        for (slot, id) in self.template.slots.iter().zip(&ids) {
            let value = values.get(slot.id).map_or("", String::as_str);
            let value = match slot.context {
                crate::Context::Subject => Html::text(value),
                crate::Context::PlainText => {
                    let mut field = InlineField::new(format!("{id}_text"), "", slot.label, value);
                    field.editable = false;
                    render(InlineLongText { field, rows: 5 })?
                }
            };
            facts.push(Fact {
                id,
                title: slot.label,
                value,
            });
        }
        render(Facts {
            id: group,
            items: facts,
        })
    }

    fn content(&self) -> Result<Vec<Html>> {
        let inherited = &self.wording.inherited;
        let current = &self.wording.effective;
        let mut parts = vec![render(Card {
            title: "Wording source",
            body: self.selectors()?,
            ..Default::default()
        })?];
        parts.push(render(Card {
            id: "template-inherited",
            title: "Inherited wording",
            body: self.wording_card(inherited, "inherited")?,
            ..Default::default()
        })?);
        if self.wording.customised && self.draft.is_none() {
            parts.push(render(Card {
                id: "template-current",
                title: "Published wording",
                body: self.wording_card(current, "published")?,
                ..Default::default()
            })?);
        }
        let preview = self
            .template
            .preview(self.draft.as_ref().unwrap_or(&current.slots));
        if let Some(draft) = &self.draft {
            let problem = preview.as_ref().err();
            let actions = render(Cluster {
                parts: vec![
                    button("Cancel", "cancel_customise")?,
                    render(Button {
                        button_type: "submit",
                        variant: ButtonVariant::Primary,
                        body: Html::text("Publish"),
                        working: "Publishing…",
                        ..Default::default()
                    })?,
                ],
            })?;
            let mut errors = ErrorSummary::new("template-publish-errors");
            if let Some(problem) = problem.filter(|_| self.show_errors) {
                errors.push(format!("draft_{}", problem.field), &problem.message);
            }
            let mut body = render(errors)?;
            body.push(&self.fields(draft, problem)?);
            let form = Form {
                id: "template-draft",
                event: "preview_template",
                change: true,
                class: "",
                body,
                actions,
            }
            .submit_event("publish_template")
            .map_err(|_| Error::Internal)?;
            parts.push(render(Card {
                id: "template-editor",
                title: "Custom wording",
                body: form,
                ..Default::default()
            })?);
        } else if self.can_manage {
            let mut actions = vec![button(
                if self.wording.customised {
                    "Edit wording"
                } else {
                    "Customise"
                },
                "customise_template",
            )?];
            if self.wording.customised {
                actions.push(button("Restore inherited wording", "ask_restore")?);
            }
            parts.push(render(Cluster { parts: actions })?);
        }
        let body = match preview {
            Ok(values) => self.facts(&values, "sample")?,
            Err(_) => Html::text("Correct the wording above to see the preview."),
        };
        parts.push(render(Card {
            id: "template-preview",
            title: "Preview with sample details",
            body,
            ..Default::default()
        })?);
        if self.confirm {
            parts.push(render(ConfirmDialog {
                id: "template-restore",
                consequence: "Restore inherited wording?",
                detail: self
                    .status
                    .as_ref()
                    .filter(|(kind, _)| matches!(kind, StatusKind::Error))
                    .map_or(
                        "Future messages will use the inherited wording shown on this page.",
                        |(_, message)| message,
                    ),
                confirm: "Restore inherited wording",
                working: "Restoring…",
                confirm_event: "restore_template",
                cancel_event: "cancel_restore",
            })?);
        }
        Ok(parts)
    }

    fn wording_card(&self, wording: &crate::Resolved, form: &str) -> Result<Html> {
        let origin = match &wording.origin {
            Origin::Packaged { version } => format!("Bilibi default · Version {version}"),
            Origin::Tenant { revision } => format!("Workspace · Revision {revision}"),
            Origin::Company {
                company_id,
                revision,
            } => {
                let name = self
                    .companies
                    .iter()
                    .find(|company| company.id.get() == *company_id)
                    .map_or("Company", |company| company.name.as_str());
                format!("{name} · Revision {revision}")
            }
        };
        render(Stack {
            parts: vec![
                Html::text(&format!("{} · {origin}", language_label(&wording.language))),
                self.facts(&wording.slots, form)?,
            ],
        })
    }

    fn render(&self) -> Result<LiveBody> {
        let mut header = render(BackLink {
            href: INDEX,
            title: "Templates",
            ..Default::default()
        })?;
        header.push(&render(Header {
            title: Html::text(self.template.label),
            ..Default::default()
        })?);
        let mut parts = self.content()?;
        if let Some((kind, message)) = self.status.as_ref().filter(|_| !self.confirm) {
            parts.insert(
                0,
                render(Alert {
                    kind: *kind,
                    id: "template-status",
                    body: Html::text(message),
                    ..Default::default()
                })?,
            );
        }
        header.push(&render(Stack { parts })?);
        let mut query = HashMap::from([("language".into(), self.language.clone())]);
        if let Some(company) = self.company {
            query.insert("company".into(), company.to_string());
        }
        Ok(LiveBody {
            html: page("template-detail", PageVariant::Detail, header)?,
            query,
            ..Default::default()
        })
    }

    async fn write(&mut self, request: &LiveRequest<'_, Scope>, event: &str) -> Result<()> {
        let mut input = json!({
            "key":self.template.key, "company_id":self.company, "language":self.language,
            "revision":self.wording.revision,
        });
        let contract = if event == "publish_template" {
            let draft = self.draft.as_ref().ok_or(Error::Invalid)?;
            input["slots"] = json!(draft);
            operations::PUBLISH
        } else {
            if !self.confirm {
                return Err(Error::Invalid);
            }
            operations::RESTORE
        };
        let call = Call {
            scope: request.scope,
            credential: request.credential,
            service: PAGES,
            event,
            contract,
        };
        let outcome = self.pages.contracts.submit(&call, input).await;
        self.status = Some(match outcome {
            Ok(_) => {
                self.confirm = false;
                let client = self.pages.db.get().await?;
                self.wording = self
                    .pages
                    .templates
                    .edit(
                        &**client,
                        request.scope,
                        &Selection {
                            key: self.template.key,
                            company: self.company,
                            language: &self.language,
                        },
                        false,
                    )
                    .await?;
                self.draft = None;
                (
                    StatusKind::Success,
                    if event == "publish_template" {
                        "Wording published."
                    } else {
                        "Inherited wording restored."
                    }
                    .into(),
                )
            }
            Err(bilibi_agent_api::Error::InvalidInput(fields)) => {
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| self.template.preview(draft).is_err())
                {
                    self.show_errors = true;
                    self.status = None;
                    return Ok(());
                }
                let message = fields
                    .into_values()
                    .flatten()
                    .next()
                    .unwrap_or_else(|| "Check the wording and try again.".into());
                (StatusKind::Error, message)
            }
            Err(bilibi_agent_api::Error::Backend(Error::Conflict)) => (
                StatusKind::Error,
                "Someone changed this wording. Copy your draft before reopening the template."
                    .into(),
            ),
            Err(bilibi_agent_api::Error::Backend(Error::Refused(message))) => {
                (StatusKind::Error, message)
            }
            Err(
                bilibi_agent_api::Error::Forbidden
                | bilibi_agent_api::Error::Backend(Error::Forbidden),
            ) => (
                StatusKind::Error,
                "You no longer have permission to change this wording.".into(),
            ),
            Err(_) => return Err(Error::Internal),
        });
        Ok(())
    }
}

fn field_attrs(slot: &Slot) -> Attrs {
    Attrs {
        maxlength: Some(slot.max_bytes.to_string()),
        rows: (slot.context == crate::Context::PlainText).then(|| "5".into()),
        extra: vec![("data-debounce".into(), "250".into())],
        ..Default::default()
    }
}

fn language_label(code: &str) -> String {
    let label = bilibi_locale::normalize(code)
        .map(bilibi_locale::label)
        .unwrap_or(code);
    if label == code {
        return code.into();
    }
    let label = if code.contains('-') {
        label
    } else {
        label.split_once(" (").map_or(label, |(name, _)| name)
    };
    format!("{code} · {label}")
}

fn button(label: &str, event: &str) -> Result<Html> {
    render(Button {
        button_type: "button",
        event,
        body: Html::text(label),
        ..Default::default()
    })
}

impl LiveState<Scope, Error> for Record {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    *self = Self::load(&self.pages, request.scope, self.template, values).await?
                }
                LiveInput::Event(event) => match event["type"].as_str() {
                    Some("select_template") if self.draft.is_none() && !self.confirm => {
                        let values =
                            params::form_values_checked(event, "template", params::FIELD_LIMIT)
                                .map_err(Error::refused)?;
                        *self =
                            Self::load(&self.pages, request.scope, self.template, &values).await?;
                    }
                    Some("customise_template") if self.can_manage => {
                        self.draft = Some(self.wording.effective.slots.clone());
                        self.status = None;
                    }
                    Some("cancel_customise") => {
                        self.draft = None;
                        self.status = None;
                    }
                    Some("ask_restore") if self.can_manage && self.wording.customised => {
                        self.status = None;
                        self.confirm = true;
                    }
                    Some("cancel_restore") => {
                        self.status = None;
                        self.confirm = false;
                    }
                    Some("preview_template" | "publish_template") if self.draft.is_some() => {
                        let limit = self
                            .template
                            .slots
                            .iter()
                            .map(|slot| slot.max_bytes)
                            .max()
                            .unwrap_or(0);
                        let values = params::form_values_checked(event, "draft", limit)
                            .map_err(Error::refused)?;
                        self.draft = Some(
                            values
                                .into_iter()
                                .map(|(key, value)| (key, value.replace("\r\n", "\n")))
                                .collect(),
                        );
                        self.status = None;
                        self.show_errors = false;
                        if event["type"] == "publish_template" {
                            self.write(&request, "publish_template").await?;
                        }
                    }
                    Some("restore_template") => self.write(&request, "restore_template").await?,
                    _ => return Err(Error::Invalid),
                },
            }
            self.render()
        })
    }
}

impl Pages {
    async fn record(&self, request: &PageRequest<'_, Scope>) -> Result<Record> {
        let key = request.path.get("key").ok_or(Error::NotFound)?;
        let scope = request.scope.ok_or(Error::Unauthorized)?;
        Record::load(self, scope, self.templates.get(key)?, request.query).await
    }
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "index").then_some("Templates")
    }

    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let (body, title) = match request.operation {
                "index" => (Index::new(self, &request)?.render()?, None),
                "detail" => {
                    let record = self.record(&request).await?;
                    (record.render()?, Some(record.template.label.into()))
                }
                _ => return Err(Error::NotFound),
            };
            Ok(PageBody {
                html: body.html,
                live: true,
                title,
            })
        })
    }

    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            let state: Box<dyn LiveState<Scope, Error>> = match request.operation {
                "index" => Box::new(Index::new(self, &request)?),
                "detail" => Box::new(self.record(&request).await?),
                _ => return Ok(None),
            };
            Ok(Some(state))
        })
    }
}
