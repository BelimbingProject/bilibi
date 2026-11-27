//! The Domains and extensions tab of the Software page: one grid of what this
//! server runs and what the catalogue offers, installed first, with search
//! and filters in the address.
use super::{instant, render, safe_id, text, InstantCell};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_datetime::DisplayContext;
use bilibi_ui::{
    choices, flex_table::Cell, list_state::FILTERS_FORM, params, Alert, Badge, BadgeKind,
    BuiltinColumn, Card, ColumnsOutcome, Filter, FilterToolbar, FlexRow, FlexTable, Html,
    IconButton, ListColumns, ListSpec, ListState, Modal, ModalWidth, Pagination, StatusKind,
    TableEmpty, ToolbarControl,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

const PAGE_SIZES: [u64; 4] = [25, 50, 100, 300];
/// The address keys this list owns on the page's shared query.
const QUERY_KEYS: [&str; 5] = ["search", "page", "page_size", "status", "kind"];
/// The catalogue's two lists, with the word a row shows for each.
const KINDS: [(&str, &str); 2] = [("domains", "Domain"), ("extensions", "Extension")];

pub(super) struct Modules {
    list: ListState,
    columns: ListColumns,
}

/// One row: a catalogue entry, or an installed module the catalogue omits.
struct Entry {
    kind: &'static str,
    kind_word: &'static str,
    id: String,
    name: String,
    description: String,
    /// The version in service; `None` when the module is not installed.
    installed: Option<String>,
    installed_at: Option<bilibi_ui::flex_table::Instant>,
    commit: Option<String>,
    latest: String,
    latest_at: Option<bilibi_ui::flex_table::Instant>,
    update: bool,
    compatibility: String,
    requested: bool,
    latest_commit: Option<String>,
    commands: bool,
}

fn version_label(version: Option<&str>) -> String {
    version
        .map(|v| format!("v{}", v.trim_start_matches('v')))
        .unwrap_or_else(|| "Version unavailable".into())
}
fn latest_label(
    item: &Value,
    display: &DisplayContext,
) -> (
    String,
    Option<String>,
    Option<bilibi_ui::flex_table::Instant>,
) {
    let Some(version) = item["latest_version"]
        .as_str()
        .or_else(|| item["recommended_version"].as_str())
    else {
        return ("—".into(), None, None);
    };
    let release = item["versions"]
        .as_array()
        .and_then(|versions| versions.iter().find(|r| r["version"] == version));
    (
        version_label(Some(version)),
        release
            .and_then(|r| r["commit"].as_str())
            .map(str::to_owned),
        release
            .and_then(|r| r["committed_at"].as_str())
            .and_then(|at| instant(display, at)),
    )
}

fn parse(query: &HashMap<String, String>) -> Result<ListState> {
    let spec = ListSpec {
        page_sizes: PAGE_SIZES.to_vec(),
        filters: BTreeMap::from([
            (
                "status".into(),
                Filter::one_of(&["", "installed", "available"], ""),
            ),
            (
                "kind".into(),
                Filter::one_of(&["", "domains", "extensions"], ""),
            ),
        ]),
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .map_err(|_| Error::Internal)?;
    ListState::parse(query, spec).map_err(|_| Error::Internal)
}

fn entries(
    status: &Value,
    catalogue: &Value,
    requested: &[String],
    display: &DisplayContext,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for (kind, kind_word) in KINDS {
        for item in catalogue[kind].as_array().into_iter().flatten() {
            let id = text(&item["id"]);
            let installed = status["installed"][kind]
                .as_array()
                .into_iter()
                .flatten()
                .find(|module| module["id"] == id);
            let (latest, latest_commit, latest_at) = latest_label(item, display);
            entries.push(Entry {
                kind,
                kind_word,
                id: id.to_owned(),
                name: item["name"].as_str().unwrap_or(id).to_owned(),
                description: text(&item["description"]).to_owned(),
                installed: installed.map(|m| version_label(m["version"].as_str())),
                installed_at: installed
                    .and_then(|m| m["committed_at"].as_str())
                    .and_then(|at| instant(display, at)),
                commit: installed
                    .and_then(|m| m["commit"].as_str())
                    .map(str::to_owned),
                latest,
                latest_at,
                latest_commit,
                commands: has_commands(item),
                update: item["status"] == "update_available",
                compatibility: text(&item["compatibility"]).to_owned(),
                requested: requested.iter().any(|wanted| wanted == id),
            });
        }
        for item in catalogue["uncatalogued"][kind]
            .as_array()
            .into_iter()
            .flatten()
        {
            let id = text(&item["id"]);
            entries.push(Entry {
                kind,
                kind_word,
                id: id.to_owned(),
                name: item["name"].as_str().unwrap_or(id).to_owned(),
                description: item["extension_code"]
                    .as_str()
                    .map(|code| format!("Code review: {code}"))
                    .unwrap_or_default(),
                installed: Some(version_label(item["version"].as_str())),
                installed_at: item["committed_at"]
                    .as_str()
                    .and_then(|at| instant(display, at)),
                commit: item["commit"].as_str().map(str::to_owned),
                latest: "—".into(),
                latest_at: None,
                update: false,
                compatibility: "Not in the catalogue".into(),
                requested: false,
                latest_commit: None,
                commands: false,
            });
        }
    }
    entries.sort_by_cached_key(|entry| {
        (
            entry.installed.is_none(),
            entry.name.to_lowercase(),
            entry.kind,
        )
    });
    entries
}

impl Modules {
    pub(super) fn new(query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            list: parse(query)?,
            columns: ListColumns::mount(
                "software-modules",
                vec![
                    BuiltinColumn::new("name", "Name"),
                    BuiltinColumn::new("kind", "Kind"),
                    BuiltinColumn::new("installed", "Installed version"),
                    BuiltinColumn::new("latest", "Latest version"),
                    BuiltinColumn::new("compatibility", "Compatibility"),
                    BuiltinColumn::new("description", "Description"),
                ],
            )
            .map_err(|_| Error::Internal)?,
        })
    }

    /// The address changed (Back, a shared link): read the list's part again.
    pub(super) fn params(&mut self, query: &HashMap<String, String>) -> Result<()> {
        self.list = parse(query)?;
        Ok(())
    }

    /// Puts the list's state into the page's query, replacing its old keys.
    pub(super) fn write_query(&self, query: &mut HashMap<String, String>) {
        for key in QUERY_KEYS {
            query.remove(key);
        }
        query.extend(self.list.to_params());
    }

    /// Handles the list's own events; `false` leaves the event to the page.
    pub(super) fn event(&mut self, kind: &str, value: &Value) -> Result<bool> {
        match kind {
            "modules_filters" => {
                let posted = params::form_values_checked(value, FILTERS_FORM, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.list.apply_filters(&posted);
            }
            "modules_page" => self
                .list
                .put_page(value.get("page").and_then(Value::as_str)),
            "modules_grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                if let ColumnsOutcome::Sort(_) = self.columns.handle(&values) {
                    return Err(Error::Invalid);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn body(
        &mut self,
        status: &Value,
        catalogue: &Value,
        requested: &[String],
        display: &DisplayContext,
    ) -> Result<Html> {
        let search = self.list.search.trim().to_lowercase();
        let status_filter = self.list.filters["status"].clone();
        let kind_filter = self.list.filters["kind"].clone();
        let filtered = !search.is_empty() || !status_filter.is_empty() || !kind_filter.is_empty();
        let all: Vec<Entry> = entries(status, catalogue, requested, display)
            .into_iter()
            .filter(|entry| match status_filter.as_str() {
                "installed" => entry.installed.is_some(),
                "available" => entry.installed.is_none(),
                _ => true,
            })
            .filter(|entry| kind_filter.is_empty() || kind_filter == entry.kind)
            .filter(|entry| {
                search.is_empty()
                    || [&entry.name, &entry.id, &entry.description]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&search))
            })
            .collect();
        let page = self.list.paginate(&all);
        let form = self.list.filters_form();

        let mut html = Html::EMPTY;
        if catalogue["available"] != true {
            html.push(&render(Alert {
                kind: StatusKind::Warning,
                id: "software-catalogue-unavailable",
                body: Html::text(text(&catalogue["message"])),
                ..Alert::default()
            })?);
        }
        let mut toolbar = FilterToolbar::new(
            "software-modules-filters",
            vec![
                ToolbarControl::search(
                    "software-modules-search",
                    "search",
                    "Search domains and extensions",
                    &form["search"],
                    "Search by name or description",
                ),
                ToolbarControl::select(
                    "software-modules-status",
                    "status",
                    "Status",
                    &form["status"],
                    choices(&[
                        ("All", ""),
                        ("Installed", "installed"),
                        ("Not installed", "available"),
                    ]),
                ),
                ToolbarControl::select(
                    "software-modules-kind",
                    "kind",
                    "Kind",
                    &form["kind"],
                    choices(&[
                        ("All kinds", ""),
                        ("Domains", "domains"),
                        ("Extensions", "extensions"),
                    ]),
                ),
            ],
        );
        toolbar.event = "modules_filters";
        html.push(&render(toolbar)?);

        let rows = page.entries.iter().map(row).collect::<Result<Vec<_>>>()?;
        let mut table = FlexTable::new("software-modules", self.columns.flex_columns(), rows)
            .phone_columns(&["name", "installed", "latest"]);
        table.event = "modules_grid";
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Domains and extensions";
        table.framed = false;
        table.actions = page.entries.iter().any(|entry| entry.commands);
        if page.counts.total_entries == 0 {
            table.empty = Some(TableEmpty::Content(Html::text(if filtered {
                "No domains or extensions match these filters."
            } else {
                "No domains or extensions are installed or listed."
            })));
        }
        let mut card = render(table)?;
        let mut pagination =
            Pagination::new("software-modules-pagination", page.counts, &PAGE_SIZES);
        pagination.filters_event = "modules_filters";
        pagination.page_event = "modules_page";
        card.push(&render(pagination)?);
        html.push(&render(Card {
            id: "software-modules-card",
            inner_class: "p-0",
            body: card,
            ..Card::default()
        })?);
        Ok(html)
    }
}

fn row(entry: &Entry) -> Result<FlexRow> {
    let key = format!("{}-{}", entry.kind, entry.id);
    // The space keeps a badge off the text when both fit on one line.
    let mut name = Html::text(&format!("{} ", entry.name));
    if entry.requested {
        name.push(&render(Badge::status(BadgeKind::Neutral, "Requested"))?);
    }
    let mut latest = render(BuildLabel {
        text: &entry.latest,
        commit: entry.latest_commit.as_deref(),
    })?;
    if let Some(i) = &entry.latest_at {
        latest.push(&Html::text(" · "));
        latest.push(&render(InstantCell {
            id: format!("software-{key}-latest-at"),
            i: i.clone(),
        })?);
    }
    latest.push(&Html::text(" "));
    if entry.update {
        latest.push(&render(Badge::status(
            BadgeKind::Neutral,
            "Update available",
        ))?);
    }
    let mut row = FlexRow::new(key.clone())
        .slot("name", name)
        .cell("kind", Cell::text(entry.kind_word))
        .slot("installed", {
            let mut value = render(BuildLabel {
                text: entry.installed.as_deref().unwrap_or("Not installed"),
                commit: entry.commit.as_deref(),
            })?;
            if let Some(i) = &entry.installed_at {
                value.push(&Html::text(" · "));
                value.push(&render(InstantCell {
                    id: format!("software-{key}-installed-at"),
                    i: i.clone(),
                })?);
            }
            value
        })
        .slot("latest", latest)
        .cell("compatibility", Cell::text(&entry.compatibility))
        .cell("description", Cell::text(&entry.description));
    row.dom_id = Some(format!("software-module-{key}"));
    row.label = Some(entry.name.clone());
    if entry.commands {
        let module = format!("{}:{}", entry.kind, entry.id);
        row.actions = render(IconButton {
            id: &format!("software-commands-{key}"),
            title: "Installation commands",
            event: "commands",
            name: "module",
            value: &module,
            ..IconButton::new("command-line", "Installation commands")
        })?;
    }
    Ok(row)
}

/// The `kind:id` a row's commands button sent, checked before it is kept.
pub(super) fn commands_target(module: &str) -> Option<(String, String)> {
    let (kind, id) = module.split_once(':')?;
    (KINDS.iter().any(|(known, _)| *known == kind) && safe_id(id))
        .then(|| (kind.to_owned(), id.to_owned()))
}

/// The commands that add one catalogue entry to a Bilibi code folder, with a
/// copy icon. Empty when the catalogue no longer offers commands for it.
pub(super) fn commands_dialog(catalogue: &Value, kind: &str, id: &str) -> Result<Html> {
    let Some(item) = catalogue[kind]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["id"] == id && has_commands(item))
    else {
        return Ok(Html::EMPTY);
    };
    let commands = item["local_steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    if commands.is_empty() {
        return Ok(Html::EMPTY);
    }
    let subject = format!(
        "{} {}",
        item["name"].as_str().unwrap_or(id),
        text(&item["recommended_version"])
    );
    let body = render(CommandsBox {
        commands: &commands,
    })?;
    render(Modal {
        id: "software-commands",
        title: "Installation commands",
        cancel_event: "cancel_dialog",
        width: ModalWidth::Wide,
        description: subject.trim(),
        close_icon: true,
        body,
        ..Modal::default()
    })
}

#[derive(Template)]
#[template(
    source = r#"{% import "components.html" as ui %}<div data-hook="ClipboardCopy">{% call ui::copy_box(id="software-commands-box", label="Commands", text=commands, copy_label="Copy commands") %}{% endcall %}</div>"#,
    ext = "html"
)]
struct CommandsBox<'a> {
    commands: &'a str,
}

fn has_commands(item: &Value) -> bool {
    item["status"] == "not_installed"
        && item["local_steps"]
            .as_array()
            .is_some_and(|steps| !steps.is_empty())
}

#[derive(Template)]
#[template(
    source = r#"<span{% if let Some(commit) = commit %} title="{{ commit }}" tabindex="0"{% endif %}>{{ text }}</span>"#,
    ext = "html"
)]
struct BuildLabel<'a> {
    text: &'a str,
    commit: Option<&'a str>,
}
