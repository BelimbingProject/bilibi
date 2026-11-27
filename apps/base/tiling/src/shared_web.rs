//! The `/workspace/shared-layouts` screen: a company's published workspaces and their
//! role audiences.
//!
//! The route mounts under `ui.workspace.publish`. Publishing and deleting are
//! [`Layouts`] writes that take the sealed scope and check that same
//! capability; these handlers only present the outcome. The tree to publish
//! arrives in `?t=` from the workspace's "Share current tiles" link, so the
//! form only renders when there is one; "Delete" opens the confirmation and
//! writes nothing until it is confirmed.
use crate::agent;
use crate::layouts::{self, Entry, RoleOption};
use crate::web::{audience, columns, Core, SharedActions, WORKSPACE};
use askama::Template;
use bilibi_agent_api::{page::Call, Error as ContractError};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    lists::EmptyState,
    page::{LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState},
    Button, Choice, Field, FlashMap, FlexRow, FlexTable, Html, ListColumns, MultiSelect,
    StatusKind, TableEmpty, NO_FLASH,
};
use serde::Deserialize;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};

const COLUMNS: &[(&str, &str)] = &[("label", "Label"), ("audience", "Audience")];

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    PublishLayout {
        #[serde(rename = "layout[label]")]
        label: String,
        #[serde(rename = "layout[tree]", default)]
        tree: String,
        #[serde(rename = "layout[roles][]", default)]
        roles: Vec<String>,
    },
    DeleteLayout {
        slug: String,
    },
    CancelDeleteLayout,
    ConfirmDeleteLayout,
    Grid {
        #[serde(flatten)]
        params: HashMap<String, String>,
    },
    #[serde(rename = "clear-flash")]
    ClearFlash {
        key: String,
    },
}

pub(crate) struct SharedPage {
    /// The encoded tree to publish, when the address carries a readable one.
    tree: String,
    label: String,
    roles: Vec<String>,
    entries: Vec<Entry>,
    role_options: Vec<RoleOption>,
    columns: ListColumns,
    pending_delete: Option<(i64, Entry)>,
    flash: Option<(StatusKind, String)>,
}

impl SharedPage {
    pub(crate) async fn load(
        core: &Core,
        scope: &Scope,
        query: &HashMap<String, String>,
    ) -> Result<Self> {
        let tree = query
            .get("t")
            .and_then(|tree| layouts::clean_tree(tree))
            .unwrap_or_default();
        let client = core.db.get().await?;
        let entries = core.layouts.shared(&**client, scope).await?;
        let role_options = layouts::role_options(&**client, scope).await?;
        Ok(Self {
            tree,
            label: String::new(),
            roles: Vec::new(),
            entries,
            role_options,
            columns: columns("shared-workspaces", COLUMNS)?,
            pending_delete: None,
            flash: None,
        })
    }

    async fn reload(&mut self, core: &Core, scope: &Scope) -> Result<()> {
        let client = core.db.get().await?;
        self.entries = core.layouts.shared(&**client, scope).await?;
        self.role_options = layouts::role_options(&**client, scope).await?;
        Ok(())
    }

    fn flash(&mut self, kind: StatusKind, message: impl Into<String>) {
        self.flash = Some((kind, message.into()));
    }

    async fn event(
        &mut self,
        core: &Core,
        scope: &Scope,
        credential: &str,
        event: Event,
    ) -> Result<()> {
        match event {
            Event::PublishLayout { label, tree, roles } => {
                let roles: Vec<String> = roles.into_iter().filter(|r| !r.is_empty()).collect();
                self.label = label.clone();
                self.roles = roles.clone();
                let tree = if tree.is_empty() { &self.tree } else { &tree };
                let name = layouts::clean_label(&label);
                let current = self
                    .entries
                    .iter()
                    .find(|entry| Some(&entry.label) == name.as_ref());
                let version = agent::version(current);
                let slug = name
                    .as_deref()
                    .map(|name| layouts::publish_slug(name, current, &self.entries))
                    .unwrap_or_default();
                let company = scope.current_company().ok_or(Error::Forbidden)?;
                let call = Call {
                    scope,
                    credential,
                    service: "base/tiling.pages",
                    event: "publish_layout",
                    contract: agent::PUBLISH,
                };
                match core.contracts.submit(&call, json!({"company_id":company.get(),"label":label,"slug":slug,"tree":tree,"roles":roles,"version":version})).await {
                    Ok(_) => {
                        self.reload(core, scope).await?;
                        self.flash(StatusKind::Success, "Shared the workspace.");
                        self.label.clear();
                        self.roles.clear();
                    }
                    Err(ContractError::Backend(Error::Unauthorized)) => return Err(Error::Unauthorized),
                    Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                        self.flash(StatusKind::Error, "You cannot share workspaces.");
                    }
                    Err(ContractError::InvalidInput(fields)) => {
                        let message = fields.into_values().flatten().collect::<Vec<_>>().join(". ");
                        self.flash(StatusKind::Error, message);
                    }
                    Err(ContractError::Backend(Error::Conflict)) => {
                        self.reload(core, scope).await?;
                        self.flash(StatusKind::Error, "The workspace changed. Review it and try again.");
                    }
                    Err(_) => self.flash(StatusKind::Error, "The workspace could not be shared."),
                }
            }
            Event::DeleteLayout { slug } => {
                if let Some(entry) = self.entries.iter().find(|entry| entry.slug == slug) {
                    let company = scope.current_company().ok_or(Error::Forbidden)?;
                    self.pending_delete = Some((company.get(), entry.clone()));
                }
            }
            Event::CancelDeleteLayout => self.pending_delete = None,
            Event::ConfirmDeleteLayout => {
                let Some((company, entry)) = self.pending_delete.take() else {
                    self.flash(StatusKind::Error, "You cannot manage shared workspaces.");
                    return Ok(());
                };
                let version = agent::version(Some(&entry));
                let call = Call {
                    scope,
                    credential,
                    service: "base/tiling.pages",
                    event: "confirm_delete_layout",
                    contract: agent::WITHDRAW,
                };
                match core
                    .contracts
                    .submit(
                        &call,
                        json!({"company_id":company,"slug":entry.slug,"version":version}),
                    )
                    .await
                {
                    Ok(_) => self.reload(core, scope).await?,
                    Err(ContractError::Backend(Error::Unauthorized)) => {
                        return Err(Error::Unauthorized)
                    }
                    Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                        self.flash(StatusKind::Error, "You cannot manage shared workspaces.");
                    }
                    Err(ContractError::Backend(Error::Conflict) | ContractError::NotFound) => {
                        self.reload(core, scope).await?;
                        self.flash(
                            StatusKind::Error,
                            "The workspace changed. Review it and try again.",
                        );
                    }
                    Err(_) => {
                        self.flash(
                            StatusKind::Error,
                            "The shared workspace could not be deleted.",
                        );
                    }
                }
            }
            Event::Grid { params } => {
                let _ = self.columns.handle(&params);
            }
            Event::ClearFlash { key } => {
                if self
                    .flash
                    .as_ref()
                    .is_some_and(|(kind, _)| kind.name() == key)
                {
                    self.flash = None;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn render(&self) -> Result<String> {
        let internal = |_| Error::Internal;
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                StatusKind::Error => flash.error = message,
                StatusKind::Warning => flash.warning = message,
                StatusKind::Info => flash.info = message,
            }
        }
        let label = Field::in_form("layout", "label")
            .label("Label")
            .hint("Example: Sales workspace")
            .value(self.label.clone())
            .required();
        let roles = MultiSelect::in_form("layout", "roles")
            .id("shared-workspace-roles")
            .label("Limit to roles")
            .placeholder("Everyone in this company")
            .selection_label(":count role selected|:count roles selected")
            .options(
                self.role_options
                    .iter()
                    .map(|role| Choice::from((role.name.clone(), role.code.clone())))
                    .collect(),
            )
            .values(self.roles.clone());
        let mut rows = Vec::new();
        for entry in &self.entries {
            let slug = &entry.slug;
            let mut row = FlexRow::new(slug.clone());
            row.dom_id = Some(format!("shared-workspace-{slug}"));
            row.slots.insert("label".into(), Html::text(&entry.label));
            row.slots
                .insert("audience".into(), Html::text(&audience(entry)));
            row.actions = Html::render(&SharedActions {
                open_id: &format!("shared-workspace-open-{slug}"),
                open_href: &format!("{WORKSPACE}/shared/{slug}"),
                action: Html::render(&Button {
                    id: &format!("shared-workspace-delete-{slug}"),
                    event: "delete_layout",
                    name: "slug",
                    value: slug,
                    body: Html::text("Delete"),
                    ..Button::default()
                })
                .map_err(internal)?,
            })
            .map_err(internal)?;
            rows.push(row);
        }
        let mut table = FlexTable::new("shared-workspaces", self.columns.flex_columns(), rows);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Shared workspaces";
        table.actions = true;
        table.framed = false;
        if self.entries.is_empty() {
            table.empty = Some(TableEmpty::State(EmptyState::missing(
                "No shared workspaces",
                "Arrange pages in a workspace, then share it here.",
            )));
        }
        SharedView {
            flash,
            tree: &self.tree,
            label,
            roles,
            table: Html::render(&table).map_err(internal)?,
            pending_delete: self.pending_delete.is_some(),
        }
        .render()
        .map_err(internal)
    }
}

#[derive(Template)]
#[template(path = "shared_layouts.html")]
struct SharedView<'a> {
    flash: FlashMap<'a>,
    tree: &'a str,
    label: Field,
    roles: MultiSelect,
    table: Html,
    pending_delete: bool,
}

pub(crate) struct Mounted {
    core: Arc<Core>,
    page: SharedPage,
}
impl Mounted {
    pub(crate) fn new(core: Arc<Core>, page: SharedPage) -> Self {
        Self { core, page }
    }
}

impl LiveState<Scope, Error> for Mounted {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            let before = self.page.flash.clone();
            match request.input {
                LiveInput::Refresh => self.page.reload(&self.core, request.scope).await?,
                LiveInput::Params(values) => {
                    let flash = self.page.flash.take();
                    let columns = std::mem::replace(
                        &mut self.page.columns,
                        columns("shared-workspaces", COLUMNS)?,
                    );
                    self.page = SharedPage::load(&self.core, request.scope, values).await?;
                    self.page.flash = flash;
                    self.page.columns = columns;
                }
                LiveInput::Event(value) => {
                    let event: Event =
                        serde_json::from_value(value.clone()).map_err(|_| Error::Invalid)?;
                    self.page
                        .event(&self.core, request.scope, request.credential, event)
                        .await?;
                }
            }
            let notice = match &self.page.flash {
                Some((_, message)) if self.page.flash != before => message.clone(),
                _ => String::new(),
            };
            let mut query = HashMap::new();
            if !self.page.tree.is_empty() {
                query.insert("t".to_owned(), self.page.tree.clone());
            }
            Ok(LiveBody {
                html: self.page.render()?,
                query,
                notice,
                rows: self.page.entries.len(),
                ..LiveBody::default()
            })
        })
    }
}
