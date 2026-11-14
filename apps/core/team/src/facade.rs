//! The Teams facade: tenant-wide reads under `teams.view`, writes under
//! `teams.manage` plus work access to the team's company. Every membership
//! change (a create's leads, add, remove, lead on or off) is planned by
//! [`Teams::plan_members`] and written by [`apply`], the one place a
//! membership row changes, so a later rule (team roles) has one place to
//! check.
use crate::members::{apply, MemberPlan, MemberStep};
use crate::tree::{parent_refusal, sibling_taken, tree_lock};
use crate::{KINDS, MANAGE, VIEW};
use bilibi_authz::authorize_key;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_picklists::PickLists;
use bilibi_tenancy::{Authentication, Scope};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::SystemTime,
};
use tokio_postgres::{GenericClient, Row};

pub type FieldErrors = BTreeMap<String, Vec<String>>;
pub type Checked<T> = std::result::Result<T, FieldErrors>;
pub(crate) fn invalid(field: &str, message: &str) -> FieldErrors {
    [(field.into(), vec![message.into()])].into()
}
/// How deep teams nest: a team and at most four levels of teams above it.
/// Leads of the teams above also lead the teams below, so the bound keeps
/// that roll-up short.
pub const MAX_DEPTH: i32 = 5;
/// The most people one add or remove names.
pub const MAX_BATCH: usize = 100;
/// The most members one team holds.
pub const MAX_MEMBERS: i64 = 5000;
/// The most teams a picker of teams offers.
pub const MAX_CHOICES: i64 = 500;

/// A team's own facts. The id is stable: Domains keep their own facts about
/// a team in their own tables beside it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Team {
    pub id: i64,
    pub name: String,
    /// A code of the tenant's `core.team.kind` pick-list ("team", "crew").
    pub kind: String,
    pub parent_id: Option<i64>,
    pub company_id: Option<i64>,
    pub version: i64,
}
impl Team {
    fn row(row: &Row) -> Self {
        Self {
            id: row.get("id"),
            name: row.get("name"),
            kind: row.get("kind"),
            parent_id: row.get("parent_id"),
            company_id: row.get("company_id"),
            version: row.get("version"),
        }
    }
    pub fn to_json(&self) -> Value {
        json!(self)
    }
}
/// A list row: the team with the names a reader needs beside it.
#[derive(Clone, Debug, Serialize)]
pub struct TeamRow {
    #[serde(flatten)]
    pub team: Team,
    pub parent_name: Option<String>,
    pub company_name: Option<String>,
    pub members: i64,
    /// The first few leads by name; `lead_count` says how many there are.
    pub leads: Vec<String>,
    pub lead_count: i64,
}
/// One member of a team.
#[derive(Clone, Debug, Serialize)]
pub struct Member {
    pub user_id: i64,
    pub name: String,
    pub lead: bool,
    #[serde(skip)]
    pub added_at: SystemTime,
}
/// A lead of a team above, who also leads this one.
#[derive(Clone, Debug, Serialize)]
pub struct LeadAbove {
    pub team_id: i64,
    pub team_name: String,
    pub user_id: i64,
    pub name: String,
}
/// A team a person belongs to, for the `core/team.teams_of` port.
#[derive(Clone, Debug, Serialize)]
pub struct Membership {
    pub team_id: i64,
    pub name: String,
    pub kind: String,
    pub company_id: Option<i64>,
    pub lead: bool,
}
pub struct TeamQuery<'a> {
    pub search: &'a str,
    pub kind: &'a str,
    /// Only the direct sub-teams of this team.
    pub parent: Option<i64>,
    pub limit: i64,
    pub offset: i64,
}
/// What a create submits.
#[derive(Clone, Debug, Default)]
pub struct TeamInput {
    pub name: String,
    /// Empty takes the tenant's default kind.
    pub kind: String,
    pub parent_id: Option<i64>,
    pub company_id: Option<i64>,
    /// One or more staff accounts; they become the team's first members.
    pub leads: Vec<i64>,
}
pub struct Teams {
    pub(crate) picklists: Arc<PickLists>,
    pub(crate) authentication: Arc<Authentication>,
}
const COLUMNS: &str = "id,name,kind,parent_id,company_id,version";
fn needle(search: &str) -> String {
    format!(
        "%{}%",
        search
            .trim()
            .chars()
            .take(200)
            .collect::<String>()
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}
fn checked_text(value: &str, max: usize) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= max && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}
pub(crate) fn distinct(ids: &[i64]) -> Vec<i64> {
    ids.iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl Teams {
    pub async fn get(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        id: i64,
    ) -> Result<Team> {
        authorize_key(client, scope, VIEW).await?;
        load(client, scope, id, false).await
    }
    /// One bounded page of the tenant's teams, by name. Kind and parent are
    /// filters on this one list.
    pub async fn list(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        query: &TeamQuery<'_>,
    ) -> Result<(Vec<TeamRow>, i64)> {
        authorize_key(client, scope, VIEW).await?;
        let needle = needle(query.search);
        let predicate = "FROM core_teams t WHERE t.tenant_id=$1 AND t.name ILIKE $2 AND ($3='' OR t.kind=$3) AND ($4::bigint IS NULL OR t.parent_id=$4)";
        let total = client
            .query_one(
                &format!("SELECT count(*) {predicate}"),
                &[&scope.tenant(), &needle, &query.kind, &query.parent],
            )
            .await?
            .get(0);
        let rows = client
            .query(
                &format!(
                    "SELECT t.id,t.name,t.kind,t.parent_id,t.company_id,t.version, \
                     (SELECT p.name FROM core_teams p WHERE p.tenant_id=t.tenant_id AND p.id=t.parent_id) AS parent_name, \
                     (SELECT c.name FROM companies c WHERE c.tenant_id=t.tenant_id AND c.id=t.company_id) AS company_name, \
                     (SELECT count(*) FROM core_team_members m WHERE m.tenant_id=t.tenant_id AND m.team_id=t.id) AS members, \
                     (SELECT count(*) FROM core_team_members m WHERE m.tenant_id=t.tenant_id AND m.team_id=t.id AND m.lead) AS lead_count, \
                     COALESCE((SELECT array_agg(x.name) FROM (SELECT u.name FROM core_team_members m JOIN users u ON u.tenant_id=m.tenant_id AND u.id=m.user_id \
                       WHERE m.tenant_id=t.tenant_id AND m.team_id=t.id AND m.lead ORDER BY lower(u.name),u.id LIMIT 3) x),'{{}}') AS leads \
                     {predicate} ORDER BY lower(t.name),t.id LIMIT $5 OFFSET $6"
                ),
                &[
                    &scope.tenant(),
                    &needle,
                    &query.kind,
                    &query.parent,
                    &query.limit.clamp(1, 100),
                    &query.offset.max(0),
                ],
            )
            .await?;
        Ok((
            rows.iter()
                .map(|row| TeamRow {
                    team: Team::row(row),
                    parent_name: row.get("parent_name"),
                    company_name: row.get("company_name"),
                    members: row.get("members"),
                    leads: row.get("leads"),
                    lead_count: row.get("lead_count"),
                })
                .collect(),
            total,
        ))
    }
    /// The team's parent and company names, for its record page.
    pub async fn names(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: &Team,
    ) -> Result<(Option<String>, Option<String>)> {
        authorize_key(client, scope, VIEW).await?;
        let row = client
            .query_one(
                "SELECT (SELECT name FROM core_teams WHERE tenant_id=$1 AND id=$2), (SELECT name FROM companies WHERE tenant_id=$1 AND id=$3)",
                &[&scope.tenant(), &team.parent_id, &team.company_id],
            )
            .await?;
        Ok((row.get(0), row.get(1)))
    }
    /// One page of a team's members, leads first, then by name.
    pub async fn members(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: i64,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Member>, i64)> {
        authorize_key(client, scope, VIEW).await?;
        load(client, scope, team, false).await?;
        let total = client
            .query_one(
                "SELECT count(*) FROM core_team_members WHERE tenant_id=$1 AND team_id=$2",
                &[&scope.tenant(), &team],
            )
            .await?
            .get(0);
        let rows = client
            .query(
                "SELECT m.user_id,u.name,m.lead,m.added_at FROM core_team_members m JOIN users u ON u.tenant_id=m.tenant_id AND u.id=m.user_id \
                 WHERE m.tenant_id=$1 AND m.team_id=$2 ORDER BY m.lead DESC,lower(u.name),m.user_id LIMIT $3 OFFSET $4",
                &[&scope.tenant(), &team, &limit.clamp(1, 300), &offset.max(0)],
            )
            .await?;
        Ok((rows.iter().map(member).collect(), total))
    }
    /// The team's own leads, by name.
    pub async fn leads(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: i64,
    ) -> Result<Vec<Member>> {
        authorize_key(client, scope, VIEW).await?;
        Ok(client
            .query(
                "SELECT m.user_id,u.name,m.lead,m.added_at FROM core_team_members m JOIN users u ON u.tenant_id=m.tenant_id AND u.id=m.user_id \
                 WHERE m.tenant_id=$1 AND m.team_id=$2 AND m.lead ORDER BY lower(u.name),m.user_id",
                &[&scope.tenant(), &team],
            )
            .await?
            .iter()
            .map(member)
            .collect())
    }
    /// Leads of the teams above, nearest team first: sub-teams roll up to
    /// them. Bounded by [`MAX_DEPTH`].
    pub async fn leads_above(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: i64,
    ) -> Result<Vec<LeadAbove>> {
        authorize_key(client, scope, VIEW).await?;
        Ok(client
            .query(
                "WITH RECURSIVE up(id,depth) AS (SELECT parent_id,1 FROM core_teams WHERE tenant_id=$1 AND id=$2 AND parent_id IS NOT NULL \
                   UNION ALL SELECT t.parent_id,up.depth+1 FROM up JOIN core_teams t ON t.tenant_id=$1 AND t.id=up.id WHERE t.parent_id IS NOT NULL AND up.depth<$3) \
                 SELECT t.id,t.name,m.user_id,u.name FROM up JOIN core_teams t ON t.tenant_id=$1 AND t.id=up.id \
                   JOIN core_team_members m ON m.tenant_id=$1 AND m.team_id=t.id AND m.lead JOIN users u ON u.tenant_id=$1 AND u.id=m.user_id \
                 ORDER BY up.depth,lower(u.name),m.user_id",
                &[&scope.tenant(), &team, &(MAX_DEPTH - 1)],
            )
            .await?
            .iter()
            .map(|row| LeadAbove {
                team_id: row.get(0),
                team_name: row.get(1),
                user_id: row.get(2),
                name: row.get(3),
            })
            .collect())
    }
    /// The teams a person belongs to, by name (the `core/team.teams_of` port).
    pub async fn teams_of(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        user: i64,
    ) -> Result<Vec<Membership>> {
        authorize_key(client, scope, VIEW).await?;
        Ok(client
            .query(
                "SELECT t.id,t.name,t.kind,t.company_id,m.lead FROM core_team_members m JOIN core_teams t ON t.tenant_id=m.tenant_id AND t.id=m.team_id \
                 WHERE m.tenant_id=$1 AND m.user_id=$2 ORDER BY lower(t.name),t.id LIMIT $3",
                &[&scope.tenant(), &user, &MAX_CHOICES],
            )
            .await?
            .iter()
            .map(|row| Membership {
                team_id: row.get(0),
                name: row.get(1),
                kind: row.get(2),
                company_id: row.get(3),
                lead: row.get(4),
            })
            .collect())
    }
    /// Every member id of a team, bounded by [`MAX_MEMBERS`] (the
    /// `core/team.team` port).
    pub async fn member_ids(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: i64,
    ) -> Result<Vec<(i64, bool)>> {
        authorize_key(client, scope, VIEW).await?;
        Ok(client
            .query(
                "SELECT user_id,lead FROM core_team_members WHERE tenant_id=$1 AND team_id=$2 ORDER BY user_id LIMIT $3",
                &[&scope.tenant(), &team, &MAX_MEMBERS],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }
    /// Teams by name for a picker, at most [`MAX_CHOICES`].
    pub async fn choices(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Vec<(i64, String)>> {
        authorize_key(client, scope, VIEW).await?;
        Ok(client
            .query(
                "SELECT id,name FROM core_teams WHERE tenant_id=$1 ORDER BY lower(name),id LIMIT $2",
                &[&scope.tenant(), &MAX_CHOICES],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }
    /// Whether the actor may change `team` now: `teams.manage`, work access
    /// to its company, and a company that is not archived.
    pub async fn may_change(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        team: &Team,
    ) -> Result<bool> {
        Ok(matches!(
            authorize_team(client, scope, team.company_id, false).await,
            Ok(None)
        ))
    }

    pub async fn preview_create(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        input: &TeamInput,
    ) -> Result<Checked<TeamInput>> {
        authorize_key(client, scope, MANAGE).await?;
        let mut errors = FieldErrors::new();
        let mut after = input.clone();
        match checked_text(&input.name, 200) {
            Some(name) => after.name = name,
            None => errors.extend(invalid("name", "Use 1 to 200 characters.")),
        }
        if after.kind.trim().is_empty() {
            after.kind = self
                .picklists
                .default(client, scope, KINDS)
                .await?
                .unwrap_or_default();
        }
        after.kind = after.kind.trim().to_owned();
        if let Some(refusal) = self.kind_refusal(client, scope, &after.kind).await? {
            errors.extend(refusal);
        }
        if let Some(company) = after.company_id {
            match authorize_team(client, scope, Some(company), false).await {
                Ok(None) => {}
                Ok(Some(refusal)) => errors.extend(refusal),
                Err(Error::Forbidden | Error::NotFound) => errors.extend(invalid(
                    "company_id",
                    "Choose a company you work in, or leave it empty.",
                )),
                Err(e) => return Err(e),
            }
        }
        if let Some(parent) = after.parent_id {
            if let Some(refusal) = parent_refusal(client, scope, None, parent).await? {
                errors.extend(refusal);
            }
        }
        after.leads = distinct(&input.leads);
        if after.leads.is_empty() {
            errors.extend(invalid("leads", "Choose at least one lead."));
        } else if after.leads.len() > MAX_BATCH {
            errors.extend(invalid(
                "leads",
                &format!("Choose at most {MAX_BATCH} leads at a time."),
            ));
        } else if self.live_staff(client, scope, &after.leads).await? != after.leads {
            errors.extend(invalid("leads", "Choose leads from your active staff."));
        }
        if errors.is_empty()
            && sibling_taken(client, scope, None, after.parent_id, &after.name).await?
        {
            errors.extend(invalid(
                "name",
                "A team with this name already exists here.",
            ));
        }
        Ok(if errors.is_empty() {
            Ok(after)
        } else {
            Err(errors)
        })
    }
    /// Creates a team with its leads as its first members.
    pub async fn create(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &TeamInput,
    ) -> Result<Checked<Team>> {
        tree_lock(tx, scope).await?;
        if let Some(company) = input.company_id {
            // Hold the company against a concurrent archive.
            if let Some(refusal) = authorize_team(tx.sql(), scope, Some(company), true).await? {
                return Ok(Err(refusal));
            }
        }
        let input = match self.preview_create(tx.sql(), scope, input).await? {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        if let Err(refusal) = self
            .picklists
            .check(tx, scope, KINDS, &input.kind, None)
            .await?
        {
            return Ok(Err(invalid("kind", refusal.message())));
        }
        let tenant = scope.tenant();
        let fields = input.clone();
        let team = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let row = sql
                        .query_one(
                            &format!("INSERT INTO core_teams(tenant_id,name,kind,parent_id,company_id) VALUES($1,$2,$3,$4,$5) RETURNING {COLUMNS}"),
                            &[&tenant, &fields.name, &fields.kind, &fields.parent_id, &fields.company_id],
                        )
                        .await?;
                    let team = Team::row(&row);
                    Ok((
                        team.clone(),
                        vec![Change {
                            kind: "team".into(),
                            id: team.id,
                            old: json!({}),
                            new: team.to_json(),
                        }],
                    ))
                })
            })
            .await?;
        let names = self.names_of(tx.sql(), scope, &input.leads).await?;
        let plan = MemberPlan {
            team: team.clone(),
            steps: input
                .leads
                .iter()
                .map(|user| MemberStep {
                    user_id: *user,
                    name: names.get(user).cloned().unwrap_or_default(),
                    before: None,
                    after: Some(true),
                })
                .collect(),
            unchanged: Vec::new(),
        };
        apply(tx, scope, &plan).await?;
        Ok(Ok(team))
    }
    /// Name, kind, parent team or company, with the version read.
    pub async fn preview_update(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Value,
    ) -> Result<Checked<Team>> {
        authorize_key(client, scope, MANAGE).await?;
        let before = load(client, scope, id, false).await?;
        if let Some(refusal) = authorize_team(client, scope, before.company_id, false).await? {
            return Ok(Err(refusal));
        }
        if before.version != version {
            return Ok(Err(invalid(
                "version",
                "This team changed. Reload before saving.",
            )));
        }
        let Some(fields) = patch.as_object().filter(|f| !f.is_empty()) else {
            return Ok(Err(invalid(
                "patch",
                "Name at least one team fact to change.",
            )));
        };
        let mut after = before.clone();
        for (field, value) in fields {
            let id_of = |value: &Value| -> std::result::Result<Option<i64>, ()> {
                match value {
                    Value::Null => Ok(None),
                    Value::String(s) if s.trim().is_empty() => Ok(None),
                    Value::String(s) => s.trim().parse().map(Some).map_err(|_| ()),
                    Value::Number(n) => n.as_i64().filter(|v| *v > 0).map(Some).ok_or(()),
                    _ => Err(()),
                }
            };
            match field.as_str() {
                "name" => match value.as_str().and_then(|v| checked_text(v, 200)) {
                    Some(name) => after.name = name,
                    None => return Ok(Err(invalid("name", "Use 1 to 200 characters."))),
                },
                "kind" => {
                    let kind = value.as_str().unwrap_or_default().trim().to_owned();
                    if kind != before.kind {
                        if let Some(refusal) = self.kind_refusal(client, scope, &kind).await? {
                            return Ok(Err(refusal));
                        }
                    }
                    after.kind = kind;
                }
                "parent_id" => {
                    let Ok(parent) = id_of(value) else {
                        return Ok(Err(invalid("parent_id", "Choose a team from the list.")));
                    };
                    if parent != before.parent_id {
                        if let Some(parent) = parent {
                            if let Some(refusal) =
                                parent_refusal(client, scope, Some(id), parent).await?
                            {
                                return Ok(Err(refusal));
                            }
                        }
                    }
                    after.parent_id = parent;
                }
                "company_id" => {
                    let Ok(company) = id_of(value) else {
                        return Ok(Err(invalid(
                            "company_id",
                            "Choose a company from the list.",
                        )));
                    };
                    if company != before.company_id {
                        if let Some(company) = company {
                            match authorize_team(client, scope, Some(company), false).await {
                                Ok(None) => {}
                                Ok(Some(refusal)) => return Ok(Err(refusal)),
                                Err(Error::Forbidden | Error::NotFound) => {
                                    return Ok(Err(invalid(
                                        "company_id",
                                        "Choose a company you work in, or leave it empty.",
                                    )))
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    }
                    after.company_id = company;
                }
                _ => {
                    return Ok(Err(invalid(
                        field,
                        "This team fact cannot be changed here.",
                    )))
                }
            }
        }
        if (after.name.to_lowercase() != before.name.to_lowercase()
            || after.parent_id != before.parent_id)
            && sibling_taken(client, scope, Some(id), after.parent_id, &after.name).await?
        {
            return Ok(Err(invalid(
                "name",
                "A team with this name already exists here.",
            )));
        }
        Ok(Ok(after))
    }
    pub async fn update(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Value,
    ) -> Result<Checked<Team>> {
        // Parent moves serialize, so two moves cannot close a loop.
        tree_lock(tx, scope).await?;
        let before = load(tx.sql(), scope, id, true).await?;
        if let Some(refusal) = authorize_team(tx.sql(), scope, before.company_id, true).await? {
            return Ok(Err(refusal));
        }
        let mut after = match self
            .preview_update(tx.sql(), scope, id, version, patch)
            .await?
        {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        if after.company_id != before.company_id {
            if let Some(company) = after.company_id {
                if let Some(refusal) = authorize_team(tx.sql(), scope, Some(company), true).await? {
                    return Ok(Err(refusal));
                }
            }
        }
        if after.kind != before.kind {
            if let Err(refusal) = self
                .picklists
                .check(tx, scope, KINDS, &after.kind, None)
                .await?
            {
                return Ok(Err(invalid("kind", refusal.message())));
            }
        }
        after.version += 1;
        let tenant = scope.tenant();
        let saved = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    sql.execute(
                        "UPDATE core_teams SET name=$3,kind=$4,parent_id=$5,company_id=$6,version=$7 WHERE tenant_id=$1 AND id=$2",
                        &[&tenant, &id, &after.name, &after.kind, &after.parent_id, &after.company_id, &after.version],
                    )
                    .await?;
                    let change = Change {
                        kind: "team".into(),
                        id,
                        old: before.to_json(),
                        new: after.to_json(),
                    };
                    Ok((after, vec![change]))
                })
            })
            .await?;
        Ok(Ok(saved))
    }
    /// Checks a membership change without writing: who it adds, removes or
    /// makes lead, and who it leaves alone because it already holds.
    async fn kind_refusal(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        kind: &str,
    ) -> Result<Option<FieldErrors>> {
        Ok(
            match self.picklists.label(client, scope, KINDS, kind).await? {
                Some(label) if !label.retired => None,
                Some(_) => Some(invalid("kind", "This kind is retired; choose another.")),
                None => Some(invalid("kind", "Choose a kind from the list.")),
            },
        )
    }
    pub(crate) async fn live_staff(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        users: &[i64],
    ) -> Result<Vec<i64>> {
        let mut users = distinct(users);
        users.retain(|u| *u > 0);
        self.authentication.live_staff(client, scope, &users).await
    }
    pub(crate) async fn names_of(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        users: &[i64],
    ) -> Result<BTreeMap<i64, String>> {
        Ok(client
            .query(
                "SELECT id,name FROM users WHERE tenant_id=$1 AND id=ANY($2)",
                &[&scope.tenant(), &users],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }
}
fn member(row: &Row) -> Member {
    Member {
        user_id: row.get(0),
        name: row.get(1),
        lead: row.get(2),
        added_at: row.get(3),
    }
}
pub(crate) async fn load(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    id: i64,
    lock: bool,
) -> Result<Team> {
    client
        .query_opt(
            &format!(
                "SELECT {COLUMNS} FROM core_teams WHERE tenant_id=$1 AND id=$2{}",
                if lock { " FOR UPDATE" } else { "" }
            ),
            &[&scope.tenant(), &id],
        )
        .await?
        .as_ref()
        .map(Team::row)
        .ok_or(Error::NotFound)
}
/// `teams.manage`, and for a team of a company, work access there and a
/// company that is not archived (`Some` refusal). A team never reaches a
/// company its manager cannot already work in.
pub(crate) async fn authorize_team(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    company: Option<i64>,
    lock: bool,
) -> Result<Option<FieldErrors>> {
    authorize_key(client, scope, MANAGE).await?;
    let Some(company) = company else {
        return Ok(None);
    };
    bilibi_company::authorize_work_target(client, scope, company, MANAGE).await?;
    match bilibi_company::require_writable(client, scope, company, lock).await {
        Ok(_) => Ok(None),
        Err(bilibi_company::WriteError::Archived) => Ok(Some(invalid(
            "company_id",
            "This team's company is archived, so the team cannot change.",
        ))),
        Err(bilibi_company::WriteError::Access(error)) => Err(error),
        Err(_) => Err(Error::NotFound),
    }
}
