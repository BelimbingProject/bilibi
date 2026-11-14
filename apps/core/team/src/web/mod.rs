//! `/teams` and `/teams/{id}`: one list, one record page, one create dialog.
//! Every write goes through the operation contracts (`PageContracts`), the
//! same ones agents and the "Add to team" action reach.
mod dialogs;
mod list;
mod record;
use crate::{LeadAbove, Member, Team, TeamRow, Teams, MANAGE, PAGES};
use askama::Template;
use bilibi_agent_api::page::{Call, PageContracts};
use bilibi_database::{Db, Error, Result};
use bilibi_grid::{PageColumns, PageViews};
use bilibi_picklists::PickLists;
use bilibi_records::Records;
use bilibi_tenancy::{PartyProofs, Scope};
use bilibi_ui::{page::*, *};
use bilibi_user::{StaffChoice, UserService};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, LazyLock},
};

/// The drop target of the add-members review (`lib.rs`).
pub(crate) const ADD_TARGET: &str = "team.add_members";
const SIZES: [u64; 3] = [25, 50, 100];
/// How many sub-teams the record page shows before linking to the list.
const SUB_LIMIT: i64 = 25;
static LIST_SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: SIZES.to_vec(),
        filters: [
            ("kind".into(), Filter::string("")),
            ("parent".into(), Filter::string("")),
        ]
        .into(),
        omit_defaults: true,
        omit_blank: vec!["search".into(), "kind".into(), "parent".into()],
        ..ListSpec::default()
    }
    .shared()
    .expect("team list spec")
});
static MEMBER_SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: SIZES.to_vec(),
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("team member spec")
});

#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub teams: Arc<Teams>,
    pub users: Arc<UserService>,
    pub picklists: Arc<PickLists>,
    pub parties: Arc<PartyProofs>,
    pub records: Arc<Records>,
    pub contracts: Arc<PageContracts>,
    pub views: Arc<PageViews>,
    pub display: Arc<bilibi_datetime::DisplayPolicy>,
    pub locales: Arc<bilibi_locale::Locales>,
}
impl Pages {
    fn state(&self, r: &PageRequest<'_, Scope>) -> Result<State> {
        let id = match r.operation {
            "record" => Some(
                r.path
                    .get("id")
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &i64| *v > 0)
                    .ok_or(Error::NotFound)?,
            ),
            "list" => None,
            _ => return Err(Error::NotFound),
        };
        let spec = if id.is_some() {
            MEMBER_SPEC.clone()
        } else {
            LIST_SPEC.clone()
        };
        let (table, builtins) = if id.is_some() {
            ("team-members", record::member_columns())
        } else {
            ("teams", list::columns())
        };
        Ok(State {
            pages: self.clone(),
            id,
            framed: r.framed,
            list: ListState::parse(r.query, spec).map_err(|_| Error::Invalid)?,
            columns: PageColumns::mount(self.views.clone(), table, builtins)
                .map_err(|_| Error::Internal)?,
            sub_columns: PageColumns::mount(self.views.clone(), "team-subteams", list::columns())
                .map_err(|_| Error::Internal)?,
            rows: Vec::new(),
            total: 0,
            record: None,
            subs: Vec::new(),
            sub_total: 0,
            selection: Selection {
                event: "select".into(),
                noun: "members".into(),
                ..Selection::default()
            },
            page_keys: Vec::new(),
            can_create: false,
            kinds: Vec::new(),
            team_choices: Vec::new(),
            create: None,
            add: None,
            removing: Vec::new(),
            flash: None,
            redirect: None,
            commits: CommitStates::default(),
            reload: true,
            initial_query: Some(r.query.clone()),
            drop: r.query.get(bilibi_records::DROP_PARAM).cloned(),
        })
    }
}
impl bilibi_ui::page::Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        Some(if operation == "list" { "Teams" } else { "Team" })
    }
    fn render<'a>(&'a self, r: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = r.scope.ok_or(Error::Unauthorized)?;
            let mut state = self.state(&r)?;
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
                title: state.record.as_ref().map(|r| r.team.name.clone()),
            })
        })
    }
    fn mount<'a>(&'a self, r: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            Ok(Some(
                Box::new(self.state(&r)?) as Box<dyn LiveState<Scope, Error>>
            ))
        })
    }
}

/// What the record page reads about its team.
pub(crate) struct Record {
    team: Team,
    parent_name: Option<String>,
    company_name: Option<String>,
    kind_label: String,
    members: Vec<Member>,
    member_total: u64,
    /// Members whose account is no longer active staff.
    inactive: BTreeSet<i64>,
    leads: Vec<Member>,
    above: Vec<LeadAbove>,
    /// The reader may change this team now.
    can_change: bool,
    archived: bool,
    companies: Vec<Choice>,
    history: bool,
    /// When each member on the page joined, as the reader reads dates.
    added: BTreeMap<i64, String>,
}
/// The staff chosen in a dialog, kept on the server across searches.
#[derive(Default)]
pub(crate) struct Picker {
    selected: Vec<(i64, String)>,
    query: String,
    hits: Vec<StaffChoice>,
}
#[derive(Default)]
pub(crate) struct CreateDraft {
    values: HashMap<String, String>,
    /// The companies the reader works in, offered for the team's company.
    companies: Vec<Choice>,
    leads: Picker,
    errors: BTreeMap<String, Vec<String>>,
}
#[derive(Default)]
pub(crate) struct AddDraft {
    /// The team the people join; chosen in the dialog on the list page.
    team: Option<i64>,
    people: Picker,
    errors: BTreeMap<String, Vec<String>>,
}
pub(crate) struct State {
    pages: Pages,
    id: Option<i64>,
    framed: bool,
    /// The teams list's state, or the members table's paging on a record.
    list: ListState,
    columns: PageColumns,
    sub_columns: PageColumns,
    rows: Vec<TeamRow>,
    total: u64,
    record: Option<Record>,
    subs: Vec<TeamRow>,
    sub_total: i64,
    selection: Selection,
    page_keys: Vec<String>,
    can_create: bool,
    kinds: Vec<Choice>,
    team_choices: Vec<(i64, String)>,
    create: Option<CreateDraft>,
    add: Option<AddDraft>,
    /// Members waiting for the remove confirmation.
    removing: Vec<i64>,
    flash: Option<(StatusKind, String)>,
    redirect: Option<(String, String)>,
    commits: CommitStates,
    reload: bool,
    initial_query: Option<HashMap<String, String>>,
    /// Staff carried here by "Add to team" (`?drop=`), opened once.
    drop: Option<String>,
}
pub(crate) fn render(t: impl Template) -> Result<Html> {
    Html::render(&t).map_err(|_| Error::Internal)
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}
fn form_values(v: &Value, ns: &str) -> Result<HashMap<String, String>> {
    params::form_values_checked(v, ns, params::FIELD_LIMIT).map_err(Error::refused)
}
fn permitted<T>(result: Result<T>) -> Result<bool> {
    match result {
        Ok(_) => Ok(true),
        Err(Error::Forbidden | Error::NotFound) => Ok(false),
        Err(e) => Err(e),
    }
}
pub(crate) fn summary(prefix: &str, errors: &BTreeMap<String, Vec<String>>) -> Result<Html> {
    let mut summary = ErrorSummary::new(format!("{prefix}-errors"));
    for (field, messages) in errors {
        for message in messages {
            summary.push(
                format!("{prefix}-{}", field.replace('_', "-")),
                error_sentence(&label_of(field), message),
            );
        }
    }
    render(summary)
}
fn label_of(field: &str) -> String {
    match field {
        "parent_id" => "Parent team".into(),
        "company_id" => "Company".into(),
        "user_ids" => "People".into(),
        other => {
            let mut s = other.replace('_', " ");
            if let Some(first) = s.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            s
        }
    }
}

impl State {
    async fn load(&mut self, scope: &Scope) -> Result<()> {
        if let Some(query) = self.initial_query.take() {
            self.columns.from_params(scope, &query).await?;
        }
        let connection = self.pages.db.get().await?;
        let client = &**connection;
        let locale = self
            .pages
            .locales
            .resolve(client, scope, None)
            .await?
            .locale;
        let display = self
            .pages
            .display
            .display(client, scope, None, locale)
            .await?;
        let held = bilibi_authz::effective_capabilities(client, scope).await?;
        self.can_create = held.contains(MANAGE);
        self.kinds = self
            .pages
            .picklists
            .choices(client, scope, crate::KINDS, None)
            .await?;
        self.team_choices = self.pages.teams.choices(client, scope).await?;
        let limit = self.list.page_size as i64;
        if let Some(id) = self.id {
            let before: Vec<i64> = self
                .record
                .as_ref()
                .map(|r| r.members.iter().map(|m| m.user_id).collect())
                .unwrap_or_default();
            let record = record::read(
                &self.pages,
                client,
                scope,
                id,
                &mut self.list,
                limit,
                &held,
                &display,
            )
            .await?;
            self.reload |= before != record.members.iter().map(|m| m.user_id).collect::<Vec<_>>()
                || self.record.as_ref().is_none_or(|r| r.team != record.team);
            self.page_keys = record
                .members
                .iter()
                .map(|m| m.user_id.to_string())
                .collect();
            self.record = Some(record);
            let (subs, sub_total) = self
                .pages
                .teams
                .list(
                    client,
                    scope,
                    &crate::TeamQuery {
                        search: "",
                        kind: "",
                        parent: Some(id),
                        limit: SUB_LIMIT,
                        offset: 0,
                    },
                )
                .await?;
            self.subs = subs;
            self.sub_total = sub_total;
        } else {
            let parent = self.list.filters["parent"].parse::<i64>().ok();
            let query = |offset| crate::TeamQuery {
                search: &self.list.search,
                kind: &self.list.filters["kind"],
                parent,
                limit,
                offset,
            };
            let (rows, total) = self
                .pages
                .teams
                .list(client, scope, &query((self.list.page as i64 - 1) * limit))
                .await?;
            let (rows, total) = if self
                .list
                .clamp_to_last_page((total as u64).div_ceil(self.list.page_size), true)
            {
                self.reload = true;
                let offset = (self.list.page as i64 - 1) * limit;
                self.pages
                    .teams
                    .list(
                        client,
                        scope,
                        &crate::TeamQuery {
                            search: &self.list.search,
                            kind: &self.list.filters["kind"],
                            parent,
                            limit,
                            offset,
                        },
                    )
                    .await?
            } else {
                (rows, total)
            };
            self.reload |= self
                .rows
                .iter()
                .map(|r| (r.team.id, r.team.version, r.members))
                .ne(rows.iter().map(|r| (r.team.id, r.team.version, r.members)));
            self.rows = rows;
            self.total = total as u64;
        }
        drop(connection);
        if self.reload {
            let keys: Vec<Value> = match &self.record {
                Some(r) => r.members.iter().map(|m| m.user_id.into()).collect(),
                None => self.rows.iter().map(|r| r.team.id.into()).collect(),
            };
            self.columns.load(scope, &keys, &display).await?;
            let subs: Vec<Value> = self.subs.iter().map(|r| r.team.id.into()).collect();
            self.sub_columns.load(scope, &subs, &display).await?;
            self.reload = false;
        }
        Ok(())
    }
    /// Calls one operation contract; a refusal becomes the page's message
    /// and, for a dialog, its field errors.
    async fn submit(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
        contract: &'static str,
        input: Value,
    ) -> Result<std::result::Result<Value, BTreeMap<String, Vec<String>>>> {
        let result = self
            .pages
            .contracts
            .submit(
                &Call {
                    scope,
                    credential,
                    event,
                    contract,
                    service: PAGES,
                },
                input,
            )
            .await;
        self.reload = true;
        let message = match result {
            Ok(v) => return Ok(Ok(v)),
            Err(bilibi_agent_api::Error::Backend(
                e @ (Error::Unauthorized | Error::StepUpRequired { .. }),
            )) => return Err(e),
            Err(bilibi_agent_api::Error::InvalidInput(fields)) => {
                let message = fields
                    .iter()
                    .flat_map(|(k, vs)| vs.iter().map(move |v| error_sentence(&label_of(k), v)))
                    .collect::<Vec<_>>()
                    .join(" ");
                self.flash = Some((StatusKind::Error, message));
                return Ok(Err(fields));
            }
            Err(
                bilibi_agent_api::Error::Forbidden
                | bilibi_agent_api::Error::Backend(Error::Forbidden),
            ) => "You do not have permission to change this team.".to_owned(),
            Err(
                bilibi_agent_api::Error::NotFound
                | bilibi_agent_api::Error::Backend(Error::NotFound),
            ) => "This team or person no longer exists. Reload the page.".to_owned(),
            Err(bilibi_agent_api::Error::Backend(Error::Conflict)) => {
                "Someone else changed this team at the same moment. Try again.".to_owned()
            }
            Err(e) => e.to_string(),
        };
        self.flash = Some((StatusKind::Error, message.clone()));
        Ok(Err([("form".to_owned(), vec![message])].into()))
    }
    async fn event(&mut self, scope: &Scope, credential: &str, v: &Value) -> Result<()> {
        match text(v, "type") {
            "filters" => {
                self.list
                    .apply_filters(&form_values(v, bilibi_ui::list_state::FILTERS_FORM)?);
                self.selection.clear();
                self.reload = true;
            }
            "page" => {
                self.list.put_page(v["page"].as_str());
                self.reload = true;
            }
            "grid" | "grid_subs" => {
                let params = params::string_values_checked(v, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let columns = if text(v, "type") == "grid" {
                    &mut self.columns
                } else {
                    &mut self.sub_columns
                };
                if matches!(
                    columns.handle(scope, &params).await?,
                    bilibi_grid::Outcome::Patch
                ) {
                    self.reload = true;
                }
            }
            "clear-flash" => self.flash = None,
            "select" => self.selection.apply(
                v["op"].as_str().unwrap_or(""),
                v["key"].as_str(),
                &self.page_keys,
            ),
            bilibi_records::DROP_EVENT => {
                let records = text(v, "records").to_owned();
                self.receive_drop(scope, &records).await?;
            }
            event => return self.dialog_event(scope, credential, event, v).await,
        }
        Ok(())
    }
    /// Staff dropped on the Teams tile, or carried by "Add to team": the
    /// same add-members review as "+ Add members", with them chosen.
    async fn receive_drop(&mut self, scope: &Scope, text: &str) -> Result<()> {
        if !self.can_create {
            self.flash = Some((
                StatusKind::Error,
                "You do not have permission to change teams.".into(),
            ));
            return Ok(());
        }
        match self.pages.records.receive(scope, ADD_TARGET, text).await? {
            Ok(dropped) => {
                let mut draft = self.add.take().unwrap_or_default();
                draft.team = draft.team.or(self.id);
                for record in dropped {
                    if let (Ok(id), Some(name)) = (record.id.parse::<i64>(), record.label) {
                        if !draft.people.selected.iter().any(|(s, _)| *s == id) {
                            draft.people.selected.push((id, name));
                        }
                    }
                }
                self.add = Some(draft);
                self.flash = None;
                self.search_staff(scope, String::new()).await?;
            }
            Err(refusal) => self.flash = Some((StatusKind::Error, refusal.message())),
        }
        Ok(())
    }
    fn view(&self) -> Result<String> {
        let (header, sections, variant) = match &self.record {
            Some(record) => record::view(self, record)?,
            None => list::view(self)?,
        };
        let mut dialog = Html::EMPTY;
        if let Some(draft) = &self.create {
            dialog.push(&dialogs::create(self, draft)?);
        }
        if let Some(draft) = &self.add {
            dialog.push(&dialogs::add(self, draft)?);
        }
        if !self.removing.is_empty() {
            dialog.push(&dialogs::remove(self)?);
        }
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                StatusKind::Error => flash.error = message,
                StatusKind::Warning => flash.warning = message,
                StatusKind::Info => flash.info = message,
            }
        }
        let id = self.id.map(|id| id.to_string()).unwrap_or_default();
        let html = View {
            header,
            sections,
            dialog,
            flash,
            record_id: &id,
            variant,
        }
        .render()
        .map_err(|_| Error::Internal)?;
        // The list takes dropped staff only while its add action shows.
        let zone = self
            .pages
            .records
            .drop_target(ADD_TARGET)
            .filter(|_| self.can_create && self.id.is_none())
            .map(|target| target.zone());
        Ok(match zone {
            Some(zone) => zone.wrap("teams-drop", Html::raw(html)).to_string(),
            None => html,
        })
    }
}
impl LiveState<Scope, Error> for State {
    fn panels(&self) -> Vec<PanelSpec> {
        match &self.record {
            Some(record) if record.history => vec![PanelSpec {
                id: "team-record-history".into(),
                embed: "audit.record-history".into(),
                path: [
                    ("panel".into(), "team-record-history".into()),
                    ("kinds".into(), "team,team_member".into()),
                    ("id".into(), record.team.id.to_string()),
                ]
                .into(),
                query: HashMap::new(),
            }],
            _ => Vec::new(),
        }
    }
    fn update<'a>(&'a mut self, r: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            // Refresh permissions before processing a stale page's events.
            self.load(r.scope).await?;
            match r.input {
                LiveInput::Refresh => {}
                LiveInput::Params(v) => {
                    let spec = if self.id.is_some() {
                        MEMBER_SPEC.clone()
                    } else {
                        LIST_SPEC.clone()
                    };
                    self.list = ListState::parse(v, spec).map_err(|_| Error::Invalid)?;
                    self.initial_query = Some(v.clone());
                    if let Some(drop) = v.get(bilibi_records::DROP_PARAM) {
                        self.drop = Some(drop.clone());
                    }
                    self.reload = true;
                }
                LiveInput::Event(v) => self.event(r.scope, r.credential, v).await?,
            };
            if let Some(drop) = self.drop.take() {
                self.receive_drop(r.scope, &drop).await?;
            }
            if let Some((path, message)) = self.redirect.take() {
                return Ok(LiveBody {
                    redirect: Some(path),
                    flash: Some((StatusKind::Success, message)),
                    ..LiveBody::default()
                });
            }
            self.load(r.scope).await?;
            Ok(LiveBody {
                html: self.view()?,
                query: {
                    let mut q = self.list.to_params();
                    q.extend(self.columns.params());
                    q
                },
                ..LiveBody::default()
            })
        })
    }
}
#[derive(Template)]
#[template(path = "team_page.html")]
struct View<'a> {
    header: Html,
    sections: Vec<Html>,
    dialog: Html,
    flash: FlashMap<'a>,
    record_id: &'a str,
    variant: PageVariant,
}
