//! `core_activities` and `core_activity_guests` SQL. Every statement binds the
//! tenant; every write goes through the caller's `MutationTransaction` and
//! returns its `Change`s. Checks run before a write, so a refusal never
//! poisons the caller's transaction.
use crate::model::{self, Draft, FieldError, Guest, Kind, Response, Status};
use crate::series::{self, Apply, Frequency, Repeat, Series};
use crate::{
    Activity, Conflict, Direction, Filter, GuestRow, Invites, Mine, Page, Patch, Priority,
    Reference, Sort, State, WriteError, WriteResult, GUEST_RECORD, MANAGE, MANAGE_ALL, RECORD,
    SERIES_RECORD, VIEW, VIEW_ALL,
};
use bilibi_authz::{authorize_proof, holds_proof};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_events::{Events, NewEvent};
use bilibi_party_ids::PersonId;
use bilibi_principal_directory::{
    DirectoryClient, DirectoryFuture, PrincipalDirectory, PrincipalKind, PrincipalRef, Query,
};
use bilibi_records::Records;
use bilibi_tenancy::{ProofClient, ReadClient, Scope};
use jiff::Timestamp;
use serde_json::{json, Value};
use std::time::SystemTime;
use tokio_postgres::{types::ToSql, Row};

const COLUMNS: &str = "a.id,a.company_id,a.kind,a.subject,a.status,a.priority,a.direction,a.starts_at,a.ends_at,a.due_at,a.start_at,a.location,a.join_url,a.meeting_type,a.call_outcome,a.description,a.internal_notes,a.related_kind,a.related_id,a.shared,a.assignee_user_id,a.created_by,a.created_at,a.updated_at,a.closed_at,a.reminded_at,a.series_id,a.series_index,a.version";
/// The activities the reader takes part in, with `$2` the reader's account:
/// they created it, are assigned it or are an account guest. Nothing else
/// (filing company, a related record) reaches a private activity.
pub(crate) const MINE: &str = "(a.created_by=$2 OR a.assignee_user_id=$2 OR EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_user_id=$2))";

/// The columns a guest row is read with.
const GUEST_COLUMNS: &str =
    "id,guest_user_id,guest_person_id,guest_email,role,response,invited_at,invite_sent_at";

pub(crate) fn time(value: Option<SystemTime>) -> Result<Option<Timestamp>> {
    value
        .map(|t| Timestamp::try_from(t).map_err(|_| Error::Internal))
        .transpose()
}
pub(crate) fn system(value: Option<Timestamp>) -> Option<SystemTime> {
    value.map(SystemTime::from)
}

fn from_row(row: &Row) -> Result<Activity> {
    fn code<T>(row: &Row, column: &str, parse: fn(&str) -> Option<T>) -> Result<Option<T>> {
        row.get::<_, Option<String>>(column)
            .map(|c| parse(&c).ok_or(Error::Internal))
            .transpose()
    }
    let related = match (
        row.get::<_, Option<String>>("related_kind"),
        row.get::<_, Option<String>>("related_id"),
    ) {
        (Some(kind), Some(id)) => Some(Reference::new(kind, id)),
        _ => None,
    };
    Ok(Activity {
        id: row.get("id"),
        company_id: row.get("company_id"),
        kind: code(row, "kind", Kind::parse)?.ok_or(Error::Internal)?,
        title: row.get("subject"),
        status: code(row, "status", Status::parse)?,
        priority: code(row, "priority", Priority::parse)?,
        direction: code(row, "direction", Direction::parse)?,
        starts_at: time(row.get("starts_at"))?,
        ends_at: time(row.get("ends_at"))?,
        due_at: time(row.get("due_at"))?,
        start_at: time(row.get("start_at"))?,
        location: row.get("location"),
        join_url: row.get("join_url"),
        meeting_type: row.get("meeting_type"),
        outcome: row.get("call_outcome"),
        description: row.get("description"),
        internal_notes: row.get("internal_notes"),
        related,
        shared: row.get("shared"),
        assignee_user_id: row.get("assignee_user_id"),
        created_by: row.get("created_by"),
        created_at: time(Some(row.get("created_at")))?.ok_or(Error::Internal)?,
        updated_at: time(Some(row.get("updated_at")))?.ok_or(Error::Internal)?,
        closed_at: time(row.get("closed_at"))?,
        reminded_at: time(row.get("reminded_at"))?,
        series_id: row.get("series_id"),
        series_index: row.get("series_index"),
        version: row.get("version"),
    })
}

/// The values audit keeps for an activity.
fn audit_values(a: &Activity) -> Value {
    let iso = |t: Option<Timestamp>| t.map(|t| t.to_string());
    json!({
        "kind": a.kind.code(),
        "subject": a.title,
        "status": a.status.map(Status::code),
        "priority": a.priority.map(Priority::code),
        "direction": a.direction.map(Direction::code),
        "starts_at": iso(a.starts_at),
        "ends_at": iso(a.ends_at),
        "due_at": iso(a.due_at),
        "start_at": iso(a.start_at),
        "call_outcome": a.outcome,
        "location": a.location,
        "join_url": a.join_url,
        "meeting_type": a.meeting_type,
        "description": a.description,
        "internal_notes": a.internal_notes,
        "related": a.related.as_ref().map(|r| json!({"kind": r.kind, "id": r.id})),
        "shared": a.shared,
        "assignee_user_id": a.assignee_user_id,
        "closed_at": iso(a.closed_at),
        "series_id": a.series_id,
        "series_index": a.series_index,
        "version": a.version,
    })
}

/// The company an activity is filed under: the desk the creator works at,
/// else their own company. It grants no access.
fn filing_company(scope: &Scope) -> Option<i64> {
    scope
        .current_company()
        .or(scope.actor().organisation())
        .map(|company| company.get())
}

/// Who is reading, and whether they hold the tenant-wide oversight read.
struct Reader {
    tenant: i64,
    user: i64,
    all: bool,
}
async fn reader(client: &dyn ProofClient, scope: &Scope) -> Result<Reader> {
    authorize_proof(client, scope, VIEW).await?;
    Ok(Reader {
        tenant: scope.tenant(),
        user: scope.user(),
        all: holds_proof(client, scope, VIEW_ALL).await?,
    })
}

/// A shared activity's related record is one the reader may view, by its
/// owner's own read check (`base/records`' `authorize`), on the caller's
/// client. A private one, or one with no related record, never is.
async fn shared_readable(
    client: &dyn ProofClient,
    scope: &Scope,
    activity: &Activity,
) -> Result<bool> {
    let (true, Some(related)) = (activity.shared, &activity.related) else {
        return Ok(false);
    };
    let Some(kind) = crate::records()?.kind(&related.kind) else {
        return Ok(false);
    };
    Ok(matches!(
        kind.owner.authorize(client, scope, &related.id).await?,
        bilibi_records::Access::Readable
    ))
}

pub(crate) async fn get(client: &dyn ProofClient, scope: &Scope, id: i64) -> Result<Activity> {
    visible(client, scope, id, false).await
}

/// The live activity `id` under the reader's visibility rule: `Forbidden`
/// when it is not theirs to see, `NotFound` when the tenant has none. `lock`
/// holds it for a write.
async fn visible(client: &dyn ProofClient, scope: &Scope, id: i64, lock: bool) -> Result<Activity> {
    let reader = reader(client, scope).await?;
    let sql = format!(
        "SELECT {COLUMNS}, ($3 OR {MINE}) AS mine FROM core_activities a WHERE a.tenant_id=$1 AND a.id=$4 AND a.deleted_at IS NULL{}",
        if lock { " FOR UPDATE OF a" } else { "" }
    );
    let Some(row) = client
        .query_opt(&sql, &[&reader.tenant, &reader.user, &reader.all, &id])
        .await?
    else {
        return Err(Error::NotFound);
    };
    let activity = from_row(&row)?;
    if row.get::<_, bool>("mine") || shared_readable(client, scope, &activity).await? {
        Ok(activity)
    } else {
        Err(Error::Forbidden)
    }
}

/// Whether the reader may see `id`, for `base/records`: `NotFound` when the
/// tenant has no such live activity.
pub(crate) async fn readable_proof(client: &dyn ProofClient, scope: &Scope, id: i64) -> Result<()> {
    get(client, scope, id).await.map(|_| ())
}

pub(crate) async fn guests(
    client: &dyn ReadClient,
    scope: &Scope,
    id: i64,
) -> Result<Vec<GuestRow>> {
    client
        .query(
            &format!("SELECT {GUEST_COLUMNS} FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 ORDER BY id"),
            &[&scope.tenant(), &id],
        )
        .await?
        .iter()
        .map(guest_row)
        .collect()
}
fn guest_row(row: &Row) -> Result<GuestRow> {
    Ok(GuestRow {
        id: row.get("id"),
        user_id: row.get("guest_user_id"),
        person: row
            .get::<_, Option<i64>>("guest_person_id")
            .map(|id| PersonId::new(id).map_err(|_| Error::Internal))
            .transpose()?,
        email: row.get("guest_email"),
        role: row.get("role"),
        response: row.get("response"),
        invited_at: time(row.get("invited_at"))?,
        invite_sent_at: time(row.get("invite_sent_at"))?,
    })
}

/// Open calls and meetings between `starts_at` and `ends_at` that any of
/// `users` takes part in (as assignee, or as an account guest who has not
/// declined), other than `except`. The title shows only for an activity the
/// reader takes part in or oversees: a busy colleague's private meeting stays
/// private.
pub(crate) async fn conflicts(
    client: &dyn ReadClient,
    scope: &Scope,
    starts_at: Timestamp,
    ends_at: Timestamp,
    users: &[i64],
    except: Option<i64>,
) -> Result<Vec<Conflict>> {
    let reader = reader(client, scope).await?;
    if users.is_empty() || starts_at >= ends_at {
        return Ok(Vec::new());
    }
    let users: Vec<i64> = users.iter().copied().take(model::MAX_GUESTS).collect();
    let except = except.unwrap_or(0);
    let rows = client
        .query(
            &format!("SELECT p.user_id,a.id,a.subject,a.starts_at,a.ends_at,($3 OR {MINE}) AS visible
               FROM core_activities a
               JOIN LATERAL (
                    SELECT a.assignee_user_id AS user_id
                    UNION
                    SELECT g.guest_user_id FROM core_activity_guests g
                     WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_user_id IS NOT NULL AND g.response<>'declined'
               ) p ON p.user_id = ANY($4)
              WHERE a.tenant_id=$1 AND a.deleted_at IS NULL AND a.kind IN ('call','meeting') AND a.status='scheduled'
                AND a.starts_at < $6 AND a.ends_at > $5 AND a.id<>$7
              ORDER BY a.starts_at,a.id,p.user_id LIMIT 50"),
            &[
                &reader.tenant,
                &reader.user,
                &reader.all,
                &users,
                &SystemTime::from(starts_at),
                &SystemTime::from(ends_at),
                &except,
            ],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(Conflict {
                user: row.get(0),
                activity: row.get(1),
                title: row.get::<_, bool>(5).then(|| row.get(2)),
                starts_at: time(Some(row.get(3)))?.ok_or(Error::Internal)?,
                ends_at: time(Some(row.get(4)))?.ok_or(Error::Internal)?,
            })
        })
        .collect()
}

/// `related_readable`: the reader may view `filter.related` (the caller ran
/// `base/records.check`), so the shared activities on it count too.
pub(crate) async fn list(
    client: &dyn ReadClient,
    scope: &Scope,
    filter: &Filter,
    related_readable: bool,
) -> Result<Page> {
    let reader = reader(client, scope).await?;
    // A calendar reads the whole range it shows, up to what it can draw.
    let largest = if filter.between.is_some() {
        crate::CALENDAR_LIMIT
    } else {
        300
    };
    let page_size = filter.page_size.clamp(1, largest);
    let offset = (filter.page.max(1) - 1).saturating_mul(page_size);
    let search = filter.search.trim().chars().take(200).collect::<String>();
    let pattern = format!(
        "%{}%",
        search
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let kind = filter.kind.map(Kind::code);
    let state = match filter.state {
        State::Open => "open",
        State::Closed => "closed",
        State::All => "all",
    };
    let mine = match filter.mine {
        Mine::Any => "any",
        Mine::Assignee => "assignee",
        Mine::Guest => "guest",
    };
    let (related_kind, related_id) = filter.related.as_ref().map_or((None, None), |r| {
        (Some(r.kind.as_str()), Some(r.id.as_str()))
    });
    // A person's activities include those they are a guest of.
    let person = filter
        .related
        .as_ref()
        .filter(|r| r.kind == "person")
        .and_then(|r| r.id.parse::<i64>().ok());
    let shared = related_readable && filter.related.is_some();
    let (from, to) = filter.between.map_or((None, None), |(from, to)| {
        (Some(SystemTime::from(from)), Some(SystemTime::from(to)))
    });
    // Visible: oversight, taking part, or shared on the record the list is
    // narrowed to when the reader may view that record.
    let conditions = format!(
        "a.tenant_id=$1 AND a.deleted_at IS NULL
         AND ($3 OR {MINE} OR ($4 AND a.shared AND a.related_kind = $10 AND a.related_id = $11))
         AND ($5 = '' OR a.subject ILIKE $6)
         AND ($7::text IS NULL OR a.kind = $7)
         AND ($8 = 'all' OR ($8 = 'open') = (a.closed_at IS NULL AND a.kind <> 'note'))
         AND ($9 = 'any'
              OR ($9 = 'assignee' AND a.assignee_user_id = $2)
              OR ($9 = 'guest' AND EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_user_id=$2 AND g.role IN ('related','invited'))))
         AND ($10::text IS NULL
              OR (a.related_kind = $10 AND a.related_id = $11)
              OR ($12::bigint IS NOT NULL AND EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_person_id=$12)))
         AND ($13::timestamptz IS NULL
              OR (a.kind IN ('call','meeting') AND a.starts_at < $14 AND a.ends_at > $13)
              OR (a.kind = 'task' AND a.due_at >= $13 AND a.due_at < $14))
         AND ($15::bigint IS NULL OR a.series_id = $15)"
    );
    let params: [&(dyn ToSql + Sync); 15] = [
        &reader.tenant,
        &reader.user,
        &reader.all,
        &shared,
        &search,
        &pattern,
        &kind,
        &state,
        &mine,
        &related_kind,
        &related_id,
        &person,
        &from,
        &to,
        &filter.series,
    ];
    let total: i64 = client
        .query_opt(
            &format!("SELECT count(*) FROM core_activities a WHERE {conditions}"),
            &params,
        )
        .await?
        .ok_or(Error::Internal)?
        .get(0);
    let order = match filter.sort {
        Sort::When if filter.series.is_some() => "a.series_index",
        Sort::When => "COALESCE(a.starts_at, a.due_at, a.created_at)",
        Sort::Title => "lower(a.subject)",
        Sort::Status => "a.status",
        Sort::Created => "a.created_at",
    };
    let direction = if filter.descending { "DESC" } else { "ASC" };
    let sql = format!(
        "SELECT {COLUMNS} FROM core_activities a WHERE {conditions} ORDER BY {order} {direction} NULLS LAST, a.id {direction} LIMIT {page_size} OFFSET {offset}"
    );
    let entries = client
        .query(&sql, &params)
        .await?
        .iter()
        .map(from_row)
        .collect::<Result<Vec<_>>>()?;
    Ok(Page { entries, total })
}

/// Every write needs `core.activity.manage`, the capability the operations
/// declare; `core.activity.all.manage` only widens which activities it
/// reaches.
pub(crate) async fn may_create(client: &dyn ProofClient, scope: &Scope) -> Result<bool> {
    holds_proof(client, scope, MANAGE).await
}
pub(crate) async fn may_edit(
    client: &dyn ProofClient,
    scope: &Scope,
    activity: &Activity,
) -> Result<bool> {
    Ok(holds_proof(client, scope, MANAGE).await?
        && (own(scope, activity) || holds_proof(client, scope, MANAGE_ALL).await?))
}
fn own(scope: &Scope, activity: &Activity) -> bool {
    activity.created_by == scope.user() || activity.assignee_user_id == scope.user()
}
/// `core.activity.manage` on an activity the actor created or is assigned,
/// and with `core.activity.all.manage` on anyone's.
async fn authorize_edit(
    client: &dyn ProofClient,
    scope: &Scope,
    activity: &Activity,
) -> WriteResult<()> {
    authorize_proof(client, scope, MANAGE).await?;
    if own(scope, activity) || holds_proof(client, scope, MANAGE_ALL).await? {
        Ok(())
    } else {
        Err(WriteError::Forbidden)
    }
}

/// The activity a write acts on: live, one the actor may see, editable by
/// them, at `version`. A write never reaches, or returns, an activity the
/// actor could not open.
async fn editable(
    client: &dyn ProofClient,
    scope: &Scope,
    id: i64,
    version: i64,
    lock: bool,
) -> WriteResult<Activity> {
    let activity = visible(client, scope, id, lock)
        .await
        .map_err(|error| match error {
            Error::NotFound => WriteError::NotFound,
            Error::Forbidden => WriteError::Forbidden,
            error => WriteError::Access(error),
        })?;
    authorize_edit(client, scope, &activity).await?;
    if activity.version != version {
        return Err(WriteError::Stale);
    }
    Ok(activity)
}

/// The activity a guest change acts on: live, one the actor may see and
/// change. Guest changes touch no field of the activity, so they name no
/// version; `lock` queues them behind any other write to it.
async fn editable_now(
    client: &dyn ProofClient,
    scope: &Scope,
    id: i64,
    lock: bool,
) -> WriteResult<Activity> {
    let activity = seen(client, scope, id, lock).await?;
    authorize_edit(client, scope, &activity).await?;
    Ok(activity)
}
/// The live activity `id` the actor may see, for a write.
async fn seen(
    client: &dyn ProofClient,
    scope: &Scope,
    id: i64,
    lock: bool,
) -> WriteResult<Activity> {
    visible(client, scope, id, lock)
        .await
        .map_err(|error| match error {
            Error::NotFound => WriteError::NotFound,
            Error::Forbidden => WriteError::Forbidden,
            error => WriteError::Access(error),
        })
}

/// Directory reads on a borrowed client, so a preview checks an assignee on
/// the plan's own transaction.
pub(crate) struct Directory<'c>(pub &'c dyn ReadClient);
impl DirectoryClient for Directory<'_> {
    fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> DirectoryFuture<'a, Vec<Row>> {
        self.0.query(sql, params)
    }
}

/// What a write checks against the database (records, people and accounts),
/// and the events it records.
pub(crate) struct Store<'s> {
    pub records: &'s Records,
    pub directory: &'s PrincipalDirectory,
    pub events: &'s Events,
    pub notifications: &'s bilibi_notifications::NotificationBoundary,
    pub queue: &'s bilibi_queue::Queue,
    pub calendars: &'s bilibi_work_calendar::WorkCalendars,
}
/// Which references a change touches, so unchanged ones are not rechecked
/// (a record the actor can no longer view does not block other edits).
#[derive(Clone, Copy)]
struct Touched {
    related: bool,
    assignee: bool,
    guests: bool,
}
impl Store<'_> {
    async fn check_reference(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        reference: &Reference,
    ) -> Result<std::result::Result<Reference, &'static str>> {
        Ok(self
            .records
            .check(client, scope, &reference.kind, &reference.id)
            .await?
            .map_err(|refusal| refusal.message()))
    }
    async fn active_account(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        user: i64,
    ) -> Result<bool> {
        let Ok(principal) = PrincipalRef::new(PrincipalKind::User, user) else {
            return Ok(false);
        };
        let ranked = self
            .directory
            .rank(&Directory(client), scope, &[principal], &Query::default())
            .await
            .map_err(|_| Error::Internal)?;
        Ok(!ranked.named.is_empty())
    }
    /// The draft's field rules, then each touched reference for this actor.
    /// Returns the draft as it will be stored: checked references in their
    /// one spelling (and a merged record as its survivor).
    async fn validate(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        draft: &Draft,
        touched: Touched,
    ) -> WriteResult<Draft> {
        let mut errors = model::check(draft);
        let mut checked = draft.clone();
        if let (true, Some(related)) = (touched.related, &draft.related) {
            // An activity about an activity would make the shared rule ask
            // itself; point at the record the first one is about instead.
            if related.kind == RECORD {
                errors.push(FieldError::new("related", "can't be another activity"));
            } else {
                match self.check_reference(client, scope, related).await? {
                    Ok(reference) => checked.related = Some(reference),
                    Err(message) => errors.push(FieldError::new("related", message)),
                }
            }
        }
        if let (true, Some(assignee)) = (touched.assignee, draft.assignee) {
            if assignee > 0 && !self.active_account(client, scope, assignee).await? {
                errors.push(FieldError::new("assignee", "is not an active account"));
            }
        }
        if touched.guests && draft.guests.len() <= model::MAX_GUESTS {
            let mut guests: Vec<Guest> = Vec::new();
            for guest in &draft.guests {
                let guest = match guest {
                    Guest::User(user) => {
                        if *user > 0 && !self.active_account(client, scope, *user).await? {
                            errors.push(FieldError::new(
                                "guests",
                                format!("Account {user} is not an active account"),
                            ));
                            continue;
                        }
                        Guest::User(*user)
                    }
                    Guest::Person(person) => {
                        let reference = Reference::new("person", person.get().to_string());
                        match self.check_reference(client, scope, &reference).await? {
                            Ok(reference) => Guest::Person(
                                reference
                                    .id
                                    .parse::<i64>()
                                    .ok()
                                    .and_then(|id| PersonId::new(id).ok())
                                    .ok_or(Error::Internal)?,
                            ),
                            Err(message) => {
                                errors.push(FieldError::new(
                                    "guests",
                                    format!("Person {} {message}", person.get()),
                                ));
                                continue;
                            }
                        }
                    }
                    Guest::Address { email, related } => Guest::Address {
                        email: email.trim().to_owned(),
                        related: *related,
                    },
                };
                let duplicate = guests.iter().any(|g| match (g, &guest) {
                    (Guest::User(a), Guest::User(b)) => a == b,
                    (Guest::Person(a), Guest::Person(b)) => a == b,
                    // As the unique index compares them: `lower()`, not
                    // ASCII case alone.
                    (Guest::Address { email: a, .. }, Guest::Address { email: b, .. }) => {
                        a.to_lowercase() == b.to_lowercase()
                    }
                    _ => false,
                });
                if !duplicate {
                    guests.push(guest);
                }
            }
            checked.guests = guests;
        }
        if errors.is_empty() {
            Ok(checked)
        } else {
            Err(WriteError::Invalid(errors))
        }
    }

    pub(crate) async fn prepare_create(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        draft: &Draft,
    ) -> WriteResult<Draft> {
        authorize_proof(client, scope, MANAGE).await?;
        let mut draft = draft.clone();
        draft.title = draft.title.trim().to_owned();
        draft.assignee = Some(draft.assignee.unwrap_or(scope.user()));
        if draft.kind.closing().is_none() {
            draft.status = None;
        }
        if filing_company(scope).is_none() {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "company",
                "Your account has no company to file activities under.",
            )]));
        }
        let mut errors = Vec::new();
        let touched = Touched {
            related: true,
            assignee: true,
            guests: true,
        };
        match self.validate(client, scope, &draft, touched).await {
            Ok(draft) if errors.is_empty() => Ok(draft),
            Ok(_) => Err(WriteError::Invalid(errors)),
            Err(WriteError::Invalid(mut more)) => {
                errors.append(&mut more);
                Err(WriteError::Invalid(errors))
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn create(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        draft: &Draft,
    ) -> WriteResult<crate::Activity> {
        let draft = self.prepare_create(tx.sql(), scope, draft).await?;
        let (tenant, user) = (scope.tenant(), scope.user());
        let company = filing_company(scope).ok_or(Error::Internal)?;
        self.insert(tx, draft, tenant, company, user, None).await
    }

    pub(crate) async fn create_system_task(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: crate::SystemTaskInput,
    ) -> WriteResult<Activity> {
        authorize_proof(tx.sql(), scope, crate::SYSTEM_CREATE).await?;
        if scope.actor().principal().is_none() || tx.context().tenant != scope.tenant() {
            return Err(Error::Forbidden.into());
        }
        let company = input.company.get();
        if tx
            .sql()
            .query_opt(
                "SELECT 1 FROM companies WHERE tenant_id=$1 AND id=$2 AND deleted_at IS NULL",
                &[&scope.tenant(), &company],
            )
            .await
            .map_err(Error::from)?
            .is_none()
        {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "company",
                "does not exist",
            )]));
        }
        if !self
            .active_account(tx.sql(), scope, input.originating_user)
            .await?
        {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "originating_user",
                "is not an active account",
            )]));
        }
        let mut draft = Draft::task(&input.task);
        draft.title = draft.title.trim().to_owned();
        draft.assignee = Some(draft.assignee.unwrap_or(input.originating_user));
        let draft = self
            .validate(
                tx.sql(),
                scope,
                &draft,
                Touched {
                    related: true,
                    assignee: true,
                    guests: true,
                },
            )
            .await?;
        self.insert(
            tx,
            draft,
            scope.tenant(),
            company,
            input.originating_user,
            None,
        )
        .await
    }

    async fn insert(
        &self,
        tx: &mut MutationTransaction<'_>,
        draft: Draft,
        tenant: i64,
        company: i64,
        user: i64,
        series: Option<(i64, i16)>,
    ) -> WriteResult<Activity> {
        let assignee = draft.assignee.ok_or(Error::Internal)?;
        let (series_id, series_index) = series.unzip();
        let closed = draft.status.is_some_and(Status::closed);
        // The people taking part: the creator, the assignee, a related
        // person and the invited guests, one row each.
        let mut guests: Vec<GuestInsert> = Vec::new();
        if assignee == user {
            guests.push((Some(user), None, None, "assignee"));
        } else {
            guests.push((Some(user), None, None, "creator"));
            guests.push((Some(assignee), None, None, "assignee"));
        }
        let related_person = draft
            .related
            .as_ref()
            .filter(|r| r.kind == "person")
            .and_then(|r| r.id.parse::<i64>().ok());
        if let Some(person) = related_person {
            guests.push((None, Some(person), None, "related"));
        }
        for guest in &draft.guests {
            match guest {
                Guest::User(guest) if *guest != user && *guest != assignee => {
                    guests.push((Some(*guest), None, None, "invited"))
                }
                Guest::User(_) => {}
                Guest::Person(person) if Some(person.get()) != related_person => {
                    guests.push((None, Some(person.get()), None, "invited"))
                }
                Guest::Person(_) => {}
                Guest::Address { email, related } => guests.push((
                    None,
                    None,
                    Some(email.clone()),
                    if *related { "related" } else { "invited" },
                )),
            }
        }
        let activity = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let row = sql
                        .query_one(
                            &format!(
                                "INSERT INTO core_activities AS a (tenant_id,company_id,kind,subject,status,priority,direction,starts_at,ends_at,due_at,start_at,call_outcome,description,internal_notes,related_kind,related_id,shared,assignee_user_id,created_by,closed_at,location,join_url,meeting_type,series_id,series_index)
                                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,CASE WHEN $20 THEN now() END,$21,$22,$23,$24,$25)
                                 RETURNING {COLUMNS}"
                            ),
                            &[
                                &tenant,
                                &company,
                                &draft.kind.code(),
                                &draft.title,
                                &draft.status.map(Status::code),
                                &draft.priority.map(Priority::code),
                                &draft.direction.map(Direction::code),
                                &system(draft.starts_at),
                                &system(draft.ends_at),
                                &system(draft.due_at),
                                &system(draft.start_at),
                                &draft.outcome,
                                &draft.description,
                                &draft.internal_notes,
                                &draft.related.as_ref().map(|r| r.kind.clone()),
                                &draft.related.as_ref().map(|r| r.id.clone()),
                                &draft.shared,
                                &assignee,
                                &user,
                                &closed,
                                &draft.location,
                                &draft.join_url,
                                &draft.meeting_type,
                                &series_id,
                                &series_index,
                            ],
                        )
                        .await?;
                    let activity = from_row(&row)?;
                    let mut changes = vec![Change {
                        kind: RECORD.into(),
                        id: activity.id,
                        old: json!({}),
                        new: audit_values(&activity),
                    }];
                    for (guest_user, guest_person, guest_email, role) in guests {
                        changes.push(
                            insert_guest(sql, tenant, activity.id, guest_user, guest_person, guest_email, role)
                                .await?,
                        );
                    }
                    Ok((activity, changes))
                })
            })
            .await?;
        self.announce(tx, &activity, None).await?;
        Ok(activity)
    }

    pub(crate) async fn prepare_update(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Patch,
    ) -> WriteResult<(Activity, Draft)> {
        self.updated(client, scope, id, version, patch, false).await
    }
    async fn updated(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Patch,
        lock: bool,
    ) -> WriteResult<(Activity, Draft)> {
        let before = editable(client, scope, id, version, lock).await?;
        if patch.is_empty() {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "patch",
                "names no field to change",
            )]));
        }
        let after = model::patched(&before.draft(), patch);
        let touched = Touched {
            related: after.related != before.related,
            assignee: after.assignee != Some(before.assignee_user_id),
            guests: false,
        };
        let after = self.validate(client, scope, &after, touched).await?;
        Ok((before, after))
    }

    pub(crate) async fn update(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Patch,
    ) -> WriteResult<Activity> {
        let (before, after) = self
            .updated(tx.sql(), scope, id, version, patch, true)
            .await?;
        let tenant = scope.tenant();
        let assignee = after.assignee.ok_or(Error::Internal)?;
        let related_changed = after.related != before.related;
        let related_person = related_changed
            .then(|| after.related.as_ref().filter(|r| r.kind == "person"))
            .flatten()
            .and_then(|r| r.id.parse::<i64>().ok());
        let closed_now = after.status.is_some_and(Status::closed);
        // A new time it is due asks for a new reminder.
        let due_moved = after.starts_at != before.starts_at || after.due_at != before.due_at;
        let earlier = before.clone();
        let activity = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    // A status moving into or out of the closing ones sets or
                    // clears closed_at; an unchanged one keeps it.
                    let row = sql
                        .query_opt(
                            &format!(
                                "UPDATE core_activities a SET subject=$4,status=$5,priority=$6,direction=$7,starts_at=$8,ends_at=$9,due_at=$10,start_at=$11,call_outcome=$12,description=$13,internal_notes=$14,related_kind=$15,related_id=$16,shared=$17,assignee_user_id=$18,location=$21,join_url=$22,meeting_type=$23,
                                   closed_at=CASE WHEN NOT $19 THEN NULL WHEN a.closed_at IS NULL THEN now() ELSE a.closed_at END,
                                   reminded_at=CASE WHEN $20 OR a.assignee_user_id<>$18 THEN NULL ELSE a.reminded_at END,
                                   version=a.version+1,updated_at=now()
                                 WHERE a.tenant_id=$1 AND a.id=$2 AND a.version=$3 AND a.deleted_at IS NULL RETURNING {COLUMNS}"
                            ),
                            &[
                                &tenant,
                                &before.id,
                                &before.version,
                                &after.title,
                                &after.status.map(Status::code),
                                &after.priority.map(Priority::code),
                                &after.direction.map(Direction::code),
                                &system(after.starts_at),
                                &system(after.ends_at),
                                &system(after.due_at),
                                &system(after.start_at),
                                &after.outcome,
                                &after.description,
                                &after.internal_notes,
                                &after.related.as_ref().map(|r| r.kind.clone()),
                                &after.related.as_ref().map(|r| r.id.clone()),
                                &after.shared,
                                &assignee,
                                &closed_now,
                                &due_moved,
                                &after.location,
                                &after.join_url,
                                &after.meeting_type,
                            ],
                        )
                        .await?
                        .ok_or(Error::Conflict)?;
                    let activity = from_row(&row)?;
                    let mut changes = vec![Change {
                        kind: RECORD.into(),
                        id: activity.id,
                        old: audit_values(&before),
                        new: audit_values(&activity),
                    }];
                    if assignee != before.assignee_user_id {
                        changes.extend(reassign(sql, tenant, &before, assignee).await?);
                    }
                    // The previous record's related person or address leaves
                    // with it; a new related person takes their place.
                    if related_changed {
                        changes.extend(drop_related(sql, tenant, activity.id).await?);
                    }
                    if let Some(person) = related_person {
                        if let Some(change) = add_guest(sql, tenant, activity.id, None, Some(person), "related").await? {
                            changes.push(change);
                        }
                    }
                    Ok((activity, changes))
                })
            })
            .await
            .map_err(stale)?;
        self.announce(tx, &activity, Some(&earlier)).await?;
        self.refresh_bells(tx, &earlier, Some(&activity)).await?;
        Ok(activity)
    }

    /// A reminded, open activity is on its assignee's To do, and a call's or
    /// meeting's on its account guests' who have not declined; when it
    /// closes, goes away or changes hands, re-read the bell of each person it
    /// was or now is on.
    async fn refresh_bells(
        &self,
        tx: &mut MutationTransaction<'_>,
        before: &Activity,
        after: Option<&Activity>,
    ) -> Result<()> {
        let mut people = Vec::new();
        if before.reminded_at.is_some() && !before.is_closed() {
            people.push(before.assignee_user_id);
            if before.kind.timed() {
                let rows = tx
                    .sql()
                    .query(
                        "SELECT guest_user_id FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 AND guest_user_id IS NOT NULL AND response<>'declined'",
                        &[&tx.context().tenant, &before.id],
                    )
                    .await?;
                people.extend(rows.iter().map(|row| row.get::<_, i64>(0)));
            }
        }
        if let Some(after) = after {
            people.push(after.assignee_user_id);
        }
        self.notifications.todos_changed_for(tx, &people)
    }

    pub(crate) async fn repoint(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        from: &Reference,
        to: &Reference,
    ) -> WriteResult<i64> {
        let mut errors = Vec::new();
        let from = match self.check_reference(tx.sql(), scope, from).await? {
            Ok(from) => Some(from),
            Err(message) => {
                errors.push(FieldError::new("from", message));
                None
            }
        };
        let to = match self.check_reference(tx.sql(), scope, to).await? {
            Ok(to) => Some(to),
            Err(message) => {
                errors.push(FieldError::new("to", message));
                None
            }
        };
        let (Some(from), Some(to)) = (from, to) else {
            return Err(WriteError::Invalid(errors));
        };
        if from == to {
            return Ok(0);
        }
        let tenant = scope.tenant();
        let person = (to.kind == "person")
            .then(|| to.id.parse::<i64>().ok())
            .flatten();
        let moved_from = from.clone();
        // A write with no change is refused: nothing related, nothing moves.
        if tx
            .sql()
            .query_opt(
                "SELECT 1 FROM core_activities WHERE tenant_id=$1 AND related_kind=$2 AND related_id=$3 AND deleted_at IS NULL LIMIT 1",
                &[&tenant, &from.kind, &from.id],
            )
            .await
            .map_err(Error::from)?
            .is_none()
        {
            return Ok(0);
        }
        let moved = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let before: Vec<Activity> = sql
                        .query(
                            &format!("SELECT {COLUMNS} FROM core_activities a WHERE a.tenant_id=$1 AND a.related_kind=$2 AND a.related_id=$3 AND a.deleted_at IS NULL ORDER BY a.id FOR UPDATE"),
                            &[&tenant, &from.kind, &from.id],
                        )
                        .await?
                        .iter()
                        .map(from_row)
                        .collect::<Result<_>>()?;
                    let rows = sql
                        .query(
                            &format!("UPDATE core_activities a SET related_kind=$4,related_id=$5,version=a.version+1,updated_at=now() WHERE a.tenant_id=$1 AND a.related_kind=$2 AND a.related_id=$3 AND a.deleted_at IS NULL RETURNING {COLUMNS}"),
                            &[&tenant, &from.kind, &from.id, &to.kind, &to.id],
                        )
                        .await?;
                    let mut changes = Vec::new();
                    let mut after_all = Vec::new();
                    for row in &rows {
                        let after = from_row(row)?;
                        let old = before
                            .iter()
                            .find(|a| a.id == after.id)
                            .map_or(json!({}), audit_values);
                        if let (Some(person), Kind::Call | Kind::Meeting, false) =
                            (person, after.kind, after.is_closed())
                        {
                            // The new person replaces the related address.
                            let removed = sql
                                .query(
                                    "DELETE FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 AND role='related' AND guest_email IS NOT NULL RETURNING id,guest_email",
                                    &[&tenant, &after.id],
                                )
                                .await?;
                            for gone in removed {
                                changes.push(Change {
                                    kind: GUEST_RECORD.into(),
                                    id: gone.get(0),
                                    old: json!({"activity_id": after.id, "email": gone.get::<_, String>(1), "role": "related"}),
                                    new: json!({}),
                                });
                            }
                            if let Some(change) =
                                add_guest(sql, tenant, after.id, None, Some(person), "related").await?
                            {
                                changes.push(change);
                            }
                        }
                        changes.push(Change {
                            kind: RECORD.into(),
                            id: after.id,
                            old,
                            new: audit_values(&after),
                        });
                        after_all.push(after);
                    }
                    Ok((after_all, changes))
                })
            })
            .await?;
        // Subscribers that keep a record's history (CX's customer timeline)
        // learn which shared activities that have happened now belong to
        // `to`.
        for activity in &moved {
            self.repointed(tx, activity, &moved_from).await?;
        }
        Ok(moved.len() as i64)
    }

    /// Records `core/activity.happened` when `activity` has just become a
    /// shared activity that has happened: created so, held, completed or
    /// shared after it happened. `before` is the activity before an update or
    /// Close; `None` for a create. Keyed by the activity and its version, so
    /// a retried write records it once.
    async fn announce(
        &self,
        tx: &mut MutationTransaction<'_>,
        activity: &Activity,
        before: Option<&Activity>,
    ) -> WriteResult<()> {
        let now = activity.shared && happened(activity).is_some();
        let earlier = before.is_some_and(|b| b.shared && happened(b).is_some());
        if now && !earlier {
            self.record(tx, activity, json!({"reason": "happened"}))
                .await?;
        }
        Ok(())
    }
    async fn repointed(
        &self,
        tx: &mut MutationTransaction<'_>,
        activity: &Activity,
        from: &Reference,
    ) -> WriteResult<()> {
        if activity.shared && happened(activity).is_some() {
            self.record(
                tx,
                activity,
                json!({
                    "reason": "repointed",
                    "from": {"kind": from.kind, "id": from.id},
                }),
            )
            .await?;
        }
        Ok(())
    }
    /// The event's payload never carries the subject or any text: a
    /// subscriber shows what happened to people who may not open the
    /// activity.
    async fn record(
        &self,
        tx: &mut MutationTransaction<'_>,
        activity: &Activity,
        extra: Value,
    ) -> WriteResult<()> {
        let Some(what) = happened(activity) else {
            return Ok(());
        };
        let people: Vec<i64> = tx
            .sql()
            .query(
                "SELECT guest_person_id FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 AND guest_person_id IS NOT NULL ORDER BY id",
                &[&tx.context().tenant, &activity.id],
            )
            .await
            .map_err(Error::from)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let mut payload = json!({
            "activity": activity.id,
            "kind": activity.kind.code(),
            "happened": what,
            "occurred_at": occurred_at(activity).to_string(),
            "related": activity.related.as_ref().map(|r| json!({"kind": r.kind, "id": r.id})),
            "people": people,
        });
        if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            payload.extend(extra.clone());
        }
        let subject = format!("{RECORD}:{}", activity.id);
        let key = format!("{}:{}", activity.id, activity.version);
        self.events
            .record(
                tx,
                NewEvent {
                    name: crate::HAPPENED,
                    subject: &subject,
                    key: &key,
                    payload,
                },
            )
            .await?;
        Ok(())
    }
}

/// What an activity that has happened is: a held call or meeting, a
/// completed task, any note. `None` for one still to do, or closed some other
/// way (not held, canceled, deferred).
fn happened(activity: &Activity) -> Option<&'static str> {
    if activity
        .kind
        .closing()
        .is_some_and(|closing| activity.status != Some(closing))
    {
        return None;
    }
    Some(match activity.kind {
        Kind::Call => "call.logged",
        Kind::Meeting => "meeting.held",
        Kind::Task => "task.completed",
        Kind::Note => "note.added",
    })
}

/// When it happened: a call or meeting's start, when a task was completed,
/// when a note was written. Never ahead of now.
fn occurred_at(activity: &Activity) -> Timestamp {
    match activity.kind {
        Kind::Call | Kind::Meeting => activity.starts_at,
        Kind::Task => activity.closed_at,
        Kind::Note => Some(activity.created_at),
    }
    .unwrap_or(activity.updated_at)
    .min(Timestamp::now())
}

/// A guest row to insert: account, person, email address and role.
type GuestInsert = (Option<i64>, Option<i64>, Option<String>, &'static str);

fn stale(error: Error) -> WriteError {
    match error {
        Error::Conflict => WriteError::Stale,
        error => WriteError::Access(error),
    }
}

async fn insert_guest(
    sql: &tokio_postgres::Transaction<'_>,
    tenant: i64,
    activity: i64,
    user: Option<i64>,
    person: Option<i64>,
    email: Option<String>,
    role: &str,
) -> Result<Change> {
    let id: i64 = sql
        .query_one(
            "INSERT INTO core_activity_guests(tenant_id,activity_id,guest_user_id,guest_person_id,guest_email,role) VALUES ($1,$2,$3,$4,$5,$6) RETURNING id",
            &[&tenant, &activity, &user, &person, &email, &role],
        )
        .await?
        .get(0);
    Ok(Change {
        kind: GUEST_RECORD.into(),
        id,
        old: json!({}),
        new: json!({"activity_id": activity, "user_id": user, "person_id": person, "email": email, "role": role}),
    })
}

/// Adds an account or person guest unless they already take part.
async fn add_guest(
    sql: &tokio_postgres::Transaction<'_>,
    tenant: i64,
    activity: i64,
    user: Option<i64>,
    person: Option<i64>,
    role: &str,
) -> Result<Option<Change>> {
    let present = sql
        .query_opt(
            "SELECT 1 FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 AND (guest_user_id=$3 OR guest_person_id=$4)",
            &[&tenant, &activity, &user, &person],
        )
        .await?
        .is_some();
    if present {
        return Ok(None);
    }
    insert_guest(sql, tenant, activity, user, person, None, role)
        .await
        .map(Some)
}

/// Removes the related guests: the person or address of the record the
/// activity was about.
async fn drop_related(
    sql: &tokio_postgres::Transaction<'_>,
    tenant: i64,
    activity: i64,
) -> Result<Vec<Change>> {
    let rows = sql
        .query(
            "DELETE FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 AND role='related' RETURNING id,guest_person_id,guest_email",
            &[&tenant, &activity],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| Change {
            kind: GUEST_RECORD.into(),
            id: row.get(0),
            old: json!({"activity_id": activity, "person_id": row.get::<_, Option<i64>>(1), "email": row.get::<_, Option<String>>(2), "role": "related"}),
            new: json!({}),
        })
        .collect())
}

/// The new assignee takes the assignee role; the old one stays a guest, as
/// the creator when they created it, else as invited.
async fn reassign(
    sql: &tokio_postgres::Transaction<'_>,
    tenant: i64,
    before: &Activity,
    assignee: i64,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    let demoted = if before.assignee_user_id == before.created_by {
        "creator"
    } else {
        "invited"
    };
    for (user, role) in [(before.assignee_user_id, demoted), (assignee, "assignee")] {
        let row = sql
            .query_opt(
                "UPDATE core_activity_guests SET role=$4 WHERE tenant_id=$1 AND activity_id=$2 AND guest_user_id=$3 AND role<>$4 RETURNING id",
                &[&tenant, &before.id, &user, &role],
            )
            .await?;
        match row {
            Some(row) => changes.push(Change {
                kind: GUEST_RECORD.into(),
                id: row.get(0),
                old: json!({"activity_id": before.id, "user_id": user}),
                new: json!({"activity_id": before.id, "user_id": user, "role": role}),
            }),
            None if role == "assignee" => {
                if let Some(change) =
                    add_guest(sql, tenant, before.id, Some(user), None, "assignee").await?
                {
                    changes.push(change);
                }
            }
            None => {}
        }
    }
    Ok(changes)
}

pub(crate) async fn prepare_close(
    client: &dyn ReadClient,
    scope: &Scope,
    id: i64,
    version: i64,
) -> WriteResult<Activity> {
    let activity = editable(client, scope, id, version, false).await?;
    closable(&activity)?;
    Ok(activity)
}
fn closable(activity: &Activity) -> WriteResult<Status> {
    let Some(status) = activity.kind.closing() else {
        return Err(WriteError::Invalid(vec![FieldError::new(
            "id",
            "is a note, which has no status to close",
        )]));
    };
    if activity.is_closed() {
        return Err(WriteError::AlreadyClosed);
    }
    if let Some(error) = model::held_ended(activity.kind, Some(status), activity.ends_at) {
        return Err(WriteError::Invalid(vec![error]));
    }
    Ok(status)
}

impl Store<'_> {
    pub(crate) async fn close(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<Activity> {
        let before = editable(tx.sql(), scope, id, version, true).await?;
        let status = closable(&before)?;
        let tenant = scope.tenant();
        let earlier = before.clone();
        let activity = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let row = sql
                        .query_opt(
                            &format!("UPDATE core_activities a SET status=$4,closed_at=now(),version=a.version+1,updated_at=now() WHERE a.tenant_id=$1 AND a.id=$2 AND a.version=$3 AND a.deleted_at IS NULL AND a.closed_at IS NULL RETURNING {COLUMNS}"),
                            &[&tenant, &before.id, &before.version, &status.code()],
                        )
                        .await?
                        .ok_or(Error::Conflict)?;
                    let activity = from_row(&row)?;
                    let change = Change {
                        kind: RECORD.into(),
                        id: activity.id,
                        old: audit_values(&before),
                        new: audit_values(&activity),
                    };
                    Ok((activity, vec![change]))
                })
            })
            .await
            .map_err(stale)?;
        self.announce(tx, &activity, Some(&earlier)).await?;
        self.refresh_bells(tx, &earlier, None).await?;
        Ok(activity)
    }
}

/// The unsaved copy Close and create new returns: the closed activity's
/// values, related record and guests, with the opening status.
pub(crate) async fn follow_up(
    client: &dyn ReadClient,
    scope: &Scope,
    closed: &Activity,
) -> Result<Draft> {
    let mut draft = closed.draft();
    draft.status = closed.kind.opening();
    draft.guests = guests(client, scope, closed.id)
        .await?
        .into_iter()
        .filter(|g| g.role == "invited" || g.role == "related")
        .filter_map(|g| match (g.user_id, g.person, g.email) {
            (Some(user), _, _) if g.role == "invited" => Some(Guest::User(user)),
            (Some(_), _, _) => None,
            (None, Some(person), _) if g.role == "invited" => Some(Guest::Person(person)),
            (None, Some(_), _) => None,
            (None, None, Some(email)) => Some(Guest::Address {
                email,
                related: g.role == "related",
            }),
            _ => None,
        })
        .collect();
    Ok(draft)
}

pub(crate) async fn prepare_delete(
    client: &dyn ReadClient,
    scope: &Scope,
    id: i64,
    version: i64,
) -> WriteResult<Activity> {
    editable(client, scope, id, version, false).await
}

impl Store<'_> {
    pub(crate) async fn delete(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<()> {
        let before = editable(tx.sql(), scope, id, version, true).await?;
        let tenant = scope.tenant();
        let earlier = before.clone();
        tx.mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_opt(
                        &format!("UPDATE core_activities a SET deleted_at=now(),version=a.version+1,updated_at=now() WHERE a.tenant_id=$1 AND a.id=$2 AND a.version=$3 AND a.deleted_at IS NULL RETURNING {COLUMNS}"),
                        &[&tenant, &before.id, &before.version],
                    )
                    .await?
                    .ok_or(Error::Conflict)?;
                let after = from_row(&row)?;
                let change = Change {
                    kind: RECORD.into(),
                    id: after.id,
                    old: audit_values(&before),
                    new: json!({}),
                };
                Ok(((), vec![change]))
            })
        })
        .await
        .map_err(stale)?;
        self.refresh_bells(tx, &earlier, None).await?;
        Ok(())
    }
}

// ---- Guests and invitations -------------------------------------------------
//
// Guest changes touch no field of the activity, so they name no version and
// leave it as it was; each locks the activity, so they queue behind any other
// write to it.

/// The one guest `guest` names on `activity`: a field error when it has none.
async fn guest(
    client: &dyn ReadClient,
    scope: &Scope,
    activity: i64,
    guest: i64,
) -> WriteResult<GuestRow> {
    let row = client
        .query_opt(
            &format!("SELECT {GUEST_COLUMNS} FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 AND id=$3"),
            &[&scope.tenant(), &activity, &guest],
        )
        .await?;
    match row {
        Some(row) => Ok(guest_row(&row)?),
        None => Err(WriteError::Invalid(vec![FieldError::new(
            "guest_id",
            "is not a guest of this activity",
        )])),
    }
}

fn guest_values(guest: &GuestRow, activity: i64) -> Value {
    json!({
        "activity_id": activity,
        "user_id": guest.user_id,
        "person_id": guest.person.map(|p| p.get()),
        "email": guest.email,
        "role": guest.role,
        "response": guest.response,
    })
}

/// The assignee takes part until the activity is reassigned; anyone else may
/// leave, the creator included.
fn removable(guest: &GuestRow) -> WriteResult<()> {
    if guest.role == "assignee" {
        Err(WriteError::AssigneeNotRemovable)
    } else {
        Ok(())
    }
}

/// The checks `respond` makes: an account guest answers for themselves;
/// anyone who may change the activity records any guest's answer.
pub(crate) async fn prepare_respond(
    client: &dyn ReadClient,
    scope: &Scope,
    id: i64,
    guest_id: i64,
) -> WriteResult<(Activity, GuestRow)> {
    let activity = seen(client, scope, id, false).await?;
    let row = guest(client, scope, id, guest_id).await?;
    if row.user_id != Some(scope.user()) {
        authorize_edit(client, scope, &activity).await?;
    }
    Ok((activity, row))
}

impl Store<'_> {
    /// The checks `add_guest` makes: the activity is one the actor may
    /// change, the guest is an active account, a person they may view or an
    /// email address, not already taking part, and within `MAX_GUESTS`.
    pub(crate) async fn prepare_add_guest(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        wanted: &Guest,
    ) -> WriteResult<(Activity, Guest)> {
        let activity = editable_now(client, scope, id, false).await?;
        self.check_guest(client, scope, &activity, wanted).await
    }
    async fn check_guest(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        activity: &Activity,
        wanted: &Guest,
    ) -> WriteResult<(Activity, Guest)> {
        let mut probe = activity.draft();
        probe.guests = vec![wanted.clone()];
        let touched = Touched {
            related: false,
            assignee: false,
            guests: true,
        };
        // Only the guest's own rules: the activity's other fields were valid
        // when stored, and a rule added since must not block a guest change.
        let mut errors = model::check(&probe)
            .into_iter()
            .filter(|e| e.field == "guests")
            .collect::<Vec<_>>();
        let checked = match self.validate(client, scope, &probe, touched).await {
            Ok(checked) => Some(checked),
            Err(WriteError::Invalid(more)) => {
                errors.extend(more.into_iter().filter(|e| e.field == "guests"));
                None
            }
            Err(error) => return Err(error),
        };
        if !errors.is_empty() {
            errors.dedup();
            for error in &mut errors {
                error.field = "guest";
            }
            return Err(WriteError::Invalid(errors));
        }
        let guest = checked
            .and_then(|c| c.guests.into_iter().next())
            .ok_or(Error::Internal)?;
        let present = guests(client, scope, activity.id).await?;
        let duplicate = present.iter().any(|g| match &guest {
            Guest::User(user) => g.user_id == Some(*user),
            Guest::Person(person) => g.person == Some(*person),
            Guest::Address { email, .. } => g
                .email
                .as_ref()
                .is_some_and(|e| e.to_lowercase() == email.to_lowercase()),
        });
        if duplicate {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "guest",
                "already takes part in this activity",
            )]));
        }
        if present.len() >= model::MAX_GUESTS {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "guest",
                format!(
                    "cannot be added: an activity has at most {} guests",
                    model::MAX_GUESTS
                ),
            )]));
        }
        Ok((activity.clone(), guest))
    }

    /// Adds an invited guest. An address added here is never the related
    /// record's: that one comes with the related record.
    pub(crate) async fn add_guest(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        wanted: &Guest,
    ) -> WriteResult<GuestRow> {
        let activity = editable_now(tx.sql(), scope, id, true).await?;
        let (_, guest) = self.check_guest(tx.sql(), scope, &activity, wanted).await?;
        let (user, person, email) = match guest {
            Guest::User(user) => (Some(user), None, None),
            Guest::Person(person) => (None, Some(person.get()), None),
            Guest::Address { email, .. } => (None, None, Some(email)),
        };
        let tenant = scope.tenant();
        let row = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let change =
                        insert_guest(sql, tenant, activity.id, user, person, email, "invited")
                            .await?;
                    let row = sql
                        .query_one(
                            &format!("SELECT {GUEST_COLUMNS} FROM core_activity_guests WHERE tenant_id=$1 AND id=$2"),
                            &[&tenant, &change.id],
                        )
                        .await?;
                    Ok((guest_row(&row)?, vec![change]))
                })
            })
            .await?;
        Ok(row)
    }

    pub(crate) async fn prepare_remove_guest(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        guest_id: i64,
    ) -> WriteResult<(Activity, GuestRow)> {
        let activity = editable_now(client, scope, id, false).await?;
        let row = guest(client, scope, id, guest_id).await?;
        removable(&row)?;
        Ok((activity, row))
    }

    /// Removes a guest; a removed account guest loses the activity from
    /// their To do.
    pub(crate) async fn remove_guest(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        guest_id: i64,
    ) -> WriteResult<GuestRow> {
        let activity = editable_now(tx.sql(), scope, id, true).await?;
        let row = guest(tx.sql(), scope, id, guest_id).await?;
        removable(&row)?;
        let tenant = scope.tenant();
        let removed = row.clone();
        let activity_id = activity.id;
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "DELETE FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 AND id=$3",
                    &[&tenant, &activity_id, &removed.id],
                )
                .await?;
                let change = Change {
                    kind: GUEST_RECORD.into(),
                    id: removed.id,
                    old: guest_values(&removed, activity_id),
                    new: json!({}),
                };
                Ok(((), vec![change]))
            })
        })
        .await?;
        if let Some(user) = row.user_id {
            self.notifications.todos_changed_for(tx, &[user])?;
        }
        Ok(row)
    }

    pub(crate) async fn respond(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        guest_id: i64,
        response: Response,
    ) -> WriteResult<GuestRow> {
        let activity = seen(tx.sql(), scope, id, true).await?;
        let before = guest(tx.sql(), scope, id, guest_id).await?;
        if before.user_id != Some(scope.user()) {
            authorize_edit(tx.sql(), scope, &activity).await?;
        }
        if before.response == response.code() {
            return Ok(before);
        }
        let tenant = scope.tenant();
        let activity_id = activity.id;
        let after = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let row = sql
                        .query_one(
                            &format!("UPDATE core_activity_guests SET response=$4 WHERE tenant_id=$1 AND activity_id=$2 AND id=$3 RETURNING {GUEST_COLUMNS}"),
                            &[&tenant, &activity_id, &before.id, &response.code()],
                        )
                        .await?;
                    let after = guest_row(&row)?;
                    let change = Change {
                        kind: GUEST_RECORD.into(),
                        id: after.id,
                        old: guest_values(&before, activity_id),
                        new: guest_values(&after, activity_id),
                    };
                    Ok((after, vec![change]))
                })
            })
            .await
            .map_err(WriteError::from)?;
        // Declining takes a reminded meeting off that guest's To do.
        if let Some(user) = after.user_id {
            if activity.reminded_at.is_some() {
                self.notifications.todos_changed_for(tx, &[user])?;
            }
        }
        Ok(after)
    }

    /// Who Send invites would reach, with no write: an open call or meeting
    /// still ahead that the actor may change. Every guest but the actor.
    pub(crate) async fn prepare_send_invites(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
    ) -> WriteResult<Invites> {
        let activity = editable_now(client, scope, id, false).await?;
        self.invitable(client, scope, activity).await
    }
    async fn invitable(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        activity: Activity,
    ) -> WriteResult<Invites> {
        if !activity.kind.timed() {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "id",
                "is not a call or meeting; only those have invitations",
            )]));
        }
        if activity.is_closed() {
            return Err(WriteError::AlreadyClosed);
        }
        if activity.starts_at.is_none_or(|at| at <= Timestamp::now()) {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "starts_at",
                "has passed; invitations are for a call or meeting still ahead",
            )]));
        }
        let guests: Vec<GuestRow> = guests(client, scope, activity.id)
            .await?
            .into_iter()
            .filter(|g| g.user_id != Some(scope.user()))
            .collect();
        if guests.is_empty() {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "guests",
                "has nobody to invite but you; add a guest first",
            )]));
        }
        // Emailing someone without an account asks every invitation gate as
        // the sender (a consent check); the sender must be allowed to ask.
        if guests.iter().any(|g| g.user_id.is_none())
            && !crate::invitations::may_ask(client, scope).await?
        {
            return Err(WriteError::Forbidden);
        }
        let emailable = crate::invitations::guest_emails(client, scope, &guests)
            .await
            .len();
        Ok(Invites {
            activity,
            guests,
            emailable,
        })
    }

    /// Stamps every guest but the actor as invited now, tells the account
    /// guests in the bell, and queues the email to the others (the
    /// invitation worker, which runs as the actor), all in the caller's
    /// transaction.
    pub(crate) async fn send_invites(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
    ) -> WriteResult<Invites> {
        let activity = editable_now(tx.sql(), scope, id, true).await?;
        let invites = self.invitable(tx.sql(), scope, activity).await?;
        let (tenant, user, activity_id) = (scope.tenant(), scope.user(), invites.activity.id);
        let (invited_at, invited) = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let rows = sql
                        .query(
                            &format!("UPDATE core_activity_guests SET invited_at=now() WHERE tenant_id=$1 AND activity_id=$2 AND guest_user_id IS DISTINCT FROM $3 RETURNING {GUEST_COLUMNS}"),
                            &[&tenant, &activity_id, &user],
                        )
                        .await?;
                    let invited = rows.iter().map(guest_row).collect::<Result<Vec<_>>>()?;
                    let at = invited
                        .iter()
                        .find_map(|g| g.invited_at)
                        .ok_or(Error::Internal)?;
                    let changes = invited
                        .iter()
                        .map(|g| Change {
                            kind: GUEST_RECORD.into(),
                            id: g.id,
                            old: json!({"activity_id": activity_id}),
                            new: json!({"activity_id": activity_id, "invited_at": at.to_string()}),
                        })
                        .collect();
                    Ok(((at, invited), changes))
                })
            })
            .await?;
        let accounts: Vec<i64> = invited.iter().filter_map(|g| g.user_id).collect();
        let accounts = self.notifications.eligible(tx, &accounts).await?;
        if !accounts.is_empty() {
            let kind = invites.activity.kind.label().to_lowercase();
            let title: String = invites.activity.title.chars().take(140).collect();
            let notice = bilibi_notifications::Notice::new(format!("Invitation: {title}"))
                .kind("core.activity.invitation")
                .body(format!(
                    "You are invited to this {kind}. Open it for the time and the details, and to answer."
                ))
                .url(format!("/activities/{activity_id}"))
                .icon("calendar");
            self.notifications
                .notify(tx, scope, &accounts, &notice)
                .await?;
        }
        if invited.iter().any(|g| g.user_id.is_none()) {
            self.queue
                .enqueue_for(
                    tx,
                    scope,
                    crate::invitations::WORKER,
                    json!({"activity_id": activity_id, "invited_at": invited_at.to_string()}),
                )
                .await?;
        }
        Ok(Invites {
            activity: invites.activity,
            guests: invited,
            emailable: invites.emailable,
        })
    }
}

// ---- Repeating series -------------------------------------------------------
//
// A series is its rule and one activity per occurrence. Every check runs on
// every occurrence before the first write, so a refusal writes nothing.

/// What a repeating create will store: the checked draft, one draft per
/// occurrence, and the rule's resolved parts.
pub(crate) struct SeriesPlan {
    pub draft: Draft,
    pub occurrences: Vec<Draft>,
    pub weekdays: Vec<i16>,
    pub month_day: Option<i16>,
    pub work_calendar: Option<String>,
}

fn invalid(error: FieldError) -> WriteError {
    WriteError::Invalid(vec![error])
}

/// A series' rule and how many of its occurrences are live.
pub(crate) async fn series(client: &dyn ReadClient, scope: &Scope, id: i64) -> Result<Series> {
    let row = client
        .query_opt(
            "SELECT s.id,s.frequency,s.every,s.weekdays,s.month_day,s.until::text AS until,s.occurrences,s.work_calendar,s.timezone,s.created_by,s.created_at,
                    (SELECT count(*) FROM core_activities a WHERE a.tenant_id=s.tenant_id AND a.series_id=s.id AND a.deleted_at IS NULL) AS live
               FROM core_activity_series s WHERE s.tenant_id=$1 AND s.id=$2",
            &[&scope.tenant(), &id],
        )
        .await?
        .ok_or(Error::NotFound)?;
    let weekdays = row
        .get::<_, Vec<i16>>("weekdays")
        .into_iter()
        .map(|day| {
            i8::try_from(day)
                .ok()
                .and_then(|day| jiff::civil::Weekday::from_monday_one_offset(day).ok())
        })
        .collect::<Option<Vec<_>>>()
        .ok_or(Error::Internal)?;
    let until = row
        .get::<_, Option<String>>("until")
        .map(|until| {
            until
                .parse::<jiff::civil::Date>()
                .map_err(|_| Error::Internal)
        })
        .transpose()?;
    let work_calendar: Option<String> = row.get("work_calendar");
    Ok(Series {
        id: row.get("id"),
        repeat: Repeat {
            frequency: Frequency::parse(row.get("frequency")).ok_or(Error::Internal)?,
            every: row.get("every"),
            weekdays,
            until,
            count: row.get("occurrences"),
            working_days_only: work_calendar.is_some(),
            timezone: row.get("timezone"),
        },
        month_day: row.get("month_day"),
        work_calendar,
        live: row.get("live"),
        created_by: row.get("created_by"),
        created_at: time(Some(row.get("created_at")))?.ok_or(Error::Internal)?,
    })
}

/// The live occurrences of `series` a change reaches: from number `from`
/// on, or all of them.
async fn members(
    client: &dyn ReadClient,
    scope: &Scope,
    series: i64,
    apply: Apply,
    from: i16,
) -> Result<Vec<Activity>> {
    client
        .query(
            &format!("SELECT {COLUMNS} FROM core_activities a WHERE a.tenant_id=$1 AND a.series_id=$2 AND a.deleted_at IS NULL AND ($3 OR a.series_index >= $4) ORDER BY a.series_index"),
            &[&scope.tenant(), &series, &(apply == Apply::All), &from],
        )
        .await?
        .iter()
        .map(from_row)
        .collect()
}

/// The occurrence a series change is named by: one the actor may change, at
/// `version`, in a series. Returns it with its series and number.
async fn series_anchor(
    client: &dyn ReadClient,
    scope: &Scope,
    id: i64,
    version: i64,
) -> WriteResult<(Activity, i64, i16)> {
    let anchor = editable(client, scope, id, version, false).await?;
    match (anchor.series_id, anchor.series_index) {
        (Some(series), Some(index)) => Ok((anchor, series, index)),
        _ => Err(invalid(FieldError::new(
            "id",
            "is not an occurrence of a repeating series",
        ))),
    }
}

/// The patch one occurrence gets from a change made on `anchor`. A new time
/// moves an open occurrence by the same wall-clock amount in the series'
/// zone and gives it the same length; a closed one keeps the times it
/// happened at.
fn member_patch(
    patch: &Patch,
    anchor: &Activity,
    member: &Activity,
    zone: &jiff::tz::TimeZone,
) -> Patch {
    let mut out = Patch {
        starts_at: None,
        ends_at: None,
        due_at: None,
        start_at: None,
        ..patch.clone()
    };
    if member.is_closed() {
        return out;
    }
    if member.kind.timed() && (patch.starts_at.is_some() || patch.ends_at.is_some()) {
        if let (Some(old_start), Some(old_end), Some(start)) =
            (anchor.starts_at, anchor.ends_at, member.starts_at)
        {
            let new_start = patch.starts_at.unwrap_or(old_start);
            let new_end = patch.ends_at.unwrap_or(old_end);
            let moved = series::wall_gap(old_start, new_start, zone);
            let length = series::wall_gap(new_start, new_end, zone);
            if let Some(start) = series::shifted(start, moved, zone) {
                out.starts_at = Some(start);
                out.ends_at = series::shifted(start, length, zone);
            }
        }
    }
    if member.kind == Kind::Task {
        let new_due = patch.due_at.unwrap_or(anchor.due_at);
        if patch.due_at.is_some() {
            out.due_at = Some(match (new_due, anchor.due_at, member.due_at) {
                (Some(new), Some(old), Some(due)) => {
                    series::shifted(due, series::wall_gap(old, new, zone), zone)
                }
                (new, _, _) => new,
            });
        }
        let member_due = out.due_at.unwrap_or(member.due_at);
        match patch.start_at {
            Some(Some(start)) => {
                out.start_at = Some(match (new_due, member_due) {
                    (Some(due), Some(own)) => {
                        series::shifted(own, series::wall_gap(due, start, zone), zone)
                    }
                    _ => Some(start),
                });
            }
            Some(None) => out.start_at = Some(None),
            None => {
                if let (true, Some(start), Some(old), Some(new)) = (
                    patch.due_at.is_some(),
                    member.start_at,
                    member.due_at,
                    member_due,
                ) {
                    out.start_at = Some(series::shifted(
                        start,
                        series::wall_gap(old, new, zone),
                        zone,
                    ));
                }
            }
        }
    }
    out
}

impl Store<'_> {
    pub(crate) async fn prepare_series(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        draft: &Draft,
        repeat: &Repeat,
    ) -> WriteResult<SeriesPlan> {
        let mut errors = repeat.check();
        if !draft.kind.timed() && draft.kind != Kind::Task {
            errors.push(FieldError::new(
                "repeat",
                "is for calls, meetings and tasks; a note does not repeat",
            ));
        }
        if draft.status.is_some_and(Status::closed) {
            errors.push(FieldError::new(
                "status",
                "must be planned for a repeating call; a held call happened once",
            ));
        }
        if draft.kind == Kind::Task && draft.due_at.is_none() {
            errors.push(FieldError::new("due_at", "is needed for a repeating task"));
        }
        let checked = match self.prepare_create(client, scope, draft).await {
            Ok(checked) => Some(checked),
            Err(WriteError::Invalid(more)) => {
                errors.extend(more);
                None
            }
            Err(error) => return Err(error),
        };
        if !errors.is_empty() {
            return Err(WriteError::Invalid(errors));
        }
        let draft = checked.ok_or(Error::Internal)?;
        let zone = jiff::tz::TimeZone::get(&repeat.timezone).map_err(|_| {
            invalid(FieldError::new(
                "repeat.timezone",
                "is not a time zone name such as Asia/Kuala_Lumpur",
            ))
        })?;
        let first = series::anchor(&draft, &zone).ok_or_else(|| {
            invalid(FieldError::new(
                "repeat",
                "needs a start time for a call or meeting, or a due date for a task",
            ))
        })?;
        let work_calendar = if repeat.working_days_only {
            let company = scope
                .current_company()
                .or(scope.actor().organisation())
                .ok_or(Error::Internal)?;
            match self.calendars.default_for(scope, company).await? {
                Some(code) => Some(code),
                None => {
                    return Err(invalid(FieldError::new(
                        "repeat.working_days_only",
                        "needs a default work calendar for your company; ask an administrator to set one",
                    )))
                }
            }
        } else {
            None
        };
        let dates = match &work_calendar {
            Some(code) => {
                let last = repeat.working_window(first);
                let days = self.calendars.days(scope, code, first, last).await?;
                let working: std::collections::HashMap<jiff::civil::Date, bool> = days
                    .iter()
                    .map(|day| {
                        (
                            day.date,
                            day.day_type == bilibi_work_calendar::DayType::Working,
                        )
                    })
                    .collect();
                let answer = |date: jiff::civil::Date| working.get(&date).copied();
                repeat.dates(first, Some(&answer))
            }
            None => repeat.dates(first, None),
        }
        .map_err(invalid)?;
        let occurrences = series::occurrences(&draft, &zone, &dates).map_err(invalid)?;
        let mut weekdays: Vec<i16> = repeat
            .weekdays
            .iter()
            .map(|day| i16::from(day.to_monday_one_offset()))
            .collect();
        if repeat.frequency == Frequency::Weekly && weekdays.is_empty() {
            weekdays.push(i16::from(first.weekday().to_monday_one_offset()));
        }
        weekdays.sort_unstable();
        weekdays.dedup();
        Ok(SeriesPlan {
            draft,
            occurrences,
            weekdays,
            month_day: (repeat.frequency == Frequency::Monthly).then(|| i16::from(first.day())),
            work_calendar,
        })
    }

    pub(crate) async fn create_series(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        draft: &Draft,
        repeat: &Repeat,
    ) -> WriteResult<Vec<Activity>> {
        let plan = self.prepare_series(tx.sql(), scope, draft, repeat).await?;
        let (tenant, user) = (scope.tenant(), scope.user());
        let company = filing_company(scope).ok_or(Error::Internal)?;
        let rule = repeat.clone();
        let (weekdays, month_day, work_calendar) = (
            plan.weekdays.clone(),
            plan.month_day,
            plan.work_calendar.clone(),
        );
        let series_id = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let until = rule.until.map(|until| until.to_string());
                    let id: i64 = sql
                        .query_one(
                            "INSERT INTO core_activity_series(tenant_id,frequency,every,weekdays,month_day,until,occurrences,work_calendar,timezone,created_by)
                             VALUES ($1,$2,$3,$4,$5,$6::text::date,$7,$8,$9,$10) RETURNING id",
                            &[
                                &tenant,
                                &rule.frequency.code(),
                                &rule.every,
                                &weekdays,
                                &month_day,
                                &until,
                                &rule.count,
                                &work_calendar,
                                &rule.timezone,
                                &user,
                            ],
                        )
                        .await?
                        .get(0);
                    let change = Change {
                        kind: SERIES_RECORD.into(),
                        id,
                        old: json!({}),
                        new: json!({
                            "frequency": rule.frequency.code(),
                            "every": rule.every,
                            "weekdays": weekdays,
                            "month_day": month_day,
                            "until": until,
                            "occurrences": rule.count,
                            "work_calendar": work_calendar,
                            "timezone": rule.timezone,
                        }),
                    };
                    Ok((id, vec![change]))
                })
            })
            .await?;
        let mut created = Vec::with_capacity(plan.occurrences.len());
        for (index, occurrence) in plan.occurrences.into_iter().enumerate() {
            let index = i16::try_from(index + 1).map_err(|_| Error::Internal)?;
            created.push(
                self.insert(
                    tx,
                    occurrence,
                    tenant,
                    company,
                    user,
                    Some((series_id, index)),
                )
                .await?,
            );
        }
        Ok(created)
    }

    /// Each occurrence a series change reaches, with the patch it gets.
    pub(crate) async fn prepare_update_series(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        version: i64,
        apply: Apply,
        patch: &Patch,
    ) -> WriteResult<Vec<(Activity, Patch)>> {
        let (anchor, series_id, index) = series_anchor(client, scope, id, version).await?;
        if patch.is_empty() {
            return Err(invalid(FieldError::new(
                "patch",
                "names no field to change",
            )));
        }
        if patch.status.is_some() {
            return Err(invalid(FieldError::new(
                "status",
                "changes one occurrence at a time; change it on that occurrence",
            )));
        }
        let rule = series(client, scope, series_id).await?;
        let zone =
            jiff::tz::TimeZone::get(&rule.repeat.timezone).unwrap_or(jiff::tz::TimeZone::UTC);
        Ok(members(client, scope, series_id, apply, index)
            .await?
            .into_iter()
            .map(|member| {
                let patch = member_patch(patch, &anchor, &member, &zone);
                (member, patch)
            })
            .filter(|(_, patch)| !patch.is_empty())
            .collect())
    }

    pub(crate) async fn update_series(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        apply: Apply,
        patch: &Patch,
    ) -> WriteResult<Vec<Activity>> {
        let plans = self
            .prepare_update_series(tx.sql(), scope, id, version, apply, patch)
            .await?;
        for (member, patch) in &plans {
            self.prepare_update(tx.sql(), scope, member.id, member.version, patch)
                .await?;
        }
        let mut updated = Vec::with_capacity(plans.len());
        for (member, patch) in plans {
            updated.push(
                self.update(tx, scope, member.id, member.version, &patch)
                    .await?,
            );
        }
        Ok(updated)
    }

    pub(crate) async fn delete_series(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        apply: Apply,
    ) -> WriteResult<Vec<Activity>> {
        let members = prepare_delete_series(tx.sql(), scope, id, version, apply).await?;
        for member in &members {
            self.delete(tx, scope, member.id, member.version).await?;
        }
        Ok(members)
    }
}

/// The occurrences a series delete removes, each one the actor may change.
pub(crate) async fn prepare_delete_series(
    client: &dyn ReadClient,
    scope: &Scope,
    id: i64,
    version: i64,
    apply: Apply,
) -> WriteResult<Vec<Activity>> {
    let (_, series_id, index) = series_anchor(client, scope, id, version).await?;
    let members = members(client, scope, series_id, apply, index).await?;
    for member in &members {
        editable(client, scope, member.id, member.version, false).await?;
    }
    Ok(members)
}
