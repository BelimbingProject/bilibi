use super::fields;
use super::Pending;
use crate::{Company, FieldError, NewCompany, Params};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_grid::{PageColumns, PageViews};
use bilibi_tenancy::Scope;
use bilibi_ui::*;
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};
use tokio_postgres::GenericClient;

pub(crate) fn render(value: &impl Template) -> Result<Html> {
    Html::render(value).map_err(|_| Error::Internal)
}
pub(crate) async fn allowed(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    key: &str,
) -> Result<bool> {
    match bilibi_authz::authorize_key(client, scope, key).await {
        Ok(()) => Ok(true),
        Err(Error::Forbidden) => Ok(false),
        Err(e) => Err(e),
    }
}
pub(super) fn columns(
    views: Arc<PageViews>,
    masked: &HashSet<String>,
    custom: &[bilibi_custom_fields::Definition],
) -> Result<PageColumns> {
    let mut columns: Vec<BuiltinColumn> = [
        ("name", "Name", true),
        ("code", "Code", false),
        ("parent_name", "Parent", false),
        ("status", "Status", true),
        ("jurisdiction", "Jurisdiction", true),
    ]
    .into_iter()
    .filter(|(id, _, _)| !masked.contains(*id))
    .map(|(id, label, sort)| {
        let column = BuiltinColumn::new(id, label);
        if sort {
            column.sortable(id)
        } else {
            column
        }
    })
    .collect();
    // The page's own columns are the defaults; a tenant's custom fields are
    // there to add from the column picker, and sort by their own key (not
    // several choices or a record, which have no order to sort by).
    let defaults: Vec<String> = columns.iter().map(|column| column.id.clone()).collect();
    for field in custom.iter().take(crate::list_fields::MAX_CUSTOM_COLUMNS) {
        let id = crate::list_fields::custom_id(&field.key);
        let column = BuiltinColumn::new(&id, &field.label);
        columns.push(if crate::list_fields::custom_sortable(field.kind) {
            column.sortable(&id)
        } else {
            column
        });
    }
    let record_fields = record_slots(masked)?;
    let scalar_sorts = crate::field_contributions::definitions()?
        .into_iter()
        .map(|(_, field)| (field.grid_column.into(), field.id.into()))
        .collect();
    let defaults: Vec<&str> = defaults.iter().map(String::as_str).collect();
    PageColumns::mount(views, "companies", columns)
        .map(|columns| columns.with_defaults(&defaults))
        .and_then(|columns| columns.with_record_slots(&record_fields))
        .map(|columns| columns.with_scalar_sorts(scalar_sorts))
        .map_err(|_| Error::Internal)
}

/// Facts the Company page actually fills into its own row slots. Keep other
/// Company Grid fields as projections.
pub(super) fn record_slots(masked: &HashSet<String>) -> Result<Vec<bilibi_ui::RecordField>> {
    let filled_slots = ["name", "code", "parent_name", "status", "jurisdiction"];
    let mut record_fields = fields::ALL
        .iter()
        .filter(|field| {
            field
                .slot_id()
                .is_some_and(|slot| filled_slots.contains(&slot.as_str()))
        })
        .copied()
        .collect::<Vec<_>>();
    let installed = bilibi_grid::installed().map_err(|_| Error::Internal)?;
    if let Some(table) = installed.table("companies") {
        let contributed_ids = crate::field_contributions::definitions()
            .map_err(|_| Error::Internal)?
            .into_iter()
            .map(|(_, field)| field.id)
            .collect::<HashSet<_>>();
        let additions = table
            .record_fields()
            .iter()
            .filter(|field| contributed_ids.contains(field.id))
            .filter(|field| !record_fields.iter().any(|existing| existing.id == field.id))
            .filter(|field| !masked.contains(field.id))
            .copied()
            .collect::<Vec<_>>();
        record_fields.extend(additions);
    }
    Ok(record_fields)
}
pub(super) fn badge(status: &str) -> Result<Html> {
    render(&Badge::status(
        match status {
            "active" => BadgeKind::Success,
            "pending" => BadgeKind::Warning,
            "suspended" => BadgeKind::Danger,
            _ => BadgeKind::Neutral,
        },
        status,
    ))
}
fn page(id: &str, variant: PageVariant, body: Html) -> Result<String> {
    Page {
        id,
        variant,
        body,
        ..Page::default()
    }
    .render()
    .map_err(|_| Error::Internal)
}
fn card(id: &str, body: Html, flat: bool) -> Result<Html> {
    render(&Card {
        id,
        inner_class: if flat { "p-0" } else { "p-5 sm:p-6" },
        body,
        ..Card::default()
    })
}
fn link(id: &str, title: &str, href: &str) -> Result<Html> {
    render(&ActionLink {
        id,
        icon: "manage",
        title,
        href,
        body: Html::text(title),
        ..ActionLink::default()
    })
}
fn back(id: &str) -> Result<Html> {
    render(&BackLink {
        id,
        href: "/companies",
        title: "Back to companies",
        ..BackLink::default()
    })
}

#[derive(Template)]
#[template(path = "list.html")]
struct ListBody {
    header: Html,
    toolbar: Html,
    card: Html,
}
#[derive(Template)]
#[template(path = "company_name.html")]
struct CompanyName<'a> {
    link: Html,
    legal: Option<&'a str>,
}
pub(super) async fn list(
    db: &bilibi_database::Db,
    scope: &Scope,
    data: &crate::Page,
    params: &Params,
    columns: &PageColumns,
    workspace: bool,
    extension_fields: ExtensionListFields<'_>,
    add_to: &[bilibi_records::AddTo],
) -> Result<String> {
    let custom_columns: Vec<&crate::list_fields::AvailableField> = extension_fields
        .available
        .iter()
        .filter(|field| field.custom.is_some())
        .filter(|field| columns.columns.iter().any(|column| column.id == field.id))
        .collect();
    let named = custom_references(
        scope,
        &custom_columns
            .iter()
            .filter_map(|field| field.custom.as_ref())
            .collect::<Vec<_>>(),
        &extension_fields.custom_values.values().collect::<Vec<_>>(),
    )
    .await?;
    let connection = db.get().await?;
    let client = &**connection;
    let mut actions = Html::EMPTY;
    if allowed(client, scope, crate::CREATE).await? {
        actions.push(&render(&CreateButton {
            noun: "Company",
            href: "/companies/create",
            id: "companies-add",
            class: "",
            event: "",
            button_type: "button",
        })?);
    }
    actions.push(&link(
        "companies-department-types",
        "Department Types",
        "/companies/department-types",
    )?);
    actions.push(&link(
        "companies-legal-entity-types",
        "Legal Entity Types",
        "/companies/legal-entity-types",
    )?);
    actions.push(&link(
        "companies-relationship-types",
        "Relationship Roles",
        "/companies/relationship-types",
    )?);
    let header = render(&Header {
        title_actions: Html::EMPTY,
        title: Html::text("Companies"),
        subtitle: "",
        actions: render(&Actions {
            class: "flex flex-wrap items-center gap-2 sm:gap-3",
            body: actions,
        })?,
    })?;
    let placeholder = if extension_fields.masked.contains("email")
        && extension_fields.masked.contains("jurisdiction")
    {
        "Search by name, code, or legal name..."
    } else if extension_fields.masked.contains("email") {
        "Search by name, code, legal name, or jurisdiction..."
    } else if extension_fields.masked.contains("jurisdiction") {
        "Search by name, code, legal name, or email..."
    } else {
        "Search by name, code, legal name, email, or jurisdiction..."
    };
    let mut toolbar = render(&FilterToolbar::new(
        "companies-filters",
        vec![
            ToolbarControl::search(
                "companies-search",
                "search",
                "Search companies",
                &params.q,
                placeholder,
            ),
            ToolbarControl::select(
                "companies-status-filter",
                "status_filter",
                "Status filter",
                &params.status,
                choices(&[
                    ("All statuses", ""),
                    ("Active", "active"),
                    ("Suspended", "suspended"),
                    ("Pending", "pending"),
                    ("Archived", "archived"),
                ]),
            )
            .grid_field("companies.status"),
        ],
    ))?;
    toolbar.push(&field_filter_controls(&extension_fields, data)?);
    let ids: Vec<i64> = data.rows.iter().map(|row| row.id).collect();
    let choice_labels = extension_choice_labels(client, scope, &extension_fields).await?;
    let facts=client.query("SELECT c.id,p.name,(t.company_id IS NOT NULL) FROM companies c LEFT JOIN companies p ON p.id=c.parent_id AND p.tenant_id=c.tenant_id AND p.deleted_at IS NULL LEFT JOIN tenant_primary_companies t ON t.tenant_id=c.tenant_id AND t.company_id=c.id WHERE c.tenant_id=$1 AND c.id=ANY($2)",&[&scope.tenant(),&ids]).await?;
    let mut rows = columns.rows.clone();
    for row in &mut rows {
        let company = data
            .rows
            .iter()
            .find(|c| c.id.to_string() == row.key)
            .ok_or(Error::Internal)?;
        let href = format!("/companies/{}", company.id);
        let id = company.id.to_string();
        let name = render(&RecordLink {
            workspace,
            kind: "core/company",
            record_id: &id,
            href: &href,
            class: "font-medium text-ink-strong hover:underline",
            body: Html::text(&company.name),
        })?;
        let name = render(&CompanyName {
            link: name,
            legal: if columns.mode() == RowMode::Normal {
                company
                    .legal_name
                    .as_deref()
                    .filter(|name| *name != company.name)
            } else {
                None
            },
        })?;
        row.slots.insert("name".into(), name);
        row.slots.insert(
            "code".into(),
            render(&Code {
                text: &company.code,
            })?,
        );
        row.slots.insert("status".into(), badge(&company.status)?);
        row.slots.insert(
            "jurisdiction".into(),
            render(&ReadValue {
                value: company.jurisdiction.as_deref(),
                empty: "—",
            })?,
        );
        if let Some(fact) = facts.iter().find(|r| r.get::<_, i64>(0) == company.id) {
            row.slots.insert(
                "parent_name".into(),
                render(&ReadValue {
                    value: fact.get::<_, Option<String>>(1).as_deref(),
                    empty: "None",
                })?,
            );
            if fact.get::<_, bool>(2) {
                row.actions = render(&Badge::status(BadgeKind::Neutral, "Primary"))?;
            }
        }
        // Where a company may go from here (issue 265): the row drags onto a
        // page that accepts companies and offers the same shortcut as a link.
        if !add_to.is_empty() {
            row.record = Some(("company".into(), id.clone()));
            row.actions.push(&render(&AddToMenu::new(
                format!("company-{id}-add-to"),
                company.name.clone(),
                add_to
                    .iter()
                    .map(|target| AddToItem {
                        label: target.label.into(),
                        href: target.href(&[bilibi_records::Reference::new("company", id.clone())]),
                        disabled: None,
                    })
                    .collect(),
            ))?);
        }
        if let Some(fields) = extension_fields.values.get(&company.id) {
            for (_, definition) in
                crate::field_contributions::definitions().map_err(|_| Error::Internal)?
            {
                let slot = fields::ALL
                    .iter()
                    .chain(
                        bilibi_grid::installed()
                            .map_err(|_| Error::Internal)?
                            .table("companies")
                            .ok_or(Error::Internal)?
                            .record_fields()
                            .iter(),
                    )
                    .find(|field| field.id == definition.id)
                    .and_then(|field| field.slot_id())
                    .ok_or(Error::Internal)?;
                if columns.columns.iter().any(|column| column.id == slot) {
                    let value = fields.get(definition.id).and_then(Value::as_str);
                    let display_value = match (definition.kind, value) {
                        (crate::CompanyFieldKind::Date, Some(value)) => value
                            .parse::<jiff::civil::Date>()
                            .ok()
                            .map(|date| extension_fields.display.render_date(date))
                            .unwrap_or_else(|| value.to_owned()),
                        (_, Some(value)) => value.to_owned(),
                        (_, None) => String::new(),
                    };
                    row.slots.insert(
                        slot.clone(),
                        if extension_fields.masked.contains(definition.id) {
                            restricted(&format!("company-{}-restricted", definition.id))?
                        } else {
                            render(&ReadValue {
                                value: (!display_value.is_empty())
                                    .then_some(display_value.as_str()),
                                empty: "—",
                            })?
                        },
                    );
                    if !extension_fields.masked.contains(definition.id)
                        && matches!(definition.kind, crate::CompanyFieldKind::Choice { .. })
                    {
                        if let Some(code) = value {
                            if let Some(label) = choice_labels
                                .get(definition.id)
                                .and_then(|labels| labels.get(code))
                            {
                                row.slots.insert(slot, Html::text(label));
                            }
                        }
                    }
                }
            }
        }
    }
    let custom_fields = crate::field_contributions::custom_fields()?;
    for row in &mut rows {
        let id: i64 = row.key.parse().map_err(|_| Error::Internal)?;
        for field in &custom_columns {
            let definition = field.custom.as_ref().ok_or(Error::Internal)?;
            let held = extension_fields
                .custom_values
                .get(&id)
                .and_then(|values| values.get(&definition.key));
            let value = held.and_then(Value::as_str);
            if field.kind == crate::list_fields::ListKind::Reference {
                let pill = reference_pill(
                    &named,
                    definition,
                    value,
                    &format!("company-{id}-{}", field.id.replace('_', "-")),
                    workspace,
                )?;
                row.slots.insert(
                    field.id.clone(),
                    match pill {
                        Some(pill) => render(&pill)?,
                        None => render(&ReadValue {
                            value: None,
                            empty: "—",
                        })?,
                    },
                );
                continue;
            }
            let shown = match (&field.kind, value) {
                (crate::list_fields::ListKind::Choices, _) => {
                    custom_fields
                        .choice_text(client, scope, definition, held)
                        .await?
                }
                (crate::list_fields::ListKind::Boolean, Some(value)) => match value {
                    "true" => "Yes".into(),
                    "false" => "No".into(),
                    _ => value.to_owned(),
                },
                (crate::list_fields::ListKind::Date, Some(value)) => value
                    .parse::<jiff::civil::Date>()
                    .ok()
                    .map(|date| extension_fields.display.render_date(date))
                    .unwrap_or_else(|| value.to_owned()),
                (crate::list_fields::ListKind::Choice { .. }, Some(code)) => choice_labels
                    .get(&field.id)
                    .and_then(|labels| labels.get(code))
                    .cloned()
                    .unwrap_or_else(|| code.to_owned()),
                (_, Some(value)) => value.to_owned(),
                (_, None) => String::new(),
            };
            row.slots.insert(
                field.id.clone(),
                render(&ReadValue {
                    value: (!shown.is_empty()).then_some(shown.as_str()),
                    empty: "—",
                })?,
            );
        }
    }
    let mut table = columns.flex_table("companies", rows).sorted(
        Some(&params.sort),
        Some(if params.dir == "desc" {
            SortDir::Desc
        } else {
            SortDir::Asc
        }),
    );
    table.caption = "Companies";
    table.framed = false;
    table.actions = true;
    let filtered =
        !params.q.is_empty() || !params.status.is_empty() || !params.field_filters.is_empty();
    let title = if data.limited {
        "Company list limited".into()
    } else if filtered {
        format!(
            "No {}companies{}",
            if params.status.is_empty() {
                String::new()
            } else {
                format!("{} ", params.status)
            },
            if params.q.is_empty() {
                String::new()
            } else {
                format!(" match “{}”", params.q)
            }
        )
    } else {
        "No companies yet".into()
    };
    let recovery = if filtered {
        render(&Button {
            id: "companies-clear",
            event: "clear_filters",
            body: Html::text("Clear search and filter"),
            ..Button::default()
        })?
    } else {
        Html::EMPTY
    };
    table.empty = Some(TableEmpty::State(
        EmptyState::missing(
            &title,
            if data.limited {
                &data.notice
            } else if filtered {
                "Clear the search and filter to see every company."
            } else {
                "Create your first company."
            },
        )
        .action(recovery),
    ));
    let mut contents = render(&table)?;
    contents.push(&render(&Pagination::new(
        "companies-pagination",
        PageCounts::new(params.page as u64, params.size as u64, data.total as u64),
        &[25, 50, 100, 300],
    ))?);
    page(
        "companies-index",
        PageVariant::List,
        render(&ListBody {
            header,
            toolbar,
            card: card("companies-card", contents, true)?,
        })?,
    )
}

/// Names every record the tenant's record fields `definitions` hold among
/// `values`, in one call with the reader's own access, keyed by the
/// reference as stored. A record the reader cannot see comes back unnamed.
pub(super) async fn custom_references(
    scope: &Scope,
    definitions: &[&bilibi_custom_fields::Definition],
    values: &[&serde_json::Map<String, Value>],
) -> Result<HashMap<bilibi_records::Reference, bilibi_records::Resolved>> {
    let mut references: BTreeSet<bilibi_records::Reference> = BTreeSet::new();
    for definition in definitions
        .iter()
        .filter(|definition| definition.kind == bilibi_custom_fields::Kind::Reference)
    {
        let target = definition.target.as_deref().ok_or(Error::Internal)?;
        references.extend(
            values
                .iter()
                .filter_map(|values| values.get(&definition.key)?.as_str())
                .filter(|id| !id.is_empty())
                .map(|id| bilibi_records::Reference::new(target, id)),
        );
    }
    if references.is_empty() {
        return Ok(HashMap::new());
    }
    let references: Vec<_> = references.into_iter().collect();
    Ok(crate::field_contributions::custom_fields()?
        .records()
        .resolve(scope, &references)
        .await?
        .into_iter()
        .map(|resolved| (resolved.reference.clone(), resolved))
        .collect())
}

/// A record field's value as the shared pill (unavailable when the reader
/// cannot see it, never its raw id); `None` when it holds none.
fn reference_pill(
    named: &HashMap<bilibi_records::Reference, bilibi_records::Resolved>,
    definition: &bilibi_custom_fields::Definition,
    id: Option<&str>,
    dom_id: &str,
    workspace: bool,
) -> Result<Option<RelatedRecord>> {
    let Some(id) = id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let target = definition.target.as_deref().ok_or(Error::Internal)?;
    let resolved = named
        .get(&bilibi_records::Reference::new(target, id))
        .ok_or(Error::Internal)?;
    Ok(Some(resolved.pill(format!("{dom_id}-pill"), workspace)))
}

pub(super) struct ExtensionListFields<'a> {
    pub masked: &'a HashSet<String>,
    pub values: &'a std::collections::BTreeMap<i64, crate::CompanyFieldValues>,
    /// The tenant's custom values of the companies on the page.
    pub custom_values: &'a std::collections::BTreeMap<i64, serde_json::Map<String, Value>>,
    pub display: &'a bilibi_datetime::DisplayContext,
    pub picklists: &'a bilibi_picklists::PickLists,
    pub filters: &'a [crate::FieldFilter],
    pub available: &'a [crate::list_fields::AvailableField],
    pub filter_notice: &'a str,
}

async fn extension_choice_labels(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    fields: &ExtensionListFields<'_>,
) -> Result<HashMap<String, HashMap<String, String>>> {
    let mut labels: HashMap<String, HashMap<String, String>> = HashMap::new();
    for available in fields.available {
        let crate::list_fields::ListKind::Choice { picklist } = &available.kind else {
            continue;
        };
        let codes = match available.custom_key() {
            Some(key) => fields
                .custom_values
                .values()
                .filter_map(|values| values.get(key))
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>(),
            None => fields
                .values
                .values()
                .filter_map(|values| values.get(available.id.as_str()))
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>(),
        };
        for code in codes {
            if let Some(choice) = available.choices.iter().find(|choice| choice.value == code) {
                labels
                    .entry(available.id.clone())
                    .or_default()
                    .insert(code, choice.label.clone());
            } else if let Some(label) = fields
                .picklists
                .label(client, scope, picklist, &code)
                .await?
            {
                let text = if label.retired {
                    format!("{} (Retired)", label.text)
                } else {
                    label.text
                };
                labels
                    .entry(available.id.clone())
                    .or_default()
                    .insert(code, text);
            }
        }
    }
    Ok(labels)
}

#[derive(Template)]
#[template(
    source = "<code class=\"text-xs font-medium tabular-nums\">{{ text }}</code>",
    ext = "html"
)]
struct Code<'a> {
    text: &'a str,
}

struct Options {
    types: Vec<Choice>,
    countries: Vec<Choice>,
}
async fn options(client: &(impl GenericClient + Sync), create: bool) -> Result<Options> {
    let types = crate::reference_types::list_legal_entity_types(
        client,
        crate::reference_types::TypeSort::Name,
        crate::reference_types::Direction::Asc,
    )
    .await?
    .into_iter()
    .filter(|t| !create || t.is_active)
    .map(|t| Choice::new(t.name, t.id.to_string()))
    .collect();
    let countries = client
        .query(
            "SELECT iso,country FROM geonames_countries ORDER BY country",
            &[],
        )
        .await?
        .into_iter()
        .map(|r| Choice::new(r.get::<_, String>(1), r.get::<_, String>(0)))
        .collect();
    Ok(Options { types, countries })
}
/// The parent picker: a search over the tenant's companies (Base Records,
/// under the reader's own permissions) instead of a list of every company.
/// The owner still checks the chosen parent on save.
pub(super) struct ParentPicker<'a> {
    pub query: &'a str,
    pub hits: Vec<PickerHit>,
    /// The create form's chosen parent: its id and name.
    pub chosen: Option<(i64, &'a str)>,
    /// The record page's Change was pressed: search instead of the pill.
    pub changing: bool,
}
pub(super) const PARENT_FORM: &str = "parent";
/// The record pickers of the tenant's record fields, each keyed by its
/// `cf_<key>` name: the typed search and its hits, and the create form's
/// chosen record (the record page saves a pick at once).
#[derive(Default)]
pub(super) struct CustomPickers {
    pub queries: HashMap<String, String>,
    pub hits: HashMap<String, Vec<PickerHit>>,
    pub chosen: HashMap<String, bilibi_records::Resolved>,
}
/// The form namespace a record field's search posts in
/// (`custom_reference[cf_<key>]` for the held choice).
pub(super) const CUSTOM_REFERENCE_FORM: &str = "custom_reference";
/// The record page's search event of a record field's picker.
pub(super) const CUSTOM_REFERENCE_SEARCH: &str = "custom_reference_search";
/// Every record field's pick and clear events, with the field's name
/// (`cf_<key>`) as `key`: fixed names, so the page declares them with their
/// contract.
pub(super) const CUSTOM_PICK: &str = "pick_custom_reference";
pub(super) const CUSTOM_CLEAR: &str = "clear_custom_reference";
/// The picker of the record field `definition` (named `name`), with the
/// record it holds or was chosen, if any, and its refusal beneath.
/// `saves` is true on the record page, where clearing saves at once.
fn custom_picker(
    pickers: &CustomPickers,
    definition: &bilibi_custom_fields::Definition,
    name: &str,
    selected: Option<(String, RelatedRecord)>,
    errors: Vec<String>,
    saves: bool,
) -> Result<Html> {
    let picker = RecordPicker::new(
        &format!("company-{}", name.replace('_', "-")),
        CUSTOM_REFERENCE_FORM,
        name,
        &definition.label,
        pickers
            .queries
            .get(name)
            .map(String::as_str)
            .unwrap_or_default(),
        pickers
            .hits
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or_default(),
        selected
            .as_ref()
            .map(|(value, pill)| (value.as_str(), pill.clone())),
        (CUSTOM_PICK, CUSTOM_CLEAR),
        errors,
    )
    .key(name)
    .search_name(format!("custom_reference_search[{name}]"));
    render(&if saves { picker.in_fact() } else { picker })
}
fn parent_pill(id: i64, label: &str, link: bool, workspace: bool) -> RelatedRecord {
    RelatedRecord::new(
        "company-parent-pill",
        "core/company",
        id.to_string(),
        RecordState::Named {
            label: label.to_owned(),
            href: link.then(|| format!("/companies/{id}")),
        },
    )
    .workspace(workspace)
}
/// `label` names the search box: the record page's fact already says
/// "Parent Company", the create form's field does not.
fn parent_picker(
    picker: &ParentPicker<'_>,
    label: &str,
    selected: Option<(i64, RelatedRecord)>,
    events: (&str, &str),
    errors: Vec<String>,
) -> Result<Html> {
    let value = selected
        .as_ref()
        .map(|(id, pill)| (format!("company:{id}"), pill.clone()));
    render(&RecordPicker::new(
        "company-parent",
        PARENT_FORM,
        "parent",
        label,
        picker.query,
        &picker.hits,
        value
            .as_ref()
            .map(|(value, pill)| (value.as_str(), pill.clone())),
        events,
        errors,
    ))
}
fn choice(
    words: &bilibi_grid::RecordWords,
    id: &str,
    name: &str,
    value: String,
    options: Vec<Choice>,
    status: CommitStatus,
    company: &Company,
    editable: bool,
) -> Result<Html> {
    let display = options
        .iter()
        .find(|c| c.value == value)
        .map(|c| c.label.as_str())
        .unwrap_or("—");
    let editor = if name == "jurisdiction" {
        render(
            &Combobox::new(format!("{id}-input"), name)
                .value(&value)
                .options(options.clone())
                .placeholder("None")
                .aria_label(words.get(&fields::JURISDICTION))
                .wrapper_class("mb-0"),
        )?
    } else {
        render(
            &Field::named(name)
                .id(format!("{id}-input"))
                .kind(InputKind::Select)
                .value(&value)
                .prompt("None")
                .options(options.clone())
                .aria_label(words.get(&fields::get(name)))
                .wrapper_class("mb-0"),
        )?
    };
    let display = if value.is_empty() {
        render(&MutedValue { text: display })?
    } else {
        Html::text(display)
    };
    let mut field = InlineField::new(
        id,
        name,
        format!("Edit {}", words.get(&fields::get(name))),
        value,
    )
    .version(company.version)
    .status(status)
    .allow_empty();
    field.id_value = company.id.to_string();
    field.editable = editable;
    render(&InlineChoice {
        field,
        display,
        editor,
    })
}
#[derive(Template)]
#[template(path = "detail.html")]
struct DetailBody {
    id: i64,
    header: Html,
    sections: Html,
    modal: Html,
}
#[derive(Template)]
#[template(path = "company_status.html")]
struct StatusBody {
    badge: Html,
    operations: Vec<(&'static str, &'static str)>,
    final_status: bool,
}
fn operation_name(op: crate::lifecycle::Operation) -> (&'static str, &'static str) {
    use crate::lifecycle::Operation::*;
    match op {
        Activate => ("activate", "Activate"),
        Reactivate => ("reactivate", "Reactivate"),
        Suspend => ("suspend", "Suspend"),
        Archive => ("archive", "Archive"),
    }
}
/// What the detail page's live state hands its view: commit outcomes, the
/// open dialog, the notice and the Departments table arrangement.
pub(super) struct DetailState<'a> {
    pub facts: &'a [bilibi_ports::RecordFactValue],
    pub statuses: &'a CommitStates,
    pub pending: Option<&'a Pending>,
    pub notice: &'a str,
    pub pending_activity: Option<&'a (String, i64)>,
    pub departments_columns: &'a mut ListColumns,
    pub children_columns: &'a ListColumns,
    pub workspace: bool,
    pub display: &'a bilibi_datetime::DisplayContext,
    /// Anchors of the contributed record panels the reader may mount.
    pub panels: &'a [&'a str],
    pub parent: ParentPicker<'a>,
    pub custom: &'a CustomPickers,
}
pub(super) async fn detail(
    db: &bilibi_database::Db,
    scope: &Scope,
    id: i64,
    picklists: &bilibi_picklists::PickLists,
    state: DetailState<'_>,
) -> Result<String> {
    let DetailState {
        facts: key_facts,
        statuses,
        pending,
        notice,
        pending_activity,
        departments_columns,
        children_columns,
        workspace,
        display,
        panels,
        parent,
        custom,
    } = state;
    let (company, masked, custom_defs, held) = {
        let connection = db.get().await?;
        let client = &**connection;
        let (company, masked) = crate::detail(client, scope, id).await?;
        let custom_defs = crate::field_contributions::custom_fields()?
            .active(
                client,
                scope,
                crate::field_contributions::CUSTOM_RECORD_TYPE,
            )
            .await?;
        let held = crate::field_contributions::custom_read(client, scope, &[id])
            .await?
            .remove(&id)
            .unwrap_or_default();
        (company, masked, custom_defs, held)
    };
    let named = custom_references(scope, &custom_defs.iter().collect::<Vec<_>>(), &[&held]).await?;
    let connection = db.get().await?;
    let client = &**connection;
    let words = bilibi_grid::RecordWords::load(client, scope, "companies").await?;
    let editable = allowed(client, scope, crate::UPDATE).await? && !company.archived();
    let can_target = allowed(client, scope, crate::TENANT_WIDE).await?
        || crate::organisation::reaches_without_tenant_wide(scope, id);
    let editable = editable && can_target;
    let hierarchy = crate::hierarchy::hierarchy(client, scope, id).await?;
    let mut chain = Html::EMPTY;
    if let Some(higher) = hierarchy.ancestors.first().map(|node| node.level - 1) {
        if higher > 0 {
            chain.push(&Html::text(&format!(
                "… {higher} higher level{} / ",
                if higher == 1 { "" } else { "s" }
            )));
        }
    }
    for (index, node) in hierarchy.ancestors.iter().enumerate() {
        if index > 0 {
            chain.push(&Html::text(" / "));
        }
        chain.push(&render(&ActionLink {
            id: &format!("company-ancestor-{}", node.id),
            href: &format!("/companies/{}", node.id),
            title: &node.name,
            body: Html::text(&node.name),
            ..ActionLink::default()
        })?);
    }
    if !hierarchy.ancestors.is_empty() {
        chain = render(&Facts {
            id: "company-parent-chain",
            items: vec![Fact {
                title: "Parent chain",
                id: "company-parent-chain-links",
                value: chain,
            }],
        })?;
    }
    let primary = crate::primary::is_primary(client, scope, id).await?;
    let opts = options(client, false).await?;
    let mut actions = badge(&company.status)?;
    actions.push(&render(&LivePanelAnchor {
        id: "company-record-history",
    })?);
    actions.push(&back("company-back")?);
    let header = render(&Header {
        title_actions: Html::EMPTY,
        title: Html::text(&company.name),
        subtitle: company
            .legal_name
            .as_deref()
            .filter(|n| *n != company.name)
            .unwrap_or(&company.code),
        actions: render(&Actions {
            class: "flex items-center gap-3",
            body: actions,
        })?,
    })?;
    let mut sections = vec![
        render(&RecordFacts {
            id: "company-key-facts",
            items: key_facts
                .iter()
                .map(|fact| StatItem::text(&fact.label, &fact.value))
                .collect(),
        })?,
        chain,
    ];
    for (show, kind, message) in [
        (
            primary,
            StatusKind::Info,
            "This is your organisation's primary company.",
        ),
        (
            company.archived(),
            StatusKind::Warning,
            "This company is archived and read-only. Archiving is final.",
        ),
        (
            !notice.is_empty(),
            if notice == "Company status changed." {
                StatusKind::Success
            } else {
                StatusKind::Error
            },
            notice,
        ),
    ] {
        if show {
            sections.push(render(&Alert {
                kind,
                id: "",
                class: "",
                body: Html::text(message),
            })?);
        }
    }
    let mut facts = Vec::new();
    for (name, value) in [
        ("name", company.name.as_str()),
        ("code", company.code.as_str()),
        ("legal_name", company.legal_name.as_deref().unwrap_or("")),
    ] {
        facts.push(Fact {
            title: words.get(&fields::get(name)),
            id: "",
            value: text_fact(
                &words,
                &company,
                name,
                value,
                editable,
                false,
                statuses.get(name),
            )?,
        });
    }
    facts.push(Fact {
        title: words.get(&fields::STATUS),
        id: "detail-status",
        value: render(&StatusBody {
            badge: badge(&company.status)?,
            operations: if editable {
                crate::lifecycle::operations_from(&company.status)
                    .into_iter()
                    .map(operation_name)
                    .collect()
            } else {
                Vec::new()
            },
            final_status: company.archived(),
        })?,
    });
    facts.push(Fact {
        title: words.get(&fields::LEGAL_ENTITY_TYPE_ID),
        id: "detail-legal-entity-type",
        value: choice(
            &words,
            "company-legal-entity-type",
            "legal_entity_type_id",
            company
                .legal_entity_type_id
                .map(|v| v.to_string())
                .unwrap_or_default(),
            opts.types,
            statuses.get("legal_entity_type_id"),
            &company,
            editable,
        )?,
    });
    for (name, value) in [
        (
            "registration_number",
            company.registration_number.as_deref().unwrap_or(""),
        ),
        ("tax_id", company.tax_id.as_str()),
    ] {
        facts.push(Fact {
            title: words.get(&fields::get(name)),
            id: "",
            value: text_fact(
                &words,
                &company,
                name,
                value,
                editable,
                masked.contains(name),
                statuses.get(name),
            )?,
        });
    }
    facts.push(Fact {
        title: words.get(&fields::JURISDICTION),
        id: "detail-jurisdiction",
        value: if masked.contains("jurisdiction") {
            restricted("company-jurisdiction-restricted")?
        } else {
            choice(
                &words,
                "company-jurisdiction",
                "jurisdiction",
                company.jurisdiction.clone().unwrap_or_default(),
                opts.countries,
                statuses.get("jurisdiction"),
                &company,
                editable,
            )?
        },
    });
    for (name, value) in [
        ("email", company.email.as_str()),
        ("website", company.website.as_deref().unwrap_or("")),
    ] {
        facts.push(Fact {
            title: words.get(&fields::get(name)),
            id: "",
            value: text_fact(
                &words,
                &company,
                name,
                value,
                editable,
                masked.contains(name),
                statuses.get(name),
            )?,
        });
    }
    let contributed = crate::field_contributions::read(client, scope, &[id]).await?;
    let contributed = contributed.get(&id).cloned().unwrap_or_default();
    for (_, definition) in crate::field_contributions::definitions().map_err(|_| Error::Internal)? {
        let value = contributed
            .get(definition.id)
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id_attr = format!("company-{}", definition.id.replace('_', "-"));
        let value_html = if masked.contains(definition.id) {
            restricted(&id_attr)?
        } else if let crate::CompanyFieldKind::Choice { picklist } = definition.kind {
            let label = if value.is_empty() {
                None
            } else {
                picklists.label(client, scope, picklist, value).await?
            };
            let display_text = label
                .map(|label| {
                    if label.retired {
                        format!("{} (Retired)", label.text)
                    } else {
                        label.text
                    }
                })
                .unwrap_or_else(|| "—".into());
            let mut field = InlineField::new(
                id_attr,
                definition.id,
                format!("Edit {}", words.field(definition.id, definition.label)),
                value,
            )
            .version(company.version)
            .allow_empty()
            .status(statuses.get(definition.id));
            field.save_event = "save_contributed_field".into();
            field.id_value = id.to_string();
            field.editable = editable;
            let editor = Combobox::new(format!("{}-input", definition.id), definition.id)
                .value(value)
                .options(picklists.choices(client, scope, picklist, None).await?)
                .wrapper_class("mb-0");
            render(&InlineChoice {
                field,
                display: Html::text(&display_text),
                editor: render(&editor)?,
            })?
        } else {
            let (input_type, display_value) = match definition.kind {
                crate::CompanyFieldKind::Text { .. } => ("text", None),
                crate::CompanyFieldKind::Date => (
                    "date",
                    (!value.is_empty()).then(|| {
                        value
                            .parse::<jiff::civil::Date>()
                            .ok()
                            .map(|date| display.render_date(date))
                            .unwrap_or_else(|| value.to_owned())
                    }),
                ),
                crate::CompanyFieldKind::Choice { .. } => unreachable!("handled above"),
            };
            let mut field = InlineField::new(
                id_attr,
                definition.id,
                format!("Edit {}", words.field(definition.id, definition.label)),
                value,
            )
            .version(company.version)
            .allow_empty()
            .status(statuses.get(definition.id))
            .input_type(input_type);
            field.save_event = "save_contributed_field".into();
            if let Some(display_value) = display_value {
                field = field.display_value(display_value);
            }
            field.id_value = id.to_string();
            field.editable = editable;
            render(&InlineEdit { field })?
        };
        facts.push(Fact {
            title: words.field(definition.id, definition.label),
            id: definition.id,
            value: value_html,
        });
    }
    let custom_fields = crate::field_contributions::custom_fields().map_err(|_| Error::Internal)?;
    if !custom_defs.is_empty() {
        for definition in &custom_defs {
            let name = crate::field_contributions::custom_name(&definition.key);
            let stored = held.get(&definition.key);
            let value = stored.and_then(Value::as_str).unwrap_or_default();
            let id_attr = format!("company-{}", name.replace('_', "-"));
            if definition.kind == bilibi_custom_fields::Kind::Reference {
                let pill = reference_pill(&named, definition, Some(value), &id_attr, workspace)?;
                let value_html = if editable {
                    let errors = match statuses.get(&name) {
                        CommitStatus::Error(message) => vec![message],
                        _ => Vec::new(),
                    };
                    let target = definition.target.as_deref().ok_or(Error::Internal)?;
                    let selected = pill.map(|pill| (format!("{target}:{value}"), pill));
                    render(&Form {
                        id: &format!("{id_attr}-form"),
                        event: CUSTOM_REFERENCE_SEARCH,
                        change: true,
                        body: custom_picker(custom, definition, &name, selected, errors, true)?,
                        ..Form::default()
                    })?
                } else {
                    match pill {
                        Some(pill) => render(&pill)?,
                        None => render(&MutedValue { text: "—" })?,
                    }
                };
                facts.push(Fact {
                    title: &definition.label,
                    id: "",
                    value: value_html,
                });
                continue;
            }
            // Several choices edit as their codes joined by commas (a code
            // never holds one), the shape the shared editor saves.
            let codes = bilibi_custom_fields::codes_of(stored);
            let inline_value = if definition.kind == bilibi_custom_fields::Kind::Choices {
                codes.join(",")
            } else {
                value.to_owned()
            };
            let mut field = InlineField::new(
                id_attr,
                name.clone(),
                format!("Edit {}", definition.label),
                inline_value,
            )
            .version(company.version)
            .allow_empty()
            .status(statuses.get(&name));
            field.save_event = "save_custom_field".into();
            field.id_value = id.to_string();
            field.editable = editable;
            if let Some(help) = &definition.help {
                field.hint = help.clone();
            }
            let value_html = match definition.kind {
                bilibi_custom_fields::Kind::Choice => {
                    let display_text = custom_fields
                        .choice_text(client, scope, definition, stored)
                        .await?;
                    let editor = Combobox::new(format!("{name}-input"), &name)
                        .value(value)
                        .options(
                            custom_fields
                                .choice_options(client, scope, definition, &[value])
                                .await?,
                        )
                        .wrapper_class("mb-0");
                    render(&InlineChoice {
                        field,
                        display: Html::text(if display_text.is_empty() {
                            "—"
                        } else {
                            &display_text
                        }),
                        editor: render(&editor)?,
                    })?
                }
                bilibi_custom_fields::Kind::Choices => {
                    let display_text = custom_fields
                        .choice_text(client, scope, definition, stored)
                        .await?;
                    let editor = MultiSelect::new(format!("{name}-input"), &name)
                        .label(&definition.label)
                        .in_fact()
                        .options(
                            custom_fields
                                .choice_options(client, scope, definition, &codes)
                                .await?,
                        )
                        .values(codes.iter().map(|code| (*code).to_owned()).collect())
                        .wrapper_class("mb-0");
                    render(&InlineChoice {
                        field,
                        display: Html::text(if display_text.is_empty() {
                            "—"
                        } else {
                            &display_text
                        }),
                        editor: render(&editor)?,
                    })?
                }
                bilibi_custom_fields::Kind::Date => {
                    field = field.input_type("date");
                    if let Ok(date) = value.parse::<jiff::civil::Date>() {
                        field = field.display_value(display.render_date(date));
                    }
                    render(&InlineEdit { field })?
                }
                bilibi_custom_fields::Kind::LongText => {
                    render(&bilibi_ui::InlineLongText { field, rows: 5 })?
                }
                bilibi_custom_fields::Kind::Boolean => {
                    let editor = Field::named(&name)
                        .id(format!("{name}-input"))
                        .label(&definition.label)
                        .value(value)
                        .kind(InputKind::Switch);
                    render(&InlineChoice {
                        field,
                        display: Html::text(match value {
                            "true" => "Yes",
                            "false" => "No",
                            _ => "—",
                        }),
                        editor: render(&editor)?,
                    })?
                }
                bilibi_custom_fields::Kind::Email => render(&InlineEdit {
                    field: field.input_type("email"),
                })?,
                bilibi_custom_fields::Kind::Phone => render(&InlineEdit {
                    field: field.input_type("tel"),
                })?,
                bilibi_custom_fields::Kind::Link => render(&InlineEdit {
                    field: field.input_type("url"),
                })?,
                bilibi_custom_fields::Kind::Number => {
                    field = field.input_type("text");
                    render(&InlineEdit { field })?
                }
                bilibi_custom_fields::Kind::Text => render(&InlineEdit { field })?,
                bilibi_custom_fields::Kind::Reference => unreachable!("handled above"),
            };
            facts.push(Fact {
                title: &definition.label,
                id: "",
                value: value_html,
            });
        }
    }
    let current = match company.parent_id {
        Some(parent) => client
            .query_opt(
                "SELECT COALESCE(NULLIF(legal_name,''),name) FROM companies WHERE tenant_id=$1 AND id=$2 AND deleted_at IS NULL",
                &[&scope.tenant(), &parent],
            )
            .await?
            .map(|row| parent_pill(parent, &row.get::<_, String>(0), true, workspace))
            .map(|pill| (parent, pill)),
        None => None,
    };
    let value = if editable {
        let errors = match statuses.get("parent_id") {
            CommitStatus::Error(message) => vec![message],
            _ => Vec::new(),
        };
        let (selected, actions) = if parent.changing || current.is_none() {
            let mut actions = Html::EMPTY;
            if parent.changing {
                actions.push(&render(&Button {
                    id: "company-parent-keep",
                    event: "keep_parent",
                    button_type: "button",
                    body: Html::text("Keep current"),
                    ..Button::default()
                })?);
            }
            if current.is_some() {
                actions.push(&render(&Button {
                    id: "company-parent-remove",
                    event: "remove_parent",
                    button_type: "button",
                    body: Html::text("Remove parent"),
                    ..Button::default()
                })?);
            }
            (None, actions)
        } else {
            (current, Html::EMPTY)
        };
        render(&Form {
            id: "company-parent-form",
            event: "parent_search",
            change: true,
            body: parent_picker(
                &parent,
                words.get(&fields::PARENT_ID),
                selected,
                ("pick_parent", "change_parent"),
                errors,
            )?,
            actions,
            ..Form::default()
        })?
    } else {
        match current {
            Some((_, pill)) => render(&pill)?,
            None => render(&MutedValue { text: "None" })?,
        }
    };
    facts.push(Fact {
        title: words.get(&fields::PARENT_ID),
        id: "detail-parent",
        value,
    });
    let context = client
        .query_one(
            "SELECT scope_activities,metadata FROM companies WHERE tenant_id=$1 AND id=$2",
            &[&scope.tenant(), &id],
        )
        .await?;
    for (index, name) in [(0, "scope_activities"), (1, "metadata")] {
        let value: Option<Value> = context.get(index);
        if name == "scope_activities" && value.as_ref().is_none_or(|value| value.is_array()) {
            let activities = value
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            let mut field = InlineField::new(
                "company-new-activity",
                "scope_activities",
                format!("Add {}", words.get(&fields::ACTIVITIES)),
                "",
            )
            .version(company.version)
            .status(statuses.get("scope_activities"))
            .placeholder("Add activity");
            field.id_value = id.to_string();
            field.class = "min-w-56".into();
            field.save_event = "add_activity".into();
            field.editable = editable;
            facts.push(Fact {
                title: words.get(&fields::get(name)),
                id: "scope-activities-section",
                value: render(&ActivityBody {
                    activities,
                    editable,
                    add: render(&InlineEdit { field })?,
                })?,
            });
            continue;
        }
        let mut field = InlineField::new(
            format!("company-{name}"),
            name,
            format!("Edit {}", words.get(&fields::get(name))),
            value
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                .unwrap_or_default(),
        )
        .version(company.version)
        .allow_empty()
        .status(statuses.get(name));
        field.id_value = id.to_string();
        field.editable = editable;
        field.class = "font-mono text-xs".into();
        field.input_class = "font-mono text-xs".into();
        facts.push(Fact {
            title: words.get(&fields::get(name)),
            id: "",
            value: render(&InlineLongText { field, rows: 5 })?,
        });
    }
    let mut details = render(&SectionHeading {
        id: "company-details-heading",
        title: "Company Details",
        ..SectionHeading::default()
    })?;
    details.push(&render(&Facts {
        id: "company-details",
        items: facts,
    })?);
    sections.push(render(&Card {
        id: "company-details-card",
        inner_class: "p-5 sm:p-6",
        role: "region",
        labelledby: "company-details-heading",
        body: details,
        ..Card::default()
    })?);
    // Child-owning slices contribute the remaining panels; subsidiaries are
    // Company-owned and can be shown immediately without an upward dependency.
    // Keep the platform's bounded read; the extra row makes truncation explicit.
    let children = client.query(
        "SELECT c.id,c.name,c.status,t.name AS legal_entity_type_name,CASE WHEN $3 THEN NULL ELSE c.jurisdiction END AS jurisdiction FROM companies c LEFT JOIN company_legal_entity_types t ON t.id=c.legal_entity_type_id WHERE c.tenant_id=$1 AND c.parent_id=$2 AND c.deleted_at IS NULL ORDER BY c.id LIMIT 301",
        &[&scope.tenant(), &id, &masked.contains("jurisdiction")],
    ).await?;
    if !children.is_empty() {
        let truncated = children.len() > 300;
        let mut rows = Vec::new();
        for child in children.into_iter().take(300) {
            let key = child.get::<_, i64>(0).to_string();
            let href = format!("/companies/{key}");
            let name = child.get::<_, String>(1);
            let status = child.get::<_, String>(2);
            rows.push(
                FlexRow::new(&key)
                    .slot(
                        "name",
                        render(&RecordLink {
                            workspace,
                            kind: "core/company",
                            record_id: &key,
                            href: &href,
                            class: "font-medium hover:underline",
                            body: Html::text(&name),
                        })?,
                    )
                    .slot("status", badge(&status)?)
                    .slot(
                        "legal_entity_type",
                        Html::text(
                            child
                                .get::<_, Option<String>>("legal_entity_type_name")
                                .as_deref()
                                .unwrap_or("—"),
                        ),
                    )
                    .slot(
                        "jurisdiction",
                        if masked.contains("jurisdiction") {
                            render(&Restricted::new(format!("subsidiary-{key}-jurisdiction")))?
                        } else {
                            Html::text(
                                child
                                    .get::<_, Option<String>>("jurisdiction")
                                    .as_deref()
                                    .unwrap_or("—"),
                            )
                        },
                    ),
            );
        }
        let mut children_body = render(&SectionHeading {
            title: "Subsidiaries",
            count: Some(rows.len()),
            description: if truncated {
                "Showing the first 300 subsidiaries. Use Companies to search the full directory."
            } else {
                ""
            },
            ..SectionHeading::default()
        })?;
        let mut table = FlexTable::new(&children_columns.id, children_columns.flex_columns(), rows);
        table.event = "children_grid";
        table.zoom = children_columns.zoom;
        table.suggestions = children_columns.flex_suggestions();
        table.add_query = &children_columns.add_query;
        table.framed = false;
        table.caption = "Subsidiaries";
        children_body.push(&render(&table)?);
        sections.push(card("company-subsidiaries-card", children_body, true)?);
    }
    sections.push(departments_section(client, scope, id, departments_columns).await?);
    let modal = if let Some((activity, _)) = pending_activity {
        render(&ConfirmDialog {
            id: "company-remove-activity-dialog",
            consequence: &format!(
                "Business activity “{activity}” will be removed from this company."
            ),
            detail: "The company and its other activities are kept.",
            confirm: "Remove",
            working: "Removing…",
            confirm_event: "remove_activity",
            cancel_event: "cancel_remove_activity",
        })?
    } else if let Some(pending) = pending {
        let (_, verb) = operation_name(pending.operation);
        let consequence = format!(
            "{} will be {}.",
            company.name,
            match pending.operation {
                crate::lifecycle::Operation::Activate => "activated",
                crate::lifecycle::Operation::Reactivate => "reactivated",
                crate::lifecycle::Operation::Suspend => "suspended",
                crate::lifecycle::Operation::Archive => "archived",
            }
        );
        let detail = match pending.operation {
            crate::lifecycle::Operation::Archive => {
                "Its records are kept. Archiving is final: an archived company cannot be reactivated."
            }
            crate::lifecycle::Operation::Suspend => {
                "Its records are kept. Reactivating the company lifts the suspension."
            }
            crate::lifecycle::Operation::Activate => {
                "The company becomes active. It can be suspended later."
            }
            crate::lifecycle::Operation::Reactivate => {
                "The suspension is lifted and the company is active again."
            }
        };
        let mut reason = Field::named("reason")
            .id("company-lifecycle-reason")
            .label("Reason")
            .hint("Optional. Recorded with this change in the audit trail.")
            .kind(InputKind::Textarea)
            .value(&pending.reason)
            .attr("maxlength", "500")
            .errors(if pending.error.is_empty() {
                vec![]
            } else {
                vec![pending.error.clone()]
            });
        reason.attrs.autofocus = true;
        let body = render(&LifecycleForm {
            reason: render(&reason)?,
            cancel: render(&Button {
                id: "company-lifecycle-cancel",
                event: "cancel_lifecycle",
                button_type: "button",
                body: Html::text("Cancel"),
                ..Button::default()
            })?,
            confirm: render(&Button {
                id: "company-lifecycle-confirm",
                button_type: "submit",
                working: match pending.operation {
                    crate::lifecycle::Operation::Activate => "Activating…",
                    crate::lifecycle::Operation::Reactivate => "Reactivating…",
                    crate::lifecycle::Operation::Suspend => "Suspending…",
                    crate::lifecycle::Operation::Archive => "Archiving…",
                },
                variant: if pending.operation == crate::lifecycle::Operation::Archive {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Primary
                },
                body: Html::text(verb),
                ..Button::default()
            })?,
        })?;
        render(&Modal {
            id: "company-lifecycle-modal",
            title: &consequence,
            description: detail,
            cancel_event: "cancel_lifecycle",
            body,
            ..Modal::default()
        })?
    } else {
        Html::EMPTY
    };
    page(
        "company-detail-page",
        PageVariant::Detail,
        render(&DetailBody {
            id,
            header,
            sections: render(&LivePanelStack {
                parts: &sections,
                panel_ids: panels,
            })?,
            modal,
        })?,
    )
}
#[derive(Template)]
#[template(path = "company_lifecycle_form.html")]
struct LifecycleForm {
    reason: Html,
    cancel: Html,
    confirm: Html,
}
fn restricted(id: &str) -> Result<Html> {
    render(&Restricted::new(id))
}
fn text_fact(
    words: &bilibi_grid::RecordWords,
    company: &Company,
    name: &str,
    value: &str,
    editable: bool,
    masked: bool,
    status: CommitStatus,
) -> Result<Html> {
    let id = format!("company-{}", name.replace('_', "-"));
    if masked {
        return restricted(&id);
    }
    let mut field = InlineField::new(
        id,
        name,
        format!("Edit {}", words.get(&fields::get(name))),
        value,
    )
    .version(company.version)
    .status(status);
    field.id_value = company.id.to_string();
    field.editable = editable;
    field.class = match name {
        "name" => "font-semibold",
        "code" => "font-mono",
        _ => "",
    }
    .into();
    if !["name", "code"].contains(&name) {
        field.allow_empty = true;
    }
    render(&InlineEdit { field })
}

#[derive(Template)]
#[template(path = "create.html")]
struct CreateBody {
    header: Html,
    summary: Html,
    form: Html,
    candidates: Html,
    confirmation: Html,
}
#[derive(Template)]
#[template(path = "duplicate_confirm.html")]
struct DuplicateConfirm {
    body: Html,
}
pub(super) struct CreateState<'a> {
    pub values: &'a HashMap<String, String>,
    pub errors: &'a [FieldError],
    pub candidates: &'a [crate::DuplicateCandidate],
    pub columns: &'a ListColumns,
    pub confirming: bool,
    pub likely: bool,
    pub reason: &'a str,
    pub parent: ParentPicker<'a>,
    pub custom: &'a CustomPickers,
}
pub(super) fn duplicate_builtins() -> Vec<BuiltinColumn> {
    [
        ("name", "Company"),
        ("status", "Status"),
        ("score", "Score"),
        ("facts", "Matched facts"),
    ]
    .into_iter()
    .map(|(id, label)| {
        let column = BuiltinColumn::new(id, label);
        match id {
            "name" | "status" => column.grid_field(format!("companies.{id}")),
            _ => column,
        }
    })
    .collect()
}
pub(super) async fn create(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    picklists: &bilibi_picklists::PickLists,
    state: CreateState<'_>,
) -> Result<String> {
    let CreateState {
        values,
        errors,
        candidates,
        columns,
        confirming,
        likely,
        reason,
        parent,
        custom,
    } = state;
    bilibi_authz::authorize_key(client, scope, crate::CREATE).await?;
    let masked = crate::field_contributions::effective_restrictions(client, scope).await?;
    let words = bilibi_grid::RecordWords::load(client, scope, "companies").await?;
    let opts = options(client, true).await?;
    let mut fields = Vec::new();
    for (name, kind, placeholder) in [
        ("parent_id", InputKind::Select, ""),
        ("status", InputKind::Select, ""),
        ("name", InputKind::Text, "Company display name"),
        ("code", InputKind::Text, ""),
        (
            "legal_name",
            InputKind::Text,
            "Registered legal entity name",
        ),
        ("legal_entity_type_id", InputKind::Select, ""),
        ("registration_number", InputKind::Text, ""),
        ("tax_id", InputKind::Text, ""),
        ("jurisdiction", InputKind::Select, ""),
        ("email", InputKind::Email, ""),
        ("website", InputKind::Text, "example.com"),
    ] {
        let label = words.get(&fields::get(name));
        let id = format!("company-{}", name.replace('_', "-"));
        if masked.contains(name) {
            fields.push(render(&RestrictedField::new(&id, label))?);
            continue;
        }
        if name == "parent_id" {
            let errors = errors
                .iter()
                .filter(|e| e.field == name)
                .map(|e| e.message.clone())
                .collect();
            let selected = parent
                .chosen
                .map(|(id, label)| (id, parent_pill(id, label, false, false)));
            fields.push(parent_picker(
                &parent,
                label,
                selected,
                ("pick_parent", "clear_parent"),
                errors,
            )?);
            continue;
        }
        let mut field = Field::named(name)
            .id(id)
            .kind(kind)
            .label(label)
            .value(values.get(name).cloned().unwrap_or_default())
            .placeholder(placeholder)
            .errors(
                errors
                    .iter()
                    .filter(|e| e.field == name)
                    .map(|e| e.message.clone())
                    .collect(),
            );
        if ["name", "status"].contains(&name) {
            field = field.required();
        }
        field = match name {
            "legal_entity_type_id" => field.prompt("Select type...").options(opts.types.clone()),
            "jurisdiction" => field
                .prompt("Select country...")
                .options(opts.countries.clone()),
            "status" => field.options(choices(&[("Active", "active"), ("Pending", "pending")])),
            "code" => field
                .hint("Clear the code to let Bilibi choose one.")
                .attr("maxlength", "255")
                .attr("data-edited-marker", "code_edited"),
            _ => field.attr("maxlength", if name == "name" { "160" } else { "255" }),
        };
        if name == "jurisdiction" {
            fields.push(render(
                &Combobox::new(&field.id, name)
                    .label(label)
                    .value(&field.value)
                    .options(opts.countries.clone())
                    .placeholder("Select country...")
                    .errors(field.errors.clone()),
            )?);
        } else {
            fields.push(render(&field)?);
        }
    }
    let custom_definitions =
        crate::field_contributions::definitions().map_err(|_| Error::Internal)?;
    let mut extension_fields = Vec::new();
    for (_, definition) in custom_definitions {
        if masked.contains(definition.id) {
            extension_fields.push(render(&RestrictedField::new(
                format!("company-{}", definition.id),
                words.field(definition.id, definition.label),
            ))?);
            continue;
        }
        let mut field = Field::named(definition.id)
            .id(format!("company-{}", definition.id))
            .label(words.field(definition.id, definition.label))
            .value(values.get(definition.id).cloned().unwrap_or_default())
            .errors(
                errors
                    .iter()
                    .filter(|error| error.field == definition.id)
                    .map(|error| error.message.clone())
                    .collect(),
            );
        match definition.kind {
            crate::CompanyFieldKind::Text { max_length } => {
                field = field.kind(InputKind::Text).attr(
                    "maxlength",
                    &max_length
                        .min(crate::field_contributions::MAX_TEXT_FIELD_LENGTH)
                        .to_string(),
                );
            }
            crate::CompanyFieldKind::Date => field = field.kind(InputKind::Date),
            crate::CompanyFieldKind::Choice { picklist } => {
                let choices = picklists.choices(client, scope, picklist, None).await?;
                let value = match values.get(definition.id) {
                    Some(value) => value.clone(),
                    None => picklists
                        .default(client, scope, picklist)
                        .await?
                        .unwrap_or_default(),
                };
                extension_fields.push(render(
                    &Combobox::new(format!("company-{}", definition.id), definition.id)
                        .label(words.field(definition.id, definition.label))
                        .value(value)
                        .options(choices)
                        .errors(
                            errors
                                .iter()
                                .filter(|error| error.field == definition.id)
                                .map(|error| error.message.clone())
                                .collect(),
                        ),
                )?);
                continue;
            }
        }
        extension_fields.push(render(&field)?);
    }
    let mut additional_fields = Html::EMPTY;
    if !extension_fields.is_empty() {
        additional_fields.push(&render(&SectionHeading {
            title: "Additional fields",
            description: "Fields provided by installed Extensions.",
            ..SectionHeading::default()
        })?);
        for field in &extension_fields {
            additional_fields.push(field);
        }
    }
    let custom_defs = crate::field_contributions::custom_fields()
        .map_err(|_| Error::Internal)?
        .active(
            client,
            scope,
            crate::field_contributions::CUSTOM_RECORD_TYPE,
        )
        .await?;
    if !custom_defs.is_empty() {
        additional_fields.push(&render(&SectionHeading {
            title: "Custom fields",
            description: "Fields your organisation added to its companies.",
            ..SectionHeading::default()
        })?);
        for definition in &custom_defs {
            let name = crate::field_contributions::custom_name(&definition.key);
            let field_errors: Vec<String> = errors
                .iter()
                .filter(|error| error.field == name.as_str())
                .map(|error| error.message.clone())
                .collect();
            let value = values.get(&name).cloned().unwrap_or_default();
            match definition.kind {
                // One value per tick, posted as `cf_<key>[]` and kept joined
                // by commas between renders (a code never holds one).
                bilibi_custom_fields::Kind::Choices => {
                    let posted = values
                        .get(&format!("{name}[]"))
                        .cloned()
                        .unwrap_or_default();
                    let codes: Vec<String> =
                        bilibi_custom_fields::codes_of(Some(&bilibi_custom_fields::codes_value([
                            posted.as_str(),
                        ])))
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                    let held: Vec<&str> = codes.iter().map(String::as_str).collect();
                    let mut select =
                        MultiSelect::new(format!("company-{}", name.replace('_', "-")), &name)
                            .label(&definition.label)
                            .options(
                                crate::field_contributions::custom_fields()
                                    .map_err(|_| Error::Internal)?
                                    .choice_options(client, scope, definition, &held)
                                    .await?,
                            )
                            .values(codes.clone())
                            .errors(field_errors);
                    if let Some(help) = &definition.help {
                        select = select.hint(help);
                    }
                    additional_fields.push(&render(&select)?);
                    continue;
                }
                bilibi_custom_fields::Kind::Reference => {
                    let selected = custom.chosen.get(&name).map(|chosen| {
                        (
                            format!("{}:{}", chosen.reference.kind, chosen.id),
                            chosen.pill(format!("company-{}-pill", name.replace('_', "-")), false),
                        )
                    });
                    additional_fields.push(&custom_picker(
                        custom,
                        definition,
                        &name,
                        selected,
                        field_errors,
                        false,
                    )?);
                    continue;
                }
                _ => {}
            }
            if let (bilibi_custom_fields::Kind::Choice, Some(list)) =
                (definition.kind, definition.picklist.as_deref())
            {
                let value = if values.contains_key(&name) {
                    value
                } else {
                    picklists
                        .default(client, scope, list)
                        .await?
                        .unwrap_or_default()
                };
                additional_fields.push(&render(
                    &Combobox::new(format!("company-{}", name.replace('_', "-")), &name)
                        .label(&definition.label)
                        .value(value)
                        .options(picklists.choices(client, scope, list, None).await?)
                        .errors(field_errors),
                )?);
                continue;
            }
            let mut field = Field::named(&name)
                .id(format!("company-{}", name.replace('_', "-")))
                .label(&definition.label)
                .value(value)
                .errors(field_errors);
            if let Some(help) = &definition.help {
                field = field.hint(help);
            }
            field = match definition.kind {
                bilibi_custom_fields::Kind::Date => field.kind(InputKind::Date),
                bilibi_custom_fields::Kind::Number => {
                    field.kind(InputKind::Text).attr("inputmode", "decimal")
                }
                bilibi_custom_fields::Kind::LongText => {
                    field.kind(InputKind::Textarea).attr("rows", "5")
                }
                bilibi_custom_fields::Kind::Boolean => field.kind(InputKind::Switch),
                bilibi_custom_fields::Kind::Email => field.kind(InputKind::Email),
                bilibi_custom_fields::Kind::Phone => field.kind(InputKind::Tel),
                bilibi_custom_fields::Kind::Link => field.kind(InputKind::Url),
                bilibi_custom_fields::Kind::Text
                | bilibi_custom_fields::Kind::Choice
                | bilibi_custom_fields::Kind::Choices
                | bilibi_custom_fields::Kind::Reference => field.kind(InputKind::Text),
            };
            additional_fields.push(&render(&field)?);
        }
    }
    let context = ["scope_activities", "metadata"]
        .into_iter()
        .map(|name| {
            let label = words.get(&fields::get(name));
            render(
                &Field::named(name)
                    .id(format!("company-{name}"))
                    .label(label)
                    .kind(InputKind::Textarea)
                    .placeholder(if name == "metadata" {
                        r#"{"employee_count":120}"#
                    } else {
                        r#"{"industry":"Manufacturing"}"#
                    })
                    .value(values.get(name).cloned().unwrap_or_default())
                    .errors(
                        errors
                            .iter()
                            .filter(|e| e.field == name)
                            .map(|e| e.message.clone())
                            .collect(),
                    ),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let header = render(&Header {
        title: Html::text("Create Company"),
        subtitle: "",
        actions: back("company-create-back")?,
        ..Header::default()
    })?;
    let submit = render(&Button {
        id: "company-save",
        button_type: "submit",
        variant: ButtonVariant::Primary,
        working: "Creating…",
        body: Html::text("Create Company"),
        ..Button::default()
    })?;
    let mut body = Html::EMPTY;
    for field in fields
        .into_iter()
        .chain(std::iter::once(additional_fields))
        .chain(context)
    {
        body.push(&field);
    }
    let mut actions = submit;
    if likely {
        actions.push(&render(&Button {
            id: "company-create-anyway",
            button_type: "submit",
            name: "intent",
            value: "anyway",
            body: Html::text("Create anyway"),
            ..Button::default()
        })?);
    }
    actions.push(&render(&BackLink {
        id: "company-cancel",
        href: "/companies",
        title: "Cancel",
        ..BackLink::default()
    })?);
    let form = Form {
        id: "company-form",
        event: "validate_company",
        change: true,
        class: "",
        body: card("company-create-fields", body, false)?,
        actions,
    };
    let form = form
        .submit_event("create_company")
        .map_err(|_| Error::Internal)?;
    let mut summary = ErrorSummary::new("company-create-errors");
    for error in errors {
        summary.push(
            if error.field == "create_anyway" {
                "company-create-anyway".into()
            } else {
                format!("company-{}", error.field.replace('_', "-"))
            },
            &error.message,
        );
    }
    let mut candidate_body = Html::EMPTY;
    if likely {
        candidate_body.push(&render(&SectionHeading {
            title: "Possible duplicates",
            description: if candidates.is_empty() {
                "Likely duplicate companies exist that you cannot see. Give a reason to create another one."
            } else {
                "Use an existing company, or give a reason to create another one."
            },
            ..SectionHeading::default()
        })?);
    }
    if !candidates.is_empty() {
        let state = ListState::parse(
            &HashMap::new(),
            Arc::new(ListSpec {
                page_sizes: vec![100],
                default_page_size: 100,
                ..ListSpec::default()
            }),
        )
        .map_err(|_| Error::Internal)?;
        let rows = candidates
            .iter()
            .take(state.page_size as usize)
            .map(|c| {
                let mut row = FlexRow::new(c.id.to_string())
                    .slot("name", Html::text(&c.name))
                    .slot("status", badge(&c.status)?)
                    .slot("score", Html::text(&format!("{:.2}", c.score)))
                    .slot(
                        "facts",
                        Html::text(
                            &c.matched_facts
                                .iter()
                                .map(|s| match s.as_str() {
                                    "registration_number" => "Registration number",
                                    "website" => "Website host",
                                    "email" => "Email domain",
                                    "phone" => "Phone",
                                    _ => "Name",
                                })
                                .collect::<Vec<_>>()
                                .join(", "),
                        ),
                    );
                row.actions = render(&ActionLink {
                    id: &format!("company-use-{}", c.id),
                    href: &format!("/companies/{}", c.id),
                    title: "Use existing",
                    body: Html::text("Use existing"),
                    ..ActionLink::default()
                })?;
                Ok(row)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut table = FlexTable::new(&columns.id, columns.flex_columns(), rows);
        table.event = "duplicate_grid";
        table.zoom = columns.zoom;
        table.suggestions = columns.flex_suggestions();
        table.add_query = &columns.add_query;
        table.actions = true;
        table.caption = "Possible duplicate companies";
        candidate_body.push(&render(&table)?);
    }
    let confirmation = if confirming {
        let field = render(
            &Field::named("reason")
                .id("company-duplicate-reason")
                .label("Reason for creating another company")
                .kind(InputKind::Textarea)
                .value(reason)
                .required()
                .attr("maxlength", "500")
                .errors(
                    errors
                        .iter()
                        .filter(|e| e.field == "create_anyway")
                        .map(|e| e.message.clone())
                        .collect(),
                ),
        )?;
        let body = render(&Form {
            id: "company-duplicate-reason-form",
            event: "duplicate_reason",
            change: true,
            class: "",
            body: field,
            actions: Html::EMPTY,
        })?;
        render(&DuplicateConfirm { body })?
    } else {
        Html::EMPTY
    };
    page(
        "company-create-page",
        PageVariant::Form,
        render(&CreateBody {
            header,
            summary: render(&summary)?,
            form,
            candidates: candidate_body,
            confirmation,
        })?,
    )
}

pub(super) fn new_company(
    values: &HashMap<String, String>,
) -> (
    NewCompany,
    crate::CompanyFieldValues,
    Option<Value>,
    Option<Value>,
    Vec<FieldError>,
) {
    let text = |name| values.get(name).filter(|s| !s.trim().is_empty()).cloned();
    let mut errors = Vec::new();
    let mut id = |name: &'static str| match text(name) {
        None => None,
        Some(v) => match v.parse::<i64>() {
            Ok(v) if v > 0 => Some(v),
            _ => {
                errors.push(FieldError::new(name, "is invalid"));
                None
            }
        },
    };
    let new = NewCompany {
        name: text("name").unwrap_or_default(),
        code: text("code"),
        status: text("status"),
        parent_id: id("parent_id"),
        legal_name: text("legal_name"),
        registration_number: text("registration_number"),
        tax_id: text("tax_id"),
        legal_entity_type_id: id("legal_entity_type_id"),
        jurisdiction: text("jurisdiction"),
        email: text("email"),
        website: text("website"),
        ..NewCompany::default()
    };
    let mut company_fields = crate::CompanyFieldValues::new();
    if let Ok(definitions) = crate::field_contributions::definitions() {
        for (_, definition) in definitions {
            if let Some(value) = text(definition.id) {
                company_fields.insert(definition.id.into(), Value::String(value));
            }
        }
    }
    // A tenant's own fields post as `cf_<key>` (a record field's chosen id
    // too), several choices as `cf_<key>[]` with their codes joined by
    // commas; the check against the definitions happens in the write, which
    // refuses a key that is not one or a value of the wrong shape.
    let custom: serde_json::Map<String, Value> = values
        .iter()
        .filter_map(|(name, value)| {
            let key = name.strip_prefix("cf_")?;
            match key.strip_suffix("[]") {
                Some(key) => Some((key, bilibi_custom_fields::codes_value([value.as_str()]))),
                None => Some((key, Value::String(value.trim().to_owned())))
                    .filter(|(_, value)| value.as_str() != Some("")),
            }
        })
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    if !custom.is_empty() {
        company_fields.insert(
            crate::field_contributions::CUSTOM.into(),
            Value::Object(custom),
        );
    }
    let mut object = |name: &'static str| {
        text(name).and_then(|value| match serde_json::from_str::<Value>(&value) {
            Ok(v) if v.is_object() => Some(v),
            _ => {
                errors.push(FieldError::new(name, "must be a valid JSON object"));
                None
            }
        })
    };
    let activities = object("scope_activities");
    let metadata = object("metadata");
    for (name, value) in values {
        if let Some(field) = name
            .strip_suffix("[]")
            .filter(|field| field.starts_with("cf_"))
        {
            if value
                .split(',')
                .filter(|code| !code.trim().is_empty())
                .count()
                > bilibi_picklists::VALUES_MAX
            {
                errors.push(FieldError::new_owned(
                    field.to_owned(),
                    format!("must have at most {} choices", bilibi_picklists::VALUES_MAX),
                ));
            }
        }
    }
    (new, company_fields, activities, metadata, errors)
}

#[derive(Template)]
#[template(path = "company_activities.html")]
struct ActivityBody {
    activities: Vec<String>,
    editable: bool,
    add: Html,
}

#[derive(Template)]
#[template(source = "<div class=\"{{ class }}\">{{ body }}</div>", ext = "html")]
pub(super) struct Actions<'a> {
    pub class: &'a str,
    pub body: Html,
}
#[derive(Template)]
#[template(
    source = "<span{% if value.is_none() %} class=\"text-ink-faint\"{% endif %}>{{ value.unwrap_or(empty) }}</span>",
    ext = "html"
)]
struct ReadValue<'a> {
    value: Option<&'a str>,
    empty: &'a str,
}

#[derive(Template)]
#[template(
    source = "<span class=\"text-ink-muted\">{{ text }}</span>",
    ext = "html"
)]
struct MutedValue<'a> {
    text: &'a str,
}

// Children columns are independent of Departments.
pub(super) fn children_builtins() -> Vec<BuiltinColumn> {
    vec![
        BuiltinColumn::new("name", "Name").grid_field("companies.name"),
        BuiltinColumn::new("status", "Status").grid_field("companies.status"),
        BuiltinColumn::new("legal_entity_type", "Legal Entity Type")
            .grid_field("legal_entity_types.name"),
        BuiltinColumn::new("jurisdiction", "Jurisdiction").grid_field("companies.jurisdiction"),
    ]
}
/// The detail page's Departments table:
/// built-in columns only, with Manage leading to the departments page.
pub(super) fn department_builtins() -> Vec<BuiltinColumn> {
    vec![
        BuiltinColumn::new("type", "Department Type").grid_field("departments.type_name"),
        BuiltinColumn::new("category", "Category"),
        BuiltinColumn::new("head", "Head"),
        BuiltinColumn::new("status", "Status").grid_field("departments.status"),
    ]
}
#[derive(Template)]
#[template(path = "department_section.html")]
struct DepartmentSection {
    heading: Html,
    table: Html,
}
async fn departments_section(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    id: i64,
    columns: &mut ListColumns,
) -> Result<Html> {
    let departments = crate::departments::list(
        client,
        scope,
        id,
        crate::departments::DepartmentSort::Name,
        crate::reference_types::Direction::Asc,
    )
    .await?;
    let heads = crate::departments::head_names(client, scope, &departments).await?;
    let href = format!("/companies/{id}/departments");
    let heading = render(&SectionHeading {
        id: "company-departments-heading",
        title: "Departments",
        count: Some(departments.len()),
        actions: render(&ActionLink {
            id: "company-departments-manage",
            icon: "manage",
            title: "Manage departments",
            href: &href,
            body: Html::text("Manage"),
            ..ActionLink::default()
        })?,
        ..SectionHeading::default()
    })?;
    let mut rows = Vec::with_capacity(departments.len());
    for department in &departments {
        let key = department.id.to_string();
        let mut row = FlexRow::new(&key);
        row.dom_id = Some(format!("department-{key}"));
        let head = department
            .head_affiliation_id
            .and_then(|head| heads.get(&head))
            .map(String::as_str)
            .unwrap_or("—");
        let muted = |text: &str| {
            Html::raw(format!(
                "<span class=\"text-sm text-ink-subtle\">{}</span>",
                Html::text(text)
            ))
        };
        rows.push(
            row.slot(
                "type",
                Html::raw(format!(
                    "<span class=\"font-medium text-ink-strong\">{}</span>",
                    Html::text(&department.type_name)
                )),
            )
            .slot("category", muted(&department.type_category))
            .slot("head", muted(head))
            .slot(
                "status",
                render(&Badge::status(
                    if department.status == "active" {
                        BadgeKind::Success
                    } else {
                        BadgeKind::Warning
                    },
                    &super::reference_types::capitalize(&department.status),
                ))?,
            ),
        );
    }
    let mut table = FlexTable::new("company-departments-table", columns.flex_columns(), rows);
    table.event = "departments_grid";
    table.zoom = columns.zoom;
    table.suggestions = columns.flex_suggestions();
    table.add_query = &columns.add_query;
    table.caption = "Departments";
    table.framed = false;
    if departments.is_empty() {
        table.empty = Some(TableEmpty::Content(Html::text(
            "No departments configured.",
        )));
    }
    render(&Card {
        id: "company-departments-card",
        inner_class: "p-0",
        role: "region",
        labelledby: "company-departments-heading",
        body: render(&DepartmentSection {
            heading,
            table: render(&table)?,
        })?,
        ..Card::default()
    })
}

// List filters use shared forms; each condition stays
// together at narrow widths instead of being reordered as a search control.
fn field_filter_controls(state: &ExtensionListFields<'_>, page: &crate::Page) -> Result<Html> {
    let mut result = Html::EMPTY;
    for (index, message) in [state.filter_notice, page.notice.as_str()]
        .into_iter()
        .enumerate()
    {
        if !message.is_empty() {
            result.push(&render(&Alert {
                id: &format!("company-field-filter-notice-{index}"),
                kind: StatusKind::Warning,
                body: Html::text(message),
                ..Alert::default()
            })?);
        }
    }
    for (index, filter) in state.filters.iter().enumerate() {
        let prefix = format!("company-field-filter-{index}");
        let selected = state
            .available
            .iter()
            .find(|field| field.id == filter.field && field.kind.filterable());
        let mut choices = vec![Choice::new("Choose a field", "")];
        choices.extend(
            state
                .available
                .iter()
                .filter(|field| field.kind.filterable())
                .map(|field| Choice::new(&field.label, &field.id)),
        );
        if selected.is_none() && !filter.field.is_empty() {
            choices.push(Choice::new("Unavailable field", &filter.field));
        }
        let mut body = render(
            &Field::in_form("condition", "index")
                .id(format!("{prefix}-index"))
                .kind(InputKind::Hidden)
                .value(index.to_string()),
        )?;
        body.push(&render(
            &Field::in_form("condition", "field")
                .id(format!("{prefix}-field"))
                .label("Field")
                .kind(InputKind::Select)
                .value(&filter.field)
                .options(choices),
        )?);
        if let Some(field) = selected {
            let ops = crate::list_fields::operators(&field.kind);
            body.push(&render(
                &Field::in_form("condition", "operator")
                    .id(format!("{prefix}-operator"))
                    .label("Condition")
                    .kind(InputKind::Select)
                    .value(&filter.operator)
                    .options(
                        ops.iter()
                            .map(|(key, label)| Choice::new(*label, *key))
                            .collect(),
                    ),
            )?);
            let errors = page
                .filter_errors
                .get(index)
                .and_then(Option::as_ref)
                .cloned()
                .into_iter()
                .collect();
            let mut value = Field::in_form("condition", "value")
                .id(format!("{prefix}-value"))
                .label("Value")
                .value(&filter.value)
                .errors(errors);
            value = match &field.kind {
                crate::list_fields::ListKind::Text => value.attr("maxlength", "256"),
                crate::list_fields::ListKind::Number => value.attr("inputmode", "decimal"),
                crate::list_fields::ListKind::Date => {
                    if filter.value.is_empty()
                        || crate::field_contributions::valid_date(&filter.value)
                    {
                        value.kind(InputKind::Date)
                    } else {
                        // Native date controls erase malformed URL drafts; keep
                        // those editable until the person corrects the value.
                        value.kind(InputKind::Text)
                    }
                }
                crate::list_fields::ListKind::Boolean
                | crate::list_fields::ListKind::Choice { .. } => {
                    let mut choices = vec![Choice::new("Choose a value", "")];
                    choices.extend(field.choices.clone());
                    if !filter.value.is_empty() && !choices.iter().any(|c| c.value == filter.value)
                    {
                        choices.push(Choice::new("Unavailable value", &filter.value));
                    }
                    value.kind(InputKind::Select).options(choices)
                }
                // One code the company holds among its several; none
                // chosen applies nothing.
                crate::list_fields::ListKind::Choices => {
                    let mut choices = vec![Choice::new(
                        format!("Any {}", field.label.to_lowercase()),
                        "",
                    )];
                    choices.extend(field.choices.clone());
                    if !filter.value.is_empty() && !choices.iter().any(|c| c.value == filter.value)
                    {
                        choices.push(Choice::new("Unavailable value", &filter.value));
                    }
                    value.kind(InputKind::Select).options(choices)
                }
                crate::list_fields::ListKind::Reference => value,
            };
            body.push(&render(&value)?);
        }
        let remove = render(&Button {
            event: "field_filter_remove",
            name: "index",
            value: &index.to_string(),
            body: Html::text("Remove filter"),
            ..Button::default()
        })?;
        result.push(&render(&Form {
            id: &prefix,
            event: "field_filter_change",
            change: true,
            class: "",
            body: render(&Actions {
                class: "grid grid-cols-1 gap-2 sm:grid-cols-3",
                body,
            })?,
            actions: remove,
        })?);
    }
    if state.available.iter().any(|field| field.kind.filterable())
        && state.filters.len() < crate::list_fields::MAX_FILTERS
    {
        result.push(&render(&Button {
            id: "company-field-filter-add",
            event: "field_filter_add",
            body: Html::text("Add field filter"),
            ..Button::default()
        })?);
    }
    Ok(result)
}
