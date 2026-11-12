//! The portal's Documents (issue 524): `/portal/documents`, the documents
//! staff shared with the signed-in customer; each one's latest file at
//! `/portal/documents/{id}/file`; and the same list as the `portal.documents`
//! panel, which a Domain's portal page mounts with its record's `kind` and
//! `id` in the panel path to show only that record's shared documents.
//!
//! Every read is `Documents::shared_with_me` or `Documents::read_shared_file`
//! with the customer's `PortalScope`, so the share decides what shows and the
//! facade audits every download and refusal. Nothing here writes. Dates show
//! in UTC: a customer has no company zone.
use super::{render, size, Region};
use crate::{
    Documents, PortalFilter, Reference, ScanStatus, SharedDocument, Sort, WriteError, PORTAL_INDEX,
};
use bilibi_database::{Error, Result};
use bilibi_tenancy::PortalScope;
use bilibi_ui::{
    download::{Download, DownloadFuture, DownloadRequest, FileBody},
    flex_table::Cell,
    list_state::{Filter, ListSpec, ListState, SortDir, FILTERS_FORM},
    page::*,
    params, ActionLink, BuiltinColumn, Card, ColumnsOutcome, FilterToolbar, FlexRow, FlexTable,
    Header, Html, ListColumns, PageCounts, PageVariant, Pagination, SectionHeading, TableEmpty,
    ToolbarControl,
};
use jiff::{tz::TimeZone, Timestamp};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

/// The portal pages' registry service.
pub const PORTAL: &str = "core/document.portal";
/// The portal download's registry service.
pub const PORTAL_FILES: &str = "core/document.portal_files";
/// Rows the panel shows; View all opens the list.
const ROWS: i64 = 10;
const PAGE_SIZES: [u64; 3] = [25, 50, 100];
/// `related=<kind>:<id>` narrows the list to one record's shared documents.
const RELATED: &str = "related";

/// Where a shared document's latest file downloads.
pub fn portal_file_path(id: i64) -> String {
    format!("{PORTAL_INDEX}/{id}/file")
}

static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        sortable: [
            ("name".to_owned(), SortDir::Asc),
            ("publish_date".to_owned(), SortDir::Desc),
            ("updated".to_owned(), SortDir::Desc),
        ]
        .into_iter()
        .collect(),
        default_sort: Some("updated".into()),
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: PAGE_SIZES[0],
        filters: [(RELATED.to_owned(), Filter::string(""))]
            .into_iter()
            .collect(),
        omit_blank: vec!["search".into(), RELATED.into()],
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("portal documents list spec")
});

fn related_of(value: &str) -> Option<Reference> {
    let (kind, id) = value.split_once(':')?;
    (!kind.is_empty() && kind.len() <= 100 && !id.is_empty() && id.len() <= 64)
        .then(|| Reference::new(kind, id))
}

/// A day as the customer reads it, in UTC.
fn day(at: Timestamp) -> String {
    at.to_zoned(TimeZone::UTC).date().to_string()
}

/// The last day a share works: it ends at the start of the next day.
fn last_day(end: Timestamp) -> String {
    day(end
        .checked_sub(jiff::SignedDuration::from_secs(1))
        .unwrap_or(end))
}

fn columns(id: &str, sortable: bool) -> Result<ListColumns> {
    let sorted = |key: &str, label: &str| {
        let column = BuiltinColumn::new(key, label);
        if !sortable {
            return column;
        }
        let mut column = column.sortable(key);
        column.sort_id = Some(format!("{id}-sort-{}", key.replace('_', "-")));
        column
    };
    ListColumns::mount(
        id,
        vec![
            sorted("name", "Name"),
            sorted("publish_date", "Published"),
            sorted("updated", "Updated"),
            BuiltinColumn::new("file", "File"),
            BuiltinColumn::new("shared_until", "Available until"),
            BuiltinColumn::new("download", "Download"),
        ],
    )
    .map_err(|_| Error::Internal)
}

/// One shared document's row: its file downloads only when its scan is clean.
fn row(prefix: &str, d: &SharedDocument) -> Result<FlexRow> {
    let id = d.id.to_string();
    let file = match (&d.file_name, d.byte_size) {
        (Some(name), Some(bytes)) => format!("{name} · {}", size(bytes)),
        (Some(name), None) => name.clone(),
        _ => "—".into(),
    };
    let download = if d.downloadable() {
        render(ActionLink {
            id: &format!("{prefix}-{id}-download"),
            icon: "arrow-down-tray",
            title: &format!("Download {}", d.name),
            href: &portal_file_path(d.id),
            body: Html::text("Download"),
            ..ActionLink::default()
        })?
    } else if d.scan_status == ScanStatus::Pending {
        Html::text("Being checked; try again later")
    } else {
        Html::text("Not available")
    };
    let mut row = FlexRow::new(&id)
        .cell("name", Cell::text(&d.name))
        .cell("publish_date", Cell::text(d.publish_date.to_string()))
        .cell("updated", Cell::text(day(d.last_revision_at)))
        .cell("file", Cell::text(file))
        .cell(
            "shared_until",
            Cell::text(d.shared_until.map_or_else(|| "No end".into(), last_day)),
        )
        .slot("download", download);
    row.dom_id = Some(format!("{prefix}-{id}"));
    Ok(row)
}

/// The portal pages and the panel.
pub struct PortalPages(pub Arc<Documents>);
impl Page<PortalScope, Error> for PortalPages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "portal_list").then_some("Documents")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, PortalScope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let portal = request.scope.ok_or(Error::Unauthorized)?;
            let mut state = self.state(&request)?;
            let body = state
                .update(LiveRequest {
                    scope: portal,
                    credential: "",
                    input: LiveInput::Refresh,
                })
                .await?;
            Ok(PageBody {
                html: body.html,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(
        &'a self,
        request: PageRequest<'a, PortalScope>,
    ) -> MountFuture<'a, PortalScope, Error> {
        Box::pin(async move {
            request.scope.ok_or(Error::Unauthorized)?;
            Ok(Some(self.state(&request)?))
        })
    }
}
impl PortalPages {
    fn state(
        &self,
        request: &PageRequest<'_, PortalScope>,
    ) -> Result<Box<dyn LiveState<PortalScope, Error>>> {
        Ok(match request.operation {
            "portal_list" => Box::new(List::new(self.0.clone(), request.query)?),
            "portal_panel" => Box::new(Panel::new(self.0.clone(), request)?),
            _ => return Err(Error::NotFound),
        })
    }
}

/// `/portal/documents`: every document shared with the customer, searchable
/// and paged.
struct List {
    documents: Arc<Documents>,
    list: ListState,
    columns: ListColumns,
}
impl List {
    fn new(documents: Arc<Documents>, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            documents,
            list: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: columns("portal-documents-table", true)?,
        })
    }
    fn filter(&self) -> PortalFilter {
        PortalFilter {
            search: self.list.search.clone(),
            related: self
                .list
                .filters
                .get(RELATED)
                .and_then(|v| related_of(v.as_str())),
            sort: match self.list.sort_by.as_deref() {
                Some("name") => Sort::Name,
                Some("publish_date") => Sort::PublishDate,
                _ => Sort::LastRevision,
            },
            descending: self.list.sort_dir == Some(SortDir::Desc),
            page: i64::try_from(self.list.page).unwrap_or(1),
            page_size: i64::try_from(self.list.page_size).unwrap_or(25),
        }
    }
    async fn view(&mut self, portal: &PortalScope) -> Result<String> {
        let mut page = self
            .documents
            .shared_with_me(portal, &self.filter())
            .await?;
        let total_pages = u64::try_from(page.total)
            .unwrap_or(0)
            .div_ceil(self.list.page_size.max(1));
        if self.list.clamp_to_last_page(total_pages, true) {
            page = self
                .documents
                .shared_with_me(portal, &self.filter())
                .await?;
        }
        let rows = page
            .entries
            .iter()
            .map(|d| row("portal-document", d))
            .collect::<Result<Vec<_>>>()?;
        let mut body = render(Header {
            title: Html::text("Documents"),
            subtitle: "Files shared with you. Download the latest version of each.",
            ..Header::default()
        })?;
        let form = self.list.filters_form();
        body.push(&render(FilterToolbar::new(
            "portal-documents-filters",
            vec![ToolbarControl::search(
                "portal-documents-search",
                "search",
                "Search documents",
                form.get("search").map_or("", String::as_str),
                "Search by name...",
            )],
        ))?);
        let mut table = FlexTable::new("portal-documents-table", self.columns.flex_columns(), rows)
            .sorted(self.list.sort_by.as_deref(), self.list.sort_dir);
        table.zoom = self.columns.zoom;
        table.framed = false;
        table.caption = "Documents shared with you";
        table.empty = Some(TableEmpty::Content(Html::text(
            if self.list.search.is_empty() {
                "Nothing has been shared with you yet."
            } else {
                "No shared document matches that name."
            },
        )));
        let mut card = render(table)?;
        card.push(&render(Pagination::new(
            "portal-documents-pagination",
            PageCounts::new(
                self.list.page,
                self.list.page_size,
                u64::try_from(page.total).unwrap_or(0),
            ),
            &PAGE_SIZES,
        ))?);
        body.push(&render(Card {
            id: "portal-documents-card",
            inner_class: "p-0",
            body: card,
            ..Card::default()
        })?);
        Ok(render(bilibi_ui::Page {
            id: "portal-documents-page",
            variant: PageVariant::List,
            body,
            ..bilibi_ui::Page::default()
        })?
        .to_string())
    }
}
impl LiveState<PortalScope, Error> for List {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, PortalScope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.list =
                        ListState::parse(values, SPEC.clone()).map_err(|_| Error::Internal)?;
                }
                LiveInput::Event(event) => {
                    let get = |key: &str| event.get(key).and_then(Value::as_str);
                    match get("type") {
                        Some("filters") => self.list.apply_filters(
                            &params::form_values_checked(event, FILTERS_FORM, params::FIELD_LIMIT)
                                .map_err(Error::refused)?,
                        ),
                        Some("sort") => {
                            if !self.list.next_sort(get("sort").unwrap_or_default()) {
                                return Err(Error::Invalid);
                            }
                        }
                        Some("page") => self.list.put_page(get("page")),
                        Some("grid") => {
                            let values = params::string_values_checked(event, params::FIELD_LIMIT)
                                .map_err(Error::refused)?;
                            if let ColumnsOutcome::Sort(key) = self.columns.handle(&values) {
                                if !self.list.next_sort(&key) {
                                    return Err(Error::Invalid);
                                }
                            }
                        }
                        _ => return Err(Error::Invalid),
                    }
                }
            }
            Ok(LiveBody {
                html: self.view(request.scope).await?,
                query: self.list.to_params(),
                ..LiveBody::default()
            })
        })
    }
}

/// The `portal.documents` panel: the newest shared documents, narrowed to the
/// record its host page names (`kind` and `id` in the panel path).
struct Panel {
    documents: Arc<Documents>,
    anchor: String,
    related: Option<Reference>,
    columns: ListColumns,
}
impl Panel {
    fn new(documents: Arc<Documents>, request: &PageRequest<'_, PortalScope>) -> Result<Self> {
        let anchor = request.path.get("panel").ok_or(Error::Invalid)?;
        if anchor.is_empty()
            || anchor.len() > 100
            || !anchor
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Invalid);
        }
        let related = match (request.path.get("kind"), request.path.get("id")) {
            (None, None) => None,
            (Some(kind), Some(id)) => {
                Some(related_of(&format!("{kind}:{id}")).ok_or(Error::Invalid)?)
            }
            _ => return Err(Error::Invalid),
        };
        Ok(Self {
            documents,
            anchor: anchor.clone(),
            related,
            columns: columns(&format!("{anchor}-table"), false)?,
        })
    }
    async fn view(&self, portal: &PortalScope) -> Result<Html> {
        let anchor = &self.anchor;
        let page = self
            .documents
            .shared_with_me(
                portal,
                &PortalFilter {
                    related: self.related.clone(),
                    page_size: ROWS,
                    ..PortalFilter::default()
                },
            )
            .await?;
        let rows = page
            .entries
            .iter()
            .map(|d| row(&format!("{anchor}-document"), d))
            .collect::<Result<Vec<_>>>()?;
        let heading_id = format!("{anchor}-heading");
        let mut body = render(SectionHeading {
            id: &heading_id,
            title: "Documents",
            description: if self.related.is_some() {
                "Files shared with you about this record."
            } else {
                "Files shared with you."
            },
            ..SectionHeading::default()
        })?;
        let table_id = format!("{anchor}-table");
        let mut table = FlexTable::new(&table_id, self.columns.flex_columns(), rows);
        table.zoom = self.columns.zoom;
        table.event = "grid";
        table.target = anchor;
        table.caption = "Documents shared with you";
        table.framed = false;
        table.class = "mt-4";
        table.empty = Some(TableEmpty::Content(Html::text(
            "Nothing has been shared with you here yet.",
        )));
        body.push(&render(table)?);
        if page.total > ROWS {
            let list = match &self.related {
                Some(record) => format!(
                    "{PORTAL_INDEX}?{RELATED}={}",
                    url::form_urlencoded::byte_serialize(
                        format!("{}:{}", record.kind, record.id).as_bytes()
                    )
                    .collect::<String>()
                ),
                None => PORTAL_INDEX.to_owned(),
            };
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
            target: true,
            body: card,
        })
    }
}
impl LiveState<PortalScope, Error> for Panel {
    /// Every wakeup and refresh rereads with the customer's shares. Its only
    /// events are the table's own column and zoom controls.
    fn update<'a>(&'a mut self, request: LiveRequest<'a, PortalScope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                if value.get("type").and_then(Value::as_str) != Some("grid") {
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

/// `/portal/documents/{id}/file`: the latest file of a document shared with
/// the customer, read and audited by `Documents::read_shared_file` every
/// time. A refusal is audited there and answers as the staff download does.
pub struct PortalFiles(pub Arc<Documents>);
impl Download<PortalScope, Error> for PortalFiles {
    fn serve<'a>(&'a self, request: DownloadRequest<'a, PortalScope>) -> DownloadFuture<'a, Error> {
        Box::pin(async move {
            let id = request
                .path
                .get("id")
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|v| *v > 0)
                .ok_or(Error::NotFound)?;
            match self.0.read_shared_file(request.scope, id).await {
                Ok(file) => Ok(FileBody::attachment(
                    file.bytes,
                    &file.content_type,
                    &file.file_name,
                )),
                Err(WriteError::Access(error)) => Err(error),
                Err(WriteError::Quarantined | WriteError::ScanPending) => Err(Error::Forbidden),
                Err(_) => Err(Error::NotFound),
            }
        })
    }
}
