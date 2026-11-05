//! `/documents`: the library the reader may see, with search, category,
//! type and status filters and `related=<kind>:<id>` in the address, the New
//! document dialog, and Export CSV of the filtered metadata.
use super::{
    dialog::{self, NewDocument},
    event, file_path, filter_of, path, related_of, render, Clock, Pages, Region, INDEX,
};
use crate::{
    Document, Filter, Marks, Reference, Sort, Source, LIST_CATEGORY, LIST_STATUS, LIST_TYPE,
};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    list_state::{InvalidPageSize, FILTERS_FORM},
    lists::RecordLink,
    page::*,
    params, ActionLink, BuiltinColumn, Button, ButtonVariant, Card, Choice, ColumnsOutcome,
    EmptyState, Filter as ListFilter, FilterToolbar, FlashGroup, FlexRow, FlexTable, Header, Html,
    ListColumns, ListSpec, ListState, PageCounts, Pagination, SortDir, StatusKind, TableEmpty,
    ToolbarControl, NO_FLASH,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
};

const SIZES: [u64; 3] = [25, 50, 100];
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        sortable: BTreeMap::from([
            ("last_revision".into(), SortDir::Desc),
            ("name".into(), SortDir::Asc),
            ("publish_date".into(), SortDir::Desc),
            ("created".into(), SortDir::Desc),
        ]),
        default_sort: Some("last_revision".into()),
        page_sizes: SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        invalid_page_size: InvalidPageSize::Default,
        filters: BTreeMap::from([
            ("category".into(), ListFilter::string("all")),
            ("type".into(), ListFilter::string("all")),
            ("status".into(), ListFilter::string("all")),
            ("show".into(), ListFilter::string("all")),
        ]),
        omit_blank: vec!["search".into()],
        ..ListSpec::default()
    }
    .shared()
    .expect("document list spec")
});

fn columns() -> Result<ListColumns> {
    ListColumns::mount(
        "documents",
        vec![
            BuiltinColumn::new("name", "Name").sortable("name"),
            BuiltinColumn::new("file", "Latest file"),
            BuiltinColumn::new("category", "Category"),
            BuiltinColumn::new("type", "Type"),
            BuiltinColumn::new("status", "Status"),
            BuiltinColumn::new("marks", "Marks"),
            BuiltinColumn::new("revision", "Revision"),
            BuiltinColumn::new("publish_date", "Published").sortable("publish_date"),
            BuiltinColumn::new("last_revision", "Last revision").sortable("last_revision"),
        ],
    )
    .map_err(|_| Error::Internal)
}

/// A code's label from a list's choices; a retired code shows as itself.
fn label(options: &[Choice], code: Option<&str>) -> String {
    match code {
        None => String::new(),
        Some(code) => options
            .iter()
            .find(|c| c.value == code)
            .map_or_else(|| code.to_owned(), |c| c.label.clone()),
    }
}

/// A filter's choices: every value, then the list's.
fn with_all(label: &str, mut options: Vec<Choice>) -> Vec<Choice> {
    options.insert(0, Choice::new(label, "all"));
    options
}

pub(crate) struct Index {
    pages: Pages,
    list: ListState,
    columns: ListColumns,
    related: Option<Reference>,
    framed: bool,
    uploads: Uploads,
    dialog: Option<NewDocument>,
    flash: Option<(StatusKind, String)>,
    /// Each list's choices, read once at mount.
    categories: Vec<Choice>,
    types: Vec<Choice>,
    statuses: Vec<Choice>,
}
impl Index {
    pub async fn new(
        pages: Pages,
        scope: &Scope,
        query: &HashMap<String, String>,
        framed: bool,
    ) -> Result<Self> {
        let categories = pages.choices(scope, LIST_CATEGORY, None).await?;
        let types = pages.choices(scope, LIST_TYPE, None).await?;
        let statuses = pages.choices(scope, LIST_STATUS, None).await?;
        Ok(Self {
            pages,
            list: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: columns()?,
            related: related_of(query),
            framed,
            uploads: dialog::uploads(),
            dialog: None,
            flash: None,
            categories,
            types,
            statuses,
        })
    }
    /// The filters in the address form `filter_of` reads.
    fn query(&self) -> HashMap<String, String> {
        let mut query = self.list.to_params();
        if let Some(related) = &self.related {
            query.insert("related".into(), format!("{}:{}", related.kind, related.id));
        }
        query
    }
    fn filter(&self) -> Filter {
        let mut query = self.query();
        query.insert("search".into(), self.list.search.clone());
        Filter {
            sort: match self.list.sort_by.as_deref() {
                Some("name") => Sort::Name,
                Some("publish_date") => Sort::PublishDate,
                Some("created") => Sort::Created,
                _ => Sort::LastRevision,
            },
            descending: self.list.sort_dir == Some(SortDir::Desc),
            page: self.list.page as i64,
            page_size: self.list.page_size as i64,
            ..filter_of(&query)
        }
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
            "filters" => {
                let filters = params::form_values_checked(value, FILTERS_FORM, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.list.apply_filters(&filters);
            }
            "clear" => {
                self.related = None;
                self.list.apply_filters(&HashMap::from([
                    ("search".into(), String::new()),
                    ("category".into(), "all".into()),
                    ("type".into(), "all".into()),
                    ("status".into(), "all".into()),
                    ("show".into(), "all".into()),
                ]));
            }
            "page" => self
                .list
                .put_page(value.get("page").and_then(Value::as_str)),
            "sort" => {
                self.list.next_sort(value["sort"].as_str().unwrap_or(""));
            }
            "grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                if let ColumnsOutcome::Sort(key) = self.columns.handle(&values) {
                    self.list.next_sort(&key);
                }
            }
            "new" => {
                if self.pages.documents.may_manage(scope).await? {
                    self.dialog = Some(NewDocument::new(&self.pages, scope, None).await?);
                } else {
                    self.flash = Some((
                        StatusKind::Error,
                        "You do not have permission to create documents.".into(),
                    ));
                }
            }
            "close_modal" => self.dialog = None,
            "create" => {
                let pages = self.pages.shared();
                let Some(dialog) = &mut self.dialog else {
                    return Ok(None);
                };
                if let Some(id) = dialog.submit(&pages, scope, credential, value).await? {
                    self.dialog = None;
                    return Ok(Some(LiveBody {
                        redirect: Some(path(id)),
                        flash: Some((StatusKind::Success, "Document created.".into())),
                        ..LiveBody::default()
                    }));
                }
            }
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(None)
    }

    fn row(
        &self,
        clock: &Clock,
        d: &Document,
        latest: Option<&crate::Revision>,
        marks: Marks,
    ) -> Result<FlexRow> {
        let id = d.id.to_string();
        let file = match (d.source, latest) {
            (Source::External, _) => match &d.external_url {
                Some(url) => render(ActionLink {
                    id: &format!("documents-{id}-link"),
                    href: url,
                    body: Html::text("Open link"),
                    ..ActionLink::default()
                })?,
                None => Html::text("—"),
            },
            (Source::Internal, Some(r)) => match &r.file_name {
                Some(name) => render(ActionLink {
                    id: &format!("documents-{id}-file"),
                    href: &file_path(d.id, r.revision),
                    body: Html::text(name),
                    ..ActionLink::default()
                })?,
                None => Html::text("—"),
            },
            _ => Html::text("—"),
        };
        let mut row = FlexRow::new(&id)
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
            .slot("file", file)
            .slot(
                "category",
                Html::text(&label(&self.categories, d.category.as_deref())),
            )
            .slot(
                "type",
                Html::text(&label(&self.types, d.document_type.as_deref())),
            )
            .slot(
                "status",
                Html::text(&label(&self.statuses, Some(&d.status))),
            )
            .slot(
                "marks",
                Html::text(match (marks.favourite, marks.following) {
                    (true, true) => "Favourite · Following",
                    (true, false) => "Favourite",
                    (false, true) => "Following",
                    (false, false) => "",
                }),
            )
            .slot("revision", Html::text(&d.revision.to_string()))
            .slot("publish_date", Html::text(&clock.date(d.publish_date)));
        row.cells.insert(
            "last_revision".into(),
            bilibi_ui::flex_table::Cell::instant(clock.instant(d.last_revision_at)),
        );
        row.dom_id = Some(format!("documents-{id}"));
        Ok(row)
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let clock = self.pages.clock(scope).await?;
        let mut page = self.pages.documents.list(scope, &self.filter()).await?;
        let total_pages = (page.total.max(0) as u64).div_ceil(self.list.page_size);
        if self.list.clamp_to_last_page(total_pages, true) {
            page = self.pages.documents.list(scope, &self.filter()).await?;
        }
        let _ = self.columns.load(&page.entries, |d| d.id.to_string());
        let latest = if self.pages.documents.may_download(scope).await? {
            self.pages
                .documents
                .latest_revisions(scope, &page.entries)
                .await?
        } else {
            HashMap::new()
        };
        let ids: Vec<i64> = page.entries.iter().map(|d| d.id).collect();
        let marks = self.pages.documents.marks(scope, &ids).await?;
        let rows = page
            .entries
            .iter()
            .map(|d| {
                self.row(
                    &clock,
                    d,
                    latest.get(&d.id),
                    marks.get(&d.id).copied().unwrap_or_default(),
                )
            })
            .collect::<Result<Vec<_>>>()?;

        let mut actions = render(ActionLink {
            id: "documents-export",
            href: &export_href(&self.query()),
            body: Html::text("Export CSV"),
            ..ActionLink::default()
        })?;
        if self.pages.documents.may_manage(scope).await? {
            actions.push(&render(Button {
                id: "documents-new",
                button_type: "button",
                event: "new",
                variant: ButtonVariant::Primary,
                body: Html::text("New document"),
                ..Button::default()
            })?);
        }
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                _ => flash.error = message,
            }
        }
        let mut body = render(FlashGroup {
            id: "documents-flash",
            flash,
        })?;
        let subtitle = match &self.related {
            Some(_) => "Linked to one record. Clear the filters to see every document.",
            None => "Agreements, brochures and other files, with every revision.",
        };
        body.push(&render(Header {
            title: Html::text("Documents"),
            subtitle,
            actions,
            ..Header::default()
        })?);
        let form = self.list.filters_form();
        body.push(&render(FilterToolbar::new(
            "documents-filters",
            vec![
                ToolbarControl::search(
                    "documents-search",
                    "search",
                    "Search documents",
                    &form["search"],
                    "Search by name...",
                ),
                ToolbarControl::select(
                    "documents-category",
                    "category",
                    "Category",
                    &form["category"],
                    with_all("All categories", self.categories.clone()),
                ),
                ToolbarControl::select(
                    "documents-type",
                    "type",
                    "Type",
                    &form["type"],
                    with_all("All types", self.types.clone()),
                ),
                ToolbarControl::select(
                    "documents-status",
                    "status",
                    "Status",
                    &form["status"],
                    with_all("All statuses", self.statuses.clone()),
                ),
                ToolbarControl::select(
                    "documents-show",
                    "show",
                    "Show",
                    &form["show"],
                    vec![
                        Choice::new("All documents", "all"),
                        Choice::new("My favourites", "favourites"),
                        Choice::new("Documents I follow", "following"),
                    ],
                ),
            ],
        ))?);
        let mut table = FlexTable::new("documents", self.columns.flex_columns(), rows)
            .sorted(self.list.sort_by.as_deref(), self.list.sort_dir);
        table.event = "grid";
        table.sort_event = "sort";
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Documents";
        table.framed = false;
        if page.entries.is_empty() {
            let clear = render(Button {
                id: "documents-clear",
                button_type: "button",
                event: "clear",
                body: Html::text("Clear filters"),
                ..Button::default()
            })?;
            table.empty = Some(TableEmpty::State(
                EmptyState::missing(
                    "No documents match",
                    "Clear the filters to see every document you may see.",
                )
                .action(clear),
            ));
        }
        let mut card = render(table)?;
        card.push(&render(Pagination::new(
            "documents-pagination",
            PageCounts::new(
                self.list.page,
                self.list.page_size,
                page.total.max(0) as u64,
            ),
            &SIZES,
        ))?);
        body.push(&render(Card {
            id: "documents-card",
            inner_class: "p-0",
            body: card,
            ..Card::default()
        })?);
        if let Some(dialog) = &self.dialog {
            body.push(&dialog.render(
                &self.pages,
                &self.uploads,
                "documents-new",
                "close_modal",
                self.framed,
            )?);
        }
        let region = render(Region {
            id: "documents-region",
            target: false,
            body,
        })?;
        let html = render(bilibi_ui::Page {
            id: "documents-index",
            body: region,
            ..bilibi_ui::Page::default()
        })?
        .to_string();
        Ok(LiveBody {
            html,
            query: self.query(),
            rows: page.entries.len(),
            ..LiveBody::default()
        })
    }
}

/// The export's address: the list's filters, no paging.
fn export_href(query: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = ["search", "category", "type", "status", "show", "related"]
        .iter()
        .filter_map(|key| {
            query
                .get(*key)
                .filter(|v| !v.is_empty() && *v != "all")
                .map(|v| format!("{key}={}", encode(v)))
        })
        .collect();
    pairs.sort();
    if pairs.is_empty() {
        format!("{INDEX}/export.csv")
    } else {
        format!("{INDEX}/export.csv?{}", pairs.join("&"))
    }
}
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~:".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

impl LiveState<Scope, Error> for Index {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(query) => {
                    self.list =
                        ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?;
                    self.related = related_of(query);
                }
                LiveInput::Event(value) => {
                    if let Some(body) = self
                        .handle(request.scope, request.credential, value)
                        .await?
                    {
                        return Ok(body);
                    }
                }
            }
            self.view(request.scope).await
        })
    }
    fn uploads(&mut self) -> Option<&mut Uploads> {
        Some(&mut self.uploads)
    }
}
