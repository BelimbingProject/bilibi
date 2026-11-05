//! The Documents panel on a record page: the documents linked to the record
//! (newest revision first), read through `Documents::list`, which decides
//! which of them the reader may see. The panel writes nothing. Adding,
//! linking and unlinking happen on the Documents list and record page, so
//! there is one door; the panel's buttons lead there.
//!
//! Record pages find this panel by the embeds in `contributions.rs` and pass
//! the record in the panel path: `company_id` or `person_id` from the Core
//! company and person pages, `kind` and `id` from a Domain's own record page
//! (the `record.documents` slot).
use super::{path, render, Pages, Region, INDEX};
use crate::{Filter, Reference, LIST_CATEGORY};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    flex_table::Cell, lists::RecordLink, page::*, params, ActionLink, BuiltinColumn, Card, FlexRow,
    FlexTable, Html, ListColumns, SectionHeading, TableEmpty,
};

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
        })
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
        let card = render(Card {
            id: &format!("{anchor}-card"),
            inner_class: "p-5 sm:p-6",
            role: "region",
            labelledby: &heading_id,
            body,
            ..Card::default()
        })?;
        render(Region {
            id: anchor,
            target: false,
            body: card,
        })
    }
}

impl LiveState<Scope, Error> for Panel {
    /// The panel only reads, so every wakeup, refresh and address change
    /// rereads with the reader's access. Its one event is the table's own
    /// column and zoom controls.
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                if value.get("type").and_then(serde_json::Value::as_str) != Some("grid") {
                    return Err(Error::Invalid);
                }
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.columns.handle(&values);
            }
            Ok(LiveBody {
                html: self.view(request.scope).await?.to_string(),
                ..LiveBody::default()
            })
        })
    }
}
