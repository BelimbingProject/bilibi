//! A team's record page: its facts read first and edit in place, then its
//! members (add, remove several at once, lead on or off) and sub-teams.
use super::{list, permitted, render, Pages, Record, State, SUB_LIMIT};
use crate::MANAGE;
use askama::Template;
use bilibi_database::Result;
use bilibi_datetime::{DisplayContext, Format, Precision};
use bilibi_tenancy::Scope;
use bilibi_ui::*;
use std::collections::{BTreeMap, BTreeSet};
use tokio_postgres::GenericClient;

const AUDIT_LIST: &str = "admin.audit.log.list";

pub(super) fn member_columns() -> Vec<BuiltinColumn> {
    vec![
        BuiltinColumn::new("name", "Name"),
        BuiltinColumn::new("lead", "Lead"),
        BuiltinColumn::new("added", "Added"),
    ]
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn read(
    pages: &Pages,
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    id: i64,
    list: &mut ListState,
    limit: i64,
    held: &BTreeSet<&str>,
    display: &DisplayContext,
) -> Result<Record> {
    let teams = &pages.teams;
    let team = teams.get(client, scope, id).await?;
    let (parent_name, company_name) = teams.names(client, scope, &team).await?;
    let kind_label = match pages
        .picklists
        .label(client, scope, crate::KINDS, &team.kind)
        .await?
    {
        Some(label) if label.retired => format!("{} (retired)", label.text),
        Some(label) => label.text,
        None => team.kind.clone(),
    };
    let offset = |list: &ListState| (list.page as i64 - 1) * limit;
    let (mut members, mut total) = teams
        .members(client, scope, id, limit, offset(list))
        .await?;
    if list.clamp_to_last_page((total as u64).div_ceil(list.page_size), true) {
        (members, total) = teams
            .members(client, scope, id, limit, offset(list))
            .await?;
    }
    let ids: Vec<i64> = members.iter().map(|m| m.user_id).collect();
    let live: BTreeSet<i64> = teams
        .authentication
        .live_staff(client, scope, &ids)
        .await?
        .into_iter()
        .collect();
    let inactive = ids.iter().copied().filter(|u| !live.contains(u)).collect();
    let mut added = BTreeMap::new();
    for m in &members {
        if let Ok(at) = jiff::Timestamp::try_from(m.added_at) {
            added.insert(
                m.user_id,
                display.render(at, Format::DateTime, Precision::Minute).text,
            );
        }
    }
    let can_change = held.contains(MANAGE) && teams.may_change(client, scope, &team).await?;
    let archived = match team.company_id {
        Some(company) => matches!(
            bilibi_company::require_writable(client, scope, company, false).await,
            Err(bilibi_company::WriteError::Archived)
        ),
        None => false,
    };
    let mut companies = vec![Choice::new("No company", "")];
    if can_change {
        companies.extend(
            pages
                .parties
                .reachable_organisations(client, scope)
                .await?
                .into_iter()
                .filter(|c| !c.archived)
                .map(|c| Choice::new(c.name, c.id.get().to_string())),
        );
    }
    if let (Some(company), Some(name)) = (team.company_id, &company_name) {
        if !companies.iter().any(|c| c.value == company.to_string()) {
            companies.push(Choice::new(name, company.to_string()));
        }
    }
    let leads = teams.leads(client, scope, id).await?;
    let above = teams.leads_above(client, scope, id).await?;
    let history = held.contains(AUDIT_LIST)
        || permitted(bilibi_authz::authorize_key(client, scope, AUDIT_LIST).await)?;
    Ok(Record {
        added,
        team,
        parent_name,
        company_name,
        kind_label,
        members,
        member_total: total as u64,
        inactive,
        leads,
        above,
        can_change,
        archived,
        companies,
        history,
    })
}
fn choice_fact<'a>(
    state: &State,
    record: &Record,
    name: &'a str,
    label: &'a str,
    value: String,
    display: String,
    options: Vec<Choice>,
) -> Result<Fact<'a>> {
    let mut field = InlineField::new(
        format!("team-{}", name.replace('_', "-")),
        name,
        label,
        &value,
    )
    .version(record.team.version);
    field.editable = record.can_change;
    field.allow_empty = name != "kind";
    field.status = state.commits.get(name);
    let editor = render(
        Field::named(name)
            .id(format!("team-{}-choice", name.replace('_', "-")))
            .kind(InputKind::Select)
            .options(options)
            .value(value)
            .label(label),
    )?;
    Ok(Fact {
        title: label,
        value: render(InlineChoice {
            field,
            display: Html::text(&display),
            editor,
        })?,
        ..Fact::default()
    })
}
fn facts(state: &State, record: &Record) -> Result<Html> {
    let team = &record.team;
    let mut name = InlineField::new("team-name", "name", "Name", &team.name).version(team.version);
    name.editable = record.can_change;
    name.status = state.commits.get("name");
    let mut parents = vec![Choice::new("No parent team", "")];
    parents.extend(
        state
            .team_choices
            .iter()
            .filter(|(id, _)| *id != team.id)
            .map(|(id, name)| Choice::new(name, id.to_string())),
    );
    let mut kinds = state.kinds.clone();
    if !kinds.iter().any(|c| c.value == team.kind) {
        kinds.push(Choice::new(&record.kind_label, &team.kind));
    }
    let names = |people: Vec<&str>| {
        if people.is_empty() {
            "—".to_owned()
        } else {
            people.join(", ")
        }
    };
    let mut items = vec![
        Fact {
            title: "Name",
            value: render(InlineEdit { field: name })?,
            ..Fact::default()
        },
        choice_fact(
            state,
            record,
            "kind",
            "Kind",
            team.kind.clone(),
            record.kind_label.clone(),
            kinds,
        )?,
        choice_fact(
            state,
            record,
            "parent_id",
            "Parent team",
            team.parent_id.map(|v| v.to_string()).unwrap_or_default(),
            record.parent_name.clone().unwrap_or_else(|| "—".into()),
            parents,
        )?,
        choice_fact(
            state,
            record,
            "company_id",
            "Company",
            team.company_id.map(|v| v.to_string()).unwrap_or_default(),
            record.company_name.clone().unwrap_or_else(|| "—".into()),
            record.companies.clone(),
        )?,
        Fact {
            title: "Leads",
            value: Html::text(&names(
                record.leads.iter().map(|m| m.name.as_str()).collect(),
            )),
            ..Fact::default()
        },
    ];
    if !record.above.is_empty() {
        let above: Vec<String> = record
            .above
            .iter()
            .map(|l| format!("{} ({})", l.name, l.team_name))
            .collect();
        items.push(Fact {
            title: "Also led by",
            value: Html::text(&above.join(", ")),
            ..Fact::default()
        });
    }
    render(Card {
        title: "Details",
        body: render(Facts {
            items,
            ..Facts::default()
        })?,
        ..Card::default()
    })
}
fn members(state: &State, record: &Record) -> Result<Html> {
    let added = &record.added;
    let sole_lead = record.leads.len() == 1;
    let mut rows = Vec::new();
    for (m, row) in record.members.iter().zip(&state.columns.rows) {
        let mut name = Html::text(&m.name);
        if record.inactive.contains(&m.user_id) {
            name.push(&Html::text(" "));
            name.push(&bilibi_ui::status_badge(
                BadgeKind::Neutral,
                "Inactive account",
            ));
        }
        let toggle_label = format!("Lead: {}", m.name);
        let value = format!("{}:{}", m.user_id, if m.lead { 0 } else { 1 });
        let id = format!("team-member-{}-lead", m.user_id);
        let toggle = render(Toggle {
            on: m.lead,
            label: &toggle_label,
            id: &id,
            event: "set_lead",
            name: "lead",
            value: &value,
            disabled: !record.can_change || (m.lead && sole_lead),
        })?;
        let mut row = row.clone().slot("name", name).slot("lead", toggle).slot(
            "added",
            Html::text(added.get(&m.user_id).map(String::as_str).unwrap_or("—")),
        );
        row.dom_id = Some(format!("team-member-{}", m.user_id));
        row.label = Some(m.name.clone());
        if record.can_change {
            let label = format!("Remove {} from the team", m.name);
            let id = format!("team-member-{}-remove", m.user_id);
            let value = m.user_id.to_string();
            row.actions = render(IconButton {
                id: &id,
                kind: IconKind::Danger,
                title: "Remove from team",
                event: "remove_one",
                name: "user",
                value: &value,
                ..IconButton::new("delete", &label)
            })?;
        }
        rows.push(row);
    }
    let mut table = state.columns.flex_table("team-members", rows);
    table.caption = "Members";
    table.framed = false;
    table.actions = record.can_change;
    if record.can_change {
        let mut selection = state.selection.clone();
        selection.matching = record.member_total;
        selection.actions = render(Button {
            id: "team-members-remove-selected",
            button_type: "button",
            event: "remove_selected",
            body: Html::text("Remove from team"),
            ..Button::default()
        })?;
        table.selection = Some(selection);
    }
    table.empty = Some(TableEmpty::Content(Html::text("No members yet.")));
    let mut body = render(table)?;
    body.push(&render(Pagination::new(
        "team-member-pagination",
        PageCounts::new(state.list.page, state.list.page_size, record.member_total),
        &super::SIZES,
    ))?);
    render(Card {
        id: "team-members-card",
        body,
        inner_class: "p-0",
        ..Card::default()
    })
}
fn sub_teams(state: &State, record: &Record) -> Result<Html> {
    let mut table = list::table(state, &state.sub_columns, "team-subteams", &state.subs)?;
    table.caption = "Sub-teams";
    table.framed = false;
    table.event = "grid_subs";
    table.empty = Some(TableEmpty::Content(Html::text("No sub-teams.")));
    let mut body = render(table)?;
    if state.sub_total > SUB_LIMIT {
        let href = format!("/teams?parent={}", record.team.id);
        let label = format!("All {} sub-teams", state.sub_total);
        body.push(&render(Button {
            href: &href,
            id: "team-subteams-all",
            body: Html::text(&label),
            ..Button::default()
        })?);
    }
    render(Card {
        id: "team-subteams-card",
        body,
        inner_class: "p-0",
        ..Card::default()
    })
}
#[derive(Template)]
#[template(
    source = "<div id=\"team-record-history-slot\" data-live-ignore><div id=\"team-record-history\"></div></div>",
    ext = "html"
)]
struct HistoryAnchor;

pub(super) fn view(state: &State, record: &Record) -> Result<(Html, Vec<Html>, PageVariant)> {
    let team = &record.team;
    let mut sections = vec![render(BackLink {
        href: crate::LIST_PATH,
        title: "Teams",
        ..BackLink::default()
    })?];
    if record.archived {
        sections.push(render(EmptyState::missing(
            "Archived company",
            "This team's company is archived, so the team cannot change.",
        ))?);
    }
    sections.push(facts(state, record)?);
    let add = if record.can_change {
        render(Button {
            id: "team-add-members",
            button_type: "button",
            event: "add_open",
            body: Html::text("+ Add members"),
            ..Button::default()
        })?
    } else {
        Html::EMPTY
    };
    sections.push(render(SectionHeading {
        title: "Members",
        id: "team-members-heading",
        count: Some(record.member_total as usize),
        description: if record.can_change {
            "A team keeps at least one lead."
        } else {
            ""
        },
        actions: add,
        ..SectionHeading::default()
    })?);
    sections.push(members(state, record)?);
    let new_sub = if state.can_create && !record.archived {
        let value = team.id.to_string();
        render(Button {
            id: "team-new-sub",
            button_type: "button",
            event: "new",
            name: "parent",
            value: &value,
            body: Html::text("+ Sub-team"),
            ..Button::default()
        })?
    } else {
        Html::EMPTY
    };
    sections.push(render(SectionHeading {
        title: "Sub-teams",
        id: "team-subteams-heading",
        count: Some(state.sub_total as usize),
        actions: new_sub,
        ..SectionHeading::default()
    })?);
    sections.push(sub_teams(state, record)?);
    let header = render(Header {
        title: Html::text(&team.name),
        title_actions: if record.history {
            render(HistoryAnchor)?
        } else {
            Html::EMPTY
        },
        ..Header::default()
    })?;
    Ok((header, sections, PageVariant::Detail))
}
