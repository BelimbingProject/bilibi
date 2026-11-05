//! The Activities panel on a record page: the Core company and person
//! pages, and any record page that renders the generic `record.activities`
//! embed for its own kind. Planned and History tabs, New call, New task and
//! New note with the record filled in, and Close with a confirmation.
use super::{
    event, form::Dialog, icon, refusal, render, sentences, status_badge, BrowserZone, Clock, Pages,
    Refusal, Region,
};
use crate::{Activity, Kind, Reference, State};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    lists::RecordLink, page::*, params, ActionLink, BuiltinColumn, Button, ButtonVariant, Card,
    ConfirmDialog, FlexRow, FlexTable, Html, Icon, IconButton, ListColumns, NoticeKind,
    PanelNotice, SectionHeading, Tab, TableEmpty, Tabs,
};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Rows per tab; View all opens the list filtered to the record.
const ROWS: i64 = 10;

pub(crate) struct Panel {
    pages: Pages,
    record: Reference,
    anchor: String,
    framed: bool,
    history: bool,
    columns: ListColumns,
    zone: BrowserZone,
    dialog: Option<Dialog>,
    /// The activity Close asks about: its id, title and version.
    confirm: Option<(i64, String, i64)>,
    notice: Option<(NoticeKind, String)>,
}

/// The record the panel is about, from the path its host page passes.
fn record(operation: &str, path: &HashMap<String, String>) -> Result<Reference> {
    let id = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| path.get(*key))
            .filter(|id| id.parse::<i64>().is_ok_and(|id| id > 0))
            .cloned()
            .ok_or(Error::Invalid)
    };
    Ok(match operation {
        "company" => Reference::new("company", id(&["company_id", "id"])?),
        "person" => Reference::new("person", id(&["person_id", "id"])?),
        _ => {
            let kind = path.get("kind").ok_or(Error::Invalid)?;
            let id = path.get("id").ok_or(Error::Invalid)?;
            if kind.is_empty() || kind.len() > 100 || id.is_empty() || id.len() > 64 {
                return Err(Error::Invalid);
            }
            Reference::new(kind.as_str(), id.as_str())
        }
    })
}

impl Panel {
    pub fn new(pages: Pages, request: &PageRequest<'_, Scope>) -> Result<Self> {
        let anchor = request.path.get("panel").ok_or(Error::Invalid)?;
        if anchor.is_empty()
            || !anchor
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            pages,
            record: record(request.operation, request.path)?,
            anchor: anchor.clone(),
            framed: request.framed,
            history: false,
            columns: ListColumns::mount(
                &format!("{anchor}-activities"),
                vec![
                    BuiltinColumn::new("title", "Subject"),
                    BuiltinColumn::new("kind", "Kind"),
                    BuiltinColumn::new("status", "Status"),
                    BuiltinColumn::new("when", "When"),
                    BuiltinColumn::new("assignee", "Assigned to"),
                ],
            )
            .map_err(|_| Error::Internal)?,
            zone: BrowserZone::new(),
            dialog: None,
            confirm: None,
            notice: None,
        })
    }

    async fn handle(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let name = event(value)?;
        if let Some(dialog) = &mut self.dialog {
            if dialog.event(&self.pages, scope, name, value).await? {
                return Ok(());
            }
        }
        match name {
            "browser_timezone" => self.zone.accept(value),
            "tab" => self.history = value["tab"].as_str() == Some("history"),
            "grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                self.columns.handle(&values);
            }
            "new" => {
                if !self.pages.activities.may_create(scope).await? {
                    self.notice = Some((
                        NoticeKind::Error,
                        "You do not have permission to create activities.".into(),
                    ));
                    return Ok(());
                }
                let kind = match value["kind"].as_str() {
                    Some("call") => Kind::Call,
                    Some("note") => Kind::Note,
                    _ => Kind::Task,
                };
                let related = self
                    .pages
                    .records
                    .resolve(scope, std::slice::from_ref(&self.record))
                    .await?
                    .into_iter()
                    .next();
                self.dialog = Some(Dialog::new(kind, related));
            }
            "close_modal" => self.dialog = None,
            "create_task" | "log_call" | "create_note" => {
                let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
                let pages = self.pages.shared();
                let Some(dialog) = &mut self.dialog else {
                    return Ok(());
                };
                if dialog.submit_event() != name {
                    return Err(Error::Invalid);
                }
                if dialog
                    .submit(&pages, scope, credential, &clock, value)
                    .await?
                    .is_some()
                {
                    self.notice = Some((
                        NoticeKind::Success,
                        format!("{} created.", dialog.kind.label()),
                    ));
                    self.dialog = None;
                    self.history = false;
                }
            }
            "request_close" => {
                let id = value["id"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                let activity = self.pages.activities.get(scope, id).await?;
                self.confirm = Some((activity.id, activity.title, activity.version));
            }
            "cancel_close" => self.confirm = None,
            "close" => {
                let Some((id, title, version)) = self.confirm.take() else {
                    return Ok(());
                };
                let call = self
                    .pages
                    .call(scope, credential, "close", "core.activity.close");
                self.notice = Some(
                    match self
                        .pages
                        .contracts
                        .submit(&call, json!({"id": id, "version": version}))
                        .await
                    {
                        Ok(_) => (NoticeKind::Success, format!("“{title}” closed.")),
                        Err(error) => match refusal(
                            error,
                            "You do not have permission to close this activity.",
                            "The activity was not closed. Try again.",
                        )? {
                            Refusal::Message(message) => (NoticeKind::Error, message),
                            Refusal::Fields(fields) => (NoticeKind::Error, sentences(&fields)),
                        },
                    },
                );
            }
            "clear_notice" | "clear-flash" => self.notice = None,
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    async fn row(
        &self,
        scope: &Scope,
        clock: &Clock,
        activity: &Activity,
        names: &HashMap<i64, String>,
    ) -> Result<FlexRow> {
        let id = activity.id.to_string();
        let mut kind = render(Icon {
            name: icon(activity.kind),
            class: "size-4",
        })?;
        kind.push(&Html::text(&format!(" {}", activity.kind.label())));
        let mut row = FlexRow::new(&id)
            .slot(
                "title",
                render(RecordLink {
                    workspace: self.framed,
                    kind: crate::MODULE,
                    record_id: &id,
                    href: &format!("/activities/{id}"),
                    class: "font-medium text-ink-strong hover:underline",
                    body: Html::text(&activity.title),
                })?,
            )
            .slot("kind", kind)
            .slot("status", status_badge(activity)?)
            .slot(
                "assignee",
                Html::text(
                    names
                        .get(&activity.assignee_user_id)
                        .map_or("—", String::as_str),
                ),
            );
        row.cells.insert(
            "when".into(),
            bilibi_ui::flex_table::Cell::instant(clock.instant(activity.when())),
        );
        if !activity.is_closed()
            && activity.kind.closing().is_some()
            && self.pages.activities.may_edit(scope, activity).await?
        {
            row.actions.push(&render(IconButton {
                id: &format!("{}-close-{id}", self.anchor),
                event: "request_close",
                name: "id",
                value: &id,
                ..IconButton::new("check-circle", "Close")
            })?);
        }
        row.dom_id = Some(format!("{}-activity-{id}", self.anchor));
        Ok(row)
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
        let state = if self.history {
            State::Closed
        } else {
            State::Open
        };
        let page = self
            .pages
            .activities
            .for_record(scope, &self.record, state, ROWS)
            .await?;
        let users: Vec<i64> = page.entries.iter().map(|a| a.assignee_user_id).collect();
        let names = self.pages.names(scope, &users).await?;
        let mut rows = Vec::with_capacity(page.entries.len());
        for activity in &page.entries {
            rows.push(self.row(scope, &clock, activity, &names).await?);
        }
        let anchor = self.anchor.clone();
        let mut actions = Html::EMPTY;
        if self.pages.activities.may_create(scope).await? {
            for (kind, label) in [
                ("call", "Log call"),
                ("task", "New task"),
                ("note", "New note"),
            ] {
                actions.push(&render(Button {
                    id: &format!("{anchor}-new-{kind}"),
                    button_type: "button",
                    event: "new",
                    name: "kind",
                    value: kind,
                    variant: if kind == "task" {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::default()
                    },
                    body: Html::text(label),
                    ..Button::default()
                })?);
            }
        }
        let heading_id = format!("{anchor}-heading");
        let mut content = render(SectionHeading {
            id: &heading_id,
            title: "Activities",
            actions,
            ..SectionHeading::default()
        })?;
        if let Some((kind, message)) = &self.notice {
            content.push(&render(PanelNotice {
                id: &format!("{anchor}-notice"),
                kind: *kind,
                dismiss_event: "clear_notice",
                class: "mb-3",
                body: Html::text(message),
            })?);
        }
        let (planned, history) = (
            format!("{anchor}-tab-planned"),
            format!("{anchor}-tab-history"),
        );
        content.push(&render(Tabs {
            id: &format!("{anchor}-tabs"),
            label: "Activities",
            tabs: vec![
                Tab {
                    id: &planned,
                    ..Tab::button("tab", "planned", "Planned", !self.history)
                },
                Tab {
                    id: &history,
                    ..Tab::button("tab", "history", "History", self.history)
                },
            ],
            ..Tabs::default()
        })?);
        let mut table = FlexTable::new(&anchor, self.columns.flex_columns(), rows);
        table.event = "grid";
        table.zoom = self.columns.zoom;
        table.caption = if self.history {
            "Closed activities"
        } else {
            "Open activities"
        };
        table.framed = false;
        table.actions = true;
        table.empty = Some(TableEmpty::Content(Html::text(if self.history {
            "Nothing closed yet."
        } else {
            "Nothing planned."
        })));
        content.push(&render(table)?);
        if page.total > ROWS {
            let state = if self.history { "closed" } else { "open" };
            let href = format!(
                "/activities?related={}:{}&state={state}",
                self.record.kind, self.record.id
            );
            content.push(&render(ActionLink {
                id: &format!("{anchor}-all"),
                href: &href,
                body: Html::text(&format!("View all {}", page.total)),
                ..ActionLink::default()
            })?);
        }
        if let Some((_, title, _)) = &self.confirm {
            content.push(&render(ConfirmDialog {
                id: &format!("{anchor}-confirm"),
                consequence: &format!("“{title}” will be closed."),
                detail: "A call or meeting is marked held, a task completed.",
                confirm: "Close",
                working: "Closing…",
                confirm_event: "close",
                cancel_event: "cancel_close",
            })?);
        }
        if let Some(dialog) = &self.dialog {
            content.push(&dialog.render(
                &self.pages,
                &clock,
                &format!("{anchor}-new"),
                "close_modal",
                self.framed,
            )?);
        }
        let card = render(Card {
            id: &format!("{anchor}-card"),
            role: "region",
            labelledby: &heading_id,
            body: content,
            ..Card::default()
        })?;
        Ok(LiveBody {
            html: render(Region {
                id: &anchor,
                target: true,
                timezone_request: self.zone.request(),
                body: card,
            })?
            .to_string(),
            ..LiveBody::default()
        })
    }
}

impl LiveState<Scope, Error> for Panel {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh | LiveInput::Params(_) => {}
                LiveInput::Event(value) => {
                    self.handle(request.scope, request.credential, value)
                        .await?;
                }
            }
            self.view(request.scope).await
        })
    }
}
