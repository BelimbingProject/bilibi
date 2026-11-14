//! Running-database proof that a team never widens company access (ADR 19):
//! a reader with `teams.view` who has no desk in a team's company does not
//! see that team, its members, its company name or its people, anywhere a
//! team is read; a team without a company and a team of a company the reader
//! works in stay visible. Explicit disposable DB with the walkthrough fixture
//! only; fails the executable when a check does not hold.
use bilibi_authz::grants::Grants;
use bilibi_database::{pool, Error, MutationBoundary};
use bilibi_team::{MemberChange, TeamInput, TeamQuery, Teams, SERVICE};
use bilibi_tenancy::Scope;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

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

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_TEAM_READS").as_deref() != Ok("disposable") {
        return Err("set BILIBI_TEAM_READS=disposable on a freshly migrated disposable DB".into());
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    // The admin (grant-all) works in companies 1 and 2; the manager, a reader
    // here, works in company 1 only (the fixture's desk).
    db.get().await?.batch_execute(
        "INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
         UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
         INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,4,2) ON CONFLICT DO NOTHING; \
         SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:43232")?;
    app.services
        .get::<Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    db.get()
        .await?
        .batch_execute(
            "INSERT INTO role_capabilities(tenant_id,role_id,capability) VALUES(1,1,'teams.view'),(1,1,'teams.manage') ON CONFLICT DO NOTHING",
        )
        .await?;
    let admin = login(&app, "admin@example.test").await?;
    let reader = login(&app, "manager@example.test").await?;
    let teams = app.services.get::<Teams>(SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let mut client = db.get().await?;

    // Lee Viewer (user 2) leads the hidden team; Sam Staff (user 3) the others.
    let mut ids = Vec::new();
    for (name, company, lead) in [
        ("Open crew", None, 3),
        ("Company 1 crew", Some(1), 3),
        ("Company 2 crew", Some(2), 2),
    ] {
        let mut tx = boundary
            .begin(&mut client, admin.mutation_context("walkthrough.team"))
            .await?;
        let team = teams
            .create(
                &mut tx,
                &admin,
                &TeamInput {
                    name: name.into(),
                    company_id: company,
                    leads: vec![lead],
                    ..TeamInput::default()
                },
            )
            .await?
            .map_err(|errors| format!("create {name}: {errors:?}"))?;
        tx.commit().await?;
        ids.push(team.id);
    }
    let (open, mine, hidden) = (ids[0], ids[1], ids[2]);
    let mut tx = boundary
        .begin(&mut client, admin.mutation_context("walkthrough.team"))
        .await?;
    teams
        .change_members(&mut tx, &admin, hidden, &MemberChange::Add(vec![3]))
        .await?
        .map_err(|errors| format!("add member: {errors:?}"))?;
    tx.commit().await?;

    let query = TeamQuery {
        search: "",
        kind: "",
        parent: None,
        limit: 100,
        offset: 0,
    };
    let (all, all_total) = teams.list(&**client, &admin, &query).await?;
    prove(
        "a reader who works in company 2 sees all three teams",
        all_total == 3 && all.len() == 3,
    )?;
    let (rows, total) = teams.list(&**client, &reader, &query).await?;
    prove(
        "the list counts and pages only the visible teams",
        total == 2 && rows.len() == 2,
    )?;
    prove(
        "a team without a company and a team of the reader's company stay visible",
        rows.iter().any(|r| r.team.id == open) && rows.iter().any(|r| r.team.id == mine),
    )?;
    prove(
        "the hidden team, its company name and its people are not listed",
        rows.iter().all(|r| {
            r.team.id != hidden
                && r.company_name.as_deref() != Some("Company 002")
                && r.leads.iter().all(|name| name != "Lee Viewer")
        }),
    )?;
    let (found, found_total) = teams
        .list(
            &**client,
            &reader,
            &TeamQuery {
                search: "Company 2",
                ..query
            },
        )
        .await?;
    prove(
        "searching for the hidden team's name finds nothing",
        found.is_empty() && found_total == 0,
    )?;
    prove(
        "get answers NotFound for the hidden team, not Forbidden",
        matches!(
            teams.get(&**client, &reader, hidden).await,
            Err(Error::NotFound)
        ),
    )?;
    prove(
        "members answers NotFound for the hidden team",
        matches!(
            teams.members(&**client, &reader, hidden, 100, 0).await,
            Err(Error::NotFound)
        ),
    )?;
    prove(
        "leads, leads above and member ids answer NotFound for the hidden team",
        matches!(
            teams.leads(&**client, &reader, hidden).await,
            Err(Error::NotFound)
        ) && matches!(
            teams.leads_above(&**client, &reader, hidden).await,
            Err(Error::NotFound)
        ) && matches!(
            teams.member_ids(&**client, &reader, hidden).await,
            Err(Error::NotFound)
        ),
    )?;
    prove(
        "the visible teams read as before",
        teams.get(&**client, &reader, open).await?.id == open
            && teams.get(&**client, &reader, mine).await?.id == mine
            && teams.members(&**client, &reader, mine, 100, 0).await?.1 == 1
            && teams.get(&**client, &admin, hidden).await?.id == hidden,
    )?;
    prove(
        "a picker offers only the visible teams",
        teams
            .choices(&**client, &reader)
            .await?
            .iter()
            .map(|c| c.0)
            .eq([mine, open]),
    )?;
    prove(
        "a person's teams leave out the hidden team",
        teams
            .teams_of(&**client, &reader, 3)
            .await?
            .iter()
            .all(|m| m.team_id != hidden),
    )?;

    let version = teams.get(&**client, &admin, open).await?.version;
    let mut tx = boundary
        .begin(&mut client, admin.mutation_context("walkthrough.team"))
        .await?;
    teams
        .update(
            &mut tx,
            &admin,
            open,
            version,
            &serde_json::json!({"parent_id": hidden}),
        )
        .await?
        .map_err(|errors| format!("move under hidden team: {errors:?}"))?;
    tx.commit().await?;
    prove(
        "leads of a team above that the reader cannot see are not shown",
        teams
            .leads_above(&**client, &reader, open)
            .await?
            .is_empty()
            && !teams.leads_above(&**client, &admin, open).await?.is_empty(),
    )?;
    let team = teams.get(&**client, &reader, open).await?;
    prove(
        "a parent team the reader cannot see is not named",
        teams.names(&**client, &reader, &team).await?.0.is_none(),
    )?;
    prove(
        "a visible team whose parent is hidden reads with no parent id",
        team.parent_id.is_none()
            && teams
                .list(&**client, &reader, &query)
                .await?
                .0
                .iter()
                .all(|r| r.team.parent_id.is_none())
            && teams.get(&**client, &admin, open).await?.parent_id == Some(hidden),
    )?;
    let refused = |errors: &bilibi_team::FieldErrors| {
        errors
            .get("parent_id")
            .is_some_and(|m| m.iter().any(|m| m == "Choose a team from the list."))
    };
    let attempt = teams
        .preview_create(
            &**client,
            &reader,
            &TeamInput {
                name: "Under hidden".into(),
                parent_id: Some(hidden),
                leads: vec![3],
                ..TeamInput::default()
            },
        )
        .await?;
    prove(
        "a hidden team cannot be chosen as the parent of a new team",
        attempt.as_ref().is_err_and(refused),
    )?;
    let version = teams.get(&**client, &reader, mine).await?.version;
    let attempt = teams
        .preview_update(
            &**client,
            &reader,
            mine,
            version,
            &serde_json::json!({"parent_id": hidden}),
        )
        .await?;
    prove(
        "a hidden team cannot be chosen as the new parent of a team",
        attempt.as_ref().is_err_and(refused),
    )?;
    let version = teams.get(&**client, &reader, open).await?.version;
    let preview = teams
        .preview_update(
            &**client,
            &reader,
            open,
            version,
            &serde_json::json!({"name": "Company 2 crew"}),
        )
        .await?;
    prove(
        "a rename under a hidden parent does not answer for hidden siblings and hides the parent id",
        preview.as_ref().is_ok_and(|t| t.parent_id.is_none() && t.name == "Company 2 crew"),
    )?;
    let mut tx = boundary
        .begin(&mut client, reader.mutation_context("walkthrough.team"))
        .await?;
    let saved = teams
        .update(
            &mut tx,
            &reader,
            open,
            version,
            &serde_json::json!({"name": "Open crew renamed"}),
        )
        .await?
        .map_err(|errors| format!("rename under hidden parent: {errors:?}"))?;
    tx.commit().await?;
    prove(
        "a rename that keeps a hidden parent saves, returns no parent id and keeps the stored parent",
        saved.parent_id.is_none()
            && saved.name == "Open crew renamed"
            && teams.get(&**client, &admin, open).await?.parent_id == Some(hidden),
    )?;
    println!("Team reads stay within the companies the reader works in.");
    Ok(())
}
