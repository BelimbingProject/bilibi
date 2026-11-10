//! `/documents/{id}`: one document, read first and edited in place, with its
//! revisions (newest first, New revision) and the records it relates to
//! (link with the picker, Unlink). Unlink and Delete ask first; every write
//! calls its contract through `PageContracts`.
use super::{
    dialog::{self, NewRevision},
    event, file_path, path, render, size, store_refusal, Clock, Pages, Refusal, Region, INDEX,
};
use crate::{
    AccessGrant, Document, Duplicate, Link, Marks, Reference, Revision, Source, LIST_CATEGORY,
    LIST_STATUS, LIST_SUB_CATEGORY, LIST_TYPE, RECORD,
};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_records::Hit;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::*, params, ActionLink, BackLink, BuiltinColumn, Button, ButtonVariant, Card, Choice,
    CommitStates, CommitStatus, ConfirmDialog, Fact, Facts, Field, FlashGroup, FlexRow, FlexTable,
    Form, Header, Html, IconButton, InlineChoice, InlineEdit, InlineField, InputKind, ListColumns,
    PageVariant, RecordPicker, SectionHeading, StatusKind, TableEmpty, NO_FLASH,
};
use serde_json::{json, Value};

#[derive(Template)]
#[template(path = "documents_detail.html")]
struct View<'a> {
    id: &'a str,
    body: Html,
}

#[derive(Clone, PartialEq, Eq)]
enum Confirm {
    Delete,
    Unlink(Reference),
    Unshare(i64),
    /// Merge this duplicate into the document on the page.
    Merge(i64),
}

/// The fields a page edit may change, and their labels.
const FIELDS: &[(&str, &str)] = &[
    ("name", "Name"),
    ("publish_date", "Publish date"),
    ("category", "Category"),
    ("sub_category", "Sub-category"),
    ("document_type", "Type"),
    ("status", "Status"),
    ("expires_on", "Expires"),
    ("sync_id", "Integration sync ID"),
    ("related_document_id", "Related document"),
    ("relation", "Relation"),
];
fn label(field: &str) -> &'static str {
    FIELDS
        .iter()
        .find(|(key, _)| *key == field)
        .map_or("Field", |(_, label)| label)
}

pub(crate) struct Detail {
    pages: Pages,
    id: i64,
    framed: bool,
    statuses: CommitStates,
    confirm: Option<Confirm>,
    dialog: Option<NewRevision>,
    uploads: Uploads,
    revisions: ListColumns,
    query: String,
    hits: Vec<Hit>,
    flash: Option<(StatusKind, String)>,
}

impl Detail {
    pub async fn new(pages: Pages, scope: &Scope, id: i64, framed: bool) -> Result<Self> {
        // The page opens only for a document the reader may see.
        pages.documents.get(scope, id).await?;
        Ok(Self {
            pages,
            id,
            framed,
            statuses: CommitStates::default(),
            confirm: None,
            dialog: None,
            uploads: dialog::uploads(),
            revisions: ListColumns::mount(
                "document-revisions",
                vec![
                    BuiltinColumn::new("revision", "Revision"),
                    BuiltinColumn::new("label", "Label"),
                    BuiltinColumn::new("replaces", "Replaces"),
                    BuiltinColumn::new("file", "File"),
                    BuiltinColumn::new("size", "Size"),
                    BuiltinColumn::new("change_log", "Change log"),
                    BuiltinColumn::new("by", "By"),
                    BuiltinColumn::new("at", "Added"),
                ],
            )
            .map_err(|_| Error::Internal)?,
            query: String::new(),
            hits: Vec::new(),
            flash: None,
        })
    }

    /// One field's save: the patch for that field alone.
    async fn save_field(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let field = value["field"].as_str().ok_or(Error::Invalid)?;
        if !FIELDS.iter().any(|(key, _)| *key == field)
            || value["id"].as_str() != Some(&self.id.to_string())
        {
            return Err(Error::Invalid);
        }
        let submitted = value["value"].as_str().ok_or(Error::Invalid)?;
        let version = value["version"]
            .as_str()
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or(Error::Invalid)?;
        let mut patch = json!({});
        patch[field] = if field == "related_document_id" && !submitted.trim().is_empty() {
            match submitted.trim().parse::<i64>() {
                Ok(id) if id > 0 => json!(id),
                _ => {
                    self.statuses.put(
                        field,
                        CommitStatus::Error(
                            "Related document needs a positive document ID.".into(),
                        ),
                    );
                    return Ok(());
                }
            }
        } else if field == "related_document_id" && submitted.trim().is_empty() {
            Value::Null
        } else {
            json!(submitted)
        };
        if field == "related_document_id" {
            let current = self.pages.documents.get(scope, self.id).await?;
            patch["relation"] = if submitted.trim().is_empty() {
                json!("")
            } else {
                json!(current.relation.as_deref().unwrap_or("supersedes"))
            };
        }
        if field == "relation" && submitted.trim().is_empty() {
            patch["related_document_id"] = Value::Null;
        }
        let status = match self
            .pages
            .submit(
                scope,
                credential,
                "save_field",
                "core.document.update",
                json!({"id": self.id, "version": version, "patch": patch}),
                "The change was not saved. Try again.",
            )
            .await?
        {
            Ok(_) => CommitStatus::Saved,
            Err(Refusal::Fields(fields)) => {
                let reasons: Vec<&str> = fields.values().flatten().map(String::as_str).collect();
                CommitStatus::refusal(label(field), submitted, &reasons)
            }
            Err(Refusal::Message(message)) => CommitStatus::Error(message),
        };
        self.statuses.put(field, status);
        Ok(())
    }

    /// One custom field's save: a patch of that key alone.
    async fn save_custom_field(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &Value,
    ) -> Result<()> {
        let field = value["field"].as_str().ok_or(Error::Invalid)?;
        let key = field.strip_prefix("cf_").ok_or(Error::Invalid)?;
        if value["id"].as_str() != Some(&self.id.to_string()) {
            return Err(Error::Invalid);
        }
        let submitted = value["value"].as_str().ok_or(Error::Invalid)?;
        let version = value["version"]
            .as_str()
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or(Error::Invalid)?;
        let input = json!({
            "id": self.id,
            "version": version,
            "patch": {"custom": [{"key": key, "value": submitted}]},
        });
        let status = match self
            .pages
            .submit(
                scope,
                credential,
                "save_custom_field",
                "core.document.update",
                input,
                "The change was not saved. Try again.",
            )
            .await?
        {
            Ok(_) => CommitStatus::Saved,
            Err(Refusal::Fields(fields)) => {
                let reasons: Vec<&str> = fields.values().flatten().map(String::as_str).collect();
                CommitStatus::refusal("Field", submitted, &reasons)
            }
            Err(Refusal::Message(message)) => CommitStatus::Error(message),
        };
        self.statuses.put(field, status);
        Ok(())
    }

    async fn search(&mut self, scope: &Scope, value: &Value) -> Result<()> {
        let form = params::form_values_checked(value, "related", params::FIELD_LIMIT)
            .map_err(Error::refused)?;
        let query: String = form
            .get("record_search")
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
                let kinds: Vec<&str> = self
                    .pages
                    .records
                    .kinds()
                    .map(|kind| kind.key)
                    .take(bilibi_records::MAX_SEARCH_KINDS)
                    .collect();
                self.pages
                    .records
                    .search(scope, &kinds, &self.query)
                    .await?
            };
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &Value,
    ) -> Result<Option<LiveBody>> {
        let name = event(value)?;
        if let Some(dialog) = &mut self.dialog {
            if dialog
                .event(&self.pages, scope, name, value, &mut self.uploads)
                .await?
            {
                return Ok(None);
            }
        }
        match name {
            "save_field" => self.save_field(scope, credential, value).await?,
            "save_custom_field" => self.save_custom_field(scope, credential, value).await?,
            "related_change" => self.search(scope, value).await?,
            "link" => {
                let (kind, id) = value["record"]
                    .as_str()
                    .and_then(|v| v.split_once(':'))
                    .ok_or(Error::Invalid)?;
                let input = json!({"id": self.id, "record": {"kind": kind, "id": id}});
                match self
                    .pages
                    .submit(
                        scope,
                        credential,
                        "link",
                        "core.document.link",
                        input,
                        "The record was not linked. Try again.",
                    )
                    .await?
                {
                    Ok(_) => {
                        self.query.clear();
                        self.hits.clear();
                        self.statuses.put("related", CommitStatus::Saved);
                    }
                    Err(refusal) => self.statuses.put(
                        "related",
                        CommitStatus::Error(bilibi_ui::error_sentence(
                            "Related record",
                            &refusal.text(),
                        )),
                    ),
                }
            }
            "clear_pick" => {
                self.query.clear();
                self.hits.clear();
            }
            "request_unlink" => {
                let (kind, id) = value["record"]
                    .as_str()
                    .and_then(|v| v.split_once(':'))
                    .ok_or(Error::Invalid)?;
                self.confirm = Some(Confirm::Unlink(Reference::new(kind, id)));
            }
            "request_delete" => self.confirm = Some(Confirm::Delete),
            "request_unshare" => {
                let grant = value["grant"]
                    .as_str()
                    .and_then(|value| value.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                self.confirm = Some(Confirm::Unshare(grant));
            }
            "mark" => {
                let mark = value["mark"].as_str().ok_or(Error::Invalid)?;
                let current = self.marks(scope).await?;
                let input = match mark {
                    "favourite" => json!({"id": self.id, "favourite": !current.favourite}),
                    "following" => json!({"id": self.id, "following": !current.following}),
                    _ => return Err(Error::Invalid),
                };
                if let Err(refusal) = self
                    .pages
                    .submit(
                        scope,
                        credential,
                        "mark",
                        "core.document.mark",
                        input,
                        "The mark was not saved. Try again.",
                    )
                    .await?
                {
                    self.flash = Some((StatusKind::Error, refusal.text()));
                }
            }
            "copy_file" => {
                let document = self.pages.documents.get(scope, self.id).await?;
                let mut input = json!({"id": self.id});
                if document.source == Source::Internal {
                    match self.pages.documents.copy_file(scope, self.id).await {
                        Ok(stored) => input["artifact_id"] = json!(stored.artifact_id),
                        Err(error) => {
                            self.flash = Some((StatusKind::Error, store_refusal(error)?));
                            return Ok(None);
                        }
                    }
                }
                match self
                    .pages
                    .submit(
                        scope,
                        credential,
                        "copy",
                        "core.document.copy",
                        input,
                        "The document was not copied. Try again.",
                    )
                    .await?
                {
                    Ok(created) => {
                        if let Some(id) = created["result"]["id"].as_i64() {
                            return Ok(Some(LiveBody {
                                redirect: Some(path(id)),
                                flash: Some((
                                    StatusKind::Success,
                                    "Document copied. Only you can see the copy until you share it."
                                        .into(),
                                )),
                                ..LiveBody::default()
                            }));
                        }
                    }
                    Err(refusal) => self.flash = Some((StatusKind::Error, refusal.text())),
                }
            }
            "request_merge" => {
                let duplicate = value["duplicate"]
                    .as_str()
                    .and_then(|value| value.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                self.confirm = Some(Confirm::Merge(duplicate));
            }
            "merge" => {
                let Some(Confirm::Merge(duplicate)) = self.confirm.take() else {
                    return Ok(None);
                };
                let survivor = self.pages.documents.get(scope, self.id).await?;
                let source = self.pages.documents.get(scope, duplicate).await?;
                let input = json!({
                    "id": self.id,
                    "version": survivor.version,
                    "merge": [{"id": duplicate, "version": source.version}],
                });
                self.flash = Some(
                    match self
                        .pages
                        .submit(
                            scope,
                            credential,
                            "merge",
                            "core.document.merge",
                            input,
                            "The documents were not merged. Try again.",
                        )
                        .await?
                    {
                        Ok(merged) => (
                            StatusKind::Success,
                            format!(
                                "Merged “{}” into this document: {} revisions moved, and the duplicate was removed.",
                                source.name,
                                merged["result"]["revisions_moved"].as_i64().unwrap_or(0)
                            ),
                        ),
                        Err(refusal) => (StatusKind::Error, refusal.text()),
                    },
                );
            }
            "cancel_confirm" => self.confirm = None,
            "unlink" => {
                let Some(Confirm::Unlink(record)) = self.confirm.take() else {
                    return Ok(None);
                };
                let input =
                    json!({"id": self.id, "record": {"kind": record.kind, "id": record.id}});
                self.flash = Some(
                    match self
                        .pages
                        .submit(
                            scope,
                            credential,
                            "unlink",
                            "core.document.unlink",
                            input,
                            "The link was not removed. Try again.",
                        )
                        .await?
                    {
                        Ok(_) => (StatusKind::Success, "Link removed.".into()),
                        Err(refusal) => (StatusKind::Error, refusal.text()),
                    },
                );
            }
            "delete" => {
                if self.confirm.take() != Some(Confirm::Delete) {
                    return Ok(None);
                }
                let document = self.pages.documents.get(scope, self.id).await?;
                let input = json!({"id": self.id, "version": document.version});
                match self
                    .pages
                    .submit(
                        scope,
                        credential,
                        "delete",
                        "core.document.delete",
                        input,
                        "The document was not deleted. Try again.",
                    )
                    .await?
                {
                    Ok(_) => {
                        return Ok(Some(LiveBody {
                            redirect: Some(INDEX.into()),
                            flash: Some((StatusKind::Success, "Document deleted.".into())),
                            ..LiveBody::default()
                        }))
                    }
                    Err(refusal) => self.flash = Some((StatusKind::Error, refusal.text())),
                }
            }
            "share" => {
                let form = params::form_values_checked(value, "share", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let checked = |name: &str| form.get(name).is_some_and(|value| value == "true");
                let user_id = form
                    .get("account_id")
                    .and_then(|id| id.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                let mut input = json!({
                    "id": self.id,
                    "user_id": user_id,
                    "download": checked("download"),
                    "manage": checked("manage"),
                    "share": checked("allow_share"),
                    "email": checked("email"),
                });
                if let Some(until) = form
                    .get("until")
                    .map(|u| u.trim())
                    .filter(|u| !u.is_empty())
                {
                    input["until"] = json!(until);
                }
                self.flash = Some(
                    match self
                        .pages
                        .submit(
                            scope,
                            credential,
                            "share",
                            "core.document.share",
                            input,
                            "Access was not changed. Try again.",
                        )
                        .await?
                    {
                        Ok(saved) => match saved["result"]["email"].as_str() {
                            Some("rate_limited") => (
                                StatusKind::Warning,
                                "Access saved. The email was not sent: a limit on emailed links was reached. Try again later.".into(),
                            ),
                            Some(_) => (
                                StatusKind::Success,
                                "Access saved. The email is on its way.".into(),
                            ),
                            None => (StatusKind::Success, "Access saved.".into()),
                        },
                        Err(refusal) => (StatusKind::Error, refusal.text()),
                    },
                );
            }
            "unshare" => {
                let Some(Confirm::Unshare(grant)) = self.confirm.take() else {
                    return Ok(None);
                };
                self.flash = Some(
                    match self
                        .pages
                        .submit(
                            scope,
                            credential,
                            "unshare",
                            "core.document.unshare",
                            json!({"id": self.id, "grant": grant}),
                            "Access was not removed. Try again.",
                        )
                        .await?
                    {
                        Ok(_) => (StatusKind::Success, "Access removed.".into()),
                        Err(refusal) => (StatusKind::Error, refusal.text()),
                    },
                );
            }
            "new_revision" => {
                let document = self.pages.documents.get(scope, self.id).await?;
                if self.pages.documents.may_change(scope, &document).await? {
                    let revisions = self.pages.documents.revisions(scope, self.id).await?;
                    self.dialog = Some(NewRevision::new(
                        document.source,
                        document.revision,
                        &revisions,
                    ));
                }
            }
            "close_modal" => self.dialog = None,
            "add_revision" => {
                let document = self.pages.documents.get(scope, self.id).await?;
                let pages = self.pages.shared();
                let Some(dialog) = &mut self.dialog else {
                    return Ok(None);
                };
                if dialog
                    .submit(&pages, scope, credential, self.id, document.version, value)
                    .await?
                {
                    self.dialog = None;
                    self.flash = Some((StatusKind::Success, "Revision added.".into()));
                }
            }
            "grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.revisions.handle(&values);
            }
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(None)
    }

    /// The reader's own marks on this document.
    async fn marks(&self, scope: &Scope) -> Result<Marks> {
        Ok(self
            .pages
            .documents
            .marks(scope, &[self.id])
            .await?
            .get(&self.id)
            .copied()
            .unwrap_or_default())
    }

    /// Possible duplicates, each with Merge when the reader may merge.
    fn duplicates(&self, duplicates: &[Duplicate], may_merge: bool) -> Result<Html> {
        let mut body = render(SectionHeading {
            id: "document-duplicates-heading",
            title: "Possible duplicates",
            description: "Documents you may see with the same file, link or name.",
            ..SectionHeading::default()
        })?;
        let mut ids = Vec::with_capacity(duplicates.len());
        let mut items = Vec::with_capacity(duplicates.len());
        for duplicate in duplicates {
            let d = &duplicate.document;
            let id = d.id.to_string();
            let reasons = duplicate
                .reasons
                .iter()
                .map(|reason| reason.text())
                .collect::<Vec<_>>()
                .join(" · ");
            let mut value = render(bilibi_ui::lists::RecordLink {
                workspace: self.framed,
                kind: crate::MODULE,
                record_id: &id,
                href: &path(d.id),
                class: "font-medium text-ink-strong hover:underline",
                body: Html::text(&d.name),
            })?;
            if may_merge {
                value.push(&render(Button {
                    id: &format!("document-duplicate-{id}-merge"),
                    button_type: "button",
                    event: "request_merge",
                    name: "duplicate",
                    value: &id,
                    body: Html::text("Merge into this document"),
                    ..Button::default()
                })?);
            }
            ids.push(format!("document-duplicate-{id}"));
            items.push((reasons, value));
        }
        body.push(&render(Facts {
            id: "document-duplicate-list",
            items: items
                .iter()
                .zip(&ids)
                .map(|((title, value), id)| Fact {
                    title,
                    id,
                    value: value.clone(),
                })
                .collect(),
        })?);
        render(Card {
            id: "document-duplicates",
            class: "mt-6",
            role: "region",
            labelledby: "document-duplicates-heading",
            body,
            ..Card::default()
        })
    }

    fn inline(
        &self,
        d: &Document,
        key: &str,
        value: impl Into<String>,
        editable: bool,
    ) -> InlineField {
        let mut field = InlineField::new(
            format!("document-{}", key.replace('_', "-")),
            key,
            format!("Edit {}", label(key).to_lowercase()),
            value,
        )
        .version(d.version)
        .status(self.statuses.get(key));
        field.id_value = d.id.to_string();
        if !editable {
            field = field.readonly();
        }
        field
    }
    /// A select edited in place; `empty` adds a None choice that clears it.
    fn choice(
        &self,
        d: &Document,
        key: &str,
        current: (&str, &str),
        mut options: Vec<Choice>,
        empty: bool,
        editable: bool,
    ) -> Result<Html> {
        if empty {
            options.insert(0, Choice::new("None", ""));
        }
        if !current.0.is_empty() && !options.iter().any(|c| c.value == current.0) {
            // A retired code the document keeps.
            options.push(Choice::new(current.1, current.0));
        }
        let mut field = self.inline(d, key, current.0, editable);
        if empty {
            field = field.allow_empty();
        }
        let editor = render(
            Field::named(key)
                .id(format!("document-{}-editor", key.replace('_', "-")))
                .label_class("sr-only")
                .label(label(key))
                .kind(InputKind::Select)
                .value(current.0)
                .options(options),
        )?;
        render(InlineChoice {
            field,
            display: Html::text(if current.1.is_empty() {
                "—"
            } else {
                current.1
            }),
            editor,
        })
    }
    fn date(
        &self,
        clock: &Clock,
        d: &Document,
        key: &str,
        value: Option<jiff::civil::Date>,
        editable: bool,
    ) -> Result<Html> {
        let mut field = self
            .inline(
                d,
                key,
                value.map(|v| v.to_string()).unwrap_or_default(),
                editable,
            )
            .allow_empty()
            .input_type("date");
        if let Some(value) = value {
            field = field.display_value(clock.date(value));
        }
        render(InlineEdit { field })
    }

    /// The tenant's custom fields as facts that edit in place: title, key
    /// and value.
    async fn custom_rows(
        &self,
        scope: &Scope,
        d: &Document,
        editable: bool,
    ) -> Result<Vec<(String, String, Html)>> {
        let mut rows = Vec::new();
        for definition in self.pages.documents.custom_fields(scope).await? {
            let name = format!("cf_{}", definition.key);
            let value = d
                .custom
                .get(&definition.key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let mut field = InlineField::new(
                format!("document-{}", name.replace('_', "-")),
                name.clone(),
                format!("Edit {}", definition.label),
                value.clone(),
            )
            .version(d.version)
            .allow_empty()
            .status(self.statuses.get(&name));
            field.save_event = "save_custom_field".into();
            field.id_value = d.id.to_string();
            if !editable {
                field = field.readonly();
            }
            if let Some(help) = &definition.help {
                field.hint = help.clone();
            }
            let html = match (definition.kind, definition.picklist.as_deref()) {
                (bilibi_custom_fields::Kind::Choice, Some(list)) => {
                    let shown = self.pages.label(scope, list, Some(&value)).await?;
                    let editor = bilibi_ui::Combobox::new(format!("{name}-input"), &name)
                        .value(&value)
                        .options(self.pages.choices(scope, list, None).await?)
                        .wrapper_class("mb-0");
                    render(InlineChoice {
                        field,
                        display: Html::text(if shown.is_empty() { "—" } else { &shown }),
                        editor: render(editor)?,
                    })?
                }
                (bilibi_custom_fields::Kind::Date, _) => {
                    field = field.input_type("date");
                    render(InlineEdit { field })?
                }
                _ => render(InlineEdit { field })?,
            };
            rows.push((
                definition.label.clone(),
                format!("custom-{}", definition.key),
                html,
            ));
        }
        Ok(rows)
    }

    async fn facts(
        &self,
        scope: &Scope,
        clock: &Clock,
        d: &Document,
        editable: bool,
    ) -> Result<Html> {
        let pages = &self.pages;
        let names = pages.names(scope, &[d.created_by]).await?;
        let name = |id: i64| names.get(&id).cloned().unwrap_or_default();
        let category = pages
            .label(scope, LIST_CATEGORY, d.category.as_deref())
            .await?;
        let sub_category = pages
            .label(scope, LIST_SUB_CATEGORY, d.sub_category.as_deref())
            .await?;
        let document_type = pages
            .label(scope, LIST_TYPE, d.document_type.as_deref())
            .await?;
        let status = pages.label(scope, LIST_STATUS, Some(&d.status)).await?;
        let mut items: Vec<(&str, &str, Html)> = vec![
            (
                "Name",
                "name",
                render(InlineEdit {
                    field: self.inline(d, "name", d.name.clone(), editable),
                })?,
            ),
            (
                "Publish date",
                "publish-date",
                self.date(clock, d, "publish_date", Some(d.publish_date), editable)?,
            ),
            (
                "Source",
                "source",
                Html::text(match d.source {
                    Source::Internal => "A file",
                    Source::External => "A link",
                }),
            ),
            (
                "Category",
                "category",
                self.choice(
                    d,
                    "category",
                    (d.category.as_deref().unwrap_or_default(), &category),
                    pages.choices(scope, LIST_CATEGORY, None).await?,
                    true,
                    editable,
                )?,
            ),
        ];
        if let Some(parent) = &d.category {
            items.push((
                "Sub-category",
                "sub-category",
                self.choice(
                    d,
                    "sub_category",
                    (d.sub_category.as_deref().unwrap_or_default(), &sub_category),
                    pages
                        .choices(scope, LIST_SUB_CATEGORY, Some(parent))
                        .await?,
                    true,
                    editable,
                )?,
            ));
        }
        items.push((
            "Type",
            "type",
            self.choice(
                d,
                "document_type",
                (
                    d.document_type.as_deref().unwrap_or_default(),
                    &document_type,
                ),
                pages.choices(scope, LIST_TYPE, None).await?,
                true,
                editable,
            )?,
        ));
        items.push((
            "Status",
            "status",
            self.choice(
                d,
                "status",
                (&d.status, &status),
                pages.choices(scope, LIST_STATUS, None).await?,
                false,
                editable,
            )?,
        ));
        let expiry = {
            let mut html = self.date(clock, d, "expires_on", d.expires_on, editable)?;
            if d.expires_on.is_some_and(|on| on < clock.today()) {
                html.push(&Html::text(" · Expired"));
            }
            html
        };
        items.push(("Expires", "expires-on", expiry));
        let custom_rows = self.custom_rows(scope, d, editable).await?;
        for (title, key, html) in &custom_rows {
            items.push((title.as_str(), key.as_str(), html.clone()));
        }
        items.push((
            "Integration sync ID",
            "sync-id",
            render(InlineEdit {
                field: self
                    .inline(
                        d,
                        "sync_id",
                        d.sync_id.clone().unwrap_or_default(),
                        editable,
                    )
                    .allow_empty(),
            })?,
        ));
        let related_id = d
            .related_document_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        let related_visible = if let Some(id) = d.related_document_id {
            match pages.documents.get(scope, id).await {
                Ok(_) => true,
                Err(Error::NotFound | Error::Forbidden) => false,
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        let mut related = render(InlineEdit {
            field: self
                .inline(d, "related_document_id", related_id, editable)
                .allow_empty()
                .input_type("number"),
        })?;
        if related_visible {
            related.push(&render(ActionLink {
                id: "document-related-document-link",
                href: &super::path(d.related_document_id.unwrap_or_default()),
                body: Html::text("Open"),
                ..ActionLink::default()
            })?);
        }
        items.push(("Related document", "related-document", related));
        items.push((
            "Relation",
            "relation",
            self.choice(
                d,
                "relation",
                (
                    d.relation.as_deref().unwrap_or_default(),
                    d.relation.as_deref().unwrap_or_default(),
                ),
                vec![
                    Choice::new("Supersedes", "supersedes"),
                    Choice::new("Replaces", "replaces"),
                ],
                true,
                editable && d.related_document_id.is_some(),
            )?,
        ));
        items.push((
            "Revision",
            "revision",
            Html::text(&format!(
                "{} · {}",
                d.revision,
                clock.text(d.last_revision_at)
            )),
        ));
        if let Some(url) = &d.external_url {
            items.push((
                "Link",
                "link",
                render(ActionLink {
                    id: "document-link",
                    href: url,
                    body: Html::text(url),
                    ..ActionLink::default()
                })?,
            ));
        }
        items.push((
            "Created by",
            "created-by",
            Html::text(&format!(
                "{} · {}",
                name(d.created_by),
                clock.text(d.created_at)
            )),
        ));
        let ids: Vec<String> = items
            .iter()
            .map(|(_, key, _)| format!("document-fact-{key}"))
            .collect();
        render(Facts {
            id: "document-facts",
            items: items
                .into_iter()
                .zip(&ids)
                .map(|((title, _, value), id)| Fact { title, id, value })
                .collect(),
        })
    }

    async fn revisions(
        &mut self,
        scope: &Scope,
        clock: &Clock,
        d: &Document,
        revisions: &[Revision],
        editable: bool,
    ) -> Result<Html> {
        let may_download = self.pages.documents.may_download(scope).await?;
        let users: Vec<i64> = revisions.iter().map(|r| r.created_by).collect();
        let names = self.pages.names(scope, &users).await?;
        let _ = self.revisions.load(revisions, |r| r.revision.to_string());
        let mut rows = Vec::with_capacity(revisions.len());
        let numbers: std::collections::HashMap<i64, i32> =
            revisions.iter().map(|r| (r.id, r.revision)).collect();
        for r in revisions {
            let number = r.revision.to_string();
            let file = match (&r.file_name, &r.external_url) {
                (Some(name), _) if r.scan_status == crate::ScanStatus::Quarantined => {
                    Html::text(&format!("{name} (quarantined)"))
                }
                (Some(name), _) if may_download => render(ActionLink {
                    id: &format!("document-revision-{number}-file"),
                    href: &file_path(d.id, r.revision),
                    body: Html::text(name),
                    ..ActionLink::default()
                })?,
                (Some(name), _) => Html::text(name),
                (None, Some(url)) => render(ActionLink {
                    id: &format!("document-revision-{number}-link"),
                    href: url,
                    body: Html::text(url),
                    ..ActionLink::default()
                })?,
                _ => Html::text("—"),
            };
            let mut row = FlexRow::new(&number)
                .slot("revision", Html::text(&number))
                .slot("label", Html::text(r.label.as_deref().unwrap_or("")))
                .slot(
                    "replaces",
                    Html::text(
                        &r.replaces_revision_id
                            .and_then(|id| numbers.get(&id).copied())
                            .map(|n| n.to_string())
                            .unwrap_or_default(),
                    ),
                )
                .slot("file", file)
                .slot(
                    "size",
                    Html::text(&r.byte_size.map(size).unwrap_or_default()),
                )
                .slot("change_log", Html::text(&r.change_log))
                .slot(
                    "by",
                    Html::text(names.get(&r.created_by).map_or("—", String::as_str)),
                );
            row.cells.insert(
                "at".into(),
                bilibi_ui::flex_table::Cell::instant(clock.instant(r.created_at)),
            );
            row.dom_id = Some(format!("document-revision-{number}"));
            rows.push(row);
        }
        let actions = if editable {
            render(Button {
                id: "document-new-revision",
                button_type: "button",
                event: "new_revision",
                variant: ButtonVariant::Primary,
                body: Html::text("New revision"),
                ..Button::default()
            })?
        } else {
            Html::EMPTY
        };
        let mut body = render(SectionHeading {
            id: "document-revisions-heading",
            title: "Revisions",
            description: "Newest first. Every revision keeps its own file.",
            actions,
            ..SectionHeading::default()
        })?;
        let mut table = FlexTable::new("document-revisions", self.revisions.flex_columns(), rows);
        table.event = "grid";
        table.zoom = self.revisions.zoom;
        table.caption = "Revisions";
        table.framed = false;
        table.empty = Some(TableEmpty::Content(Html::text("No revisions.")));
        body.push(&render(table)?);
        render(Card {
            id: "document-revisions",
            class: "mt-6",
            role: "region",
            labelledby: "document-revisions-heading",
            body,
            ..Card::default()
        })
    }

    async fn related(&self, scope: &Scope, links: &[Link], editable: bool) -> Result<Html> {
        let references: Vec<Reference> = links.iter().map(|l| l.record.clone()).collect();
        let named = self.pages.records.resolve(scope, &references).await?;
        let mut body = render(SectionHeading {
            id: "document-related-heading",
            title: "Related records",
            description: if links.is_empty() {
                "This document is not linked to a record."
            } else {
                ""
            },
            ..SectionHeading::default()
        })?;
        let mut items: Vec<(String, Html)> = Vec::new();
        for (link, resolved) in links.iter().zip(&named) {
            let key = format!("{}:{}", link.record.kind, link.record.id);
            let mut value =
                render(resolved.pill(format!("document-related-{}", link.id), self.framed))?;
            if editable {
                value.push(&render(IconButton {
                    id: &format!("document-unlink-{}", link.id),
                    event: "request_unlink",
                    name: "record",
                    value: &key,
                    ..IconButton::new("x-mark", "Unlink")
                })?);
            }
            items.push((format!("document-related-item-{}", link.id), value));
        }
        let kinds: Vec<&'static str> = links
            .iter()
            .map(|l| self.pages.records.kind_label(&l.record.kind))
            .collect();
        body.push(&render(Facts {
            id: "document-related-facts",
            items: items
                .iter()
                .zip(&kinds)
                .map(|((id, value), title)| Fact {
                    title,
                    id,
                    value: value.clone(),
                })
                .collect(),
        })?);
        if editable {
            let errors = match self.statuses.get("related") {
                CommitStatus::Error(message) => vec![message],
                _ => Vec::new(),
            };
            let hits: Vec<Hit> = self
                .hits
                .iter()
                .filter(|h| !(h.kind == RECORD && h.id == self.id.to_string()))
                .cloned()
                .collect();
            let picker = render(RecordPicker::new(
                "document-related",
                "related",
                "record",
                "Link a record",
                &self.query,
                &self.pages.records.picker_hits(&hits),
                None,
                ("link", "clear_pick"),
                errors,
            ))?;
            body.push(&render(Form {
                id: "document-related-form",
                event: "related_change",
                change: true,
                body: picker,
                ..Form::default()
            })?);
        }
        render(Card {
            id: "document-related",
            class: "mt-6",
            role: "region",
            labelledby: "document-related-heading",
            body,
            ..Card::default()
        })
    }

    async fn sharing(&self, scope: &Scope, clock: &Clock, grants: &[AccessGrant]) -> Result<Html> {
        let users: Vec<i64> = grants.iter().map(|grant| grant.user_id).collect();
        let names = self.pages.names(scope, &users).await?;
        let mut body = render(SectionHeading {
            id: "document-sharing-heading",
            title: "Access",
            description: "Only the people listed here can find this document.",
            ..SectionHeading::default()
        })?;
        let mut ids = Vec::with_capacity(grants.len());
        let mut items = Vec::with_capacity(grants.len());
        for grant in grants {
            let title = names
                .get(&grant.user_id)
                .cloned()
                .unwrap_or_else(|| format!("Account {}", grant.user_id));
            let mut permissions = vec!["View"];
            if grant.can_download {
                permissions.push("Download");
            }
            if grant.can_manage {
                permissions.push("Manage");
            }
            if grant.can_share {
                permissions.push("Share");
            }
            let mut text = permissions.join(" · ");
            if let Some(until) = grant.expires_at {
                text.push_str(&format!(
                    " · {} {}",
                    if until <= jiff::Timestamp::now() {
                        "Ended"
                    } else {
                        "Until"
                    },
                    clock.text(until)
                ));
            }
            let mut value = Html::text(&text);
            if grant.user_id != scope.user() {
                value.push(&render(Button {
                    id: &format!("document-access-{}-remove", grant.id),
                    button_type: "button",
                    event: "request_unshare",
                    name: "grant",
                    value: &grant.id.to_string(),
                    variant: ButtonVariant::Danger,
                    body: Html::text("Remove"),
                    ..Button::default()
                })?);
            }
            ids.push(format!("document-access-{}", grant.id));
            items.push((title, value));
        }
        body.push(&render(Facts {
            id: "document-access-list",
            items: items
                .iter()
                .zip(&ids)
                .map(|((title, value), id)| Fact {
                    title,
                    id,
                    value: value.clone(),
                })
                .collect(),
        })?);

        let mut fields = render(
            Field::in_form("share", "account_id")
                .id("document-share-account")
                .label("Account ID")
                .kind(InputKind::Number)
                .hint("Use an active account ID."),
        )?;
        fields.push(&render(
            Field::in_form("share", "until")
                .id("document-share-until")
                .label("Access until")
                .kind(InputKind::Date)
                .hint("The last day (UTC) it works. Leave empty for no end."),
        )?);
        for (key, label, checked) in [
            ("download", "Allow downloads", true),
            ("manage", "Allow changes", false),
            ("allow_share", "Allow sharing", false),
            ("email", "Email them a link", false),
        ] {
            fields.push(&render(
                Field::in_form("share", key)
                    .id(format!("document-share-{key}"))
                    .label(label)
                    .kind(InputKind::Checkbox)
                    .checked(checked),
            )?);
        }
        body.push(&render(Form {
            id: "document-share-form",
            event: "share",
            body: fields,
            actions: render(Button {
                id: "document-share-save",
                button_type: "submit",
                variant: ButtonVariant::Primary,
                working: "Saving…",
                body: Html::text("Save access"),
                ..Button::default()
            })?,
            ..Form::default()
        })?);
        render(Card {
            id: "document-sharing",
            class: "mt-6",
            role: "region",
            labelledby: "document-sharing-heading",
            body,
            ..Card::default()
        })
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let clock = self.pages.clock(scope).await?;
        let d = self.pages.documents.get(scope, self.id).await?;
        let editable = self.pages.documents.may_change(scope, &d).await?;
        let mut actions = Html::EMPTY;
        let marks = self.marks(scope).await?;
        for (id, event, mark, on, off) in [
            (
                "document-favourite",
                "mark",
                "favourite",
                "Remove favourite",
                "Favourite",
            ),
            ("document-follow", "mark", "following", "Unfollow", "Follow"),
        ] {
            let active = if mark == "favourite" {
                marks.favourite
            } else {
                marks.following
            };
            actions.push(&render(Button {
                id,
                button_type: "button",
                event,
                name: "mark",
                value: mark,
                body: Html::text(if active { on } else { off }),
                ..Button::default()
            })?);
        }
        if self.pages.documents.may_manage(scope).await? {
            actions.push(&render(Button {
                id: "document-copy",
                button_type: "button",
                event: "copy_file",
                working: "Copying…",
                body: Html::text("Copy"),
                ..Button::default()
            })?);
        }
        if editable {
            actions.push(&render(Button {
                id: "document-delete",
                button_type: "button",
                event: "request_delete",
                variant: ButtonVariant::Danger,
                body: Html::text("Delete"),
                ..Button::default()
            })?);
        }
        actions.push(&render(BackLink {
            href: INDEX,
            id: "document-back",
            title: "Back to documents",
            ..BackLink::default()
        })?);
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                _ => flash.error = message,
            }
        }
        let mut body = render(FlashGroup {
            id: "document-flash",
            flash,
        })?;
        body.push(&render(Header {
            title: Html::text(&d.name),
            actions,
            ..Header::default()
        })?);
        let mut details = render(SectionHeading {
            id: "document-details-heading",
            title: "Details",
            ..SectionHeading::default()
        })?;
        details.push(&self.facts(scope, &clock, &d, editable).await?);
        body.push(&render(Card {
            id: "document-details",
            role: "region",
            labelledby: "document-details-heading",
            body: details,
            ..Card::default()
        })?);
        let revisions = self.pages.documents.revisions(scope, self.id).await?;
        body.push(
            &self
                .revisions(scope, &clock, &d, &revisions, editable)
                .await?,
        );
        let links = self.pages.documents.links(scope, self.id).await?;
        body.push(&self.related(scope, &links, editable).await?);
        let duplicates = self.pages.documents.duplicates(scope, self.id).await?;
        if !duplicates.is_empty() {
            let may_merge = self.pages.documents.may_merge(scope, &d).await?;
            body.push(&self.duplicates(&duplicates, may_merge)?);
        }
        if let Ok(grants) = self.pages.documents.access(scope, self.id).await {
            body.push(&self.sharing(scope, &clock, &grants).await?);
        }
        let merge_name = match &self.confirm {
            Some(Confirm::Merge(id)) => self
                .pages
                .documents
                .get(scope, *id)
                .await
                .ok()
                .map(|d| d.name),
            _ => None,
        };
        if let Some(confirm) = &self.confirm {
            let (consequence, detail, verb, working, event) = match confirm {
                Confirm::Merge(_) => (
                    format!(
                        "“{}” will be merged into “{}”.",
                        merge_name.as_deref().unwrap_or("This document"),
                        d.name
                    ),
                    "Its revisions follow this document's latest, with their change logs. Links, access for active accounts and favourites combine, then the duplicate is removed.",
                    "Merge",
                    "Merging…",
                    "merge",
                ),
                Confirm::Delete => (
                    format!("“{}” will be deleted with all {} revisions.", d.name, revisions.len()),
                    "Every revision's file is deleted too, and the document leaves every record it relates to.",
                    "Delete",
                    "Deleting…",
                    "delete",
                ),
                Confirm::Unlink(_) => (
                    format!("“{}” will no longer relate to this record.", d.name),
                    "The document and the record both stay.",
                    "Unlink",
                    "Unlinking…",
                    "unlink",
                ),
                Confirm::Unshare(_) => (
                    "This person will lose access to the document.".into(),
                    "They will no longer find, view or download it through this grant.",
                    "Remove access",
                    "Removing…",
                    "unshare",
                ),
            };
            body.push(&render(ConfirmDialog {
                id: "document-confirm",
                consequence: &consequence,
                detail,
                confirm: verb,
                working,
                confirm_event: event,
                cancel_event: "cancel_confirm",
            })?);
        }
        if let Some(dialog) = &self.dialog {
            body.push(&dialog.render(&self.uploads, "document-revision", "close_modal")?);
        }
        let region = render(Region {
            id: "document-region",
            target: false,
            body,
        })?;
        let id = self.id.to_string();
        let html = render(bilibi_ui::Page {
            id: "document-detail",
            variant: PageVariant::Detail,
            body: render(View {
                id: &id,
                body: region,
            })?,
            ..bilibi_ui::Page::default()
        })?
        .to_string();
        Ok(LiveBody {
            html,
            rows: 1,
            ..LiveBody::default()
        })
    }
}

impl LiveState<Scope, Error> for Detail {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                if let Some(body) = self
                    .handle(request.scope, request.credential, value)
                    .await?
                {
                    return Ok(body);
                }
            }
            self.view(request.scope).await
        })
    }
    fn uploads(&mut self) -> Option<&mut Uploads> {
        Some(&mut self.uploads)
    }
}
