//! `core_activities` and `core_activity_guests` SQL. Every statement binds the
//! tenant; every write goes through the caller's `MutationTransaction` and
//! returns its `Change`s. Checks run before a write, so a refusal never
//! poisons the caller's transaction.
use crate::model::{self, Draft, FieldError, Guest, Kind, Status};
use crate::{
    Activity, Direction, Filter, GuestRow, Mine, Page, Patch, Priority, Reference, Sort, State,
    WriteError, WriteResult, GUEST_RECORD, MANAGE, MANAGE_ALL, RECORD, VIEW, VIEW_ALL,
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

const COLUMNS: &str = "a.id,a.company_id,a.kind,a.subject,a.status,a.priority,a.direction,a.starts_at,a.ends_at,a.due_at,a.start_at,a.location,a.join_url,a.meeting_type,a.call_outcome,a.description,a.internal_notes,a.related_kind,a.related_id,a.shared,a.assignee_user_id,a.created_by,a.created_at,a.updated_at,a.closed_at,a.reminded_at,a.version";
/// The activities the reader takes part in, with `$2` the reader's account:
/// they created it, are assigned it or are an account guest. Nothing else
/// (filing company, a related record) reaches a private activity.
pub(crate) const MINE: &str = "(a.created_by=$2 OR a.assignee_user_id=$2 OR EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_user_id=$2))";

fn time(value: Option<SystemTime>) -> Result<Option<Timestamp>> {
    value
        .map(|t| Timestamp::try_from(t).map_err(|_| Error::Internal))
        .transpose()
}
fn system(value: Option<Timestamp>) -> Option<SystemTime> {
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
        "description": a.description,
        "internal_notes": a.internal_notes,
        "related": a.related.as_ref().map(|r| json!({"kind": r.kind, "id": r.id})),
        "shared": a.shared,
        "assignee_user_id": a.assignee_user_id,
        "closed_at": iso(a.closed_at),
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
            "SELECT id,guest_user_id,guest_person_id,guest_email,role,response FROM core_activity_guests WHERE tenant_id=$1 AND activity_id=$2 ORDER BY id",
            &[&scope.tenant(), &id],
        )
        .await?
        .iter()
        .map(|row| {
            Ok(GuestRow {
                id: row.get(0),
                user_id: row.get(1),
                person: row
                    .get::<_, Option<i64>>(2)
                    .map(|id| PersonId::new(id).map_err(|_| Error::Internal))
                    .transpose()?,
                email: row.get(3),
                role: row.get(4),
                response: row.get(5),
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
    let page_size = filter.page_size.clamp(1, 300);
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
              OR ($12::bigint IS NOT NULL AND EXISTS (SELECT 1 FROM core_activity_guests g WHERE g.tenant_id=a.tenant_id AND g.activity_id=a.id AND g.guest_person_id=$12)))"
    );
    let params: [&(dyn ToSql + Sync); 12] = [
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

/// Directory reads on a borrowed client, so a preview checks an assignee on
/// the plan's own transaction.
struct Directory<'c>(&'c dyn ReadClient);
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
        if draft.kind == Kind::Meeting {
            errors.push(FieldError::new(
                "kind",
                "Meetings are not available yet; log a call or create a task.",
            ));
        }
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
        let assignee = draft.assignee.ok_or(Error::Internal)?;
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
                                "INSERT INTO core_activities AS a (tenant_id,company_id,kind,subject,status,priority,direction,starts_at,ends_at,due_at,start_at,call_outcome,description,internal_notes,related_kind,related_id,shared,assignee_user_id,created_by,closed_at)
                                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,CASE WHEN $20 THEN now() END)
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
                                "UPDATE core_activities a SET subject=$4,status=$5,priority=$6,direction=$7,starts_at=$8,ends_at=$9,due_at=$10,start_at=$11,call_outcome=$12,description=$13,internal_notes=$14,related_kind=$15,related_id=$16,shared=$17,assignee_user_id=$18,
                                   closed_at=CASE WHEN NOT $19 THEN NULL WHEN a.closed_at IS NULL THEN now() ELSE a.closed_at END,
                                   reminded_at=CASE WHEN $20 THEN NULL ELSE a.reminded_at END,
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
        Ok(activity)
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
    if let Some(error) = model::held_call_ended(activity.kind, Some(status), activity.ends_at) {
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
        .filter_map(|g| match (g.person, g.email) {
            (Some(person), _) if g.role == "invited" => Some(Guest::Person(person)),
            (Some(_), _) => None,
            (None, Some(email)) => Some(Guest::Address {
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

pub(crate) async fn delete(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    id: i64,
    version: i64,
) -> WriteResult<()> {
    let before = editable(tx.sql(), scope, id, version, true).await?;
    let tenant = scope.tenant();
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
    .map_err(stale)
}
