//! Running-database proof of record reach (#683): a role gives a capability
//! with reach mine, my team's or all; Grid keeps a reader to the rows their
//! reach admits in row pages, summaries, projections and text search; Core
//! Activity's oversight read and edit follow the same rule; team membership
//! applies at the next read; a member reaches what the team's role gives
//! unless the user is restricted (a direct deny, or a narrower direct allow);
//! and no grant door lets an actor give more reach than they hold. Explicit disposable DB with the
//! walkthrough fixture only; exits non-zero when a check does not hold.
//!
//! BILIBI_RECORD_REACH=disposable cargo run -p bilibi-web --example record-reach
use bilibi_authz::{
    grants::{CreatedRole, Direct, Grants},
    roles::NewRole,
    Reach,
};
use bilibi_core::activity::{Activities, Filter, State, WriteError};
use bilibi_core::team::{MemberChange, TeamInput, Teams};
use bilibi_database::{pool, Db, Error, MutationBoundary};
use bilibi_grid::{Agg, Bounds, Measure, PageSpec, SummarySpec};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const TASKS: &str = bilibi_grid::reference::TASKS;
const ACTIVITY_VIEW: &str = "core.activity.view";
const ACTIVITY_ALL_VIEW: &str = "core.activity.all.view";
const ACTIVITY_MANAGE: &str = "core.activity.manage";
const ACTIVITY_ALL_MANAGE: &str = "core.activity.all.manage";

type Failure = Box<dyn std::error::Error>;

fn prove(label: &str, passed: bool) -> Result<(), Failure> {
    if !passed {
        return Err(format!("FAIL: {label}").into());
    }
    println!("PASS: {label}");
    Ok(())
}
async fn login(app: &bilibi_web::App, email: &str) -> Result<Scope, Failure> {
    let login = app
        .user
        .login(email, bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    Ok(app.security.guard(&login.token).await?)
}

struct Proof {
    db: Db,
    boundary: Arc<MutationBoundary>,
    grants: Arc<Grants>,
    teams: Arc<Teams>,
    activities: Arc<Activities>,
}
impl Proof {
    async fn role(&self, admin: &Scope, name: &str) -> Result<i64, Failure> {
        match self
            .grants
            .create_role(
                admin,
                &NewRole {
                    company_id: Some(1),
                    name: name.into(),
                    code: name.to_lowercase().replace(' ', "_"),
                    description: "Disposable record reach proof".into(),
                },
            )
            .await?
        {
            CreatedRole::Created(id) => Ok(id),
            CreatedRole::Refused(errors) => Err(format!("create role {name}: {errors:?}").into()),
        }
    }
    /// The reference task names `scope` reads through every Grid door, each
    /// door's answer checked against the others.
    async fn tasks(&self, scope: &Scope) -> Result<BTreeSet<String>, Failure> {
        let page = bilibi_grid::page_keys(
            &self.db,
            scope,
            "reference_tasks",
            &PageSpec {
                filters: Vec::new(),
                sorts: Vec::new(),
                timezone: "UTC".into(),
                offset: 0,
                limit: 100,
                bounds: Bounds::default(),
            },
        )
        .await?;
        // Every id of the tenant, so the projection itself must drop the rest.
        let every: Vec<Value> = self
            .db
            .get()
            .await?
            .query("SELECT id FROM base_grid_ref_tasks ORDER BY id", &[])
            .await?
            .iter()
            .map(|row| json!(row.get::<_, i64>(0)))
            .collect();
        let projected =
            bilibi_grid::project(&self.db, scope, "reference_tasks", &every, &["name"]).await?;
        let names: BTreeSet<String> = projected
            .rows
            .iter()
            .filter_map(|row| row.cells.values().next()?.as_str().map(str::to_owned))
            .collect();
        let summary = bilibi_grid::summarize(
            &self.db,
            scope,
            "reference_tasks",
            &SummarySpec {
                filters: Vec::new(),
                groupings: Vec::new(),
                measures: vec![Measure {
                    column: "hours".into(),
                    agg: Agg::Sum,
                }],
                timezone: "UTC".into(),
                max_groups: 10,
                bounds: Bounds::default(),
            },
        )
        .await?;
        let client = self.db.get().await?;
        let search =
            bilibi_grid::query::text_search_sql(&**client, scope, "reference_tasks", &["name"])
                .await?;
        let found = client
            .query(
                &format!("SELECT count(*) FROM ({search}) s"),
                &[&scope.tenant(), &scope.user(), &"task"],
            )
            .await?[0]
            .get::<_, i64>(0);
        let hours: i64 = client
            .query_one(
                "SELECT COALESCE(sum(hours),0)::bigint FROM base_grid_ref_tasks WHERE tenant_id=$1 AND name=ANY($2)",
                &[&scope.tenant(), &names.iter().collect::<Vec<_>>()],
            )
            .await?
            .get(0);
        let total_hours = summary.total.values[0]
            .as_str()
            .and_then(|v| v.parse::<i64>().ok())
            .or_else(|| summary.total.values[0].as_i64())
            .unwrap_or(0);
        if page.total != names.len() as i64
            || page.keys.len() != names.len()
            || summary.total.rows != names.len() as i64
            || found != names.len() as i64
            || total_hours != hours
        {
            return Err(format!(
                "Grid doors disagree: page {} / {}, projection {names:?}, summary rows {} hours {total_hours} (expected {hours}), search {found}",
                page.total,
                page.keys.len(),
                summary.total.rows
            )
            .into());
        }
        Ok(names)
    }
    async fn set_reach(
        &self,
        actor: &Scope,
        role: i64,
        key: &str,
        reach: Option<Reach>,
    ) -> Result<Result<(), String>, Failure> {
        match self
            .grants
            .set_role_capability(actor, role, key, reach)
            .await
        {
            Ok(()) => Ok(Ok(())),
            Err(Error::Forbidden) => Ok(Err("forbidden".into())),
            Err(Error::Refused(why)) => Ok(Err(why)),
            Err(error) => Err(error.into()),
        }
    }
    async fn members(&self, admin: &Scope, team: i64, change: MemberChange) -> Result<(), Failure> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                admin.mutation_context("walkthrough.record-reach"),
            )
            .await?;
        self.teams
            .change_members(&mut tx, admin, team, &change)
            .await?
            .map_err(|e| format!("members: {e:?}"))?;
        tx.commit().await?;
        Ok(())
    }
    async fn team_role(
        &self,
        admin: &Scope,
        team: i64,
        role: i64,
        attach: bool,
    ) -> Result<(), Failure> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                admin.mutation_context("walkthrough.record-reach"),
            )
            .await?;
        self.teams
            .change_role(&mut tx, admin, team, role, attach)
            .await?
            .map_err(|e| format!("team role: {e:?}"))?;
        tx.commit().await?;
        Ok(())
    }
    async fn direct(&self, scope: &Scope, rule: Option<Direct>) -> Result<(), Failure> {
        self.grants.set_direct(scope, 2, TASKS, rule).await?;
        Ok(())
    }
    async fn close(&self, scope: &Scope, id: i64) -> Result<Result<(), String>, Failure> {
        let version = self.activities.get(scope, id).await?.version;
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                scope.mutation_context("walkthrough.record-reach"),
            )
            .await?;
        match self.activities.close(&mut tx, scope, id, version).await {
            Ok(_) => {
                tx.commit().await?;
                Ok(Ok(()))
            }
            Err(WriteError::Forbidden) => Ok(Err("forbidden".into())),
            Err(error) => Ok(Err(format!("{error:?}"))),
        }
    }
    async fn activity_titles(&self, scope: &Scope) -> Result<BTreeSet<String>, Failure> {
        let page = self
            .activities
            .list(
                scope,
                &Filter {
                    state: State::All,
                    page_size: 100,
                    ..Filter::default()
                },
            )
            .await?;
        Ok(page.entries.into_iter().map(|a| a.title).collect())
    }
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_RECORD_REACH").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_RECORD_REACH=disposable on a freshly migrated disposable DB".into(),
        );
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    // Lee Viewer (2) is the reader, Sam Staff (3) shares a team with them,
    // Ada Manager (1) does not. Each has a desk in company 1.
    db.get().await?.batch_execute(
        "INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human'),(10,1,'sam_staff','Sam Staff','human'),(11,1,'lee_viewer','Lee Viewer','human') ON CONFLICT DO NOTHING; \
         UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; UPDATE users SET person_id=10 WHERE tenant_id=1 AND id=3; UPDATE users SET person_id=11 WHERE tenant_id=1 AND id=2; \
         INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,10,1),(1,11,1) ON CONFLICT DO NOTHING; \
         SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:43237")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let admin = login(&app, "admin@example.test").await?;
    let manager = login(&app, "manager@example.test").await?;
    let viewer = login(&app, "viewer@example.test").await?;
    let proof = Proof {
        db: db.clone(),
        boundary: app
            .services
            .get::<MutationBoundary>("base/database.mutations")?,
        grants: grants.clone(),
        teams: app.services.get::<Teams>(bilibi_core::team::SERVICE)?,
        activities: app
            .services
            .get::<Activities>(bilibi_core::activity::SERVICE)?,
    };

    // A team of Lee and Sam, created through Core Team (its members reach
    // Authz through `members::apply`).
    let mut client = db.get().await?;
    let mut tx = proof
        .boundary
        .begin(
            &mut client,
            admin.mutation_context("walkthrough.record-reach"),
        )
        .await?;
    let team = proof
        .teams
        .create(
            &mut tx,
            &admin,
            &TeamInput {
                name: "Reach crew".into(),
                leads: vec![2],
                ..TeamInput::default()
            },
        )
        .await?
        .map_err(|e| format!("team: {e:?}"))?
        .id;
    tx.commit().await?;
    drop(client);
    proof
        .members(&admin, team, MemberChange::Add(vec![3]))
        .await?;

    db.get().await?.execute(
        "INSERT INTO base_grid_ref_tasks(tenant_id,name,owner_user_id,team_id,hours) VALUES \
         (1,'Lee task',2,NULL,1),(1,'Sam task',3,NULL,2),(1,'Ada team task',1,$1,4),(1,'Ada task',1,NULL,8),(1,'Alex task',4,NULL,16),(2,'Other tenant task',5,NULL,32)",
        &[&team],
    ).await?;

    // The reader's role: reference tasks with reach "mine".
    let readers = proof.role(&admin, "Task readers").await?;
    proof
        .set_reach(&admin, readers, TASKS, Some(Reach::Own))
        .await?
        .map_err(|e| format!("grant own: {e}"))?;
    grants.assign_role(&admin, 2, readers, true).await?;

    let lee = set(&["Lee task"]);
    prove(
        "mine: every Grid door (row page, projection, summary, search) shows only the reader's own task",
        proof.tasks(&viewer).await? == lee,
    )?;
    let catalog = bilibi_grid::Catalog::for_scope(&**db.get().await?, &viewer).await?;
    prove(
        "the reader's catalog says the table reaches mine",
        catalog.table("reference_tasks").and_then(|t| t.reach()) == Some(Reach::Own),
    )?;

    proof
        .set_reach(&admin, readers, TASKS, Some(Reach::Team))
        .await?
        .map_err(|e| format!("grant team: {e}"))?;
    prove(
        "my team's: the reader also sees a teammate's task and a task filed under their team, never another owner's",
        proof.tasks(&viewer).await? == set(&["Lee task", "Sam task", "Ada team task"]),
    )?;

    proof
        .members(&admin, team, MemberChange::Remove(vec![3]))
        .await?;
    prove(
        "leaving the team applies at the next read: Sam's task is gone",
        proof.tasks(&viewer).await? == set(&["Lee task", "Ada team task"]),
    )?;
    proof
        .members(&admin, team, MemberChange::Add(vec![3]))
        .await?;

    proof
        .set_reach(&admin, readers, TASKS, Some(Reach::All))
        .await?
        .map_err(|e| format!("grant all: {e}"))?;
    prove(
        "all: every task of the reader's tenant, never another tenant's",
        proof.tasks(&viewer).await?
            == set(&[
                "Lee task",
                "Sam task",
                "Ada team task",
                "Ada task",
                "Alex task",
            ]),
    )?;

    // The team's role: Lee reaches what the team can see, unless restricted
    // on the user.
    grants.assign_role(&admin, 2, readers, false).await?;
    let team_readers = proof.role(&admin, "Team task readers").await?;
    proof
        .set_reach(&admin, team_readers, TASKS, Some(Reach::Team))
        .await?
        .map_err(|e| format!("grant team role: {e}"))?;
    proof.team_role(&admin, team, team_readers, true).await?;
    let team_tasks = set(&["Lee task", "Sam task", "Ada team task"]);
    prove(
        "a team role gives its member my team's through every Grid door",
        proof.tasks(&viewer).await? == team_tasks,
    )?;
    proof
        .direct(&admin, Some(Direct::Allow(Reach::Own)))
        .await?;
    prove(
        "a direct allow reaching mine on the user caps the team role's my team's through every Grid door",
        proof.tasks(&viewer).await? == lee,
    )?;
    let catalog = bilibi_grid::Catalog::for_scope(&**db.get().await?, &viewer).await?;
    prove(
        "the capped reader's catalog says the table reaches mine",
        catalog.table("reference_tasks").and_then(|t| t.reach()) == Some(Reach::Own),
    )?;
    proof.direct(&admin, None).await?;
    prove(
        "clearing the user's direct allow restores the team's reach",
        proof.tasks(&viewer).await? == team_tasks,
    )?;
    proof.direct(&admin, Some(Direct::Deny)).await?;
    let denied = bilibi_grid::page_keys(
        &db,
        &viewer,
        "reference_tasks",
        &PageSpec {
            filters: Vec::new(),
            sorts: Vec::new(),
            timezone: "UTC".into(),
            offset: 0,
            limit: 10,
            bounds: Bounds::default(),
        },
    )
    .await;
    prove(
        "a direct deny on the user removes the table though the team role gives my team's",
        denied.is_err(),
    )?;
    proof.direct(&admin, None).await?;
    prove(
        "a capability whose owner did not opt in cannot be allowed directly with less than all",
        grants
            .set_direct(
                &admin,
                2,
                "grid.reference.view",
                Some(Direct::Allow(Reach::Own)),
            )
            .await
            .is_err_and(|why| matches!(why, Error::Refused(ref text) if text.contains("always reaches all"))),
    )?;
    proof.team_role(&admin, team, team_readers, false).await?;
    grants.assign_role(&admin, 2, readers, true).await?;

    grants
        .set_direct(&admin, 2, TASKS, Some(Direct::Deny))
        .await?;
    let denied = bilibi_grid::page_keys(
        &db,
        &viewer,
        "reference_tasks",
        &PageSpec {
            filters: Vec::new(),
            sorts: Vec::new(),
            timezone: "UTC".into(),
            offset: 0,
            limit: 10,
            bounds: Bounds::default(),
        },
    )
    .await;
    prove(
        "a direct deny still wins over a role's reach: the table is gone",
        denied.is_err(),
    )?;
    grants.set_direct(&admin, 2, TASKS, None).await?;

    // Escalation: Ada may administer roles but holds tasks only as "mine".
    let ada_role = proof.role(&admin, "Role clerks").await?;
    for key in ["admin.authz.role.update", "admin.authz.role.list"] {
        proof
            .set_reach(&admin, ada_role, key, Some(Reach::All))
            .await?
            .map_err(|e| format!("grant {key}: {e}"))?;
    }
    proof
        .set_reach(&admin, ada_role, TASKS, Some(Reach::Own))
        .await?
        .map_err(|e| format!("grant ada own: {e}"))?;
    grants.assign_role(&admin, 1, ada_role, true).await?;
    let scratch = proof.role(&admin, "Scratch").await?;
    prove(
        "an actor holding mine cannot give a role all",
        proof
            .set_reach(&manager, scratch, TASKS, Some(Reach::All))
            .await?
            == Err("forbidden".into()),
    )?;
    prove(
        "an actor holding mine cannot give a role my team's",
        proof
            .set_reach(&manager, scratch, TASKS, Some(Reach::Team))
            .await?
            == Err("forbidden".into()),
    )?;
    prove(
        "an actor holding mine can give mine",
        proof
            .set_reach(&manager, scratch, TASKS, Some(Reach::Own))
            .await?
            .is_ok(),
    )?;
    for reach in [Reach::All, Reach::Team] {
        prove(
            &format!(
                "an actor holding mine cannot give a direct allow reaching {}",
                reach.code()
            ),
            matches!(
                grants
                    .set_direct(&manager, 3, TASKS, Some(Direct::Allow(reach)))
                    .await,
                Err(Error::Forbidden)
            ),
        )?;
    }
    prove(
        "an actor holding mine can give a direct allow reaching mine",
        grants
            .set_direct(&manager, 3, TASKS, Some(Direct::Allow(Reach::Own)))
            .await
            .is_ok(),
    )?;
    grants.set_direct(&admin, 3, TASKS, None).await?;
    proof
        .set_reach(&admin, scratch, TASKS, Some(Reach::Team))
        .await?
        .map_err(|e| format!("scratch team: {e}"))?;
    prove(
        "an actor holding mine cannot assign a role that gives my team's",
        matches!(
            grants.assign_role(&manager, 3, scratch, true).await,
            Err(Error::Forbidden)
        ),
    )?;
    prove(
        "a capability whose owner did not opt in cannot be limited",
        proof
            .set_reach(&admin, scratch, "grid.reference.view", Some(Reach::Own))
            .await?
            .is_err_and(|why| why.contains("always reaches all")),
    )?;

    // Clearing a direct rule is a widening: the actor must hold the key as
    // far as the account reaches once it is gone, whatever the rule was.
    grants.assign_role(&admin, 2, readers, false).await?;
    proof.team_role(&admin, team, team_readers, true).await?;
    proof
        .direct(&admin, Some(Direct::Allow(Reach::Own)))
        .await?;
    prove(
        "an actor holding mine cannot clear the user's cap of mine when the team role gives my team's",
        matches!(
            grants.set_direct(&manager, 2, TASKS, None).await,
            Err(Error::Forbidden)
        ),
    )?;
    prove(
        "the refused clear leaves the user at mine",
        proof.tasks(&viewer).await? == lee,
    )?;
    prove(
        "an actor holding mine may deny the capped user",
        grants
            .set_direct(&manager, 2, TASKS, Some(Direct::Deny))
            .await
            .is_ok(),
    )?;
    prove(
        "but clearing that deny is refused: the user would reach my team's",
        matches!(
            grants.set_direct(&manager, 2, TASKS, None).await,
            Err(Error::Forbidden)
        ),
    )?;
    proof
        .direct(&admin, Some(Direct::Allow(Reach::Own)))
        .await?;
    grants.assign_role(&admin, 1, scratch, true).await?;
    grants
        .set_direct(&admin, 1, TASKS, Some(Direct::Allow(Reach::Own)))
        .await?;
    prove(
        "an actor capped at mine cannot clear their own cap",
        matches!(
            grants.set_direct(&manager, 1, TASKS, None).await,
            Err(Error::Forbidden)
        ),
    )?;
    prove(
        "an actor capped at mine may deny themselves",
        grants
            .set_direct(&manager, 1, TASKS, Some(Direct::Deny))
            .await
            .is_ok(),
    )?;
    prove(
        "but cannot lift their own cap by clearing that deny",
        matches!(
            grants.set_direct(&manager, 1, TASKS, None).await,
            Err(Error::Forbidden)
        ),
    )?;
    grants.set_direct(&admin, 1, TASKS, None).await?;
    grants.assign_role(&admin, 1, scratch, false).await?;
    proof.direct(&admin, None).await?;
    prove(
        "an actor holding all clears the user's cap, and the user is back at my team's",
        proof.tasks(&viewer).await? == team_tasks,
    )?;
    proof.team_role(&admin, team, team_readers, false).await?;
    grants.assign_role(&admin, 2, readers, true).await?;

    // Core Activity: the oversight read and edit follow reach on the assignee.
    db.get().await?.batch_execute(
        "INSERT INTO core_activities(tenant_id,company_id,kind,subject,status,priority,assignee_user_id,created_by) VALUES \
         (1,1,'task','Sam call list','not_started','medium',3,3),(1,1,'task','Ada call list','not_started','medium',1,1)",
    ).await?;
    let ids: Vec<i64> = db
        .get()
        .await?
        .query(
            "SELECT id FROM core_activities WHERE tenant_id=1 AND subject IN ('Sam call list','Ada call list') ORDER BY subject DESC",
            &[],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let (sam_activity, ada_activity) = (ids[0], ids[1]);
    let overseers = proof.role(&admin, "Activity overseers").await?;
    for (key, reach) in [
        (ACTIVITY_VIEW, Reach::All),
        (ACTIVITY_MANAGE, Reach::All),
        (ACTIVITY_ALL_VIEW, Reach::Team),
        (ACTIVITY_ALL_MANAGE, Reach::Team),
    ] {
        proof
            .set_reach(&admin, overseers, key, Some(reach))
            .await?
            .map_err(|e| format!("grant {key}: {e}"))?;
    }
    grants.assign_role(&admin, 2, overseers, true).await?;
    let titles = proof.activity_titles(&viewer).await?;
    prove(
        "oversight with my team's: the list shows a teammate's private activity and not another person's",
        titles.contains("Sam call list") && !titles.contains("Ada call list"),
    )?;
    prove(
        "oversight with my team's: opening another person's activity is refused",
        matches!(
            proof.activities.get(&viewer, ada_activity).await,
            Err(Error::Forbidden)
        ),
    )?;
    prove(
        "my team's edit: the reader may close a teammate's activity",
        proof.close(&viewer, sam_activity).await?.is_ok(),
    )?;
    proof
        .set_reach(&admin, overseers, ACTIVITY_ALL_VIEW, Some(Reach::All))
        .await?
        .map_err(|e| format!("view all: {e}"))?;
    prove(
        "oversight with all: the reader now sees everyone's activity",
        proof.activities.get(&viewer, ada_activity).await.is_ok(),
    )?;
    prove(
        "but edit reaching my team's still refuses another person's activity in the write",
        proof.close(&viewer, ada_activity).await? == Err("forbidden".into()),
    )?;
    println!("record reach: every check held");
    Ok(())
}
