use super::{page, render, Pages, INDEX};
use crate::Templates;
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    list_state::FILTERS_FORM,
    page::{LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, PageRequest},
    params, BuiltinColumn, Card, FilterToolbar, FlexRow, FlexTable, Header, Html, ListColumns,
    ListSpec, ListState, PageVariant, Pagination, RecordLink, TableEmpty, ToolbarControl,
};
use std::sync::Arc;

const PAGE_SIZES: &[u64] = &[25, 50, 100];

pub(super) struct Index {
    templates: Arc<Templates>,
    state: ListState,
    columns: ListColumns,
    framed: bool,
}

impl Index {
    pub(super) fn new(pages: &Pages, request: &PageRequest<'_, Scope>) -> Result<Self> {
        let spec = ListSpec {
            page_sizes: PAGE_SIZES.to_vec(),
            ..Default::default()
        };
        let state = ListState::parse(request.query, Arc::new(spec)).map_err(|_| Error::Internal)?;
        Ok(Self {
            templates: pages.templates.clone(),
            columns: ListColumns::mount(
                "templates",
                vec![
                    BuiltinColumn::new("template", "Template"),
                    BuiltinColumn::new("audience", "Sent to"),
                    BuiltinColumn::new("key", "Key"),
                ],
            )
            .map_err(|_| Error::Internal)?,
            state,
            framed: request.framed,
        })
    }

    pub(super) fn render(&mut self) -> Result<LiveBody> {
        let needle = self.state.search.to_lowercase();
        let mut all: Vec<_> = self
            .templates
            .list()
            .filter(|entry| {
                entry.label.to_lowercase().contains(&needle) || entry.key.contains(&needle)
            })
            .collect();
        all.sort_by_key(|entry| entry.label);
        let list = self.state.paginate(&all);
        let rows = list
            .entries
            .iter()
            .map(|entry| {
                let href = format!("{INDEX}/{}", entry.key);
                Ok(FlexRow::new(entry.key)
                    .slot(
                        "template",
                        render(RecordLink {
                            workspace: self.framed,
                            kind: "base/templates",
                            record_id: entry.key,
                            href: &href,
                            class: "",
                            body: Html::text(entry.label),
                        })?,
                    )
                    .slot("key", Html::text(entry.key))
                    .slot(
                        "audience",
                        Html::text(match entry.audience {
                            crate::Audience::Customer => "Customers",
                            crate::Audience::Staff => "Staff",
                        }),
                    ))
            })
            .collect::<Result<Vec<_>>>()?;
        let shown = rows.len();
        let columns = self
            .columns
            .flex_columns()
            .into_iter()
            .map(|column| {
                if column.id == "template" {
                    column.fixed()
                } else {
                    column
                }
            })
            .collect();
        let mut table =
            FlexTable::new("templates", columns, rows).phone_columns(&["template", "audience"]);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Templates";
        table.empty = Some(TableEmpty::Content(Html::text(if needle.is_empty() {
            "Templates appear here when an installed module provides them."
        } else {
            "No templates match. Try a different search."
        })));
        let body = [
            render(Header {
                title: Html::text("Templates"),
                subtitle: "Choose the wording people receive from your workspace or company.",
                ..Default::default()
            })?,
            render(FilterToolbar::new(
                "template-search",
                vec![ToolbarControl::search(
                    "template-query",
                    "search",
                    "Search templates",
                    &self.state.search,
                    "Search by name…",
                )],
            ))?,
            render(Card {
                inner_class: "p-0",
                body: [
                    render(table)?,
                    render(Pagination::new("templates-pages", list.counts, PAGE_SIZES))?,
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            })?,
        ]
        .into_iter()
        .collect();
        Ok(LiveBody {
            html: page("templates-index", PageVariant::List, body)?,
            query: self.state.to_params(),
            rows: shown,
            ..Default::default()
        })
    }
}

impl LiveState<Scope, Error> for Index {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.state = ListState::parse(values, Arc::new(self.state.spec().clone()))
                        .map_err(|_| Error::Internal)?;
                }
                LiveInput::Event(event) => match event["type"].as_str() {
                    Some("filters") => self.state.apply_filters(
                        &params::form_values_checked(event, FILTERS_FORM, params::FIELD_LIMIT)
                            .map_err(Error::refused)?,
                    ),
                    Some("page") => self.state.put_page(event["page"].as_str()),
                    Some("grid") => {
                        let values = params::string_values_checked(event, params::FIELD_LIMIT)
                            .map_err(Error::refused)?;
                        if values.get("op").is_none_or(|op| op != "remove")
                            || values.get("spec").is_none_or(|spec| spec != "template")
                        {
                            self.columns.handle(&values);
                        }
                    }
                    _ => return Err(Error::Invalid),
                },
            }
            self.render()
        })
    }
}
