//! The Documents panel on a record page: the documents linked to the record
//! (newest revision first), read through `Documents::list`, which decides
//! which of them the reader may see. Its one write is Quick add, which files
//! a document already related to the record through `core.document.create`.
//! Linking, unlinking and everything else happen on the Documents list and
//! record page, so there is one door; the panel's other buttons lead there.
//!
//! Record pages find this panel by the embeds in `contributions.rs` and pass
//! the record in the panel path: `company_id` or `person_id` from the Core
//! company and person pages, `kind` and `id` from a Domain's own record page
//! (the `record.documents` slot).
use super::{
    dialog::{self, QuickDocument},
    event, path, render, Pages, Region, INDEX,
};
use crate::{Filter, Reference, LIST_CATEGORY};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    flex_table::Cell, lists::RecordLink, page::*, params, ActionLink, BuiltinColumn, Button, Card,
    FlexRow, FlexTable, Html, ListColumns, NoticeKind, PanelNotice, SectionHeading, TableEmpty,
};
use serde_json::Value;

/// Rows shown; View all opens the Documents list filtered to the record.
const ROWS: i64 = 10;

/// The record the panel is about, from the path its host page passes.
fn record(operation: &str, request: &PageRequest<'_, Scope>) -> Result<Reference> {
    let positive = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| request.path.get(*key))
            .filter(|id| id.parse::<i64>().is_ok_and(|id| id > 0))
            .cloned()
            .ok_or(Error::Invalid)
    };
    Ok(match operation {
        "company_panel" => Reference::new("company", positive(&["company_id", "id"])?),
        "person_panel" => Reference::new("person", positive(&["person_id", "id"])?),
        "record_panel" => {
            let kind = request.path.get("kind").ok_or(Error::Invalid)?;
            let id = request.path.get("id").ok_or(Error::Invalid)?;
            if kind.is_empty() || kind.len() > 100 || id.is_empty() || id.len() > 64 {
                return Err(Error::Invalid);
            }
            Reference::new(kind.as_str(), id.as_str())
        }
        _ => return Err(Error::NotFound),
    })
}

/// One mounted panel: the record it is about and where it sits on the page.
pub(crate) struct Panel {
    pages: Pages,
    record: Reference,
    anchor: String,
    framed: bool,
    columns: ListColumns,
    quick: Option<QuickDocument>,
    uploads: Uploads,
    notice: Option<(NoticeKind, String)>,
}

impl Panel {
    pub(crate) fn new(pages: Pages, request: &PageRequest<'_, Scope>) -> Result<Self> {
        let anchor = request.path.get("panel").ok_or(Error::Invalid)?;
        if anchor.is_empty()
            || !anchor
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            pages,
            record: record(request.operation, request)?,
            anchor: anchor.clone(),
            framed: request.framed,
            columns: ListColumns::mount(
                &format!("{anchor}-table"),
                vec![
                    BuiltinColumn::new("name", "Name"),
                    BuiltinColumn::new("category", "Category"),
                    BuiltinColumn::new("revision", "Revision"),
                    BuiltinColumn::new("publish_date", "Published"),
                ],
            )
            .map_err(|_| Error::Internal)?,
            quick: None,
            uploads: dialog::uploads(),
            notice: None,
        })
    }

    /// The Quick add, opened for this record when the reader may see it.
    async fn open_quick(&mut self, scope: &Scope) -> Result<()> {
        if !self.pages.documents.may_manage(scope).await? {
            self.notice = Some((
                NoticeKind::Error,
                "You do not have permission to create documents.".into(),
            ));
            return Ok(());
        }
        let fixed = self
            .pages
            .records
            .resolve(scope, std::slice::from_ref(&self.record))
            .await?
            .into_iter()
            .next();
        if fixed.is_none() {
            self.notice = Some((
                NoticeKind::Error,
                "This record could not be found, so a document cannot be added to it.".into(),
            ));
            return Ok(());
        }
        self.uploads.clear(dialog::UPLOAD);
        self.quick = Some(QuickDocument::new(&self.pages, scope, fixed).await?);
        Ok(())
    }

    async fn view(&self, scope: &Scope) -> Result<Html> {
        let anchor = &self.anchor;
        let page = self
            .pages
            .documents
            .list(
                scope,
                &Filter {
                    related: Some(self.record.clone()),
                    page_size: ROWS,
                    ..Filter::default()
                },
            )
            .await?;
        let clock = self.pages.clock(scope).await?;
        let mut rows = Vec::with_capacity(page.entries.len());
        for d in &page.entries {
            let id = d.id.to_string();
            let category = self
                .pages
                .label(scope, LIST_CATEGORY, d.category.as_deref())
                .await?;
            rows.push(
                FlexRow::new(&id)
                    .slot(
                        "name",
                        render(RecordLink {
                            workspace: self.framed,
                            kind: crate::MODULE,
                            record_id: &id,
                            href: &path(d.id),
                            class: "font-medium text-ink-strong hover:underline",
                            body: Html::text(&d.name),
                        })?,
                    )
                    .cell(
                        "category",
                        Cell::text(if category.is_empty() {
                            "—"
                        } else {
                            &category
                        }),
                    )
                    .cell("revision", Cell::text(d.revision.to_string()))
                    .cell("publish_date", Cell::text(clock.date(d.publish_date))),
            );
        }
        let list = format!("{INDEX}?related={}:{}", self.record.kind, self.record.id);
        let mut actions = Html::EMPTY;
        if self.pages.documents.may_manage(scope).await? {
            actions.push(&render(Button {
                id: &format!("{anchor}-quick"),
                button_type: "button",
                event: "quick",
                body: Html::text("Quick add"),
                ..Button::default()
            })?);
            actions.push(&render(ActionLink {
                id: &format!("{anchor}-add"),
                href: &list,
                body: Html::text("Add document"),
                ..ActionLink::default()
            })?);
        }
        let heading_id = format!("{anchor}-heading");
        let mut body = render(SectionHeading {
            id: &heading_id,
            title: "Documents",
            description: "Documents that relate to this record.",
            actions,
            ..SectionHeading::default()
        })?;
        let table_id = format!("{anchor}-table");
        let mut table = FlexTable::new(&table_id, self.columns.flex_columns(), rows);
        table.zoom = self.columns.zoom;
        table.event = "grid";
        table.target = anchor;
        table.caption = "Documents";
        table.framed = false;
        table.class = "mt-4";
        table.empty = Some(TableEmpty::Content(Html::text(
            "No documents you may see relate to this record yet.",
        )));
        body.push(&render(table)?);
        if page.total > ROWS {
            body.push(&render(ActionLink {
                id: &format!("{anchor}-all"),
                href: &list,
                body: Html::text(&format!("View all {}", page.total)),
                ..ActionLink::default()
            })?);
        }
        let mut card = Html::EMPTY;
        if let Some((kind, message)) = &self.notice {
            card.push(&render(PanelNotice {
                id: &format!("{anchor}-notice"),
                kind: *kind,
                dismiss_event: "clear_notice",
                class: "mb-3",
                body: Html::text(message),
            })?);
        }
        card.push(&render(Card {
            id: &format!("{anchor}-card"),
            inner_class: "p-5 sm:p-6",
            role: "region",
            labelledby: &heading_id,
            body,
            ..Card::default()
        })?);
        if let Some(quick) = &self.quick {
            card.push(&quick.render(
                &self.uploads,
                &format!("{anchor}-quick"),
                "close_quick",
                self.framed,
            )?);
        }
        render(Region {
            id: anchor,
            target: true,
            body: card,
        })
    }
}

impl LiveState<Scope, Error> for Panel {
    /// Every wakeup, refresh and address change rereads with the reader's
    /// access. Its events are the table's own column and zoom controls and
    /// the Quick add dialog.
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                self.handle(request.scope, request.credential, value).await?;
            }
            Ok(LiveBody {
                html: self.view(request.scope).await?.to_string(),
                ..LiveBody::default()
            })
        })
    }
    fn uploads(&mut self) -> Option<&mut Uploads> {
        Some(&mut self.uploads)
    }
}

impl Panel {
    async fn handle(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let name = event(value)?;
        if let Some(quick) = &mut self.quick {
            if quick.event(name, value)? {
                return Ok(());
            }
        }
        match name {
            "grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.columns.handle(&values);
            }
            "quick" => self.open_quick(scope).await?,
            "close_quick" => {
                self.quick = None;
                self.uploads.clear(dialog::UPLOAD);
            }
            "quick_create" => {
                let pages = self.pages.shared();
                let Some(quick) = &mut self.quick else {
                    return Ok(());
                };
                if let Some(added) = quick
                    .submit(&pages, scope, credential, &mut self.uploads, value)
                    .await?
                {
                    self.quick = None;
                    self.notice = Some((
                        NoticeKind::Success,
                        format!("Document “{}” added.", added.name),
                    ));
                }
            }
            "clear_notice" => self.notice = None,
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
}
