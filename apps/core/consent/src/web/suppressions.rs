//! `/consent/suppressions`: every address and person not contacted on a
//! channel, with search, channel, reason and active or lifted filters in
//! the address, and Lift after a confirmation.
use super::{event, lift, render, subject, LiftDraft, Pages};
use crate::{Channel, Reason, State, Suppression, SuppressionFilter, MANAGE};
use bilibi_database::{Error, Result};
use bilibi_datetime::{DisplayContext, Format, Precision};
use bilibi_records::Reference;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    choices, list_state::InvalidPageSize, list_state::FILTERS_FORM, page::*, params, BuiltinColumn,
    Button, Card, ColumnsOutcome, EmptyState, Filter as ListFilter, FilterToolbar, FlashGroup,
    FlexRow, FlexTable, Header, Html, IconButton, ListColumns, ListSpec, ListState, PageCounts,
    Pagination, StatusKind, TableEmpty, ToolbarControl, NO_FLASH,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
};

const SIZES: [u64; 3] = [25, 50, 100];
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    let mut channels = vec!["all"];
    channels.extend(Channel::SENDING.iter().map(|c| c.code()));
    let mut reasons = vec!["all"];
    reasons.extend(Reason::ALL.iter().map(|r| r.code()));
    ListSpec {
        sortable: BTreeMap::new(),
        page_sizes: SIZES.to_vec(),
        default_page_size: 25,
        page_size_param: "per_page".into(),
        invalid_page_size: InvalidPageSize::Default,
        filters: BTreeMap::from([
            ("channel".into(), ListFilter::one_of(&channels, "all")),
            ("reason".into(), ListFilter::one_of(&reasons, "all")),
            (
                "state".into(),
                ListFilter::one_of(&["active", "lifted", "all"], "active"),
            ),
        ]),
        omit_blank: vec!["search".into()],
        ..ListSpec::default()
    }
    .shared()
    .expect("suppression list spec")
});

fn columns() -> Result<ListColumns> {
    ListColumns::mount(
        "consent-suppressions",
        vec![
            BuiltinColumn::new("subject", "Address or person"),
            BuiltinColumn::new("channel", "Channel"),
            BuiltinColumn::new("reason", "Reason"),
            BuiltinColumn::new("suppressed", "Suppressed"),
            BuiltinColumn::new("evidence", "Evidence"),
            BuiltinColumn::new("lifted", "Lifted"),
        ],
    )
    .map_err(|_| Error::Internal)
}

fn instant(display: &DisplayContext, at: jiff::Timestamp) -> bilibi_ui::flex_table::Instant {
    let r = display.render(at, Format::DateTime, Precision::Minute);
    bilibi_ui::flex_table::Instant::new(&r.iso, &r.text, &r.company, &r.utc, r.mode.as_str())
}

pub(crate) struct Index {
    pages: Pages,
    list: ListState,
    columns: ListColumns,
    framed: bool,
    confirm: Option<LiftDraft>,
    flash: Option<(StatusKind, String)>,
}
impl Index {
    pub fn new(pages: Pages, query: &HashMap<String, String>, framed: bool) -> Result<Self> {
        Ok(Self {
            pages,
            list: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: columns()?,
            framed,
            confirm: None,
            flash: None,
        })
    }
    fn filter(&self) -> SuppressionFilter {
        SuppressionFilter {
            search: self.list.search.trim().chars().take(200).collect(),
            channel: Channel::parse(&self.list.filters["channel"]),
            reason: Reason::parse(&self.list.filters["reason"]),
            state: match self.list.filters["state"].as_str() {
                "lifted" => State::Lifted,
                "all" => State::All,
                _ => State::Active,
            },
            page: self.list.page as i64,
            page_size: self.list.page_size as i64,
        }
    }

    async fn handle(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        match event(value)? {
            "filters" => {
                let filters = params::form_values_checked(value, FILTERS_FORM, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.list.apply_filters(&filters);
            }
            "clear" => {
                self.list.apply_filters(&HashMap::from([
                    ("search".into(), String::new()),
                    ("channel".into(), "all".into()),
                    ("reason".into(), "all".into()),
                    ("state".into(), "all".into()),
                ]));
            }
            "page" => self
                .list
                .put_page(value.get("page").and_then(Value::as_str)),
            "grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                if let ColumnsOutcome::Sort(key) = self.columns.handle(&values) {
                    self.list.next_sort(&key);
                }
            }
            "request_lift" => {
                let id = value["id"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                if !self.pages.consent.may(scope, MANAGE).await? {
                    self.flash = Some((
                        StatusKind::Error,
                        "You do not have permission to lift suppressions.".into(),
                    ));
                    return Ok(());
                }
                let company = scope.current_company().ok_or(Error::Forbidden)?.get();
                let suppression = self.pages.consent.suppression(scope, company, id).await?;
                let name = match suppression.contact.person {
                    Some(person) => self
                        .pages
                        .records
                        .resolve(scope, &[Reference::new("person", person.get().to_string())])
                        .await?
                        .into_iter()
                        .next()
                        .and_then(|r| r.label),
                    None => None,
                };
                let name = name.unwrap_or_else(|| subject(&suppression));
                self.confirm = Some(LiftDraft::new(&suppression, name));
            }
            "lift_reason" => {
                if let Some(draft) = &mut self.confirm {
                    draft.accept(value);
                }
            }
            "cancel_lift" => self.confirm = None,
            "confirm_lift" => {
                let Some(draft) = &mut self.confirm else {
                    return Ok(());
                };
                let Some(reason) = draft.ready() else {
                    return Ok(());
                };
                let outcome = lift(&self.pages, scope, credential, draft, &reason).await?;
                self.confirm = None;
                self.flash = Some(match outcome {
                    Ok(message) => (StatusKind::Success, message),
                    Err(message) => (StatusKind::Error, message),
                });
            }
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    fn row(
        &self,
        display: &DisplayContext,
        suppression: &Suppression,
        person: Option<&bilibi_records::Resolved>,
        names: &HashMap<i64, String>,
        manage: bool,
    ) -> Result<FlexRow> {
        let id = suppression.id.to_string();
        let subject = match person {
            Some(resolved) => {
                render(resolved.pill(format!("consent-suppression-{id}-person"), self.framed))?
            }
            None => Html::text(&subject(suppression)),
        };
        let lifted = match suppression.lifted_at {
            Some(at) => {
                let by = suppression
                    .lifted_by
                    .and_then(|user| names.get(&user).cloned())
                    .or_else(|| suppression.lifted_principal.clone())
                    .unwrap_or_default();
                Html::text(&format!(
                    "{} by {by}: {}",
                    super::when(display, at),
                    suppression.lift_reason.as_deref().unwrap_or_default()
                ))
            }
            None => Html::text("—"),
        };
        let mut row = FlexRow::new(&id)
            .slot("subject", subject)
            .slot("channel", Html::text(suppression.channel.label()))
            .slot("reason", Html::text(suppression.reason.label()))
            .slot(
                "evidence",
                Html::text(suppression.evidence.as_deref().unwrap_or("—")),
            )
            .slot("lifted", lifted);
        row.cells.insert(
            "suppressed".into(),
            bilibi_ui::flex_table::Cell::instant(instant(display, suppression.suppressed_at)),
        );
        if manage && suppression.lifted_at.is_none() {
            row.actions.push(&render(IconButton {
                id: &format!("consent-suppression-lift-{id}"),
                event: "request_lift",
                name: "id",
                value: &id,
                ..IconButton::new("lock-open", "Lift")
            })?);
        }
        row.dom_id = Some(format!("consent-suppression-{id}"));
        Ok(row)
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let display = self.pages.display(scope).await?;
        let company = scope.current_company().ok_or(Error::Forbidden)?.get();
        let mut page = self
            .pages
            .consent
            .suppressions(scope, company, &self.filter())
            .await?;
        let total_pages = (page.total.max(0) as u64).div_ceil(self.list.page_size);
        if self.list.clamp_to_last_page(total_pages, true) {
            page = self
                .pages
                .consent
                .suppressions(scope, company, &self.filter())
                .await?;
        }
        let people: Vec<Reference> = page
            .entries
            .iter()
            .filter_map(|s| s.contact.person)
            .map(|p| Reference::new("person", p.get().to_string()))
            .collect();
        let named = self.pages.records.resolve(scope, &people).await?;
        let mut named = named.iter();
        let users: Vec<i64> = page.entries.iter().filter_map(|s| s.lifted_by).collect();
        let names = self.pages.names(scope, &users).await?;
        let manage = self.pages.consent.may(scope, MANAGE).await?;
        let _ = self.columns.load(&page.entries, |s| s.id.to_string());
        let mut rows = Vec::with_capacity(page.entries.len());
        for suppression in &page.entries {
            let person = suppression
                .contact
                .person
                .and_then(|_| named.next())
                .filter(|r| r.label.is_some());
            rows.push(self.row(&display, suppression, person, &names, manage)?);
        }

        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                _ => flash.error = message,
            }
        }
        let mut body = render(FlashGroup {
            id: "consent-suppressions-flash",
            flash,
        })?;
        body.push(&render(Header {
            title: Html::text("Suppressions"),
            subtitle: "Addresses and people not contacted on a channel after a hard bounce, a complaint or an unsubscribe.",
            ..Header::default()
        })?);
        let form = self.list.filters_form();
        let mut channels = vec![("All channels".to_owned(), "all".to_owned())];
        channels.extend(
            Channel::SENDING
                .iter()
                .map(|c| (c.label().to_owned(), c.code().to_owned())),
        );
        let mut reasons = vec![("All reasons".to_owned(), "all".to_owned())];
        reasons.extend(
            Reason::ALL
                .iter()
                .map(|r| (r.label().to_owned(), r.code().to_owned())),
        );
        body.push(&render(FilterToolbar::new(
            "consent-suppressions-filters",
            vec![
                ToolbarControl::search(
                    "consent-suppressions-search",
                    "search",
                    "Search suppressions",
                    &form["search"],
                    "Search by address or person ID...",
                ),
                ToolbarControl::select(
                    "consent-suppressions-channel",
                    "channel",
                    "Channel",
                    &form["channel"],
                    choices(&channels),
                ),
                ToolbarControl::select(
                    "consent-suppressions-reason",
                    "reason",
                    "Reason",
                    &form["reason"],
                    choices(&reasons),
                ),
                ToolbarControl::select(
                    "consent-suppressions-state",
                    "state",
                    "Active or lifted",
                    &form["state"],
                    choices(&[
                        ("Active", "active"),
                        ("Lifted", "lifted"),
                        ("Active and lifted", "all"),
                    ]),
                ),
            ],
        ))?);
        let mut table = FlexTable::new("consent-suppressions", self.columns.flex_columns(), rows);
        table.event = "grid";
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Suppressions";
        table.actions = true;
        table.framed = false;
        if page.entries.is_empty() {
            let clear = render(Button {
                id: "consent-suppressions-clear",
                button_type: "button",
                event: "clear",
                body: Html::text("Clear filters"),
                ..Button::default()
            })?;
            table.empty = Some(TableEmpty::State(
                EmptyState::missing(
                    "No suppressions match",
                    "Clear the filters to see every suppression, lifted ones included.",
                )
                .action(clear),
            ));
        }
        let mut card = render(table)?;
        card.push(&render(Pagination::new(
            "consent-suppressions-pagination",
            PageCounts::new(
                self.list.page,
                self.list.page_size,
                page.total.max(0) as u64,
            ),
            &SIZES,
        ))?);
        body.push(&render(Card {
            id: "consent-suppressions-card",
            inner_class: "p-0",
            body: card,
            ..Card::default()
        })?);
        if let Some(draft) = &self.confirm {
            body.push(&draft.render("consent-suppressions-lift")?);
        }
        let html = render(bilibi_ui::Page {
            id: "consent-suppressions",
            body,
            ..bilibi_ui::Page::default()
        })?
        .to_string();
        Ok(LiveBody {
            html,
            query: self.list.to_params(),
            rows: page.entries.len(),
            ..LiveBody::default()
        })
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
                }
                LiveInput::Event(value) => {
                    self.handle(request.scope, request.credential, value)
                        .await?;
                }
            }
            self.view(request.scope).await
        })
    }
}
