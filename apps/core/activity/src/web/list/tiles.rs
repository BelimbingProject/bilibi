//! The Activities list's status tiles (`display=board`) and the free-slot
//! chooser of its calendar: reading the tasks into base/ui `Board` columns,
//! the tile drop and Move dialog that set a task's status through
//! `core.activity.update`, and the dialog that asks which kind a free
//! calendar slot is for. A child of `list.rs`, so it shares `Index`'s state.
use super::Index;
use crate::web::{refusal, render, sentences, urgency, Clock, Refusal};
use crate::{Activity, Filter, Kind, Sort, State, Status};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    board::{Board, BoardCard, BoardColumn, BoardMove},
    calendar::{CalendarSlot, CalendarWhen},
    choices, Button, ButtonVariant, Field, Form, Html, InputKind, Modal, ModalWidth, StatusKind,
};
use std::collections::HashMap;

/// The open tasks the status tiles read, the most pressing first: more is
/// refused with a notice.
const BOARD_TASKS: i64 = 300;
/// The closed tasks they read, the latest first, so a tenant's history
/// never crowds the open work off the tiles.
const BOARD_CLOSED: i64 = 100;
/// A slot chosen on the calendar with no end of its own starts at 09:00 and
/// lasts an hour.
const SLOT_HOUR: i64 = 9;

/// The tiles' columns: a task's statuses, each with how many of `tasks` hold it.
pub(super) fn board_columns(tasks: &[Activity]) -> Vec<BoardColumn> {
    Kind::Task
        .statuses()
        .iter()
        .map(|status| {
            let total = tasks.iter().filter(|a| a.status == Some(*status)).count();
            BoardColumn::new(status.code(), status.label(), total as u64)
        })
        .collect()
}

/// A chosen slot as start and end: a day cell (no time of its own) starts
/// at 09:00 in the reader's zone and lasts an hour.
pub(super) fn slot_times(slot: &CalendarSlot, clock: &Clock) -> (jiff::Timestamp, jiff::Timestamp) {
    match &slot.when {
        CalendarWhen::Timed { start, end } => (*start, *end),
        CalendarWhen::AllDay { first, .. } => {
            let zone = jiff::tz::TimeZone::get(&clock.zone).unwrap_or(jiff::tz::TimeZone::UTC);
            let start = first
                .at(SLOT_HOUR as i8, 0, 0, 0)
                .to_zoned(zone)
                .map(|zoned| zoned.timestamp())
                .unwrap_or_else(|_| jiff::Timestamp::now());
            (start, start + jiff::SignedDuration::from_hours(1))
        }
    }
}

impl Index {
    /// A tile dropped on another column, or the Move dialog: the task's
    /// status through the change contract, with the version the tile was
    /// drawn with. `Err` is the sentence the page shows.
    pub(super) async fn set_status(
        &self,
        scope: &Scope,
        credential: &str,
        event: &str,
        change: &BoardMove,
    ) -> Result<std::result::Result<String, String>> {
        let missing = || Ok(Err("The task could not be found.".to_owned()));
        let (Ok(id), Ok(version)) = (change.id.parse::<i64>(), change.version.parse::<i64>())
        else {
            return missing();
        };
        let activity = match self.pages.activities.get(scope, id).await {
            Ok(activity) => activity,
            Err(Error::NotFound | Error::Forbidden) => return missing(),
            Err(error) => return Err(error),
        };
        if activity.kind != Kind::Task {
            return Ok(Err("Only a task has a status to move.".to_owned()));
        }
        if activity.status.map(Status::code) != Some(change.from.as_str()) {
            return Ok(Err(
                "The task moved since the page was drawn. Look at it again.".to_owned(),
            ));
        }
        let Some(to) = Status::parse(&change.to).filter(|to| Kind::Task.statuses().contains(to))
        else {
            return Ok(Err("Choose a column to move the task to.".to_owned()));
        };
        let call = self
            .pages
            .call(scope, credential, event, "core.activity.update");
        match self
            .pages
            .contracts
            .submit(
                &call,
                serde_json::json!({"id": id, "version": version, "patch": {"status": to.code()}}),
            )
            .await
        {
            Ok(_) => Ok(Ok(format!("“{}” moved to {}.", activity.title, to.label()))),
            Err(error) => Ok(Err(
                match refusal(
                    error,
                    "You do not have permission to move this task.",
                    "The task was not moved. Try again.",
                )? {
                    Refusal::Message(message) => message,
                    Refusal::Fields(fields) => sentences(&fields),
                },
            )),
        }
    }

    /// The tasks as tiles in a column for each status, drawn by base/ui
    /// `Board`. Returns the HTML, its address query and how many tiles it
    /// drew.
    pub(super) async fn board_view(
        &self,
        scope: &Scope,
        clock: &Clock,
    ) -> Result<(Html, HashMap<String, String>, usize)> {
        let mut filter = self.filter();
        filter.kind = Some(Kind::Task);
        filter.sort = Sort::When;
        filter.page = 1;
        let open = Filter {
            state: State::Open,
            descending: false,
            page_size: BOARD_TASKS,
            ..filter.clone()
        };
        let closed = Filter {
            state: State::Closed,
            descending: true,
            page_size: BOARD_CLOSED,
            ..filter
        };
        let open = self.pages.activities.list(scope, &open).await?;
        let closed = self.pages.activities.list(scope, &closed).await?;
        let total = open.total + closed.total;
        let mut entries = open.entries;
        entries.extend(closed.entries);
        let columns = board_columns(&entries);
        let mut state = self.board.clone();
        state.read_column(&self.calendar_query, &columns);
        let me = scope.user();
        let people: Vec<i64> = entries.iter().map(|a| a.assignee_user_id).collect();
        let names = self.pages.names(scope, &people).await?;
        let may_create = self.pages.activities.may_create(scope).await?;
        let edit_all = may_create && self.pages.activities.may_edit_all(scope).await?;
        let targets: Vec<&str> = Kind::Task
            .statuses()
            .iter()
            .map(|status| status.code())
            .collect();
        let now = jiff::Timestamp::now();
        let mut drawn = Board::new("activities-board", "/activities", state.clone(), columns);
        drawn.query = self.address();
        drawn.query.insert("display".into(), "board".into());
        for a in &entries {
            let Some(status) = a.status else { continue };
            let mut card = BoardCard::new(a.id.to_string(), status.code(), a.title.clone())
                .href(format!("/activities/{}", a.id))
                .urgency(urgency(a, now));
            if let Some(due) = a.due_at {
                card = card.fact(format!("Due {}", clock.text(due)));
            }
            card = card.fact(format!(
                "Assigned to {}",
                if a.assignee_user_id == me {
                    "me"
                } else {
                    names.get(&a.assignee_user_id).map_or("—", String::as_str)
                }
            ));
            let own = a.created_by == me || a.assignee_user_id == me;
            if may_create && (own || edit_all) {
                card = card.movable(a.version.to_string(), targets.iter().copied());
            }
            drawn.cards.push(card);
        }
        drawn.moving = self.board_moving.clone();
        drawn.move_errors = self.board_errors.clone();
        drawn.notice = self.board_notice.clone().or_else(|| {
            (total > entries.len() as i64).then(|| {
                (
                    StatusKind::Warning,
                    format!(
                        "Showing {} of {} tasks: the open ones by due time, then the latest closed ones. Search or choose Assigned to me to narrow them.",
                        entries.len(),
                        total
                    ),
                )
            })
        });
        let html = drawn.render().map_err(|_| Error::Internal)?;
        let mut query = self.address();
        query.insert("display".into(), "board".into());
        query.extend(state.to_params());
        Ok((html, query, entries.len()))
    }

    /// The Add dialog a free calendar slot opens: which kind to add there.
    pub(super) fn slot_dialog(
        &self,
        clock: &Clock,
        start: jiff::Timestamp,
        end: jiff::Timestamp,
    ) -> Result<Html> {
        let body = render(
            Field::in_form("slot", "kind")
                .id("activities-slot-kind")
                .label("Add")
                .kind(InputKind::Select)
                .value("meeting")
                .options(choices(&[
                    ("A meeting", "meeting"),
                    ("A call", "call"),
                    ("A task", "task"),
                ])),
        )?;
        let mut actions = render(Button {
            id: "activities-slot-cancel",
            button_type: "button",
            event: "cancel_slot",
            body: Html::text("Cancel"),
            ..Button::default()
        })?;
        actions.push(&render(Button {
            id: "activities-slot-continue",
            button_type: "submit",
            variant: ButtonVariant::Primary,
            body: Html::text("Continue"),
            ..Button::default()
        })?);
        let form = render(Form {
            id: "activities-slot-form",
            event: "slot_continue",
            change: false,
            body,
            actions,
            ..Form::default()
        })?;
        render(Modal {
            id: "activities-slot-modal",
            title: "Add at this time",
            cancel_event: "cancel_slot",
            width: ModalWidth::Compact,
            description: &format!("{} to {}.", clock.text(start), clock.text(end)),
            body: form,
            ..Modal::default()
        })
    }
}
