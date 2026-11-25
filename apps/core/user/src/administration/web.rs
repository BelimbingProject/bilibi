//! The `/users` page: the
//! shared flexible table over Grid `PageColumns` (arrangement remembered per
//! account; an address that carries `cols`/`z` wins), bounded URL state
//! (`search`, `roleIds`, `page`, `perPage`, `sortBy`, `sortDir`), the search and Roles filters, paging, and the per-row view,
//! impersonate and delete controls.
//!
//! Deleting confirms through the shared dialog: the request holds the listed
//! account whose consequence the dialog states, and `delete` acts on that held
//! account rather than on a client-supplied id, so what was confirmed is what
//! runs. Every write re-proves the actor inside Core User's command; the
//! controls only decide what to draw. Impersonation posts
//! `/admin/impersonate/{id}` (the Web host adapter over Core User's
//! `impersonate`); the control is drawn and disabled by the same rules as
//! the other row actions.
use super::{
    list_users, Entry, Membership, Options, Sort, MAX_PAGE, MAX_ROLE_IDS, MAX_SEARCH_BYTES,
    PAGE_SIZES,
};
use crate::{contributions, AccountError, InvitationError, UserService};
use askama::Template;
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayContext, DisplayPolicy, Format, Precision};
use bilibi_grid::{Outcome, PageColumns, PageViews};
use bilibi_locale::Locales;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    list_state::{InvalidPageSize, ParamNames, FILTERS_FORM},
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    params, Badge, BadgeKind, BuiltinColumn, Card, ColumnType, ConfirmDialog, CreateButton,
    FilterToolbar, FlashGroup, FlashMap, FlexRow, Header, Html, IconButton, IconKind, Instant,
    ListSpec, ListState, MultiSelect, PageCounts, Pagination, RecordLink, RowMode, SortDir,
    StatusKind, TableEmpty, ToolbarControl,
};
use jiff::Timestamp;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
    time::SystemTime,
};

const IMPERSONATE: &str = "admin.user.impersonate";
/// The URL key of the Roles filter; its ids are comma-separated in the
/// address and posted as the multi-select's `filters[roleIds][]` values.
const ROLE_IDS: &str = "roleIds";
const MEMBERSHIP: &str = "membership";
const RESET_REQUESTED: &str = "resetRequested";

/// The spec: four sorts (Created first descending), its parameter
/// names, page sizes 25/50/100/300 with `25` for anything else.
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        sortable: BTreeMap::from([
            ("name".into(), SortDir::Asc),
            ("email".into(), SortDir::Asc),
            ("desks".into(), SortDir::Asc),
            ("created_at".into(), SortDir::Desc),
        ]),
        default_sort: Some("name".into()),
        page_sizes: PAGE_SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "perPage".into(),
        invalid_page_size: InvalidPageSize::Default,
        filters: BTreeMap::from([
            (
                MEMBERSHIP.into(),
                bilibi_ui::Filter::one_of(&["", "active", "invited"], ""),
            ),
            (
                RESET_REQUESTED.into(),
                bilibi_ui::Filter::one_of(&["", "open"], ""),
            ),
        ]),
        param_names: ParamNames {
            sort_by: "sortBy".into(),
            sort_dir: "sortDir".into(),
            ..ParamNames::default()
        },
        omit_blank: vec!["search".into(), MEMBERSHIP.into(), RESET_REQUESTED.into()],
        ..ListSpec::default()
    }
    .shared()
    .expect("users list spec")
});

#[derive(Clone)]
pub struct Pages {
    pub db: Db,
    pub users: Arc<UserService>,
    pub grants: Arc<bilibi_authz::grants::Grants>,
    pub records: Arc<bilibi_records::Records>,
    pub views: Arc<PageViews>,
    pub contracts: Arc<bilibi_agent_api::page::PageContracts>,
    pub display: Arc<DisplayPolicy>,
    pub locales: Arc<Locales>,
}
impl Pages {
    async fn allowed(&self, scope: &Scope, key: &str) -> Result<bool> {
        let client = self.db.get().await?;
        match bilibi_authz::authorize_key(&**client, scope, key).await {
            Ok(()) => Ok(true),
            Err(Error::Forbidden) => Ok(false),
            Err(error) => Err(error),
        }
    }
    async fn display(&self, scope: &Scope) -> Result<DisplayContext> {
        let client = self.db.get().await?;
        let locale = self.locales.resolve(&**client, scope, None).await?.locale;
        self.display.display(&**client, scope, None, locale).await
    }
}
impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "index").then_some("User Management")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let mut state = self.state(&request).await?;
            let body = state
                .update(LiveRequest {
                    scope,
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
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            request.scope.ok_or(Error::Unauthorized)?;
            Ok(Some(self.state(&request).await?))
        })
    }
}
impl Pages {
    async fn state(
        &self,
        request: &PageRequest<'_, Scope>,
    ) -> Result<Box<dyn LiveState<Scope, Error>>> {
        if request.operation != "index" {
            return Err(Error::NotFound);
        }
        let scope = request.scope.ok_or(Error::Unauthorized)?;
        Ok(Box::new(
            Index::new(self.clone(), scope, request.query, request.framed).await?,
        ))
    }
}

fn render(template: &impl Template) -> Result<Html> {
    Html::render(template).map_err(|_| Error::Internal)
}
fn event(event: &Value) -> Result<&str> {
    event
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid)
}

/// The page's one flash message, by kind.
#[derive(Default)]
struct Flash(Option<(StatusKind, String)>);
impl Flash {
    fn success(&mut self, message: String) {
        self.0 = Some((StatusKind::Success, message));
    }
    fn error(&mut self, message: impl Into<String>) {
        self.0 = Some((StatusKind::Error, message.into()));
    }
    fn dismiss(&mut self, key: Option<&str>) {
        if self
            .0
            .as_ref()
            .is_some_and(|(kind, _)| Some(kind.name()) == key)
        {
            self.0 = None;
        }
    }
    fn map(&self) -> FlashMap<'_> {
        let mut flash = FlashMap::default();
        if let Some((kind, message)) = &self.0 {
            match kind {
                StatusKind::Success => flash.success = message,
                StatusKind::Error => flash.error = message,
                StatusKind::Warning => flash.warning = message,
                StatusKind::Info => flash.info = message,
            }
        }
        flash
    }
}

/// At most 100 unique positive ids, in the order given
/// `normalize_role_ids`); anything else is dropped.
fn normalize_role_ids<'a>(values: impl Iterator<Item = &'a str>) -> Vec<i64> {
    let mut ids = Vec::new();
    for value in values {
        let Some(id) = params::positive_id(Some(value.trim())) else {
            continue;
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
        if ids.len() == MAX_ROLE_IDS {
            break;
        }
    }
    ids
}

/// The typed text cut to 255 bytes on a character boundary.
fn bounded_search(text: &str) -> String {
    let mut end = text.len().min(MAX_SEARCH_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The initials: the first letter of the first two words.
fn initials(name: &str) -> String {
    let initials: String = name
        .split_whitespace()
        .take(2)
        .filter_map(|word| word.chars().next())
        .flat_map(char::to_uppercase)
        .collect();
    if initials.is_empty() {
        "?".into()
    } else {
        initials
    }
}

#[derive(Template)]
#[template(
    source = r#"<div class="flex items-center gap-3">{% if avatar %}<span class="grid size-8 shrink-0 place-items-center rounded-full bg-surface-sunken text-xs font-semibold text-ink">{{ initials }}</span>{% endif %}{{ name }}</div>"#,
    ext = "html"
)]
struct NameCell<'a> {
    avatar: bool,
    initials: &'a str,
    name: Html,
}
#[derive(Template)]
#[template(
    source = r#"<span class="font-medium text-ink-strong">{{ name }}</span>"#,
    ext = "html"
)]
struct PlainName<'a> {
    name: &'a str,
}
#[derive(Template)]
#[template(
    source = r#"<span class="whitespace-nowrap text-ink-muted">{{ text }}</span>"#,
    ext = "html"
)]
struct Muted<'a> {
    text: &'a str,
}
#[derive(Template)]
#[template(
    source = r#"<div class="flex flex-wrap gap-1.5">{% for badge in badges %}{{ badge }}{% endfor %}{% if badges.is_empty() %}<span class="text-ink-faint">—</span>{% endif %}</div>"#,
    ext = "html"
)]
struct RolesCell {
    badges: Vec<Html>,
}
#[derive(Template)]
#[template(
    source = r#"{% import "flex_table.html" as flex %}<span class="whitespace-nowrap tabular-nums text-ink-muted">{% call flex::instant(id=id, i=i) %}{% endcall %}</span>"#,
    ext = "html"
)]
struct CreatedCell<'a> {
    id: &'a str,
    i: Instant,
}
#[derive(Template)]
#[template(
    source = r#"<div class="flex items-center justify-end">{{ add_to }}{{ reset }}{{ impersonate }}{{ revoke }}{{ delete }}</div>"#,
    ext = "html"
)]
struct Actions {
    /// "Non-local actions": pages of other modules that take a staff
    /// account ("Add to team"), offered only when one accepts it.
    add_to: Html,
    reset: Html,
    impersonate: Html,
    revoke: Html,
    delete: Html,
}
#[derive(Template)]
#[template(
    source = r#"{{ flash }}{{ header }}{{ toolbar }}{{ card }}{{ dialog }}"#,
    ext = "html"
)]
struct Body {
    flash: Html,
    header: Html,
    toolbar: Html,
    card: Html,
    dialog: Html,
}
#[derive(Template)]
#[template(
    source = r#"<h2 id="users-table-title" class="sr-only">Workspace users</h2>{{ table }}{{ pagination }}"#,
    ext = "html"
)]
struct CardBody {
    table: Html,
    pagination: Html,
}

/// What the actor may do, read once per render; each command re-proves it.
struct Grants {
    view: bool,
    invite: bool,
    delete: bool,
    impersonate: bool,
    reset: bool,
}

struct Index {
    pages: Pages,
    list: ListState,
    role_ids: Vec<i64>,
    membership: Option<Membership>,
    reset_requested: bool,
    linked_recovery: Option<(i64, i64)>,
    columns: PageColumns,
    framed: bool,
    flash: Flash,
    /// The listed account a delete dialog is open for.
    pending_delete: Option<Entry>,
    /// The listed invitation a revoke dialog is open for.
    pending_revoke: Option<Entry>,
    pending_reset: Option<(Entry, i64)>,
    reset_error: Option<String>,
    /// The entries last drawn, so a `request_delete` names only a listed row.
    listed: Vec<Entry>,
}
impl Index {
    async fn new(
        pages: Pages,
        scope: &Scope,
        query: &HashMap<String, String>,
        framed: bool,
    ) -> Result<Self> {
        let mut columns = PageColumns::mount(pages.views.clone(), "users", builtins())
            .map_err(|_| Error::Internal)?;
        columns.from_params(scope, query).await?;
        let mut index = Self {
            pages,
            list: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            role_ids: Vec::new(),
            membership: None,
            reset_requested: false,
            linked_recovery: None,
            columns,
            framed,
            flash: Flash::default(),
            pending_delete: None,
            pending_revoke: None,
            pending_reset: None,
            reset_error: None,
            listed: Vec::new(),
        };
        index.read_params(query);
        Ok(index)
    }
    /// The URL's own values beside `ListState`'s: the Roles filter and the
    /// page cap.
    fn read_params(&mut self, query: &HashMap<String, String>) {
        self.role_ids = normalize_role_ids(
            query
                .get(ROLE_IDS)
                .map(String::as_str)
                .unwrap_or("")
                .split(','),
        );
        self.membership = query
            .get(MEMBERSHIP)
            .and_then(|value| Membership::parse(value));
        self.reset_requested = query
            .get(RESET_REQUESTED)
            .is_some_and(|value| value == "open");
        self.linked_recovery = query
            .get("recoveryTenant")
            .and_then(|tenant| params::positive_id(Some(tenant)))
            .zip(
                query
                    .get("recoveryAccount")
                    .and_then(|user| params::positive_id(Some(user))),
            );
        self.list.search = bounded_search(&self.list.search);
        self.list.page = self.list.page.min(MAX_PAGE);
    }
    fn options(&self) -> Options {
        let sort_key = self.list.sort_by.as_deref().unwrap_or("name");
        let sort = Sort::parse(sort_key).unwrap_or(Sort::Name);
        let descending = match self.list.sort_dir {
            Some(direction) => direction == SortDir::Desc,
            None => sort == Sort::CreatedAt,
        };
        Options {
            // `""` and the PHP-falsey `"0"` disable search.
            search: Some(self.list.search.clone()).filter(|text| !text.is_empty() && text != "0"),
            role_ids: self.role_ids.clone(),
            membership: self.membership,
            reset_requested: self.reset_requested,
            sort,
            descending,
            page: self.list.page,
            page_size: self.list.page_size,
        }
    }
    async fn grants(&self, scope: &Scope) -> Result<Grants> {
        Ok(Grants {
            view: self.pages.allowed(scope, contributions::VIEW).await?,
            invite: self.pages.allowed(scope, contributions::INVITE).await?,
            delete: self.pages.allowed(scope, contributions::DELETE).await?,
            impersonate: self.pages.allowed(scope, IMPERSONATE).await?,
            reset: self.pages.allowed(scope, crate::RESET_TWO_FACTOR).await?,
        })
    }
    /// The companies whose custom roles the actor may see, beside the
    /// system roles.
    async fn visible_companies(&self, scope: &Scope) -> Result<Vec<i64>> {
        let client = self.pages.db.get().await?;
        self.pages.grants.visible_companies(&**client, scope).await
    }
    async fn role_options(
        &self,
        scope: &Scope,
        companies: &[i64],
    ) -> Result<Vec<bilibi_authz::roles::RoleSummary>> {
        let client = self.pages.db.get().await?;
        Ok(bilibi_authz::roles::list(
            &**client,
            scope,
            companies,
            &bilibi_authz::roles::Query {
                search: String::new(),
                sort: bilibi_authz::roles::Sort::Name,
                descending: false,
                page: 1,
                page_size: 100,
            },
        )
        .await?
        .entries)
    }
    fn instant(display: &DisplayContext, at: SystemTime) -> Result<Instant> {
        let at = Timestamp::try_from(at).map_err(|_| Error::Internal)?;
        let rendered = display.render(at, Format::DateTime, Precision::Minute);
        Ok(Instant::new(
            &rendered.iso,
            &rendered.text,
            &rendered.company,
            &rendered.utc,
            rendered.mode.as_str(),
        ))
    }
    fn actions(
        &self,
        scope: &Scope,
        grants: &Grants,
        user: &Entry,
        add_to: &[bilibi_records::AddTo],
    ) -> Result<Html> {
        let add_to = if add_to.is_empty() || user.membership == Membership::Invited {
            Html::EMPTY
        } else {
            let key = user.id.to_string();
            render(&bilibi_ui::AddToMenu::new(
                format!("user-{}-add-to", user.id),
                user.name.clone(),
                add_to
                    .iter()
                    .map(|target| bilibi_ui::AddToItem {
                        label: target.label.into(),
                        href: target.href(&[bilibi_records::Reference::new("user", key.clone())]),
                        disabled: None,
                    })
                    .collect(),
            ))?
        };
        let own = user.id == scope.user();
        let pending = user.membership == Membership::Invited;
        let reset = if user.can_reset && user.two_factor_enabled {
            let label = format!("Reset 2FA for {}", user.name);
            render(&IconButton {
                id: &format!("user-{}-reset-2fa", user.id),
                title: "Reset 2FA",
                event: "request_reset_two_factor",
                name: "id",
                value: &user.id.to_string(),
                ..IconButton::new("shield-check", &label)
            })?
        } else {
            Html::EMPTY
        };
        let impersonate = if grants.impersonate && !pending {
            let label = format!("Impersonate {}", user.name);
            let id = format!("user-{}-impersonate", user.id);
            let href = format!("/admin/impersonate/{}", user.id);
            let impersonating = scope.impersonator().is_some();
            if !own && !impersonating {
                render(&IconButton {
                    id: &id,
                    href: &href,
                    method: "post",
                    title: "Impersonate this user",
                    ..IconButton::new("impersonate", &label)
                })?
            } else {
                render(&IconButton {
                    id: &id,
                    disabled: true,
                    title: if own {
                        "You cannot impersonate yourself"
                    } else {
                        "Cannot impersonate while impersonating"
                    },
                    ..IconButton::new("impersonate", &label)
                })?
            }
        } else {
            Html::EMPTY
        };
        let revoke = if grants.invite && pending {
            let label = format!("Revoke {}'s invitation", user.name);
            let id = format!("user-{}-revoke", user.id);
            render(&IconButton {
                id: &id,
                kind: IconKind::Danger,
                title: "Revoke invitation",
                event: "request_revoke",
                name: "id",
                value: &user.id.to_string(),
                ..IconButton::new("delete", &label)
            })?
        } else {
            Html::EMPTY
        };
        let delete = if grants.delete && !pending {
            let label = format!("Remove {}", user.name);
            let id = format!("user-{}-delete", user.id);
            let value = user.id.to_string();
            if !own {
                render(&IconButton {
                    id: &id,
                    kind: IconKind::Danger,
                    title: "Remove user",
                    event: "request_delete",
                    name: "id",
                    value: &value,
                    ..IconButton::new("delete", &label)
                })?
            } else {
                render(&IconButton {
                    id: &id,
                    kind: IconKind::Danger,
                    disabled: true,
                    title: "You cannot remove your own membership",
                    ..IconButton::new("delete", &label)
                })?
            }
        } else {
            Html::EMPTY
        };
        render(&Actions {
            add_to,
            reset,
            impersonate,
            revoke,
            delete,
        })
    }
    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        self.columns.refresh_views(scope).await?;
        let grants = self.grants(scope).await?;
        let companies = self.visible_companies(scope).await?;
        let mut page = {
            let client = self.pages.db.get().await?;
            list_users(&**client, scope, &companies, &self.options()).await?
        };
        let cross_recovery = self
            .linked_recovery
            .filter(|(tenant, _)| *tenant != scope.tenant());
        if let Some((tenant, user)) = cross_recovery {
            let client = self.pages.db.get().await?;
            page.entries = super::query::recovery_entry(&**client, scope, tenant, user)
                .await?
                .into_iter()
                .collect();
            page.total_entries = page.entries.len() as u64;
        }
        let total_pages = page.total_entries.div_ceil(self.list.page_size);
        if self.list.clamp_to_last_page(total_pages, true) {
            let client = self.pages.db.get().await?;
            page = list_users(&**client, scope, &companies, &self.options()).await?;
        }
        let display = self.pages.display(scope).await?;
        let keys: Vec<Value> = page.entries.iter().map(|e| json!(e.id)).collect();
        // Grid bounds each projection to 100
        // ids, so a large page is composed from bounded batches.
        if cross_recovery.is_some() {
            self.columns.load(scope, &[], &display).await?;
            self.columns.rows = page
                .entries
                .iter()
                .map(|entry| FlexRow::new(entry.id.to_string()))
                .collect();
        } else if keys.is_empty() {
            self.columns.load(scope, &[], &display).await?;
        } else {
            let mut rows = Vec::with_capacity(keys.len());
            for batch in keys.chunks(bilibi_grid::query::MAX_ROWS) {
                self.columns.load(scope, batch, &display).await?;
                rows.append(&mut self.columns.rows);
            }
            self.columns.rows = rows;
        }
        let compact = self.columns.mode() == RowMode::Compact;
        // Where a staff account may go from here ("Add to team"): rows become
        // draggable and offer "Non-local actions" only when a page accepts one.
        let add_to = if cross_recovery.is_some() {
            Vec::new()
        } else {
            self.pages.records.add_to(scope, "user").await?
        };
        let cross_grants = Grants {
            view: false,
            invite: false,
            delete: false,
            impersonate: false,
            reset: grants.reset,
        };
        let mut rows = self.columns.rows.clone();
        for row in &mut rows {
            let Some(user) = page.entries.iter().find(|e| e.id.to_string() == row.key) else {
                continue;
            };
            row.dom_id = Some(format!("user-{}", user.id));
            let name = if cross_recovery.is_none()
                && grants.view
                && user.membership != Membership::Invited
            {
                render(&RecordLink {
                    workspace: self.framed,
                    kind: "core/user",
                    record_id: &user.id.to_string(),
                    href: &format!("/users/{}", user.id),
                    class: "font-medium text-ink-strong hover:underline",
                    body: Html::text(&user.name),
                })?
            } else {
                render(&PlainName { name: &user.name })?
            };
            row.slots.insert(
                "name".into(),
                render(&NameCell {
                    avatar: !compact,
                    initials: &initials(&user.name),
                    name,
                })?,
            );
            row.slots
                .insert("email".into(), render(&Muted { text: &user.email })?);
            row.slots.insert(
                "desks".into(),
                Html::text(user.desks.as_deref().unwrap_or("—")),
            );
            let (membership, kind) = match user.membership {
                Membership::Active => ("Active", BadgeKind::Neutral),
                Membership::Disabled => ("Disabled", BadgeKind::Neutral),
                Membership::Invited => ("Invited", BadgeKind::Success),
            };
            row.slots.insert(
                "membership".into(),
                render(&Badge {
                    kind,
                    dot: false,
                    class: "",
                    body: Html::text(membership),
                })?,
            );
            let mut badges = Vec::with_capacity(user.roles.len());
            for role in &user.roles {
                badges.push(render(&Badge {
                    kind: if role.is_system {
                        BadgeKind::Success
                    } else {
                        BadgeKind::Neutral
                    },
                    dot: false,
                    class: "",
                    body: Html::text(&role.name),
                })?);
            }
            row.slots
                .insert("roles".into(), render(&RolesCell { badges })?);
            row.slots.insert(
                "reset_requested".into(),
                match user.reset_requested_at {
                    Some(at) => {
                        let time = render(&CreatedCell {
                            id: &format!("user-reset-requested-{}", user.id),
                            i: Self::instant(&display, at)?,
                        })?;
                        render(&Badge {
                            kind: BadgeKind::Warning,
                            dot: false,
                            class: "",
                            body: {
                                let mut body = Html::text("2FA reset requested · ");
                                body.push(&time);
                                body
                            },
                        })?
                    }
                    None => Html::text("—"),
                },
            );
            row.slots.insert(
                "created_at".into(),
                render(&CreatedCell {
                    id: &format!("user-created-{}", user.id),
                    i: Self::instant(&display, user.created_at)?,
                })?,
            );
            if !add_to.is_empty() && user.membership != Membership::Invited {
                row.record = Some(("user".into(), row.key.clone()));
            }
            row.actions = self.actions(
                scope,
                if cross_recovery.is_some() {
                    &cross_grants
                } else {
                    &grants
                },
                user,
                &add_to,
            )?;
        }
        let mut actions = Html::EMPTY;
        if grants.invite {
            actions.push(&render(&CreateButton {
                noun: "User",
                href: "/users/invite",
                id: "user-invite",
                ..CreateButton::default()
            })?);
        }
        let header = render(&Header {
            title_actions: Html::EMPTY,
            title: Html::text("User Management"),
            subtitle: "",
            actions,
        })?;
        let form = self.list.filters_form();
        let roles = self.role_options(scope, &companies).await?;
        let mut controls = vec![
            ToolbarControl::search(
                "users-search",
                "search",
                "Search users",
                &form["search"],
                "Search by name or email...",
            ),
            ToolbarControl::select(
                "users-membership-filter",
                MEMBERSHIP,
                "Membership",
                self.membership
                    .map(Membership::database_value)
                    .unwrap_or(""),
                vec![
                    bilibi_ui::Choice::new("All memberships", ""),
                    bilibi_ui::Choice::new("Active", "active"),
                    bilibi_ui::Choice::new("Disabled", "disabled"),
                    bilibi_ui::Choice::new("Invited", "invited"),
                ],
            ),
            ToolbarControl::multi_select(
                MultiSelect::in_form(FILTERS_FORM, ROLE_IDS)
                    .id("users-role-filter")
                    .label("Roles")
                    .placeholder("All roles")
                    .selection_label(":count role selected|:count roles selected")
                    .options(
                        roles
                            .iter()
                            .map(|role| bilibi_ui::Choice::new(&role.name, role.id.to_string()))
                            .collect(),
                    )
                    .values(self.role_ids.iter().map(ToString::to_string).collect()),
            ),
        ];
        if grants.reset && scope.impersonator().is_none() {
            controls.push(ToolbarControl::select(
                "users-reset-requested-filter",
                RESET_REQUESTED,
                "Reset requested",
                if self.reset_requested { "open" } else { "" },
                vec![
                    bilibi_ui::Choice::new("All requests", ""),
                    bilibi_ui::Choice::new("Reset requested", "open"),
                ],
            ));
        }
        let toolbar = render(&FilterToolbar::new("users-filters", controls))?;
        let mut table = self
            .columns
            .flex_table("users", rows)
            .sorted(self.list.sort_by.as_deref(), self.list.sort_dir);
        table.caption = "Workspace users";
        table.framed = false;
        table.actions = grants.delete
            || grants.impersonate
            || grants.invite
            || grants.reset
            || !add_to.is_empty();
        if page.entries.is_empty() {
            table.empty = Some(TableEmpty::Content(Html::text("No users found.")));
        }
        let card = render(&Card {
            id: "users-card",
            inner_class: "p-0",
            body: render(&CardBody {
                table: render(&table)?,
                pagination: render(&Pagination::new(
                    "users-pagination",
                    PageCounts::new(self.list.page, self.list.page_size, page.total_entries),
                    &PAGE_SIZES,
                ))?,
            })?,
            ..Card::default()
        })?;
        let dialog = match (&self.pending_delete, &self.pending_revoke, &self.pending_reset) {
            (Some(entry), _, _) => render(&ConfirmDialog {
                id: "delete-user-confirm",
                consequence: &format!("{}'s membership in this workspace will be removed.", entry.name),
                detail: "Their portable identity and access to other workspaces remain available.",
                confirm: "Remove",
                working: "Removing…",
                confirm_event: "delete",
                cancel_event: "cancel_delete",
            })?,
            (_, Some(entry), _) => render(&ConfirmDialog {
                id: "revoke-invitation-confirm",
                consequence: &format!("{}'s invitation will be revoked.", entry.name),
                detail: "The invitation link will stop working and the pending membership will be removed.",
                confirm: "Revoke",
                working: "Revoking…",
                confirm_event: "revoke",
                cancel_event: "cancel_revoke",
            })?,
            (_, _, Some((entry, _))) => self.pages.users.two_factor_reset_dialog(scope, &entry.name, self.reset_error.as_deref()).await?,
            (None, None, None) => Html::EMPTY,
        };
        let rows = page.entries.len();
        self.listed = page.entries;
        let html = render(&bilibi_ui::Page {
            id: "users-index",
            body: render(&Body {
                flash: render(&FlashGroup {
                    id: "flash-group",
                    flash: self.flash.map(),
                })?,
                header,
                toolbar,
                card,
                dialog,
            })?,
            ..bilibi_ui::Page::default()
        })?
        .to_string();
        let query = self.query();
        Ok(LiveBody {
            html,
            query,
            rows,
            ..LiveBody::default()
        })
    }
    fn query(&self) -> HashMap<String, String> {
        let mut query = self.list.to_params();
        if !self.role_ids.is_empty() {
            query.insert(
                ROLE_IDS.into(),
                self.role_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        if let Some(membership) = self.membership {
            query.insert(MEMBERSHIP.into(), membership.database_value().into());
        }
        if self.reset_requested {
            query.insert(RESET_REQUESTED.into(), "open".into());
        }
        if let Some((tenant, user)) = self.linked_recovery {
            query.insert("recoveryTenant".into(), tenant.to_string());
            query.insert("recoveryAccount".into(), user.to_string());
        }
        query.extend(self.columns.params());
        query
    }
    fn listed(&self, value: &Value) -> Option<&Entry> {
        let id = params::positive_id(value.get("id").and_then(Value::as_str))
            .or_else(|| value.get("id").and_then(Value::as_i64))?;
        self.listed.iter().find(|entry| entry.id == id)
    }
    async fn handle(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        match event(value)? {
            "filters" => {
                let filters = params::form_values_checked(value, FILTERS_FORM, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.list.apply_filters(&filters);
                self.list.search = bounded_search(&self.list.search);
                // The multi-select posts a hidden `""` beside its checked
                // values, so an empty selection clears the filter.
                if let Some(posted) = value
                    .get(format!("{FILTERS_FORM}[{ROLE_IDS}][]"))
                    .and_then(Value::as_array)
                {
                    self.role_ids = normalize_role_ids(posted.iter().filter_map(Value::as_str));
                    self.list.page = 1;
                }
                self.membership = filters
                    .get(MEMBERSHIP)
                    .and_then(|value| Membership::parse(value));
                self.reset_requested = filters
                    .get(RESET_REQUESTED)
                    .is_some_and(|value| value == "open");
            }
            "page" => self
                .list
                .put_page(value.get("page").and_then(Value::as_str)),
            "sort" => {
                self.list
                    .next_sort(value.get("sort").and_then(Value::as_str).unwrap_or(""));
            }
            "grid" => {
                let query = self.query();
                if let Outcome::Sort(key) = self
                    .columns
                    .handle(
                        scope,
                        &PageColumns::event_values(value)?,
                        bilibi_grid::ViewCaller {
                            contracts: &self.pages.contracts,
                            credential,
                            service: super::SERVICE,
                            event: "grid",
                            query,
                        },
                    )
                    .await?
                {
                    self.list.next_sort(&key);
                }
            }
            "request_delete" => {
                self.flash.0 = None;
                if !self.pages.allowed(scope, contributions::DELETE).await? {
                    self.flash
                        .error("You do not have permission to remove users.");
                } else if params::positive_id(value.get("id").and_then(Value::as_str))
                    == Some(scope.user())
                {
                    self.flash.error("You cannot remove your own membership.");
                } else if let Some(entry) = self.listed(value).cloned() {
                    self.pending_delete = Some(entry);
                    self.pending_revoke = None;
                    self.pending_reset = None;
                } else {
                    self.flash.error("That user no longer exists.");
                }
            }
            "cancel_delete" => self.pending_delete = None,
            "request_revoke" => {
                self.flash.0 = None;
                if !self.pages.allowed(scope, contributions::INVITE).await? {
                    self.flash
                        .error("You do not have permission to revoke invitations.");
                } else if let Some(entry) = self
                    .listed(value)
                    .filter(|entry| {
                        entry.membership == Membership::Invited && entry.invitation.is_some()
                    })
                    .cloned()
                {
                    self.pending_revoke = Some(entry);
                    self.pending_delete = None;
                    self.pending_reset = None;
                } else {
                    self.flash.error("That invitation no longer exists.");
                }
            }
            "cancel_revoke" => self.pending_revoke = None,
            "request_reset_two_factor" => {
                self.flash.0 = None;
                if !self.pages.allowed(scope, crate::RESET_TWO_FACTOR).await? {
                    self.flash.error("You do not have permission to reset 2FA.");
                } else if let Some(entry) = self
                    .listed(value)
                    .filter(|entry| entry.can_reset && entry.two_factor_enabled)
                    .cloned()
                {
                    let tenant = self
                        .linked_recovery
                        .filter(|(_, linked)| *linked == entry.id)
                        .map(|(tenant, _)| tenant)
                        .unwrap_or_else(|| scope.tenant());
                    self.pending_reset = Some((entry, tenant));
                    self.reset_error = None;
                    self.pending_delete = None;
                    self.pending_revoke = None;
                } else {
                    self.flash.error("That user no longer exists.");
                }
            }
            "cancel_reset_two_factor" => {
                self.pending_reset = None;
                self.reset_error = None;
            }
            "reset_two_factor" => {
                let Some((entry, tenant)) = self.pending_reset.clone() else {
                    return Ok(());
                };
                let proof = value
                    .get("proof")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if proof.is_empty() {
                    self.reset_error =
                        Some("Enter the current code from your authenticator app.".into());
                    return Ok(());
                }
                let presented = scope.clone().presenting_step_up(proof);
                let call = bilibi_agent_api::page::Call {
                    scope: &presented,
                    credential,
                    service: super::SERVICE,
                    event: "reset_two_factor",
                    contract: crate::two_factor::recovery::RESET,
                };
                match self
                    .pages
                    .contracts
                    .submit(&call, json!({"tenant_id":tenant,"user_id":entry.id}))
                    .await
                {
                    Ok(_) => {
                        self.pending_reset = None;
                        self.reset_error = None;
                        self.flash
                            .success(format!("{}'s 2FA was reset.", entry.name));
                    }
                    Err(bilibi_agent_api::Error::Backend(error @ Error::StepUpRequired { .. })) => {
                        let Error::StepUpRequired { reason, .. } = error else {
                            unreachable!()
                        };
                        match reason {
                            "not_enrolled" => return Err(error),
                            "invalid_code" => self.reset_error = Some("That code was not accepted. Enter the current code from your authenticator app.".into()),
                            "locked" => self.reset_error = Some("Too many wrong codes. Wait a while, then try again with a fresh code.".into()),
                            _ => self.reset_error = Some("Enter the current code from your authenticator app.".into()),
                        }
                    }
                    Err(bilibi_agent_api::Error::Backend(Error::Forbidden))
                    | Err(bilibi_agent_api::Error::Forbidden) => {
                        self.pending_reset = None;
                        self.flash.error("This account cannot be reset by you.")
                    }
                    Err(bilibi_agent_api::Error::Backend(error)) => return Err(error),
                    Err(_) => self.flash.error(format!(
                        "{}'s 2FA was not reset. Reload the page and try again.",
                        entry.name
                    )),
                }
            }
            "revoke" => {
                let Some(entry) = self.pending_revoke.take() else {
                    return Ok(());
                };
                let Some(invitation) = entry.invitation else {
                    return Ok(());
                };
                match self
                    .pages
                    .users
                    .revoke_member_invitation(scope, invitation)
                    .await
                {
                    Ok(()) => self
                        .flash
                        .success(format!("{}'s invitation was revoked.", entry.name)),
                    Err(InvitationError::Access(Error::Forbidden)) => self
                        .flash
                        .error("You do not have permission to revoke invitations."),
                    Err(InvitationError::Link) => {
                        self.flash.error("That invitation no longer exists.")
                    }
                    Err(InvitationError::Access(error)) => return Err(error),
                    Err(_) => self.flash.error(format!(
                        "{}'s invitation was not revoked. Reload the page and try again.",
                        entry.name
                    )),
                }
            }
            "delete" => {
                let Some(entry) = self.pending_delete.take() else {
                    return Ok(());
                };
                match self.pages.users.remove_membership(scope, entry.id).await {
                    Ok(()) => self
                        .flash
                        .success(format!("{}'s membership was removed.", entry.name)),
                    Err(AccountError::Access(Error::Forbidden)) => self
                        .flash
                        .error("You do not have permission to remove users."),
                    Err(AccountError::OwnAccount) => {
                        self.flash.error("You cannot remove your own membership.")
                    }
                    Err(AccountError::Outranked) => self.flash.error(format!(
                        "You cannot remove {}: they have permissions you do not have.",
                        entry.name
                    )),
                    Err(AccountError::Impersonating) => self
                        .flash
                        .error("Accounts cannot be changed while viewing as someone else."),
                    Err(AccountError::Archived) => self.flash.error(format!(
                        "{} was not removed because their company is archived.",
                        entry.name
                    )),
                    Err(AccountError::NotFound) => self.flash.error("That user no longer exists."),
                    Err(AccountError::Access(error)) => return Err(error),
                    Err(_) => self.flash.error(format!(
                        "{} was not removed. Reload the page and try again.",
                        entry.name
                    )),
                }
            }
            "clear-flash" => self.flash.dismiss(value.get("key").and_then(Value::as_str)),
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
}
impl LiveState<Scope, Error> for Index {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(query) => {
                    self.list =
                        ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?;
                    self.read_params(query);
                    self.columns.from_params(request.scope, query).await?;
                }
                LiveInput::Event(value) => {
                    self.handle(request.scope, request.credential, value)
                        .await?
                }
            }
            self.view(request.scope).await
        })
    }
}

/// The built-in columns, in their order, labels, types and sort ids.
fn builtins() -> Vec<BuiltinColumn> {
    let mut created = BuiltinColumn::new("created_at", "Created").sortable("created_at");
    created.kind = ColumnType::Datetime;
    created.sort_id = Some("users-sort-created".into());
    let mut name = BuiltinColumn::new("name", "Name").sortable("name");
    name.sort_id = Some("users-sort-name".into());
    let mut email = BuiltinColumn::new("email", "Email").sortable("email");
    email.sort_id = Some("users-sort-email".into());
    let mut company = BuiltinColumn::new("desks", "Desks").sortable("desks");
    company.sort_id = Some("users-sort-company".into());
    vec![
        name,
        email,
        company,
        BuiltinColumn::new("roles", "Roles"),
        BuiltinColumn::new("membership", "Membership"),
        BuiltinColumn::new("reset_requested", "2FA Reset"),
        created,
    ]
}

fn agent_columns<'a>(
    _client: &'a tokio_postgres::Client,
    _scope: &'a Scope,
    views: Arc<bilibi_grid::PageViews>,
) -> bilibi_grid::PageColumnsFuture<'a> {
    Box::pin(
        async move { PageColumns::mount(views, "users", builtins()).map_err(|_| Error::Internal) },
    )
}

bilibi_grid::contribute_page_metadata!(
    "core/user",
    "core/user.grid-page",
    "users",
    None,
    crate::contributions::LIST,
    Some(|| SPEC.clone()),
    &[
        (
            "roleIds",
            "Comma-separated role IDs; any selected role matches"
        ),
        (
            "resetRequested",
            "Use open to show accounts with an open 2FA reset request"
        )
    ],
    None,
    agent_columns
);
