//! The operator page (`base/labels.pages`): `/admin/labels` lists every menu
//! item and record type a tenant may rename with the module's own word, and
//! the **Label** column is edited in place. Every change goes through the
//! `base.labels.*` contracts agents call (`PageContracts::submit`), which
//! recheck the capability in their own transaction; a control the person may
//! not use is read-only, a refused edit is an outcome on its field.
use crate::store::{Kind, Labels, Stored, Subject};
use crate::{MANAGE, PAGES};
use askama::Template;
use bilibi_agent_api::{
    page::{Call, PageContracts},
    Error as ContractError,
};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    list_state::FILTERS_FORM,
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, BuiltinColumn, Card, Choice, ColumnsOutcome, CommitStates, CommitStatus, FilterToolbar,
    FlashMap, FlexRow, FlexTable, Header, Html, IconButton, IconKind, InlineEdit, InlineField,
    ListColumns, ListSpec, ListState, Pagination, RowMode, StatusKind, TableEmpty, ToolbarControl,
    NO_FLASH,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
};

pub(crate) const INDEX: &str = "/admin/labels";
const PAGE_SIZES: [u64; 3] = [25, 50, 100];
const DENIED: &str = "You do not have permission to change labels.";

static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 50,
        page_size_param: "per_page".into(),
        filters: BTreeMap::from([(
            "kind".into(),
            bilibi_ui::Filter::one_of(&["all", "menu", "record", "field"], "all"),
        )]),
        omit_blank: vec!["search".into()],
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("labels list spec")
});

/// The module's page adapter; every field is shared by mounted states.
#[derive(Clone)]
pub(crate) struct Pages {
    pub labels: Arc<Labels>,
    pub contracts: Arc<PageContracts>,
}

fn render(fragment: impl Template) -> Result<Html> {
    Html::render(&fragment).map_err(|_| Error::Internal)
}

#[derive(Template)]
#[template(
    source = r#"<span class="text-xs text-ink-muted">{{ text }}</span>"#,
    ext = "html"
)]
struct Quiet<'a> {
    text: &'a str,
}

#[derive(Template)]
#[template(
    source = r#"{% import "components.html" as ui %}{% import "feedback.html" as feedback %}<div data-shared-ui>{% call feedback::flash_group(messages=flash) %}{% endcall %}{% call ui::page(id=id) %}{{ header }}{{ toolbar }}{{ card }}{% endcall %}</div>"#,
    ext = "html"
)]
struct View<'a> {
    id: &'a str,
    flash: FlashMap<'a>,
    header: Html,
    toolbar: Html,
    card: Html,
}

fn columns() -> Vec<BuiltinColumn> {
    vec![
        BuiltinColumn::new("kind", "Kind"),
        BuiltinColumn::new("item", "Found under"),
        BuiltinColumn::new("default", "System label"),
        BuiltinColumn::new("label", "Custom label"),
    ]
}

/// A contract's answer as the page reports it: done, or the sentence to show.
fn outcome(
    result: std::result::Result<Value, ContractError>,
) -> Result<std::result::Result<(), String>> {
    Ok(match result {
        Ok(_) => Ok(()),
        Err(ContractError::Backend(
            error @ (Error::Unauthorized | Error::StepUpRequired { .. }),
        )) => return Err(error),
        Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
            Err(DENIED.into())
        }
        Err(ContractError::Backend(Error::Conflict)) => {
            Err("This label changed since you opened it; reload the page.".into())
        }
        Err(ContractError::InvalidInput(fields)) => Err(fields
            .into_values()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ")),
        Err(_) => Err(
            "The change was not saved. Try again, and tell your administrator if it keeps failing."
                .into(),
        ),
    })
}

struct Index {
    pages: Pages,
    state: ListState,
    columns: ListColumns,
    rows_changed: bool,
    shown: usize,
    framed: bool,
    statuses: CommitStates,
    notice: String,
    flash: Option<(StatusKind, String)>,
}

impl Index {
    fn new(pages: Pages, query: &HashMap<String, String>, framed: bool) -> Result<Self> {
        Ok(Self {
            pages,
            state: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: ListColumns::mount("labels", columns()).map_err(|_| Error::Internal)?,
            rows_changed: true,
            shown: 0,
            framed,
            statuses: CommitStates::default(),
            notice: String::new(),
            flash: None,
        })
    }

    fn call<'a>(
        scope: &'a Scope,
        credential: &'a str,
        event: &'a str,
        contract: &'static str,
    ) -> Call<'a> {
        Call {
            scope,
            credential,
            service: PAGES,
            event,
            contract,
        }
    }

    async fn event(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let get = |key: &str| value.get(key).and_then(Value::as_str);
        self.notice.clear();
        // `kind:subject` is the row's one identity in every event.
        let subject = |text: &str| {
            let (kind, subject) = text.split_once(':')?;
            Some((Kind::from_key(kind)?, subject.to_owned()))
        };
        match get("type").ok_or(Error::Invalid)? {
            "filters" => self.state.apply_filters(
                &params::form_values_checked(value, FILTERS_FORM, params::FIELD_LIMIT)
                    .map_err(Error::refused)?,
            ),
            "page" => self.state.put_page(get("page")),
            "grid" => match self.columns.handle(
                &params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?,
            ) {
                ColumnsOutcome::Sort(_) => return Err(Error::Invalid),
                ColumnsOutcome::Update | ColumnsOutcome::Noop => self.rows_changed = false,
            },
            "clear-flash" => {
                if self
                    .flash
                    .as_ref()
                    .is_some_and(|(kind, _)| Some(kind.name()) == get("key"))
                {
                    self.flash = None;
                }
            }
            "save_field" => {
                let id = get("id").unwrap_or_default();
                let (kind, subject) = subject(id).ok_or(Error::Invalid)?;
                let submitted = get("value").unwrap_or_default();
                let version = get("version").ok_or(Error::Invalid)?;
                let contract = if submitted.trim().is_empty() {
                    "base.labels.clear"
                } else {
                    "base.labels.set"
                };
                let mut input = json!({"kind": kind.key(), "subject": subject, "version": version});
                if contract == "base.labels.set" {
                    input["label"] = json!(submitted);
                }
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &Self::call(scope, credential, "save_field", contract),
                        input,
                    )
                    .await;
                let status = match outcome(result)? {
                    Ok(()) => CommitStatus::Saved,
                    Err(message) if message == DENIED => {
                        self.notice = DENIED.into();
                        self.statuses.clear_saved();
                        CommitStatus::Error(DENIED.into())
                    }
                    Err(message) => CommitStatus::refusal("label", submitted, &[&message]),
                };
                self.statuses.put(id.to_owned(), status);
            }
            "restore" => {
                let (id, version) = get("key")
                    .unwrap_or_default()
                    .split_once('@')
                    .ok_or(Error::Invalid)?;
                let (kind, subject) = subject(id).ok_or(Error::Invalid)?;
                let result = self
                    .pages
                    .contracts
                    .submit(
                        &Self::call(scope, credential, "restore", "base.labels.clear"),
                        json!({"kind": kind.key(), "subject": subject, "version": version}),
                    )
                    .await;
                self.flash = Some(match outcome(result)? {
                    Ok(()) => (
                        StatusKind::Success,
                        "The module's own word shows again.".into(),
                    ),
                    Err(message) => (StatusKind::Error, message),
                });
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    fn inline(&self, subject: &Subject, stored: Option<&Stored>, edits: bool) -> InlineField {
        let id = format!("{}:{}", subject.kind.key(), subject.id);
        let dom = format!(
            "label-{}-{}",
            subject.kind.key(),
            subject.id.replace('.', "-")
        );
        let mut inline = InlineField::new(
            dom,
            "label",
            "Custom label",
            stored.map_or("", |stored| stored.label.as_str()),
        )
        .version(stored.map_or_else(
            || "0:0".to_owned(),
            |stored| format!("{}:{}", stored.id, stored.version),
        ))
        .allow_empty()
        .status(self.statuses.get(&id));
        inline.placeholder = subject.default.clone();
        inline.id_value = id;
        if !edits {
            inline = inline.readonly();
        }
        inline
    }

    async fn render(&mut self, scope: &Scope) -> Result<String> {
        let labels = self.pages.labels.clone();
        let (edits, stored) = {
            let client = labels.db.get().await?;
            let edits = bilibi_authz::effective_capabilities(&**client, scope)
                .await?
                .contains(MANAGE);
            (edits, Labels::stored(&**client, scope).await?)
        };
        let kind = Kind::from_key(self.state.filters.get("kind").map_or("", String::as_str));
        let search = self.state.search.to_lowercase();
        let subjects: Vec<Subject> = labels
            .subjects()?
            .into_iter()
            .filter(|subject| kind.is_none_or(|kind| kind == subject.kind))
            .filter(|subject| {
                search.is_empty()
                    || subject.default.to_lowercase().contains(&search)
                    || subject.id.contains(&search)
                    || stored
                        .get(&(subject.kind, subject.id.clone()))
                        .is_some_and(|stored| stored.label.to_lowercase().contains(&search))
            })
            .collect();
        let page = self.state.paginate(&subjects);
        let (entries, counts) = (page.entries, page.counts);
        self.shown = entries.len();
        if self.rows_changed {
            self.columns.load(entries, |subject| {
                format!("{}:{}", subject.kind.key(), subject.id)
            });
        }
        let header = render(Header {
            title: Html::text("Labels"),
            subtitle: "Customise labels for menu items, record types and fields.",
            ..Header::default()
        })?;
        let form = self.state.filters_form();
        let toolbar = render(FilterToolbar::new(
            "labels-filters",
            vec![
                ToolbarControl::search(
                    "labels-search",
                    "search",
                    "Search labels",
                    &form["search"],
                    "Search by name...",
                ),
                ToolbarControl::select(
                    "labels-kind",
                    "kind",
                    "Kind",
                    &form["kind"],
                    vec![
                        Choice::new("All", "all"),
                        Choice::new("Menu items", "menu"),
                        Choice::new("Record types", "record"),
                        Choice::new("Fields", "field"),
                    ],
                ),
            ],
        ))?;
        let mut rows = Vec::with_capacity(entries.len());
        for subject in entries {
            let own = stored.get(&(subject.kind, subject.id.clone()));
            let key = format!("{}:{}", subject.kind.key(), subject.id);
            let mut row = FlexRow::new(key.clone())
                .slot("kind", Html::text(subject.kind.noun()))
                .slot(
                    "item",
                    render(Quiet {
                        text: if subject.trail.is_empty() {
                            "—"
                        } else {
                            &subject.trail
                        },
                    })?,
                )
                .slot("default", Html::text(&subject.default))
                .slot(
                    "label",
                    render(InlineEdit {
                        field: self.inline(subject, own, edits),
                    })?,
                );
            if let Some(own) = own.filter(|_| edits) {
                row.actions = render(IconButton {
                    id: &format!(
                        "label-restore-{}-{}",
                        subject.kind.key(),
                        subject.id.replace('.', "-")
                    ),
                    kind: IconKind::Neutral,
                    event: "restore",
                    name: "key",
                    value: &format!("{key}@{}:{}", own.id, own.version),
                    ..IconButton::new("unarchive", "Restore the system label")
                })?;
            }
            row.dom_id = Some(format!(
                "label-{}-{}",
                subject.kind.key(),
                subject.id.replace('.', "-")
            ));
            rows.push(row);
        }
        let mut table = FlexTable::new("labels-table", self.columns.flex_columns(), rows);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Labels";
        table.framed = false;
        table.actions = edits;
        if counts.total_entries == 0 {
            table.empty = Some(TableEmpty::Content(Html::text(
                "No menu item, record type or field matches this search.",
            )));
        }
        let mut body = render(table)?;
        body.push(&render(Pagination::new(
            "labels-pagination",
            counts,
            &PAGE_SIZES,
        ))?);
        let card = render(Card {
            id: "labels-card",
            inner_class: "p-0",
            body,
            ..Card::default()
        })?;
        let mut messages = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => messages.success = message,
                StatusKind::Error => messages.error = message,
                StatusKind::Warning => messages.warning = message,
                StatusKind::Info => messages.info = message,
            }
        }
        let _ = (self.framed, RowMode::Compact);
        View {
            id: "labels-page",
            flash: messages,
            header,
            toolbar,
            card,
        }
        .render()
        .map_err(|_| Error::Internal)
    }
}

impl LiveState<Scope, Error> for Index {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            self.rows_changed = true;
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.state =
                        ListState::parse(values, SPEC.clone()).map_err(|_| Error::Internal)?
                }
                LiveInput::Event(value) => {
                    self.event(request.scope, request.credential, value).await?
                }
            }
            let html = self.render(request.scope).await?;
            Ok(LiveBody {
                html,
                query: self.state.to_params(),
                rows: self.shown,
                notice: self.notice.clone(),
                ..LiveBody::default()
            })
        })
    }
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "index").then_some("Labels")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            if request.operation != "index" {
                return Err(Error::NotFound);
            }
            let html = Index::new(self.clone(), request.query, request.framed)?
                .render(scope)
                .await?;
            Ok(PageBody {
                html,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            Ok(match request.operation {
                "index" => Some(
                    Box::new(Index::new(self.clone(), request.query, request.framed)?)
                        as Box<dyn LiveState<Scope, Error>>,
                ),
                _ => None,
            })
        })
    }
}
