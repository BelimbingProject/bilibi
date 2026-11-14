//! The Teams list: search, kind and parent filters, one create action.
use super::{render, State};
use crate::TeamRow;
use bilibi_database::Result;
use bilibi_grid::PageColumns;
use bilibi_ui::*;

pub(super) fn columns() -> Vec<BuiltinColumn> {
    [
        ("name", "Name"),
        ("kind", "Kind"),
        ("leads", "Leads"),
        ("members", "Members"),
        ("parent", "Parent team"),
        ("company", "Company"),
    ]
    .into_iter()
    .map(|(k, v)| {
        if k == "members" {
            BuiltinColumn::new(k, v).right()
        } else {
            BuiltinColumn::new(k, v)
        }
    })
    .collect()
}
/// The kind's label in the reader's language; a retired kind shows its code.
pub(super) fn kind_label(state: &State, code: &str) -> String {
    state
        .kinds
        .iter()
        .find(|c| c.value == code)
        .map(|c| c.label.clone())
        .unwrap_or_else(|| code.to_owned())
}
fn leads_text(row: &TeamRow) -> String {
    let mut text = row.leads.join(", ");
    let more = row.lead_count - row.leads.len() as i64;
    if more > 0 {
        text.push_str(&format!(" +{more}"));
    }
    if text.is_empty() {
        "—".into()
    } else {
        text
    }
}
/// Rows of a teams table (the list, or a team's sub-teams).
pub(super) fn table<'a>(
    state: &State,
    columns: &'a PageColumns,
    id: &'a str,
    rows: &[TeamRow],
) -> Result<FlexTable<'a>> {
    let mut out = Vec::new();
    for (team, row) in rows.iter().zip(&columns.rows) {
        let href = format!("/teams/{}", team.team.id);
        let key = team.team.id.to_string();
        let link = render(RecordLink {
            workspace: state.framed,
            kind: "core/team",
            record_id: &key,
            href: &href,
            body: Html::text(&team.team.name),
            class: "",
        })?;
        out.push(
            row.clone()
                .slot("name", link)
                .slot("kind", Html::text(&kind_label(state, &team.team.kind)))
                .slot("leads", Html::text(&leads_text(team)))
                .slot("members", Html::text(&team.members.to_string()))
                .slot(
                    "parent",
                    Html::text(team.parent_name.as_deref().unwrap_or("—")),
                )
                .slot(
                    "company",
                    Html::text(team.company_name.as_deref().unwrap_or("—")),
                ),
        );
    }
    Ok(columns.flex_table(id, out))
}
pub(super) fn view(state: &State) -> Result<(Html, Vec<Html>, PageVariant)> {
    let filters = state.list.filters_form();
    let mut kinds = vec![Choice::new("All kinds", "")];
    kinds.extend(state.kinds.iter().cloned());
    let mut parents = vec![Choice::new("Any parent team", "")];
    parents.extend(
        state
            .team_choices
            .iter()
            .map(|(id, name)| Choice::new(name, id.to_string())),
    );
    let mut sections = vec![render(FilterToolbar::new(
        "team-filters",
        vec![
            ToolbarControl::search(
                "team-search",
                "search",
                "Search teams",
                &filters["search"],
                "Search by name…",
            ),
            ToolbarControl::select("team-kind", "kind", "Kind", &filters["kind"], kinds),
            ToolbarControl::select(
                "team-parent",
                "parent",
                "Sub-teams of",
                &filters["parent"],
                parents,
            ),
        ],
    ))?];
    let mut table = table(state, &state.columns, "teams", &state.rows)?;
    table.caption = "Teams";
    let filtered = !state.list.search.is_empty()
        || !state.list.filters["kind"].is_empty()
        || !state.list.filters["parent"].is_empty();
    table.empty = Some(TableEmpty::State(if filtered {
        EmptyState::missing("No teams match", "Change the search or filters.")
    } else if state.can_create {
        EmptyState::missing("No teams yet", "Put people who work together in a team.").action(
            render(Button {
                body: Html::text("Create the first team"),
                event: "new",
                button_type: "button",
                id: "team-empty-new",
                ..Button::default()
            })?,
        )
    } else {
        EmptyState::missing("No teams yet", "")
    }));
    let mut body = render(table)?;
    body.push(&render(Pagination::new(
        "team-pagination",
        PageCounts::new(state.list.page, state.list.page_size, state.total),
        &super::SIZES,
    ))?);
    sections.push(render(Card {
        body,
        inner_class: "p-0",
        ..Card::default()
    })?);
    let actions = if state.can_create {
        render(CreateButton {
            noun: "Team",
            event: "new",
            id: "team-new",
            button_type: "button",
            ..CreateButton::default()
        })?
    } else {
        Html::EMPTY
    };
    let header = render(Header {
        title: Html::text("Teams"),
        subtitle: "People who work together, their leads and sub-teams.",
        actions,
        ..Header::default()
    })?;
    Ok((header, sections, PageVariant::List))
}
