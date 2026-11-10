//! `/documents`: the library the reader may see, with search, category,
//! type and status filters and `related=<kind>:<id>` in the address, the New
//! document dialog, and Export CSV of the filtered metadata.
use super::{
    dialog::{self, NewDocument, QuickDocument},
    event, file_path, filter_of, path, related_of, render, Clock, Pages, Region, INDEX,
};
use crate::{
    Document, Filter, Marks, Reference, Sort, Source, LIST_CATEGORY, LIST_STATUS, LIST_TYPE,
};
use bilibi_database::{Error, Result};
use bilibi_records::Resolved;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    download::FileBody,
    list_state::{InvalidPageSize, FILTERS_FORM},
    lists::RecordLink,
    page::*,
    params, ActionLink, BuiltinColumn, Button, ButtonVariant, Card, Choice, ColumnsOutcome,
    EmptyState, Filter as ListFilter, FilterToolbar, FlashGroup, FlexRow, FlexTable, Header, Html,
    IconButton, IconKind, ListColumns, ListSpec, ListState, PageCounts, Pagination, Selection,
    SortDir, StatusKind, TableEmpty, ToolbarControl, NO_FLASH,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, LazyLock},
};

mod actions;
use actions::{Deletion, Outcome, Previewing};

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
            ("expiry".into(), ListFilter::string("all")),
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
            BuiltinColumn::new("sync_id", "Sync ID"),
            BuiltinColumn::new("related_document", "Related document"),
            BuiltinColumn::new("file", "Latest file"),
            BuiltinColumn::new("category", "Category"),
            BuiltinColumn::new("type", "Type"),
            BuiltinColumn::new("status", "Status"),
            BuiltinColumn::new("marks", "Marks"),
            BuiltinColumn::new("expires", "Expires"),
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
    quick: Option<QuickDocument>,
    flash: Option<(StatusKind, String)>,
    /// The ticked documents, which `Selection::apply` keeps: by id, or every
    /// match less those unticked since; cleared when the filters or sort
    /// change.
    selection: Selection,
    /// The keys of the rows on screen, which the page tick takes.
    page_keys: Vec<String>,
    confirm: Option<Deletion>,
    outcome: Option<Outcome>,
    preview: Option<Previewing>,
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
            quick: None,
            flash: None,
            selection: Selection {
                event: "select".into(),
                noun: "documents".into(),
                ..Selection::default()
            },
            page_keys: Vec::new(),
            confirm: None,
            outcome: None,
            preview: None,
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
    /// What the ticks belong to: the filters and the sort, not the page.
    fn fingerprint(&self) -> Vec<(String, String)> {
        let mut query: Vec<_> = self
            .query()
            .into_iter()
            .filter(|(key, _)| key != "page" && key != "per_page")
            .collect();
        query.sort();
        query
    }
    /// The record the list is filtered to, as the dialogs relate new documents
    /// to it, when the reader may see it.
    async fn related_record(&self, scope: &Scope) -> Result<Option<Resolved>> {
        match &self.related {
            Some(related) => Ok(self
                .pages
                .records
                .resolve(scope, std::slice::from_ref(related))
                .await?
                .into_iter()
                .next()),
            None => Ok(None),
        }
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
        if let Some(quick) = &mut self.quick {
            if quick.event(name, value)? {
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
                    ("expiry".into(), "all".into()),
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
                    let fixed = self.related_record(scope).await?;
                    self.dialog = Some(NewDocument::new(&self.pages, scope, fixed).await?);
                } else {
                    self.flash = Some((
                        StatusKind::Error,
                        "You do not have permission to create documents.".into(),
                    ));
                }
            }
            "close_modal" => {
                self.dialog = None;
                self.uploads.clear(dialog::UPLOAD);
            }
            "quick" => {
                if self.pages.documents.may_manage(scope).await? {
                    let fixed = self.related_record(scope).await?;
                    self.quick = Some(QuickDocument::new(&self.pages, scope, fixed).await?);
                } else {
                    self.flash = Some((
                        StatusKind::Error,
                        "You do not have permission to create documents.".into(),
                    ));
                }
            }
            "close_quick" => {
                self.quick = None;
                self.uploads.clear(dialog::UPLOAD);
            }
            "quick_create" => {
                let pages = self.pages.shared();
                let Some(quick) = &mut self.quick else {
                    return Ok(None);
                };
                if let Some(added) = quick
                    .submit(&pages, scope, credential, &mut self.uploads, value)
                    .await?
                {
                    self.quick = None;
                    self.flash = Some((
                        StatusKind::Success,
                        match added.duplicates {
                            0 => format!("Document “{}” added.", added.name),
                            n => format!(
                                "Document “{}” added. {n} other document{} you may see already hold{} this file.",
                                added.name,
                                if n == 1 { "" } else { "s" },
                                if n == 1 { "s" } else { "" },
                            ),
                        },
                    ));
                }
            }
            "select" => self.selection.apply(
                value["op"].as_str().unwrap_or(""),
                value["key"].as_str(),
                &self.page_keys,
            ),
            "request_delete" => self.request_delete(scope, value).await?,
            "cancel_delete" => self.confirm = None,
            "delete" => self.delete(scope, credential).await?,
            "close_result" => self.outcome = None,
            "preview" => self.open_preview(scope, value).await?,
            "close_preview" => self.preview = None,
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
        can_delete: bool,
        can_preview: bool,
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
            .slot("sync_id", Html::text(d.sync_id.as_deref().unwrap_or("")))
            .slot(
                "related_document",
                Html::text(
                    &d.related_document_id
                        .map(|id| format!("{} #{id}", d.relation.as_deref().unwrap_or("Related")))
                        .unwrap_or_default(),
                ),
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
            .slot(
                "expires",
                Html::text(&match d.expires_on {
                    Some(on) if on < clock.today() => format!("{} (expired)", clock.date(on)),
                    Some(on) => clock.date(on),
                    None => String::new(),
                }),
            )
            .slot("revision", Html::text(&d.revision.to_string()))
            .slot("publish_date", Html::text(&clock.date(d.publish_date)));
        row.cells.insert(
            "last_revision".into(),
            bilibi_ui::flex_table::Cell::instant(clock.instant(d.last_revision_at)),
        );
        row.dom_id = Some(format!("documents-{id}"));
        row.label = Some(d.name.clone());
        if can_preview {
            row.actions.push(&render(IconButton {
                id: &format!("documents-{id}-preview"),
                event: "preview",
                name: "id",
                value: &id,
                ..IconButton::new("eye", &format!("Preview {}", d.name))
            })?);
        }
        if can_delete {
            row.actions.push(&render(IconButton {
                kind: IconKind::Danger,
                id: &format!("documents-{id}-delete"),
                event: "request_delete",
                name: "id",
                value: &id,
                ..IconButton::new("delete", &format!("Delete {}", d.name))
            })?);
        }
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
        let may_manage = self.pages.documents.may_manage(scope).await?;
        let changeable = if may_manage {
            self.pages.documents.granted(scope, &ids, "manage").await?
        } else {
            HashSet::new()
        };
        let downloadable = if latest.is_empty() {
            HashSet::new()
        } else {
            self.pages
                .documents
                .granted(scope, &ids, "download")
                .await?
        };
        self.page_keys = ids.iter().map(i64::to_string).collect();
        let rows = page
            .entries
            .iter()
            .map(|d| {
                self.row(
                    &clock,
                    d,
                    latest.get(&d.id),
                    marks.get(&d.id).copied().unwrap_or_default(),
                    changeable.contains(&d.id),
                    downloadable.contains(&d.id)
                        && latest
                            .get(&d.id)
                            .and_then(|r| r.content_type.as_deref())
                            .is_some_and(FileBody::previewable),
                )
            })
            .collect::<Result<Vec<_>>>()?;

        let mut actions = render(ActionLink {
            id: "documents-export",
            href: &export_href(&self.query()),
            body: Html::text("Export CSV"),
            ..ActionLink::default()
        })?;
        if may_manage {
            actions.push(&render(Button {
                id: "documents-quick",
                button_type: "button",
                event: "quick",
                body: Html::text("Quick add"),
                ..Button::default()
            })?);
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
                ToolbarControl::select(
                    "documents-expiry",
                    "expiry",
                    "Expiry",
                    &form["expiry"],
                    vec![
                        Choice::new("Any expiry", "all"),
                        Choice::new("Expired", "expired"),
                        Choice::new("Expiring in 30 days", "soon"),
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
        // Ticks, the bar and a Delete on each row only for people who may
        // delete; a document they hold no manage grant on shows none.
        if may_manage {
            self.selection.matching = page.total.max(0) as u64;
            table.selection = Some(Selection {
                actions: render(Button {
                    id: "documents-delete-selected",
                    button_type: "button",
                    event: "request_delete",
                    variant: ButtonVariant::Danger,
                    body: Html::text("Delete…"),
                    ..Button::default()
                })?,
                ..self.selection.clone()
            });
        }
        table.actions = may_manage || !downloadable.is_empty();
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
        if let Some(quick) = &self.quick {
            body.push(&quick.render(
                &self.uploads,
                "documents-quick",
                "close_quick",
                self.framed,
            )?);
        }
        body.push(&self.dialogs(&page.entries)?);
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

fn export_href(query: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = [
        "search", "category", "type", "status", "show", "expiry", "related",
    ]
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
            let before = self.fingerprint();
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
            if self.fingerprint() != before {
                self.selection.clear();
            }
            self.view(request.scope).await
        })
    }
    fn uploads(&mut self) -> Option<&mut Uploads> {
        Some(&mut self.uploads)
    }
}
