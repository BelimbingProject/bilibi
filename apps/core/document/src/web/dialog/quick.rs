//! The Quick add dialog: a child of the dialogs, which it shares `State`,
//! the pick-list options and the details form with.
use super::{details_form, select, Options, State, UPLOAD};
use crate::web::{render, size, Pages};
use crate::{Source, LIST_CATEGORY, LIST_TYPE};
use bilibi_database::Result;
use bilibi_records::Resolved;
use bilibi_tenancy::Scope;
use bilibi_ui::{choices, error_sentence, page::Uploads, Html, InputKind, Modal, ModalWidth};
use serde_json::{json, Value};

/// The document a quick add filed.
pub(crate) struct Added {
    pub name: String,
    /// Other documents the reader may see that hold the same file.
    pub duplicates: usize,
}

/// A quick add: one form and one Save. The chosen file is received, kept and
/// filed in the same step, with the name taken from the file and the
/// category and type from the pick-lists' defaults, all changeable here. The
/// New document dialog stays the place for a date, status, sub-category or
/// a record to relate.
pub(crate) struct QuickDocument {
    state: State,
    /// The record the document relates to, when the page is about one.
    fixed: Option<Resolved>,
    options: Options,
}
impl QuickDocument {
    pub async fn new(pages: &Pages, scope: &Scope, fixed: Option<Resolved>) -> Result<Self> {
        let mut state = State::default();
        state.values.insert("source".into(), "internal".into());
        let client = pages.db.get().await?;
        for (key, list) in [("category", LIST_CATEGORY), ("document_type", LIST_TYPE)] {
            if let Some(code) = pages.picklists.default(&**client, scope, list).await? {
                state.values.insert(key.into(), code);
            }
        }
        drop(client);
        let options = Options {
            categories: pages.choices(scope, LIST_CATEGORY, None).await?,
            types: pages.choices(scope, LIST_TYPE, None).await?,
            ..Options::default()
        };
        Ok(Self {
            state,
            fixed,
            options,
        })
    }
    fn source(&self) -> Source {
        Source::parse(self.state.value("source")).unwrap_or_default()
    }

    /// The dialog's own events; `false` for an event it does not own.
    pub fn event(&mut self, name: &str, value: &Value) -> Result<bool> {
        match name {
            "document_change" => self.state.read(
                value,
                &[
                    "name",
                    "source",
                    "external_url",
                    "category",
                    "document_type",
                ],
            )?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The create contract's input from what the dialog holds.
    fn input(&self) -> Value {
        let mut input = json!({"source": self.state.value("source")});
        for key in ["name", "category", "document_type"] {
            let value = self.state.value(key).trim();
            if !value.is_empty() {
                input[key] = json!(value);
            }
        }
        match self.source() {
            Source::Internal => {
                if let Some(stored) = &self.state.stored {
                    input["artifact_id"] = json!(stored.artifact_id);
                }
            }
            Source::External => input["external_url"] = json!(self.state.value("external_url")),
        }
        if let Some(related) = &self.fixed {
            input["related"] = json!([{"kind": related.reference.kind, "id": related.id}]);
        }
        input
    }

    /// Receives the file, then files the document through
    /// `core.document.create`. `Some` once it is filed; a refusal stays in
    /// the dialog, with the file kept so Save can be pressed again.
    pub async fn submit(
        &mut self,
        pages: &Pages,
        scope: &Scope,
        credential: &str,
        uploads: &mut Uploads,
        value: &Value,
    ) -> Result<Option<Added>> {
        self.event("document_change", value)?;
        self.state.submitted = true;
        self.state.errors.clear();
        self.state.notice.clear();
        if self.source() == Source::Internal {
            match uploads.consume(UPLOAD).into_iter().next() {
                Some(file) => self.state.keep(pages, scope, file).await?,
                None if self.state.stored.is_some() => {}
                None => {
                    self.state
                        .errors
                        .insert("file".into(), vec!["needs a file chosen first".into()]);
                }
            }
            if !self.state.errors.is_empty() || !self.state.notice.is_empty() {
                return Ok(None);
            }
        }
        let call = pages.call(scope, credential, "quick_create", "core.document.create");
        match pages.contracts.submit(&call, self.input()).await {
            Ok(result) => Ok(Some(Added {
                name: result["result"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                duplicates: self.state.duplicates.len(),
            })),
            Err(error) => {
                self.state
                    .refused(error, "The document was not added. Try again.")?;
                Ok(None)
            }
        }
    }

    pub fn render(
        &self,
        uploads: &Uploads,
        prefix: &str,
        cancel_event: &str,
        framed: bool,
    ) -> Result<Html> {
        let state = &self.state;
        let mut details = render(select(
            state.field(prefix, "source", "Source"),
            choices(&[("A file", "internal"), ("A link", "external")]),
            "",
        ))?;
        match self.source() {
            Source::Internal => {
                details.push(&render(
                    uploads
                        .input(UPLOAD, format!("{prefix}-file"))
                        .label("File")
                        .hint("PDF, Office, OpenDocument, image, text or CSV."),
                )?);
                if let Some(errors) = state.errors.get("file") {
                    details.push(&Html::text(
                        &errors
                            .iter()
                            .map(|m| error_sentence("File", m))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ));
                }
                if let Some(stored) = &state.stored {
                    details.push(&Html::text(&format!(
                        "Uploaded: {} ({}). Choose another file to replace it.",
                        stored.file_name,
                        size(stored.byte_size)
                    )));
                }
            }
            Source::External => details.push(&render(
                state
                    .field(prefix, "external_url", "Link")
                    .kind(InputKind::Url)
                    .required()
                    .attr("maxlength", "2000")
                    .hint("An https:// address."),
            )?),
        }
        for field in [
            state
                .field(prefix, "name", "Name")
                .attr("maxlength", "255")
                .hint(match self.source() {
                    Source::Internal => "The file's name when left blank.",
                    Source::External => "Required for a link.",
                }),
            select(
                state.field(prefix, "category", "Category"),
                self.options.categories.clone(),
                "None",
            ),
            select(
                state.field(prefix, "document_type", "Type"),
                self.options.types.clone(),
                "None",
            ),
        ] {
            details.push(&render(field)?);
        }
        if let Some(fixed) = &self.fixed {
            details.push(&Html::text("Related to "));
            details.push(&render(
                fixed.pill(format!("{prefix}-related-pill"), framed),
            )?);
        }
        let mut body = state.feedback(prefix)?;
        body.push(&details_form(
            prefix,
            "quick_create",
            "Add document",
            cancel_event,
            details,
        )?);
        render(Modal {
            id: &format!("{prefix}-modal"),
            title: "Quick add",
            cancel_event,
            width: ModalWidth::Narrow,
            body,
            ..Modal::default()
        })
    }
}
