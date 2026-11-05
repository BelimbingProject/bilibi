//! Tasks, calls, meetings and notes on any record (`core/activity`), and
//! their reminders. See `README.md` and ADR 24. Bilimbi has no counterpart;
//! the pages follow the Platform's list, record and record-panel patterns
//! (Core Person), and the related record is a `base/records` reference.
mod agent_operations;
mod contributions;
mod invitations;
mod model;
mod reminders;
mod store;
mod web;

use bilibi_database::{Db, Error, MutationTransaction, Result};
use bilibi_events::Events;
use bilibi_module_registry::{Layer, Module, Service};
use bilibi_principal_directory::PrincipalDirectory;
use bilibi_records::Records;
use bilibi_tenancy::{Authentication, ReadClient, Scope};
use std::sync::{Arc, OnceLock};

pub use bilibi_records::Reference;
pub use contributions::{MANAGE, MANAGE_ALL, VIEW, VIEW_ALL};
pub use invitations::{GateFuture, InvitationGate, Invitee, INVITATION_GATES};
pub use model::{
    CallInput, CallStatus, Common, Direction, Draft, FieldError, Guest, Kind, MeetingInput,
    NoteInput, Patch, Priority, Response, Status, TaskInput, MAX_GUESTS,
};

pub const MODULE: &str = "core/activity";
pub const SERVICE: &str = "core/activity.activities";
/// The audit kind of an activity and its `base/records` kind.
pub const RECORD: &str = "core.activity.activity";
/// The audit kind of a guest row.
pub const GUEST_RECORD: &str = "core.activity.guest";
/// The event a shared activity that has happened records (README, Events).
pub const HAPPENED: &str = "core/activity.happened";
/// Named background principal permission to create a task for an originating account.
pub const SYSTEM_CREATE: &str = "core.activity.system.create";

pub struct SystemTaskInput {
    pub company: bilibi_party_ids::OrganisationId,
    pub originating_user: i64,
    pub task: TaskInput,
}

/// Base Records, for the shared-activity rule inside `base/records`' own
/// access check of an activity, which has no facade to reach.
static RECORDS: OnceLock<Arc<Records>> = OnceLock::new();
pub(crate) fn records() -> Result<&'static Records> {
    RECORDS.get().map(Arc::as_ref).ok_or(Error::Internal)
}

/// One activity as stored.
#[derive(Clone, Debug)]
pub struct Activity {
    pub id: i64,
    /// The filing company: the creator's company. It grants no access.
    pub company_id: i64,
    pub kind: Kind,
    pub title: String,
    pub status: Option<Status>,
    pub priority: Option<Priority>,
    pub direction: Option<Direction>,
    pub starts_at: Option<jiff::Timestamp>,
    pub ends_at: Option<jiff::Timestamp>,
    pub due_at: Option<jiff::Timestamp>,
    pub start_at: Option<jiff::Timestamp>,
    pub location: Option<String>,
    pub join_url: Option<String>,
    pub meeting_type: Option<String>,
    pub outcome: Option<String>,
    pub description: Option<String>,
    pub internal_notes: Option<String>,
    pub related: Option<Reference>,
    /// Readers of the related record may see it too.
    pub shared: bool,
    pub assignee_user_id: i64,
    pub created_by: i64,
    pub created_at: jiff::Timestamp,
    pub updated_at: jiff::Timestamp,
    pub closed_at: Option<jiff::Timestamp>,
    /// When the assignee was reminded; cleared when the time it is due moves.
    pub reminded_at: Option<jiff::Timestamp>,
    pub version: i64,
}
impl Activity {
    pub fn is_closed(&self) -> bool {
        self.closed_at.is_some()
    }
    /// When it happens or is due: the start of a call or meeting, a task's
    /// due date, else when it was logged.
    pub fn when(&self) -> jiff::Timestamp {
        self.starts_at.or(self.due_at).unwrap_or(self.created_at)
    }
    /// Its values as a draft, with no guests.
    pub fn draft(&self) -> Draft {
        Draft {
            kind: self.kind,
            title: self.title.clone(),
            status: self.status,
            priority: self.priority,
            direction: self.direction,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
            due_at: self.due_at,
            start_at: self.start_at,
            outcome: self.outcome.clone(),
            location: self.location.clone(),
            join_url: self.join_url.clone(),
            meeting_type: self.meeting_type.clone(),
            description: self.description.clone(),
            internal_notes: self.internal_notes.clone(),
            related: self.related.clone(),
            shared: self.shared,
            assignee: Some(self.assignee_user_id),
            guests: Vec::new(),
        }
    }
}

/// Open (calls, meetings and tasks still to do), closed (closed ones and
/// notes, which record something that happened), or both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Open,
    Closed,
    All,
}

/// Whose activities a list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mine {
    /// Every activity the reader may see.
    #[default]
    Any,
    /// Assigned to the reader.
    Assignee,
    /// The reader is a guest.
    Guest,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    When,
    Title,
    Status,
    Created,
}

/// A page of the reader's activities. `page` counts from 1.
#[derive(Clone, Debug)]
pub struct Filter {
    pub search: String,
    pub kind: Option<Kind>,
    pub state: State,
    pub mine: Mine,
    /// Related to this record; for a person, also where they are a guest.
    /// Shared activities on it count only when the reader may view it
    /// (`base/records.check`, which `list` asks).
    pub related: Option<Reference>,
    pub sort: Sort,
    pub descending: bool,
    pub page: i64,
    pub page_size: i64,
}
impl Default for Filter {
    fn default() -> Self {
        Self {
            search: String::new(),
            kind: None,
            state: State::Open,
            mine: Mine::Any,
            related: None,
            sort: Sort::When,
            descending: false,
            page: 1,
            page_size: 25,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Page {
    pub entries: Vec<Activity>,
    pub total: i64,
}

/// A guest of an activity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestRow {
    pub id: i64,
    pub user_id: Option<i64>,
    pub person: Option<bilibi_party_ids::PersonId>,
    pub email: Option<String>,
    /// `creator`, `assignee`, `related` or `invited`.
    pub role: String,
    /// `none`, `accepted`, `declined` or `tentative`.
    pub response: String,
    /// When Send invites last included them.
    pub invited_at: Option<jiff::Timestamp>,
    /// When that invitation's email went out; account guests are told in the
    /// bell and have none.
    pub invite_sent_at: Option<jiff::Timestamp>,
}

/// Who Send invites reached: every guest but the sender. Account guests are
/// told in the bell; `emailable` of the others have an address the sender
/// may use and are emailed by the invitation worker.
#[derive(Clone, Debug)]
pub struct Invites {
    pub activity: Activity,
    pub guests: Vec<GuestRow>,
    pub emailable: usize,
}

/// Another open call or meeting that `user` takes part in at the same time.
/// `title` is `None` when the reader may not see that activity.
#[derive(Clone, Debug)]
pub struct Conflict {
    pub user: i64,
    pub activity: i64,
    pub title: Option<String>,
    pub starts_at: jiff::Timestamp,
    pub ends_at: jiff::Timestamp,
}

pub type WriteResult<T> = std::result::Result<T, WriteError>;
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("The activity could not be found.")]
    NotFound,
    #[error("You may not change this activity.")]
    Forbidden,
    #[error("The activity changed since this edit began. Reload before saving.")]
    Stale,
    #[error("This activity is already closed.")]
    AlreadyClosed,
    #[error("The assignee takes part until the activity is reassigned.")]
    AssigneeNotRemovable,
    #[error("Invalid input")]
    Invalid(Vec<FieldError>),
    #[error(transparent)]
    Access(#[from] Error),
}

/// The `core/activity` facade. Every call refreshes the scope and checks its
/// capability; no call takes a tenant or an actor from input.
pub struct Activities {
    db: Db,
    authentication: Arc<Authentication>,
    records: Arc<Records>,
    directory: Arc<PrincipalDirectory>,
    events: Arc<Events>,
    notifications: Arc<bilibi_notifications::NotificationBoundary>,
    queue: Arc<bilibi_queue::Queue>,
}
impl Activities {
    async fn fresh(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Scope> {
        self.authentication.reprove(client, scope).await
    }
    pub(crate) fn records(&self) -> &Records {
        &self.records
    }
    fn store(&self) -> store::Store<'_> {
        store::Store {
            records: &self.records,
            directory: &self.directory,
            events: &self.events,
            notifications: &self.notifications,
            queue: &self.queue,
        }
    }

    /// One activity the reader may see. `NotFound` when there is none in the
    /// tenant; `Forbidden` when it is not the reader's to see.
    pub async fn get(&self, scope: &Scope, id: i64) -> Result<Activity> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        store::get(&**client, &scope, id).await
    }
    /// The guests of an activity the reader may see.
    pub async fn guests(&self, scope: &Scope, id: i64) -> Result<Vec<GuestRow>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        store::get(&**client, &scope, id).await?;
        store::guests(&**client, &scope, id).await
    }
    /// The guests' email addresses the reader may use, by guest id: an
    /// address guest's own, a person's as Core Person lets the reader read
    /// it. Account guests are told in the bell and have none.
    pub async fn guest_emails(
        &self,
        scope: &Scope,
        guests: &[GuestRow],
    ) -> Result<std::collections::HashMap<i64, String>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        Ok(invitations::guest_emails(&**client, &scope, guests).await)
    }
    /// Other open calls and meetings between `starts_at` and `ends_at` that
    /// any of `users` takes part in (as assignee, or as an account guest who
    /// has not declined), other than `except`. Another activity's title shows
    /// only to a reader who may see it.
    pub async fn conflicts(
        &self,
        scope: &Scope,
        starts_at: jiff::Timestamp,
        ends_at: jiff::Timestamp,
        users: &[i64],
        except: Option<i64>,
    ) -> Result<Vec<Conflict>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        store::conflicts(&**client, &scope, starts_at, ends_at, users, except).await
    }
    /// The unsaved copy of a closed activity Close and create new offers:
    /// its values, related record and guests, with the status reset.
    pub async fn follow_up(&self, scope: &Scope, id: i64) -> Result<Draft> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        let activity = store::get(&**client, &scope, id).await?;
        store::follow_up(&**client, &scope, &activity).await
    }
    /// A page of the activities the reader may see. With `related`, shared
    /// activities on that record count when the reader may view it.
    pub async fn list(&self, scope: &Scope, filter: &Filter) -> Result<Page> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        let mut filter = filter.clone();
        let mut readable = false;
        if let Some(related) = &filter.related {
            if let Ok(reference) = self
                .records
                .check(&**client, &scope, &related.kind, &related.id)
                .await?
            {
                filter.related = Some(reference);
                readable = true;
            }
        }
        store::list(&**client, &scope, &filter, readable).await
    }
    /// The activities on one record the reader may see: related to it, and
    /// for a person also those they are a guest of. The record itself must
    /// be one the reader may view (`base/records.check`).
    pub async fn for_record(
        &self,
        scope: &Scope,
        record: &Reference,
        state: State,
        page_size: i64,
    ) -> Result<Page> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        let record = match self
            .records
            .check(&**client, &scope, &record.kind, &record.id)
            .await?
        {
            Ok(record) => record,
            Err(bilibi_records::Refusal::Forbidden) => return Err(Error::Forbidden),
            Err(_) => return Err(Error::NotFound),
        };
        let filter = Filter {
            state,
            related: Some(record),
            descending: state == State::Closed,
            page_size,
            ..Filter::default()
        };
        store::list(&**client, &scope, &filter, true).await
    }
    /// Whether the reader may create activities.
    pub async fn may_create(&self, scope: &Scope) -> Result<bool> {
        let client = self.db.get().await?;
        store::may_create(&**client, scope).await
    }
    /// Whether the reader may change, close or delete `activity`.
    pub async fn may_edit(&self, scope: &Scope, activity: &Activity) -> Result<bool> {
        let client = self.db.get().await?;
        store::may_edit(&**client, scope, activity).await
    }

    /// The checks a create makes, with no write, on the client a plan hands
    /// its preview: the validated draft with its assignee and records
    /// resolved.
    pub async fn preview_create(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        draft: &Draft,
    ) -> WriteResult<Draft> {
        self.store().prepare_create(client, scope, draft).await
    }
    /// Creates `draft` in the caller's mutation transaction: the creator and
    /// the assignee become guests, and so does a related person. A shared
    /// held call, completed task or note records `core/activity.happened`.
    pub async fn create(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        draft: &Draft,
    ) -> WriteResult<Activity> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().create(tx, &scope, draft).await
    }
    /// A task with a due date, priority and assignee.
    pub async fn create_task(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &TaskInput,
    ) -> WriteResult<Activity> {
        self.create(tx, scope, &Draft::task(input)).await
    }
    /// Creates a follow-up task inside the caller's transaction as a named system principal.
    pub async fn create_system_task(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: SystemTaskInput,
    ) -> WriteResult<Activity> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().create_system_task(tx, &scope, input).await
    }
    /// A call that was held or is planned; a held call has ended.
    pub async fn log_call(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &CallInput,
    ) -> WriteResult<Activity> {
        self.create(tx, scope, &Draft::call(input)).await
    }
    /// A meeting with its time, place and join link; a held meeting has
    /// ended.
    pub async fn create_meeting(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &MeetingInput,
    ) -> WriteResult<Activity> {
        self.create(tx, scope, &Draft::meeting(input)).await
    }
    /// A note about a record.
    pub async fn create_note(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &NoteInput,
    ) -> WriteResult<Activity> {
        self.create(tx, scope, &Draft::note(input)).await
    }
    /// What `update` would store, with no write.
    pub async fn preview_update(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Patch,
    ) -> WriteResult<(Activity, Draft)> {
        self.store()
            .prepare_update(client, scope, id, version, patch)
            .await
    }
    /// Changes the fields `patch` names, fenced on `version`. A shared
    /// activity that has now happened, or one that has happened and is now
    /// shared, records `core/activity.happened`.
    pub async fn update(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Patch,
    ) -> WriteResult<Activity> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().update(tx, &scope, id, version, patch).await
    }
    /// The activity Close would close, with no write.
    pub async fn preview_close(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<Activity> {
        store::prepare_close(client, scope, id, version).await
    }
    /// Calls and meetings to held, tasks to completed; a shared one records
    /// `core/activity.happened`. Refused when already closed, and for a note.
    pub async fn close(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<Activity> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().close(tx, &scope, id, version).await
    }
    /// Closes the activity and returns an unsaved copy: the same values,
    /// related record and guests, with the status reset.
    pub async fn close_and_create_new(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<(Activity, Draft)> {
        let scope = self.fresh(tx.sql(), scope).await?;
        let closed = self.store().close(tx, &scope, id, version).await?;
        let copy = store::follow_up(tx.sql(), &scope, &closed).await?;
        Ok((closed, copy))
    }
    /// The activity Delete would remove, with no write.
    pub async fn preview_delete(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<Activity> {
        store::prepare_delete(client, scope, id, version).await
    }
    /// Removes task `id` when it is still about `related`, whoever created
    /// or owns it, in the caller's transaction. For the owner of `related`
    /// (Core Document's expiry reminder): call it after the owner's own
    /// authorization. A task already gone or about another record is left
    /// alone; cleanup that fails is an error, never ignored.
    pub async fn delete_related(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        related: &Reference,
    ) -> WriteResult<()> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().delete_related(tx, &scope, id, related).await
    }
    /// Removes the activity (soft delete), fenced on `version`.
    pub async fn delete(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<()> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().delete(tx, &scope, id, version).await
    }
    /// The checks `add_guest` makes, with no write.
    pub async fn preview_add_guest(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        guest: &Guest,
    ) -> WriteResult<(Activity, Guest)> {
        self.store()
            .prepare_add_guest(client, scope, id, guest)
            .await
    }
    /// Invites an account, a person the actor may view, or an email address
    /// to an activity the actor may change. Guest changes name no version and
    /// leave the activity's as it was.
    pub async fn add_guest(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        guest: &Guest,
    ) -> WriteResult<GuestRow> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().add_guest(tx, &scope, id, guest).await
    }
    /// The guest `remove_guest` would remove, with no write.
    pub async fn preview_remove_guest(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        guest: i64,
    ) -> WriteResult<(Activity, GuestRow)> {
        self.store()
            .prepare_remove_guest(client, scope, id, guest)
            .await
    }
    /// Removes a guest; the assignee stays until the activity is
    /// reassigned (`AssigneeNotRemovable`).
    pub async fn remove_guest(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        guest: i64,
    ) -> WriteResult<GuestRow> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().remove_guest(tx, &scope, id, guest).await
    }
    /// The guest whose answer `respond` would record, with no write.
    pub async fn preview_respond(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        guest: i64,
    ) -> WriteResult<(Activity, GuestRow)> {
        store::prepare_respond(client, scope, id, guest).await
    }
    /// Records a guest's answer. An account guest answers for themselves
    /// with `core.activity.view`; anyone else's answer needs the right to
    /// change the activity.
    pub async fn respond(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        guest: i64,
        response: Response,
    ) -> WriteResult<GuestRow> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().respond(tx, &scope, id, guest, response).await
    }
    /// Who Send invites would reach, with no write.
    pub async fn preview_send_invites(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
    ) -> WriteResult<Invites> {
        self.store().prepare_send_invites(client, scope, id).await
    }
    /// Invites every guest but the sender to an open call or meeting still
    /// ahead, in the caller's transaction: account guests are told in the
    /// bell, and the invitation worker emails the others after commit, each
    /// once every invitation gate allows it.
    pub async fn send_invites(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
    ) -> WriteResult<Invites> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().send_invites(tx, &scope, id).await
    }
    /// Moves every live activity related to `from` to `to`, in the caller's
    /// transaction, after `base/records.check` of both for the caller. When
    /// `to` is a person, they become the `related` guest of the moved open
    /// calls and meetings, replacing the related email address. Each moved
    /// shared activity that has happened records `core/activity.happened`
    /// with reason `repointed`. Returns how many moved.
    pub async fn repoint(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        from: &Reference,
        to: &Reference,
    ) -> WriteResult<i64> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().repoint(tx, &scope, from, to).await
    }
}

inventory::submit! { Module {
    id: MODULE, layer: Layer::Core,
    dependencies: &["base/agent_api", "base/authz", "base/database", "base/datetime", "base/events", "base/grid", "base/locale", "base/mail", "base/menu", "base/notifications", "base/party_ids", "base/principal_directory", "base/queue", "base/records", "base/schedule", "base/settings", "base/tenancy", "base/ui", "core/company", "core/person", "core/user"],
    migrations: &[
        ("core-activity-001-activities", include_str!("../migrations/001-activities.sql")),
        ("core-activity-002-invitations", include_str!("../migrations/002-invitations.sql")),
    ],
    services: &[Service { id: SERVICE, factory: |s| {
        let records: Arc<Records> = s.get(bilibi_records::SERVICE)?;
        // One installation, one Records service; a second factory run gets the same.
        let _ = RECORDS.set(records.clone());
        let activities = Arc::new(Activities {
            db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
            authentication: s.get("base/tenancy.authentication")?,
            records: records.clone(),
            directory: s.get("base/principal_directory.directory")?,
            events: s.get(bilibi_events::SERVICE)?,
            notifications: s.get("base/notifications.sink")?,
            queue: s.get("base/queue.store")?,
        });
        agent_operations::bind(activities.clone())?;
        s.get::<bilibi_notifications::NotificationBoundary>("base/notifications.sink")?
            .register_todos(Arc::new(reminders::Todos {
                db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
                authentication: s.get("base/tenancy.authentication")?,
                records,
            }))?;
        Ok(activities)
    }}, Service { id: reminders::HANDLER, factory: |s| {
        Ok(Arc::new(bilibi_queue::HandlerService(Arc::new(reminders::Sweep {
            schedule: s.get("base/schedule.store")?,
            db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
            authentication: s.get("base/tenancy.authentication")?,
            notifications: s.get("base/notifications.sink")?,
            settings: s.get("base/settings.store")?,
            mailer: s.get("base/mail.mailer")?,
            origin: std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost:3000".into()).trim_end_matches('/').to_owned(),
        }))))
    }}, Service { id: invitations::HANDLER, factory: |s| {
        Ok(Arc::new(bilibi_queue::HandlerService(Arc::new(invitations::InviteJob {
            db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
            mailer: s.get("base/mail.mailer")?,
            settings: s.get("base/settings.store")?,
            directory: s.get("base/principal_directory.directory")?,
            authentication: s.get("base/tenancy.authentication")?,
        }))))
    }}, Service { id: web::SERVICE, factory: |s| {
        Ok(Arc::new(bilibi_ui::page::PageService::<Scope, Error>(Arc::new(web::Pages {
            activities: s.get(SERVICE)?,
            records: s.get(bilibi_records::SERVICE)?,
            contracts: s.get(bilibi_agent_api::page::SERVICE)?,
            directory: s.get("base/principal_directory.directory")?,
            display: s.get("base/datetime.display")?,
            locales: s.get("base/locale.locales")?,
            db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
        }))))
    }}],
} }
