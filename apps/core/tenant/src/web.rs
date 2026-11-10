//! `/tenancy/tenants` (`core/tenant.pages`, operation `tenants`): the platform
//! tenant list, its Add Tenant dialog and the Suspend and Reactivate row
//! actions with their reason dialog.
//!
//! The route is `OperatorCapability(LIST)`: the host proves the explicit
//! operator identity and the capability on every request and live event. This
//! page re-proves the identity for its reads (`prove_operator`) and asks the
//! bound authorizer for `CREATE`, `SUSPEND` or `REACTIVATE` before it shows the
//! dialog or button and again before it writes, as the source checks `allowed?`
//! on both. A refusal is a flash, never a page-clearing error. Parent names and
//! sub-tenant counts are derived from the identity list, not from associations.
use crate::{CREATE, LIST, PAGES, REACTIVATE, SUSPEND};
use askama::Template;
use bilibi_agent_api::{
    page::{Call, PageContracts},
    Error as ContractError,
};
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayContext, DisplayPolicy, Format, Precision};
use bilibi_tenancy::{
    Authorization, Move, OperatorProof, Scope, Suspension, TenantErrors, TenantIdentity,
    TenantStatus,
};
use bilibi_ui::page::{
    LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
    PageFuture, PageRequest,
};
use bilibi_ui::{
    params, Badge, BadgeKind, BuiltinColumn, Choice, ColumnType, ColumnsOutcome, CreateButton,
    Field, FlashMap, FlexRow, FlexTable, Form, Html, IconButton, InputKind, ListColumns, ListSpec,
    ListState, SortDir, StatusKind, TableEmpty, NO_FLASH,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
};

const DENIED: &str = "You do not have permission to create a tenant.";
const MOVE_DENIED: &str = "You do not have permission to change a tenant's status.";

/// The source sorts in memory from `id` ascending; the URL carries a changed
/// sort so Back and reload restore it. There is no paging: every tenant shows.
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        sortable: BTreeMap::from([
            ("id".into(), SortDir::Asc),
            ("name".into(), SortDir::Asc),
            ("status".into(), SortDir::Asc),
        ]),
        default_sort: Some("id".into()),
        page_sizes: vec![25],
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("tenants list spec")
});

// The Add Tenant dialog is the only tenant view; the list and dialog share these labels.
bilibi_ui::record_fields! { mod fields {
    ID: bilibi_ui::RecordField::new("id", "ID"),
    NAME: bilibi_ui::RecordField::new("name", "Name"),
    PARENT: bilibi_ui::RecordField::new("parent", "Parent tenant"),
    SUB_TENANTS: bilibi_ui::RecordField::new("sub_tenants", "Sub-tenants"),
    STATUS: bilibi_ui::RecordField::new("status", "Status"),
    SUSPENSION: bilibi_ui::RecordField::new("suspension", "Suspension"),
    PRIMARY_COMPANY: bilibi_ui::RecordField::new("primary_company", "Primary company").excluded("Entered when the tenant is created; no tenant view reads it back"),
    ADDITIONAL_COMPANIES: bilibi_ui::RecordField::new("additional_companies", "Additional companies").excluded("A list of companies, not a scalar tenant field"),
} }

fn builtin(field: bilibi_ui::RecordField) -> BuiltinColumn {
    field.builtin().expect("tenant scalar column")
}

fn builtins() -> Vec<BuiltinColumn> {
    let mut id = builtin(fields::ID).sortable("id").right();
    id.kind = ColumnType::Integer;
    id.sort_id = Some("tenants-sort-id".into());
    let mut name = builtin(fields::NAME).sortable("name");
    name.sort_id = Some("tenants-sort-name".into());
    let mut sub_tenants = builtin(fields::SUB_TENANTS).right();
    sub_tenants.kind = ColumnType::Integer;
    let mut status = builtin(fields::STATUS).sortable("status");
    status.sort_id = Some("tenants-sort-status".into());
    vec![
        id,
        name,
        builtin(fields::PARENT),
        sub_tenants,
        status,
        builtin(fields::SUSPENSION),
    ]
}

/// The module's page adapter; every field is shared by mounted states.
#[derive(Clone)]
pub struct Pages {
    pub db: Db,
    pub display: Arc<DisplayPolicy>,
    pub locales: Arc<bilibi_locale::Locales>,
    pub contracts: Arc<PageContracts>,
    pub authorization: Arc<Authorization>,
}

/// One tenant as the table shows it.
struct TenantRow {
    id: i64,
    name: String,
    is_platform_operator: bool,
    parent_name: Option<String>,
    children_count: usize,
    status: TenantStatus,
    suspension: Option<Suspension>,
}

fn present(
    identities: &[TenantIdentity],
    sort_by: Option<&str>,
    descending: bool,
) -> Vec<TenantRow> {
    let names: HashMap<i64, &str> = identities
        .iter()
        .map(|identity| (identity.id, identity.name.as_str()))
        .collect();
    let mut children: HashMap<i64, usize> = HashMap::new();
    for parent in identities.iter().filter_map(|identity| identity.parent_id) {
        *children.entry(parent).or_default() += 1;
    }
    let mut rows: Vec<TenantRow> = identities
        .iter()
        .map(|identity| TenantRow {
            id: identity.id,
            name: identity.name.clone(),
            is_platform_operator: identity.is_platform_operator,
            parent_name: identity
                .parent_id
                .and_then(|parent| names.get(&parent))
                .map(|name| (*name).to_owned()),
            children_count: children.get(&identity.id).copied().unwrap_or(0),
            status: identity.status,
            suspension: identity.suspension.clone(),
        })
        .collect();
    rows.sort_by(|a, b| {
        let order = match sort_by {
            Some("name") => a.name.cmp(&b.name),
            Some("status") => a.status.as_str().cmp(b.status.as_str()),
            _ => a.id.cmp(&b.id),
        }
        .then(a.id.cmp(&b.id));
        if descending {
            order.reverse()
        } else {
            order
        }
    });
    rows
}

fn status_kind(status: TenantStatus) -> BadgeKind {
    match status {
        TenantStatus::Active => BadgeKind::Success,
        TenantStatus::Suspended => BadgeKind::Neutral,
    }
}

/// The suspension column: since when and why, or a dash while active.
fn suspension_text(display: &DisplayContext, suspension: Option<&Suspension>) -> String {
    let Some(suspension) = suspension else {
        return "—".into();
    };
    match suspension.at.parse::<jiff::Timestamp>() {
        Ok(at) => format!(
            "{}: {}",
            display.render(at, Format::DateTime, Precision::Minute).text,
            suspension.reason
        ),
        Err(_) => suspension.reason.clone(),
    }
}

/// The Suspend or Reactivate dialog while it is open: whom it is about, the
/// reason typed so far and its error. The reason form reports each change, so
/// the confirm carries it.
struct MoveDraft {
    id: i64,
    name: String,
    change: Move,
    reason: String,
    error: Option<String>,
}
impl MoveDraft {
    fn event(&self) -> &'static str {
        match self.change {
            Move::Suspend => "confirm_suspend",
            Move::Reactivate => "confirm_reactivate",
        }
    }
    fn contract(&self) -> &'static str {
        match self.change {
            Move::Suspend => SUSPEND,
            Move::Reactivate => REACTIVATE,
        }
    }
    /// The reason to submit, or `None` with the error shown in the dialog.
    fn ready(&mut self) -> Option<String> {
        let text = self.reason.trim();
        if text.is_empty() {
            self.error = Some(match self.change {
                Move::Suspend => "Say why this tenant is suspended.".into(),
                Move::Reactivate => "Say why this tenant is reactivated.".into(),
            });
            return None;
        }
        if text.chars().count() > bilibi_tenancy::REASON_MAX {
            self.error = Some("Keep the reason to 500 characters.".into());
            return None;
        }
        Some(text.to_owned())
    }
    fn render(&self) -> Result<Html> {
        let mut field = Field::named("reason")
            .id("tenant-move-reason")
            .label("Reason")
            .hint("Recorded with this change in the audit trail.")
            .kind(InputKind::Textarea)
            .value(&self.reason)
            .required()
            .attr("maxlength", "500")
            .attr("data-debounce", "300")
            .wrapper_class("mb-0");
        if let Some(error) = &self.error {
            field.errors.push(error.clone());
        }
        let body = render(Form {
            id: "tenant-move-form",
            event: "move_reason",
            change: true,
            body: render(field)?,
            ..Form::default()
        })?;
        let (consequence, detail, confirm, working) = match self.change {
            Move::Suspend => (
                format!("{} will be suspended.", self.name),
                "Everyone signed in to it is signed out now, and nobody can sign in until you reactivate it. Its data is kept.",
                "Suspend",
                "Suspending…",
            ),
            Move::Reactivate => (
                format!("{} will be reactivated.", self.name),
                "People can sign in again. Jobs cancelled while it was suspended do not restart.",
                "Reactivate",
                "Reactivating…",
            ),
        };
        render(MoveDialog {
            consequence: &consequence,
            detail,
            confirm,
            working,
            confirm_event: self.event(),
            body,
        })
    }
}

fn render(fragment: impl Template) -> Result<Html> {
    Html::render(&fragment).map_err(|_| Error::Internal)
}

#[derive(Template)]
#[template(path = "tenant_move.html")]
struct MoveDialog<'a> {
    consequence: &'a str,
    detail: &'a str,
    confirm: &'a str,
    working: &'a str,
    confirm_event: &'a str,
    body: Html,
}

#[derive(Template)]
#[template(
    source = r#"<span class="tabular-nums text-ink-muted">{{ value }}</span>"#,
    ext = "html"
)]
struct NumberCell {
    value: i64,
}

#[derive(Template)]
#[template(
    source = r#"{% import "components.html" as ui %}<span class="font-medium text-ink-strong">{{ name }}</span>{% if operator %} {% call ui::badge(class="ml-2") %}Platform operator{% endcall %}{% endif %}"#,
    ext = "html"
)]
struct NameCell<'a> {
    name: &'a str,
    operator: bool,
}

#[derive(Template)]
#[template(
    source = r#"<span class="text-ink-muted">{{ text }}</span>"#,
    ext = "html"
)]
struct MutedCell<'a> {
    text: &'a str,
}

/// The draft of the Add Tenant dialog while it is open, with the errors the
/// last submission was refused with.
#[derive(Clone, Debug, Default)]
struct Draft {
    name: String,
    parent_id: String,
    errors: TenantErrors,
    primary_company: String,
    additional_companies: String,
    company_errors: BTreeMap<String, Vec<String>>,
}

struct CreateForm {
    name: Field,
    parent: Field,
    summary: bilibi_ui::ErrorSummary,
    primary_company: Field,
    additional_companies: Field,
}

#[derive(Template)]
#[template(path = "tenants.html")]
struct TenantsView<'a> {
    flash: FlashMap<'a>,
    title_actions: Html,
    actions: Html,
    table: Html,
    create: Option<CreateForm>,
    moving: Option<Html>,
}

struct TenantsPage {
    pages: Pages,
    list: ListState,
    columns: ListColumns,
    create: Option<Draft>,
    moving: Option<MoveDraft>,
    flash: Option<(StatusKind, String)>,
}

impl TenantsPage {
    fn new(pages: &Pages, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            pages: pages.clone(),
            list: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: ListColumns::mount("tenants", builtins()).map_err(|_| Error::Internal)?,
            create: None,
            moving: None,
            flash: None,
        })
    }

    fn flash(&mut self, kind: StatusKind, message: impl Into<String>) {
        self.flash = Some((kind, message.into()));
    }

    /// Whether the actor may create a tenant: operator identity and `CREATE`,
    /// decided by the bound authorizer and logged like any decision.
    async fn create_proof(&self, scope: &Scope) -> Result<Option<OperatorProof>> {
        match self
            .pages
            .authorization
            .authorize_operator(scope, bilibi_tenancy::CREATE)
            .await
        {
            Ok(proof) => Ok(Some(proof)),
            Err(Error::Forbidden) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether the actor holds the platform capability `key`, decided by the
    /// bound authorizer and logged like any decision.
    async fn may(&self, scope: &Scope, key: &str) -> Result<bool> {
        match self
            .pages
            .authorization
            .authorize_operator(scope, key)
            .await
        {
            Ok(_) => Ok(true),
            Err(Error::Forbidden) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The reader's clock and zone, for the suspension column.
    async fn display(&self, scope: &Scope) -> Result<DisplayContext> {
        let client = self.pages.db.get().await?;
        let locale = self
            .pages
            .locales
            .resolve(&**client, scope, None)
            .await?
            .locale;
        self.pages
            .display
            .display(&**client, scope, scope.current_company(), locale)
            .await
    }

    /// The rows changed: ListColumns forgets an unfinished add box.
    fn reload(&mut self, rows: &[TenantRow]) {
        self.columns.load(rows, |row| row.id.to_string());
    }

    fn table(
        &self,
        rows: &[TenantRow],
        display: &DisplayContext,
        may: (bool, bool),
    ) -> Result<Html> {
        let mut flex_rows = Vec::with_capacity(rows.len());
        for row in rows {
            let mut flex = FlexRow::new(row.id.to_string());
            flex.dom_id = Some(format!("tenants-{}", row.id));
            flex = flex
                .slot("id", render(NumberCell { value: row.id })?)
                .slot(
                    "name",
                    render(NameCell {
                        name: &row.name,
                        operator: row.is_platform_operator,
                    })?,
                )
                .slot(
                    "parent",
                    render(MutedCell {
                        text: row.parent_name.as_deref().unwrap_or("—"),
                    })?,
                )
                .slot(
                    "sub_tenants",
                    render(NumberCell {
                        value: row.children_count as i64,
                    })?,
                )
                .slot(
                    "status",
                    render(Badge {
                        kind: status_kind(row.status),
                        dot: true,
                        class: "",
                        body: Html::text(row.status.label()),
                    })?,
                )
                .slot(
                    "suspension",
                    render(MutedCell {
                        text: &suspension_text(display, row.suspension.as_ref()),
                    })?,
                );
            match row.status {
                // The operator's own tenant cannot be suspended: nobody could
                // reactivate it, so the row offers nothing.
                TenantStatus::Active if may.0 && !row.is_platform_operator => {
                    flex.actions.push(&render(IconButton {
                        id: &format!("tenants-suspend-{}", row.id),
                        event: "request_suspend",
                        name: "id",
                        value: &row.id.to_string(),
                        ..IconButton::new("no-symbol", &format!("Suspend {}", row.name))
                    })?);
                }
                TenantStatus::Suspended if may.1 => {
                    flex.actions.push(&render(IconButton {
                        id: &format!("tenants-reactivate-{}", row.id),
                        event: "request_reactivate",
                        name: "id",
                        value: &row.id.to_string(),
                        ..IconButton::new("lock-open", &format!("Reactivate {}", row.name))
                    })?);
                }
                _ => {}
            }
            flex_rows.push(flex);
        }
        let mut table = FlexTable::new("tenants", self.columns.flex_columns(), flex_rows)
            .sorted(self.list.sort_by.as_deref(), self.list.sort_dir);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.framed = false;
        table.actions = true;
        table.caption = "Tenants";
        if rows.is_empty() {
            table.empty = Some(TableEmpty::Content(Html::text("No tenants found.")));
        }
        render(table)
    }

    fn form(&self, identities: &[TenantIdentity]) -> Option<CreateForm> {
        let draft = self.create.as_ref()?;
        let mut parents: Vec<&TenantIdentity> = identities.iter().collect();
        parents.sort_by_key(|identity| identity.id);
        let mut summary = bilibi_ui::ErrorSummary::new("tenant-create-errors");
        for (id, label, errors) in [
            ("tenant-name", fields::NAME.label, draft.errors.name.clone()),
            (
                "tenant-parent",
                fields::PARENT.label,
                draft.errors.parent_id.clone(),
            ),
            (
                "tenant-primary-company",
                fields::PRIMARY_COMPANY.label,
                draft
                    .company_errors
                    .get("primary_company")
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                "tenant-additional-companies",
                fields::ADDITIONAL_COMPANIES.label,
                draft
                    .company_errors
                    .iter()
                    .filter(|(key, _)| key.starts_with("additional_companies"))
                    .flat_map(|(_, errors)| errors.clone())
                    .collect(),
            ),
        ] {
            for error in errors {
                summary.push(id, bilibi_ui::error_sentence(label, &error));
            }
        }
        Some(CreateForm {
            summary,
            primary_company: Field::in_form("tenant", "primary_company")
                .id("tenant-primary-company")
                .label(fields::PRIMARY_COMPANY.label)
                .required()
                .attr("maxlength", "160")
                .value(draft.primary_company.clone())
                .errors(
                    draft
                        .company_errors
                        .get("primary_company")
                        .cloned()
                        .unwrap_or_default(),
                ),
            additional_companies: Field::in_form("tenant", "additional_companies")
                .id("tenant-additional-companies")
                .label(fields::ADDITIONAL_COMPANIES.label)
                .kind(InputKind::Textarea)
                .hint("One company name per line, up to 99.")
                .value(draft.additional_companies.clone())
                .errors(
                    draft
                        .company_errors
                        .iter()
                        .filter(|(key, _)| key.starts_with("additional_companies"))
                        .flat_map(|(_, errors)| errors.clone())
                        .collect(),
                ),
            name: Field::in_form("tenant", "name")
                .id("tenant-name")
                .label(fields::NAME.label)
                .placeholder("e.g. Acme Sdn Bhd")
                .required()
                .attr("maxlength", "255")
                .value(draft.name.clone())
                .errors(draft.errors.name.clone()),
            parent: Field::in_form("tenant", "parent_id")
                .id("tenant-parent")
                .kind(InputKind::Select)
                .label(fields::PARENT.label)
                .prompt("None (top-level tenant)")
                .options(
                    parents
                        .iter()
                        .map(|identity| Choice::new(identity.name.clone(), identity.id.to_string()))
                        .collect(),
                )
                .value(draft.parent_id.clone())
                .errors(draft.errors.parent_id.clone()),
        })
    }

    async fn body(&mut self, scope: &Scope, rows_changed: bool) -> Result<String> {
        // Preserve the complete directory/parent picker while reading
        // through the bounded search contract in pages of at most 100.
        let mut identities = Vec::new();
        let mut page = 1;
        loop {
            let result = self
                .pages
                .contracts
                .read(
                    scope,
                    LIST,
                    &serde_json::json!({"page":page,"page_size":100}),
                )
                .await
                .map_err(contract_error)?;
            let rows = result["result"].as_array().ok_or(Error::Internal)?;
            for row in rows {
                identities.push(TenantIdentity {
                    id: row["id"].as_i64().ok_or(Error::Internal)?,
                    parent_id: row["parent_id"].as_i64(),
                    name: row["name"].as_str().ok_or(Error::Internal)?.into(),
                    status: row["status"]
                        .as_str()
                        .and_then(TenantStatus::parse)
                        .ok_or(Error::Internal)?,
                    is_platform_operator: row["is_platform_operator"]
                        .as_bool()
                        .ok_or(Error::Internal)?,
                    suspension: match row["suspension"].as_object() {
                        Some(suspension) => Some(Suspension {
                            at: suspension["at"].as_str().ok_or(Error::Internal)?.into(),
                            reason: suspension["reason"].as_str().ok_or(Error::Internal)?.into(),
                        }),
                        None => None,
                    },
                });
            }
            if rows.is_empty()
                || identities.len() as u64
                    >= result["meta"]["page"]["total"]
                        .as_u64()
                        .ok_or(Error::Internal)?
            {
                break;
            }
            page += 1;
        }
        let can_create = self.create_proof(scope).await?.is_some();
        let may = (
            self.may(scope, Move::Suspend.capability()).await?,
            self.may(scope, Move::Reactivate.capability()).await?,
        );
        let display = self.display(scope).await?;
        let rows = present(
            &identities,
            self.list.sort_by.as_deref(),
            self.list.sort_dir == Some(SortDir::Desc),
        );
        if rows_changed {
            self.reload(&rows);
        }
        let actions = if can_create {
            render(CreateButton {
                noun: "Tenant",
                id: "tenants-add",
                event: "show_create",
                ..CreateButton::default()
            })?
        } else {
            Html::EMPTY
        };
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                StatusKind::Error => flash.error = message,
                StatusKind::Warning => flash.warning = message,
                StatusKind::Info => flash.info = message,
            }
        }
        TenantsView {
            title_actions: Html::EMPTY,
            flash,
            actions,
            table: self.table(&rows, &display, may)?,
            create: self.form(&identities),
            moving: self.moving.as_ref().map(MoveDraft::render).transpose()?,
        }
        .render()
        .map_err(|_| Error::Internal)
    }

    /// One live event. Returns whether the rows changed, so the columns reload.
    async fn event(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &serde_json::Value,
    ) -> Result<bool> {
        let get = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
        match get("type").ok_or(Error::Invalid)? {
            "clear-flash" => {
                self.flash = None;
                Ok(false)
            }
            "sort" => {
                // The source ignores an unknown column; a known one toggles.
                if let Some(column) = get("sort") {
                    if self.list.next_sort(column) {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            "grid" => {
                let params: HashMap<String, String> = value
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(key, value)| {
                        let text = match value {
                            serde_json::Value::String(text) => text.clone(),
                            serde_json::Value::Number(number) => number.to_string(),
                            _ => return None,
                        };
                        Some((key.clone(), text))
                    })
                    .collect();
                match self.columns.handle(&params) {
                    ColumnsOutcome::Sort(column) => Ok(self.list.next_sort(&column)),
                    ColumnsOutcome::Update | ColumnsOutcome::Noop => Ok(false),
                }
            }
            "show_create" => {
                if self.create_proof(scope).await?.is_some() {
                    self.create = Some(Draft::default());
                } else {
                    self.flash(StatusKind::Error, DENIED);
                }
                Ok(false)
            }
            "cancel_create" => {
                self.create = None;
                Ok(false)
            }
            "request_suspend" | "request_reactivate" => {
                let change = if get("type") == Some("request_suspend") {
                    Move::Suspend
                } else {
                    Move::Reactivate
                };
                let id = params::positive_id(get("id")).ok_or(Error::Invalid)?;
                if !self.may(scope, change.capability()).await? {
                    self.flash(StatusKind::Error, MOVE_DENIED);
                    return Ok(false);
                }
                // The name and standing come from the directory, never from the
                // event: a stale or forged row cannot name another tenant.
                let result = self
                    .pages
                    .contracts
                    .read(scope, crate::GET, &serde_json::json!({"tenant_id":id}))
                    .await;
                let tenant = match result {
                    Ok(value) => value["result"].clone(),
                    Err(ContractError::NotFound) => {
                        self.flash(StatusKind::Error, "That tenant no longer exists.");
                        return Ok(true);
                    }
                    Err(error) => return Err(contract_error(error)),
                };
                let status = tenant["status"].as_str().and_then(TenantStatus::parse);
                if status.and_then(|status| status.after(change)).is_none()
                    || (change == Move::Suspend && tenant["is_platform_operator"] == true)
                {
                    self.flash(
                        StatusKind::Error,
                        "This tenant's status changed. Review it and try again.",
                    );
                    return Ok(true);
                }
                self.moving = Some(MoveDraft {
                    id,
                    name: tenant["name"].as_str().ok_or(Error::Internal)?.into(),
                    change,
                    reason: String::new(),
                    error: None,
                });
                Ok(false)
            }
            "move_reason" => {
                if let Some(draft) = &mut self.moving {
                    draft.reason = get("reason")
                        .unwrap_or_default()
                        .chars()
                        .take(bilibi_tenancy::REASON_MAX + 1)
                        .collect();
                    draft.error = None;
                }
                Ok(false)
            }
            "cancel_move" => {
                self.moving = None;
                Ok(false)
            }
            "confirm_suspend" | "confirm_reactivate" => {
                let Some(draft) = &mut self.moving else {
                    return Ok(false);
                };
                if draft.event() != get("type").unwrap_or_default() {
                    return Err(Error::Invalid);
                }
                let Some(reason) = draft.ready() else {
                    return Ok(false);
                };
                let call = Call {
                    scope,
                    credential,
                    service: PAGES,
                    event: draft.event(),
                    contract: draft.contract(),
                };
                let input = serde_json::json!({"tenant_id":draft.id,"reason":reason});
                let done = match draft.change {
                    Move::Suspend => format!("{} is suspended.", draft.name),
                    Move::Reactivate => format!("{} is active again.", draft.name),
                };
                match self.pages.contracts.submit(&call, input).await {
                    Ok(_) => {
                        self.moving = None;
                        self.flash(StatusKind::Success, done);
                        Ok(true)
                    }
                    Err(ContractError::InvalidInput(mut fields)) => {
                        if let Some(errors) = fields.remove("reason") {
                            draft.error = errors
                                .first()
                                .map(|error| bilibi_ui::error_sentence("Reason", error));
                            return Ok(false);
                        }
                        let message = fields
                            .remove("tenant_id")
                            .and_then(|errors| errors.into_iter().next())
                            .map(|error| format!("{} {error}.", draft.name))
                            .unwrap_or_else(|| "The tenant's status was not changed.".into());
                        self.moving = None;
                        self.flash(StatusKind::Error, message);
                        Ok(true)
                    }
                    Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                        self.moving = None;
                        self.flash(StatusKind::Error, MOVE_DENIED);
                        Ok(false)
                    }
                    Err(ContractError::NotFound) => {
                        self.moving = None;
                        self.flash(StatusKind::Error, "That tenant no longer exists.");
                        Ok(true)
                    }
                    Err(error) => Err(contract_error(error)),
                }
            }
            "create" => {
                let Some(_) = self.create_proof(scope).await? else {
                    self.flash(StatusKind::Error, DENIED);
                    return Ok(false);
                };
                let posted = params::form_values_checked(value, "tenant", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let field = |key: &str| posted.get(key).cloned().unwrap_or_default();
                // The source's changeset trims the name before validation, so
                // the draft echoed back with an error is the trimmed one.
                let mut draft = Draft {
                    name: field("name").trim().to_owned(),
                    parent_id: field("parent_id"),
                    errors: TenantErrors::default(),
                    primary_company: field("primary_company"),
                    additional_companies: field("additional_companies"),
                    company_errors: BTreeMap::new(),
                };
                let parent_id = params::blank_to_none(Some(&draft.parent_id));
                let parent = parent_id.map(|text| params::positive_id(Some(&text)));
                if parent == Some(None) {
                    // A parent that is not an id cannot be cast, as the source's
                    // changeset reports.
                    draft.errors.parent_id.push("is invalid".into());
                    self.create = Some(draft);
                    return Ok(false);
                }
                let mut input = serde_json::json!({
                    "name":draft.name,
                    "primary_company":draft.primary_company,
                    "additional_companies":draft.additional_companies.lines().map(str::trim).filter(|line|!line.is_empty()).collect::<Vec<_>>()
                });
                if let Some(parent) = parent.flatten() {
                    input["parent_id"] = serde_json::json!(parent);
                }
                let call = Call {
                    scope,
                    credential,
                    service: PAGES,
                    event: "create",
                    contract: CREATE,
                };
                match self.pages.contracts.submit(&call, input).await {
                    Ok(_) => {
                        self.create = None;
                        self.flash(StatusKind::Success, "Tenant and companies created.");
                        Ok(true)
                    }
                    Err(ContractError::InvalidInput(mut fields)) => {
                        draft.errors.name = fields.remove("name").unwrap_or_default();
                        draft.errors.parent_id = fields.remove("parent_id").unwrap_or_default();
                        draft.company_errors = fields;
                        self.create = Some(draft);
                        Ok(false)
                    }
                    Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                        self.flash(StatusKind::Error, DENIED);
                        Ok(false)
                    }
                    Err(error) => Err(contract_error(error)),
                }
            }
            _ => Err(Error::Invalid),
        }
    }
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "tenants").then_some("Tenants")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            if request.operation != "tenants" {
                return Err(Error::NotFound);
            }
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let html = TenantsPage::new(self, request.query)?
                .body(scope, true)
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
            if request.operation != "tenants" {
                return Err(Error::NotFound);
            }
            let state: Box<dyn LiveState<Scope, Error>> =
                Box::new(TenantsPage::new(self, request.query)?);
            Ok(Some(state))
        })
    }
}

impl LiveState<Scope, Error> for TenantsPage {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            let rows_changed = match request.input {
                LiveInput::Refresh => true,
                LiveInput::Params(values) => {
                    self.list =
                        ListState::parse(values, SPEC.clone()).map_err(|_| Error::Internal)?;
                    true
                }
                LiveInput::Event(value) => {
                    self.event(request.scope, request.credential, value).await?
                }
            };
            let html = self.body(request.scope, rows_changed).await?;
            Ok(LiveBody {
                html,
                query: self.list.to_params(),
                ..LiveBody::default()
            })
        })
    }
}

fn contract_error(error: ContractError) -> Error {
    match error {
        ContractError::Backend(error) => error,
        ContractError::Forbidden => Error::Forbidden,
        ContractError::NotFound => Error::NotFound,
        ContractError::InvalidInput(_) => Error::Invalid,
        _ => Error::Internal,
    }
}
