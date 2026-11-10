//! The list's two actions on a row or a selection: Delete (one, or the
//! ticked rows, with a result that says what was kept and why) and the
//! in-place Preview of a latest file. Both go through the same authority as
//! the record page: `core.document.delete` per document, and the download's
//! own route for the bytes.
use super::Index;
use crate::web::{file_path, preview_path, render};
use crate::{Document, Filter};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    download::FileBody, Alert, Button, ButtonVariant, ConfirmDialog, Fact, Facts, FilePreview,
    Html, Modal, ModalWidth, StatusKind,
};
use serde_json::{json, Value};

/// The most documents one delete takes: each is its own reviewed write, so a
/// bound keeps one click from holding the page for minutes.
pub(super) const MAX_DELETE: usize = 100;
/// The most rows `Documents::list` returns in one page.
const LIST_PAGE: usize = 300;

/// What the delete confirmation is about.
pub(super) enum Deletion {
    /// One row's own Delete.
    One(i64),
    /// The ticked rows, or every row the filters match less those unticked.
    Selected,
}

/// The file the preview dialog shows: a document's latest revision.
pub(super) struct Previewing {
    pub(super) name: String,
    pub(super) file_name: String,
    pub(super) path: String,
    pub(super) download: String,
}

/// What a delete did: how many documents went and which were kept, and why.
#[derive(Default)]
pub(super) struct Outcome {
    pub(super) removed: usize,
    pub(super) refused: Vec<(String, String)>,
}

impl Index {
    /// Opens the preview of a document's latest file. The page decides nothing
    /// about access: the frame's route repeats the download's authorization
    /// (`core.document.download` and the grant's download permission) on every
    /// read, and this only refuses a row that offers no file the reader may take.
    pub(super) async fn open_preview(&mut self, scope: &Scope, value: &Value) -> Result<()> {
        let Some(id) = value["id"]
            .as_str()
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|id| *id > 0)
        else {
            return Err(Error::Invalid);
        };
        let allowed = self
            .pages
            .documents
            .granted(scope, &[id], "download")
            .await?;
        let document = match self.pages.documents.get(scope, id).await {
            Ok(document) if allowed.contains(&id) => Some(document),
            Ok(_) | Err(Error::NotFound | Error::Forbidden) => None,
            Err(error) => return Err(error),
        };
        let latest = match &document {
            Some(document) => self
                .pages
                .documents
                .latest_revisions(scope, std::slice::from_ref(document))
                .await?
                .remove(&id),
            None => None,
        };
        self.preview = match (document, latest) {
            (Some(document), Some(revision))
                if revision
                    .content_type
                    .as_deref()
                    .is_some_and(FileBody::previewable) =>
            {
                Some(Previewing {
                    name: document.name,
                    file_name: revision.file_name.unwrap_or_default(),
                    path: preview_path(id, revision.revision),
                    download: file_path(id, revision.revision),
                })
            }
            _ => {
                self.flash = Some((
                    StatusKind::Error,
                    "This document has no file you may preview.".into(),
                ));
                None
            }
        };
        Ok(())
    }

    /// Opens the delete confirmation for one row (its `id`) or the ticked rows.
    pub(super) async fn request_delete(&mut self, scope: &Scope, value: &Value) -> Result<()> {
        if !self.pages.documents.may_manage(scope).await? {
            self.flash = Some((
                StatusKind::Error,
                "You do not have permission to delete documents.".into(),
            ));
            return Ok(());
        }
        let one = value["id"]
            .as_str()
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|id| *id > 0);
        self.confirm = match one {
            Some(id) => Some(Deletion::One(id)),
            None if self.selection.count() == 0 => {
                self.flash = Some((
                    StatusKind::Info,
                    "Tick the documents to delete first.".into(),
                ));
                None
            }
            None if self.selection.count() > MAX_DELETE as u64 => {
                self.flash = Some((StatusKind::Error, too_many(self.selection.count())));
                None
            }
            None => Some(Deletion::Selected),
        };
        Ok(())
    }

    /// The ids a confirmed delete takes, or why it takes none.
    async fn targets(
        &self,
        scope: &Scope,
        what: &Deletion,
    ) -> Result<std::result::Result<Vec<i64>, String>> {
        let selection = &self.selection;
        let ids: Vec<i64> = match what {
            Deletion::One(id) => vec![*id],
            Deletion::Selected if selection.all_matching => {
                // Every match is read on one page (the list reads at most
                // `LIST_PAGE` rows), then the unticked ones are left out, so
                // rows unticked since "select all" count before the refusal.
                let filter = Filter {
                    page: 1,
                    page_size: (MAX_DELETE + selection.excluded.len() + 1).min(LIST_PAGE) as i64,
                    ..self.filter()
                };
                let page = self.pages.documents.list(scope, &filter).await?;
                let total = page.total.max(0) as usize;
                if total > LIST_PAGE {
                    return Ok(Err(format!(
                        "More than {LIST_PAGE} documents match, so the selection cannot be listed. Narrow the filters or tick fewer rows."
                    )));
                }
                let ticked = total.saturating_sub(selection.excluded.len());
                if ticked > MAX_DELETE {
                    return Ok(Err(too_many(ticked as u64)));
                }
                page.entries
                    .iter()
                    .map(|d| d.id)
                    .filter(|id| !selection.excluded.contains(&id.to_string()))
                    .collect()
            }
            Deletion::Selected => {
                if selection.selected.len() > MAX_DELETE {
                    return Ok(Err(too_many(selection.selected.len() as u64)));
                }
                selection
                    .selected
                    .iter()
                    .filter_map(|key| key.parse().ok())
                    .collect()
            }
        };
        Ok(Ok(ids))
    }

    /// Deletes each document through `core.document.delete`, one reviewed
    /// write apiece, so a refusal (a retained or held revision, a changed
    /// version, no manage grant) keeps that document and the rest still go.
    pub(super) async fn delete(&mut self, scope: &Scope, credential: &str) -> Result<()> {
        let Some(what) = self.confirm.take() else {
            return Ok(());
        };
        if !self.pages.documents.may_manage(scope).await? {
            self.flash = Some((
                StatusKind::Error,
                "You do not have permission to delete documents.".into(),
            ));
            return Ok(());
        }
        let ids = match self.targets(scope, &what).await? {
            Ok(ids) => ids,
            Err(message) => {
                self.flash = Some((StatusKind::Error, message));
                return Ok(());
            }
        };
        let mut outcome = Outcome::default();
        for id in ids {
            let document = match self.pages.documents.get(scope, id).await {
                Ok(document) => document,
                Err(Error::NotFound | Error::Forbidden) => {
                    outcome.refused.push((
                        format!("Document {id}"),
                        "It no longer exists, or you may not see it.".into(),
                    ));
                    continue;
                }
                Err(error @ (Error::Unauthorized | Error::StepUpRequired { .. })) => {
                    return Err(error)
                }
                // A database error on one document is that document's result;
                // the rest still go and the person sees what happened.
                Err(_) => {
                    outcome.refused.push((
                        format!("Document {id}"),
                        "It could not be read just now. Reload and try again.".into(),
                    ));
                    continue;
                }
            };
            let result = self
                .pages
                .submit(
                    scope,
                    credential,
                    "delete",
                    "core.document.delete",
                    json!({"id": id, "version": document.version}),
                    "The document was not deleted. Try again.",
                )
                .await;
            match result {
                Ok(Ok(_)) => outcome.removed += 1,
                Ok(Err(refusal)) => outcome.refused.push((document.name, refusal.text())),
                Err(error @ (Error::Unauthorized | Error::StepUpRequired { .. })) => {
                    return Err(error)
                }
                Err(_) => outcome.refused.push((
                    document.name,
                    "A database error stopped this one; reload to see whether it was deleted."
                        .into(),
                )),
            }
        }
        self.selection.clear();
        match (&what, outcome.removed, outcome.refused.is_empty()) {
            (Deletion::One(_), 1, true) => {
                self.flash = Some((StatusKind::Success, "Document deleted.".into()));
            }
            (Deletion::One(_), _, false) => {
                if let Some((_, why)) = outcome.refused.first() {
                    self.flash = Some((StatusKind::Error, why.clone()));
                }
            }
            _ => self.outcome = Some(outcome),
        }
        Ok(())
    }

    /// The latest file in place, with its download beside it.
    fn preview_dialog(preview: &Previewing) -> Result<Html> {
        let mut body = render(FilePreview {
            id: "documents-preview-frame",
            src: &preview.path,
            title: &format!("Preview of {}", preview.file_name),
        })?;
        body.push(&render(Button {
            id: "documents-preview-download",
            href: &preview.download,
            class: "mt-3",
            body: Html::text("Download"),
            ..Button::default()
        })?);
        body.push(&render(Button {
            id: "documents-preview-close",
            button_type: "button",
            event: "close_preview",
            variant: ButtonVariant::Primary,
            class: "mt-3 ml-2",
            body: Html::text("Close"),
            ..Button::default()
        })?);
        render(Modal {
            id: "documents-preview-modal",
            title: &preview.name,
            cancel_event: "close_preview",
            width: ModalWidth::Wide,
            body,
            ..Modal::default()
        })
    }

    /// The result of a delete that took several documents or kept some.
    fn outcome_dialog(outcome: &Outcome) -> Result<Html> {
        let plural = |n: usize| if n == 1 { "document" } else { "documents" };
        let (kind, summary) = match (outcome.removed, outcome.refused.len()) {
            (0, kept) => (
                StatusKind::Error,
                format!("No document was deleted. {kept} {} kept.", plural(kept)),
            ),
            (removed, 0) => (
                StatusKind::Success,
                format!("{removed} {} deleted.", plural(removed)),
            ),
            (removed, kept) => (
                StatusKind::Warning,
                format!(
                    "{removed} {} deleted. {kept} {} kept.",
                    plural(removed),
                    plural(kept)
                ),
            ),
        };
        let mut body = render(Alert {
            kind,
            id: "documents-result-summary",
            body: Html::text(&summary),
            ..Alert::default()
        })?;
        if !outcome.refused.is_empty() {
            let ids: Vec<String> = (0..outcome.refused.len())
                .map(|i| format!("documents-result-kept-{i}"))
                .collect();
            body.push(&render(Facts {
                id: "documents-result-kept",
                items: outcome
                    .refused
                    .iter()
                    .zip(&ids)
                    .map(|((name, why), id)| Fact {
                        title: name,
                        id,
                        value: Html::text(why),
                    })
                    .collect(),
            })?);
        }
        let actions = render(Button {
            id: "documents-result-close",
            button_type: "button",
            event: "close_result",
            variant: ButtonVariant::Primary,
            body: Html::text("Close"),
            ..Button::default()
        })?;
        body.push(&actions);
        render(Modal {
            id: "documents-result-modal",
            title: "Delete result",
            cancel_event: "close_result",
            width: ModalWidth::Narrow,
            body,
            ..Modal::default()
        })
    }
    /// The dialogs a delete or preview opens: the confirmation, the result and
    /// the preview. `entries` are the rows on screen, which name a single row.
    pub(super) fn dialogs(&self, entries: &[Document]) -> Result<Html> {
        let mut body = Html::EMPTY;
        if let Some(what) = &self.confirm {
            let plural = |n: u64| if n == 1 { "document" } else { "documents" };
            let consequence = match what {
                Deletion::One(id) => match entries.iter().find(|d| d.id == *id) {
                    Some(d) => format!("“{}” will be deleted with all its revisions.", d.name),
                    None => "This document will be deleted with all its revisions.".into(),
                },
                Deletion::Selected => {
                    let n = self.selection.count();
                    format!(
                        "{n} {} will be deleted with all their revisions.",
                        plural(n)
                    )
                }
            };
            body.push(&render(ConfirmDialog {
                    id: "documents-confirm",
                    consequence: &consequence,
                    detail: "Every revision's file is deleted too, and each document leaves every record it relates to. A document with a retained revision, or one under legal hold, is kept, and the result says why.",
                    confirm: "Delete",
                    working: "Deleting…",
                    confirm_event: "delete",
                    cancel_event: "cancel_delete",
                })?);
        }
        if let Some(outcome) = &self.outcome {
            body.push(&Self::outcome_dialog(outcome)?);
        }
        if let Some(preview) = &self.preview {
            body.push(&Self::preview_dialog(preview)?);
        }
        Ok(body)
    }
}

fn too_many(count: u64) -> String {
    format!("At most {MAX_DELETE} documents can be deleted at once, and {count} are selected. Narrow the filters or tick fewer rows.")
}
