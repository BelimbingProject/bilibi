//! The New document and New revision dialogs. A file goes in two steps:
//! Upload (`upload`, the listed page-write gap) keeps it with `store_file`,
//! then Save calls the create or add-revision contract with its
//! `artifact_id`. The owning page holds the `Uploads` the host fills and
//! passes it in.
use super::{refusal, render, size, store_refusal, Pages, Refusal};
use crate::{
    Reference, Revision, Source, StoredFile, Upload, FILE_TYPES, LIST_CATEGORY, LIST_STATUS,
    LIST_SUB_CATEGORY, LIST_TYPE, RECORD,
};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_records::{Hit, Resolved};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    choices, error_sentence,
    page::Uploads,
    params,
    upload::{UploadSpec, UploadedFile},
    ActionLink, Alert, Button, ButtonVariant, Choice, ErrorSummary, Field, Form, Html, InputKind,
    Modal, ModalWidth, RecordPicker, StatusKind,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The file input's name: the key a page consumes by.
pub const UPLOAD: &str = "file";
const FORM: &str = "document";
/// Long text fields post up to this many characters.
const LIMIT: usize = 10_000;

mod quick;
pub(crate) use quick::QuickDocument;

#[derive(Template)]
#[template(path = "documents_form.html")]
struct FormView<'a> {
    id: &'a str,
    change_event: &'a str,
    submit: &'a str,
    actions: Html,
    body: Html,
}

/// The uploads a page that offers the dialogs declares at mount.
pub fn uploads() -> Uploads {
    Uploads::new([
        UploadSpec::new(UPLOAD, bilibi_ui::upload::MAX_FILE_SIZE).accept(
            FILE_TYPES
                .iter()
                .map(|(extension, _)| format!(".{extension}")),
        ),
    ])
}

/// The posted field each contract input path refuses.
fn field_of(path: &str) -> &str {
    match path.split('.').next().unwrap_or(path) {
        "artifact_id" | "file" => "file",
        "company" | "record" => "notice",
        other => other,
    }
}
fn label_of(field: &str) -> &'static str {
    match field {
        "name" => "Name",
        "publish_date" => "Publish date",
        "source" => "Source",
        "external_url" => "Link",
        "category" => "Category",
        "sub_category" => "Sub-category",
        "document_type" => "Type",
        "status" => "Status",
        "sync_id" => "Integration sync ID",
        "related_document_id" => "Related document",
        "relation" => "Relation",
        "revision_label" | "label" => "Revision label",
        "replaces_revision_id" => "Replaces revision",
        "related" => "Related to",
        "file" => "File",
        "revision" => "Revision",
        "change_log" => "Change log",
        _ => "",
    }
}

/// What both dialogs share: the values, the stored file, the errors.
#[derive(Default)]
struct State {
    values: BTreeMap<String, String>,
    stored: Option<StoredFile>,
    /// Documents the reader may see that already hold the stored file.
    duplicates: Vec<(i64, String)>,
    errors: BTreeMap<String, Vec<String>>,
    notice: String,
    submitted: bool,
}
impl State {
    fn value(&self, key: &str) -> &str {
        self.values.get(key).map_or("", String::as_str)
    }
    fn read(&mut self, value: &Value, keys: &[&str]) -> Result<()> {
        let form = params::form_values_checked(value, FORM, LIMIT).map_err(Error::refused)?;
        for key in keys {
            if let Some(v) = form.get(*key) {
                self.values.insert((*key).into(), v.clone());
            }
        }
        if self.submitted {
            self.errors.clear();
        }
        Ok(())
    }
    /// Consumes the chosen file and keeps it with `store_file`.
    async fn receive(&mut self, pages: &Pages, scope: &Scope, uploads: &mut Uploads) -> Result<()> {
        self.errors.remove("file");
        self.notice.clear();
        let Some(file) = uploads.consume(UPLOAD).into_iter().next() else {
            self.errors
                .insert("file".into(), vec!["needs a file chosen first".into()]);
            return Ok(());
        };
        self.keep(pages, scope, file).await
    }
    /// Keeps one received file with `store_file` and names the document from
    /// it when the name is still blank.
    async fn keep(&mut self, pages: &Pages, scope: &Scope, file: UploadedFile) -> Result<()> {
        match pages
            .documents
            .store_file(
                scope,
                Upload {
                    bytes: &file.bytes,
                    file_name: &file.name,
                },
            )
            .await
        {
            Ok(stored) => {
                if self.value("name").trim().is_empty() {
                    self.values.insert("name".into(), stored.file_name.clone());
                }
                self.duplicates = pages
                    .documents
                    .duplicates_of_file(scope, &stored.sha256)
                    .await?
                    .into_iter()
                    .map(|d| (d.id, d.name))
                    .collect();
                self.stored = Some(stored);
            }
            Err(error) => {
                self.duplicates.clear();
                self.notice = store_refusal(error)?;
            }
        }
        Ok(())
    }
    /// Keeps a refused contract call's reasons on their fields.
    fn refused(&mut self, error: bilibi_agent_api::Error, failed: &str) -> Result<()> {
        match refusal(error, failed)? {
            Refusal::Fields(fields) => {
                for (path, messages) in fields {
                    let field = field_of(&path);
                    if field == "notice" {
                        self.notice = messages.join(" ");
                    } else {
                        self.errors
                            .entry(field.to_owned())
                            .or_default()
                            .extend(messages);
                    }
                }
            }
            Refusal::Message(message) => self.notice = message,
        }
        Ok(())
    }
    fn sentences(&self, key: &str) -> Vec<String> {
        self.errors
            .get(key)
            .map(|messages| {
                messages
                    .iter()
                    .map(|m| error_sentence(label_of(key), m))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn field(&self, prefix: &str, key: &str, label: &str) -> Field {
        Field::in_form(FORM, key)
            .id(format!("{prefix}-{}", key.replace('_', "-")))
            .label(label)
            .value(self.value(key))
            .errors(self.sentences(key))
            .attr("data-debounce", "300")
    }
    /// The summary of every error and the notice, above the fields.
    fn feedback(&self, prefix: &str) -> Result<Html> {
        let mut body = Html::EMPTY;
        if self.submitted && !self.errors.is_empty() {
            let mut summary = ErrorSummary::new(format!("{prefix}-errors"));
            for (key, messages) in &self.errors {
                for message in messages {
                    summary.push(
                        format!("{prefix}-{}", key.replace('_', "-")),
                        error_sentence(label_of(key), message),
                    );
                }
            }
            body.push(&render(summary)?);
        }
        if !self.duplicates.is_empty() {
            let mut warning = Html::text("A document with this file already exists: ");
            for (index, (id, name)) in self.duplicates.iter().enumerate() {
                if index > 0 {
                    warning.push(&Html::text(", "));
                }
                warning.push(&render(ActionLink {
                    id: &format!("{prefix}-duplicate-{id}"),
                    href: &super::path(*id),
                    body: Html::text(name),
                    ..ActionLink::default()
                })?);
            }
            warning.push(&Html::text(". Save only if you want another."));
            body.push(&render(Alert {
                kind: StatusKind::Warning,
                id: &format!("{prefix}-duplicates"),
                body: warning,
                ..Alert::default()
            })?);
        }
        if !self.notice.is_empty() {
            body.push(&render(Alert {
                kind: StatusKind::Error,
                id: &format!("{prefix}-notice"),
                body: Html::text(&self.notice),
                ..Alert::default()
            })?);
        }
        Ok(body)
    }
    /// The Upload step: the file input, or the file kept, with its own form.
    fn upload(&self, prefix: &str, uploads: &Uploads) -> Result<Html> {
        let mut body = Html::EMPTY;
        if let Some(stored) = &self.stored {
            body.push(&Html::text(&format!(
                "Uploaded: {} ({}). Choose another file to replace it.",
                stored.file_name,
                size(stored.byte_size)
            )));
        }
        body.push(&render(
            uploads
                .input(UPLOAD, format!("{prefix}-file"))
                .label("File")
                .hint("PDF, Office, OpenDocument, image, text or CSV. Upload it, then save."),
        )?);
        if let Some(errors) = self.errors.get("file") {
            body.push(&Html::text(
                &errors
                    .iter()
                    .map(|m| error_sentence("File", m))
                    .collect::<Vec<_>>()
                    .join(" "),
            ));
        }
        render(Form {
            id: &format!("{prefix}-upload"),
            event: "upload",
            change: false,
            class: "mb-4",
            body,
            actions: render(Button {
                id: &format!("{prefix}-upload-button"),
                button_type: "submit",
                working: "Uploading…",
                body: Html::text("Upload"),
                ..Button::default()
            })?,
        })
    }
}

fn select(field: Field, options: Vec<Choice>, prompt: &str) -> Field {
    field
        .kind(InputKind::Select)
        .prompt(prompt)
        .options(options)
}

/// The pick-list choices a document's form offers.
#[derive(Default)]
struct Options {
    categories: Vec<Choice>,
    sub_categories: Vec<Choice>,
    types: Vec<Choice>,
    statuses: Vec<Choice>,
}

/// A new document, written in the dialog.
pub(crate) struct NewDocument {
    state: State,
    /// Set by a record panel: the record the document relates to.
    fixed: Option<Resolved>,
    related: Option<Resolved>,
    query: String,
    hits: Vec<Hit>,
    options: Options,
}
impl NewDocument {
    pub async fn new(pages: &Pages, scope: &Scope, fixed: Option<Resolved>) -> Result<Self> {
        let mut state = State::default();
        state.values.insert("source".into(), "internal".into());
        let client = pages.db.get().await?;
        if let Some(status) = pages
            .picklists
            .default(&**client, scope, LIST_STATUS)
            .await?
        {
            state.values.insert("status".into(), status);
        }
        drop(client);
        let mut dialog = Self {
            state,
            fixed,
            related: None,
            query: String::new(),
            hits: Vec::new(),
            options: Options::default(),
        };
        dialog.options.categories = pages.choices(scope, LIST_CATEGORY, None).await?;
        dialog.options.types = pages.choices(scope, LIST_TYPE, None).await?;
        dialog.options.statuses = pages.choices(scope, LIST_STATUS, None).await?;
        Ok(dialog)
    }
    fn source(&self) -> Source {
        Source::parse(self.state.value("source")).unwrap_or_default()
    }

    /// The dialog's own events; `false` for an event it does not own.
    pub async fn event(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        name: &str,
        value: &Value,
        uploads: &mut Uploads,
    ) -> Result<bool> {
        match name {
            "document_change" => self.read(pages, scope, value).await?,
            "upload" => self.state.receive(pages, scope, uploads).await?,
            "pick_related" => {
                let (kind, id) = value["record"]
                    .as_str()
                    .and_then(|v| v.split_once(':'))
                    .ok_or(Error::Invalid)?;
                self.related = pages
                    .records
                    .resolve(scope, &[Reference::new(kind, id)])
                    .await?
                    .into_iter()
                    .next();
                self.state.errors.remove("related");
                self.hits.clear();
                self.query.clear();
            }
            "clear_related" => {
                self.related = None;
                self.hits.clear();
                self.query.clear();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    async fn read(&mut self, pages: &Pages, scope: &Scope, value: &Value) -> Result<()> {
        let category = self.state.value("category").to_owned();
        self.state.read(
            value,
            &[
                "name",
                "publish_date",
                "source",
                "external_url",
                "category",
                "sub_category",
                "document_type",
                "status",
                "sync_id",
                "related_document_id",
                "relation",
                "revision_label",
            ],
        )?;
        // A new category clears the sub-category and offers its own.
        if self.state.value("category") != category {
            self.state.values.remove("sub_category");
            let category = self.state.value("category").to_owned();
            self.options.sub_categories = if category.is_empty() {
                Vec::new()
            } else {
                pages
                    .choices(scope, LIST_SUB_CATEGORY, Some(&category))
                    .await?
            };
        }
        if self.fixed.is_none() {
            let form = params::form_values_checked(value, FORM, LIMIT).map_err(Error::refused)?;
            let query: String = form
                .get("related_search")
                .cloned()
                .unwrap_or_default()
                .chars()
                .take(100)
                .collect();
            if query != self.query {
                self.query = query;
                self.hits = if self.query.trim().is_empty() {
                    Vec::new()
                } else {
                    let kinds: Vec<&str> = pages
                        .records
                        .kinds()
                        .map(|kind| kind.key)
                        .filter(|key| *key != RECORD)
                        .take(bilibi_records::MAX_SEARCH_KINDS)
                        .collect();
                    pages.records.search(scope, &kinds, &self.query).await?
                };
            }
        }
        Ok(())
    }

    /// The create contract's input from what the dialog holds.
    fn input(&self) -> Value {
        let mut input = json!({"source": self.state.value("source")});
        let mut put = |key: &str| {
            let value = self.state.value(key).trim();
            if !value.is_empty() {
                input[key] = json!(value);
            }
        };
        for key in [
            "name",
            "publish_date",
            "category",
            "sub_category",
            "document_type",
            "status",
            "sync_id",
            "revision_label",
        ] {
            put(key);
        }
        if !self.state.value("related_document_id").trim().is_empty() {
            if let Ok(id) = self
                .state
                .value("related_document_id")
                .trim()
                .parse::<i64>()
            {
                input["related_document_id"] = json!(id);
            }
            input["relation"] = json!(self.state.value("relation"));
        }
        match self.source() {
            Source::Internal => {
                if let Some(stored) = &self.state.stored {
                    input["artifact_id"] = json!(stored.artifact_id);
                }
            }
            Source::External => input["external_url"] = json!(self.state.value("external_url")),
        }
        if let Some(related) = self.fixed.as_ref().or(self.related.as_ref()) {
            input["related"] = json!([{"kind": related.reference.kind, "id": related.id}]);
        }
        input
    }

    /// Saves through `core.document.create`. `Some` is the new document's id.
    pub async fn submit(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        credential: &str,
        value: &Value,
    ) -> Result<Option<i64>> {
        self.read(pages, scope, value).await?;
        self.state.submitted = true;
        self.state.errors.clear();
        self.state.notice.clear();
        if self.source() == Source::Internal && self.state.stored.is_none() {
            self.state
                .errors
                .insert("file".into(), vec!["needs to be uploaded first".into()]);
            return Ok(None);
        }
        let related = self.state.value("related_document_id").trim();
        if !related.is_empty() && related.parse::<i64>().ok().is_none_or(|id| id <= 0) {
            self.state.errors.insert(
                "related_document_id".into(),
                vec!["needs a positive document ID".into()],
            );
            return Ok(None);
        }
        let call = pages.call(scope, credential, "create", "core.document.create");
        match pages.contracts.submit(&call, self.input()).await {
            Ok(result) => Ok(result["result"]["id"].as_i64()),
            Err(error) => {
                self.state
                    .refused(error, "The document was not saved. Try again.")?;
                Ok(None)
            }
        }
    }

    pub fn render(
        &self,
        pages: &Pages,
        uploads: &Uploads,
        prefix: &str,
        cancel_event: &str,
        framed: bool,
    ) -> Result<Html> {
        let state = &self.state;
        let mut body = state.feedback(prefix)?;
        let mut fields = vec![select(
            state.field(prefix, "source", "Source"),
            choices(&[("A file", "internal"), ("A link", "external")]),
            "",
        )];
        if self.source() == Source::External {
            fields.push(
                state
                    .field(prefix, "external_url", "Link")
                    .kind(InputKind::Url)
                    .required()
                    .attr("maxlength", "2000")
                    .hint("An https:// address."),
            );
        }
        fields.extend([
            state
                .field(prefix, "name", "Name")
                .attr("maxlength", "255")
                .hint("The file's name when left blank."),
            state
                .field(prefix, "publish_date", "Publish date")
                .kind(InputKind::Date)
                .hint("Today when left blank."),
            select(
                state.field(prefix, "category", "Category"),
                self.options.categories.clone(),
                "None",
            ),
        ]);
        if !self.options.sub_categories.is_empty() {
            fields.push(select(
                state.field(prefix, "sub_category", "Sub-category"),
                self.options.sub_categories.clone(),
                "None",
            ));
        }
        fields.extend([
            select(
                state.field(prefix, "document_type", "Type"),
                self.options.types.clone(),
                "None",
            ),
            select(
                state.field(prefix, "status", "Status"),
                self.options.statuses.clone(),
                "",
            ),
            state
                .field(prefix, "sync_id", "Integration sync ID")
                .attr("maxlength", "255")
                .hint("Unique within this workspace when set."),
            state
                .field(prefix, "related_document_id", "Related document ID")
                .kind(InputKind::Number)
                .hint("Optional document you can view."),
            select(
                state.field(prefix, "relation", "Relation"),
                choices(&[
                    ("None", ""),
                    ("Supersedes", "supersedes"),
                    ("Replaces", "replaces"),
                ]),
                "",
            ),
            state
                .field(prefix, "revision_label", "First revision label")
                .attr("maxlength", "100"),
        ]);
        let mut details = Html::EMPTY;
        for field in fields {
            details.push(&render(field)?);
        }
        match &self.fixed {
            Some(fixed) => {
                details.push(&Html::text("Related to "));
                details.push(&render(
                    fixed.pill(format!("{prefix}-related-pill"), framed),
                )?);
            }
            None => {
                let selected = self.related.as_ref().map(|r| {
                    (
                        format!("{}:{}", r.reference.kind, r.id),
                        r.pill(format!("{prefix}-related-pill"), framed),
                    )
                });
                details.push(&render(RecordPicker::new(
                    &format!("{prefix}-related"),
                    FORM,
                    "related",
                    "Related to",
                    &self.query,
                    &pages.records.picker_hits(&self.hits),
                    selected
                        .as_ref()
                        .map(|(value, pill)| (value.as_str(), pill.clone())),
                    ("pick_related", "clear_related"),
                    state.sentences("related"),
                ))?);
            }
        }
        if self.source() == Source::Internal {
            body.push(&state.upload(prefix, uploads)?);
        }
        body.push(&details_form(
            prefix,
            "create",
            "Save document",
            cancel_event,
            details,
        )?);
        render(Modal {
            id: &format!("{prefix}-modal"),
            title: "New document",
            cancel_event,
            width: ModalWidth::Wide,
            body,
            ..Modal::default()
        })
    }
}

/// The details form with Cancel and Save.
fn details_form(
    prefix: &str,
    submit: &str,
    save: &str,
    cancel_event: &str,
    body: Html,
) -> Result<Html> {
    let mut actions = render(Button {
        id: &format!("{prefix}-cancel"),
        button_type: "button",
        event: cancel_event,
        body: Html::text("Cancel"),
        ..Button::default()
    })?;
    actions.push(&render(Button {
        id: &format!("{prefix}-save"),
        button_type: "submit",
        variant: ButtonVariant::Primary,
        working: "Saving…",
        body: Html::text(save),
        ..Button::default()
    })?);
    render(FormView {
        id: &format!("{prefix}-form"),
        change_event: "document_change",
        submit,
        actions,
        body,
    })
}

/// A new revision of one document, written in the dialog.
pub(crate) struct NewRevision {
    state: State,
    source: Source,
    latest: i32,
    prior: Vec<Choice>,
}
impl NewRevision {
    pub fn new(source: Source, latest: i32, revisions: &[Revision]) -> Self {
        let mut state = State::default();
        state
            .values
            .insert("revision".into(), (latest.saturating_add(1)).to_string());
        Self {
            state,
            source,
            latest,
            prior: revisions
                .iter()
                .map(|r| {
                    Choice::new(
                        format!(
                            "{}{}",
                            r.revision,
                            r.label
                                .as_ref()
                                .map(|label| format!(" - {label}"))
                                .unwrap_or_default()
                        ),
                        r.id.to_string(),
                    )
                })
                .collect(),
        }
    }
    /// The dialog's own events; `false` for an event it does not own.
    pub async fn event(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        name: &str,
        value: &Value,
        uploads: &mut Uploads,
    ) -> Result<bool> {
        match name {
            "document_change" => self.state.read(
                value,
                &[
                    "revision",
                    "change_log",
                    "external_url",
                    "label",
                    "replaces_revision_id",
                ],
            )?,
            "upload" => self.state.receive(pages, scope, uploads).await?,
            _ => return Ok(false),
        }
        Ok(true)
    }
    /// Saves through `core.document.add_revision`; `true` once saved.
    pub async fn submit(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        credential: &str,
        id: i64,
        version: i64,
        value: &Value,
    ) -> Result<bool> {
        self.state.read(
            value,
            &[
                "revision",
                "change_log",
                "external_url",
                "label",
                "replaces_revision_id",
            ],
        )?;
        self.state.submitted = true;
        self.state.errors.clear();
        self.state.notice.clear();
        let mut input =
            json!({"id": id, "version": version, "change_log": self.state.value("change_log")});
        match self.state.value("revision").trim().parse::<i64>() {
            Ok(number) if number > 0 => input["revision"] = json!(number),
            _ => {
                self.state
                    .errors
                    .insert("revision".into(), vec!["must be a whole number".into()]);
                return Ok(false);
            }
        }
        if !self.state.value("label").trim().is_empty() {
            input["label"] = json!(self.state.value("label"));
        }
        if !self.state.value("replaces_revision_id").trim().is_empty() {
            match self
                .state
                .value("replaces_revision_id")
                .trim()
                .parse::<i64>()
            {
                Ok(id) if id > 0 => input["replaces_revision_id"] = json!(id),
                _ => {
                    self.state.errors.insert(
                        "replaces_revision_id".into(),
                        vec!["needs an earlier revision".into()],
                    );
                    return Ok(false);
                }
            }
        }
        match self.source {
            Source::Internal => match &self.state.stored {
                Some(stored) => input["artifact_id"] = json!(stored.artifact_id),
                None => {
                    self.state
                        .errors
                        .insert("file".into(), vec!["needs to be uploaded first".into()]);
                    return Ok(false);
                }
            },
            Source::External => input["external_url"] = json!(self.state.value("external_url")),
        }
        let call = pages.call(
            scope,
            credential,
            "add_revision",
            "core.document.add_revision",
        );
        match pages.contracts.submit(&call, input).await {
            Ok(_) => Ok(true),
            Err(error) => {
                self.state
                    .refused(error, "The revision was not saved. Try again.")?;
                Ok(false)
            }
        }
    }
    pub fn render(&self, uploads: &Uploads, prefix: &str, cancel_event: &str) -> Result<Html> {
        let state = &self.state;
        let mut body = state.feedback(prefix)?;
        if self.source == Source::Internal {
            body.push(&state.upload(prefix, uploads)?);
        }
        let mut details = Html::EMPTY;
        if self.source == Source::External {
            details.push(&render(
                state
                    .field(prefix, "external_url", "Link")
                    .kind(InputKind::Url)
                    .required()
                    .attr("maxlength", "2000")
                    .hint("The new https:// address."),
            )?);
        }
        details.push(&render(
            state
                .field(prefix, "revision", "Revision")
                .kind(InputKind::Number)
                .attr("min", &(self.latest.saturating_add(1)).to_string())
                .hint(format!(
                    "Above {}, the latest. A number once saved never changes.",
                    self.latest
                )),
        )?);
        details.push(&render(
            state
                .field(prefix, "label", "Revision label")
                .attr("maxlength", "100"),
        )?);
        let mut prior = vec![Choice::new("None", "")];
        prior.extend(self.prior.clone());
        details.push(&render(select(
            state.field(prefix, "replaces_revision_id", "Replaces revision"),
            prior,
            "",
        ))?);
        details.push(&render(
            state
                .field(prefix, "change_log", "Change log")
                .kind(InputKind::Textarea)
                .required()
                .attr("rows", "3")
                .attr("maxlength", "2000"),
        )?);
        body.push(&details_form(
            prefix,
            "add_revision",
            "Save revision",
            cancel_event,
            details,
        )?);
        render(Modal {
            id: &format!("{prefix}-modal"),
            title: "New revision",
            cancel_event,
            width: ModalWidth::Wide,
            body,
            ..Modal::default()
        })
    }
}
