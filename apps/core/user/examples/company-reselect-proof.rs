//! Proof for #958 against an explicitly disposable, migrated database:
//! choosing the company that is already current succeeds with the answer a
//! real switch gives, writes no audit action and leaves the session as it was,
//! while a company the account cannot use is still refused. A second session
//! may also move to the desk the account already remembers, and picking a
//! fallback desk that is current but not saved saves it. Exits non-zero
//! when a check fails. Never point it at a shared database.
//!
//! ```sh
//! BILIBI_USER_DEMO=disposable cargo run -p bilibi-user --example company-reselect-proof --locked
//! ```
use bilibi_audit::RequestInfo;
use bilibi_user::{ChoiceRefusal, Switched};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Error = Box<dyn std::error::Error>;

#[derive(Default)]
struct Checks {
    failed: usize,
}
impl Checks {
    fn check(&mut self, label: &str, passed: bool, detail: impl std::fmt::Debug) {
        if passed {
            println!("PASS {label}");
        } else {
            self.failed += 1;
            println!("FAIL {label}: {detail:?}");
        }
    }
}

/// How many `event` actions the account has in the trail.
async fn actions(client: &tokio_postgres::Client, user: i64, event: &str) -> Result<i64, Error> {
    Ok(client
        .query_one(
            "SELECT count(*) FROM base_audit_actions WHERE tenant_id=1 AND actor_id=$1 AND event=$2",
            &[&user, &event],
        )
        .await?
        .get(0))
}

/// What a switch may change: the session's company, parked company and CSRF
/// secret, and the account's remembered desk.
async fn state(
    client: &tokio_postgres::Client,
    token: &str,
    user: i64,
) -> Result<(Option<i64>, Option<i64>, String, Option<i64>), Error> {
    let session = client
        .query_one(
            "SELECT company_id,held_company_id,csrf FROM sessions WHERE id=$1",
            &[&bilibi_session::session_id(token)],
        )
        .await?;
    let desk: Option<i64> = client
        .query_one(
            "SELECT last_desk_id FROM users WHERE tenant_id=1 AND id=$1",
            &[&user],
        )
        .await?
        .get(0);
    Ok((session.get(0), session.get(1), session.get(2), desk))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    if std::env::var("BILIBI_USER_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_USER_DEMO=disposable for this walkthrough".into());
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35575")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let users = &app.user;
    let request = RequestInfo::default();
    let client = db
        .get_detached("the walkthrough's own checking connection")
        .await?;
    let client: &tokio_postgres::Client = &client;
    let mut checks = Checks::default();

    // The manager works at desks 1 and 2. An existing desk is a write that
    // changes nothing, which MutationBoundary refuses as invalid.
    let first = users
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let manager = app.security.guard(&first.token).await?;
    let user = manager.user();
    let person = manager
        .actor()
        .person()
        .ok_or("fixture manager person missing")?
        .get();
    for company in [1, 2] {
        match users.set_desk(&manager, company, person, true).await {
            Ok(_) | Err(bilibi_database::Error::Invalid) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let first = users
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let current = app
        .security
        .guard(&first.token)
        .await?
        .current_company()
        .map(|id| id.get())
        .ok_or("the manager has no current desk")?;
    let other = if current == 1 { 2 } else { 1 };

    // A real switch, for the answer a re-selection must match.
    let switched_before = actions(client, user, "user.company.switched").await?;
    let switch = users
        .choose_company(&first.token, &first.csrf, Some(other), &request)
        .await?;
    checks.check(
        "a real switch answers with the chosen company",
        switch
            == Switched {
                company: Some(other),
            },
        switch,
    );
    checks.check(
        "a real switch records user.company.switched",
        actions(client, user, "user.company.switched").await? == switched_before + 1,
        "no switched action",
    );

    // Choosing the current company again, by id and by an empty choice.
    let switched = actions(client, user, "user.company.switched").await?;
    let before = state(client, &first.token, user).await?;
    let again = users
        .choose_company(&first.token, &first.csrf, Some(other), &request)
        .await;
    checks.check(
        "choosing the current company again succeeds like the switch",
        matches!(again, Ok(answer) if answer == switch),
        &again,
    );
    let twice = users
        .choose_company(&first.token, &first.csrf, Some(other), &request)
        .await;
    checks.check(
        "a second repeat (double click) succeeds too",
        matches!(twice, Ok(answer) if answer == switch),
        &twice,
    );
    let empty = users
        .choose_company(&first.token, &first.csrf, None, &request)
        .await;
    checks.check(
        "an empty choice keeps the current company and succeeds",
        matches!(empty, Ok(answer) if answer == switch),
        &empty,
    );
    let after = state(client, &first.token, user).await?;
    checks.check(
        "the repeats leave the session and remembered desk unchanged",
        before == after,
        (&before, &after),
    );
    let repeats = actions(client, user, "user.company.switched").await?;
    checks.check(
        "the repeats write no audit action",
        repeats == switched,
        (switched, repeats),
    );

    // A second session starts at the remembered desk; the first moves on, so
    // the account remembers `current`. The second may still move there.
    let second = users
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    users
        .choose_company(&first.token, &first.csrf, Some(current), &request)
        .await?;
    let switched = actions(client, user, "user.company.switched").await?;
    let moved = users
        .choose_company(&second.token, &second.csrf, Some(current), &request)
        .await;
    checks.check(
        "a session may move to the desk the account already remembers",
        matches!(moved, Ok(Switched { company: Some(id) }) if id == current),
        &moved,
    );
    let (company, _, _, desk) = state(client, &second.token, user).await?;
    checks.check(
        "that move switches the session and records the switch",
        company == Some(current)
            && desk == Some(current)
            && actions(client, user, "user.company.switched").await? == switched + 1,
        (company, desk),
    );

    // A company the account cannot use is still refused, and nothing moves.
    let before = state(client, &first.token, user).await?;
    let refused = actions(client, user, "user.company.switch_refused").await?;
    for company in [62, 100] {
        let answer = users
            .choose_company(&first.token, &first.csrf, Some(company), &request)
            .await;
        checks.check(
            &format!("company {company} without a desk is refused"),
            matches!(answer, Err(ChoiceRefusal::Unavailable)),
            &answer,
        );
    }
    let after = state(client, &first.token, user).await?;
    checks.check(
        "the refusals leave the session unchanged",
        before == after,
        (&before, &after),
    );
    checks.check(
        "each refusal records user.company.switch_refused",
        actions(client, user, "user.company.switch_refused").await? == refused + 2,
        "missing refusal actions",
    );

    // An account that never saved a desk signs in at its fallback desk; a
    // session that lost its stored choice works there too. Picking that desk
    // is a real switch that saves it, so the explicit choice is not lost.
    client
        .execute(
            "UPDATE users SET last_desk_id=NULL WHERE tenant_id=1 AND id=$1",
            &[&user],
        )
        .await?;
    let fresh = users
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    client
        .execute(
            "UPDATE sessions SET company_id=NULL WHERE id=$1",
            &[&bilibi_session::session_id(&fresh.token)],
        )
        .await?;
    let fallback = app
        .security
        .guard(&fresh.token)
        .await?
        .current_company()
        .map(|id| id.get())
        .ok_or("the manager has no fallback desk")?;
    let switched = actions(client, user, "user.company.switched").await?;
    let picked = users
        .choose_company(&fresh.token, &fresh.csrf, Some(fallback), &request)
        .await;
    checks.check(
        "picking the unsaved fallback desk succeeds",
        matches!(picked, Ok(Switched { company: Some(id) }) if id == fallback),
        &picked,
    );
    let (company, _, _, desk) = state(client, &fresh.token, user).await?;
    checks.check(
        "that pick saves the session choice and remembered desk and records the switch",
        company == Some(fallback)
            && desk == Some(fallback)
            && actions(client, user, "user.company.switched").await? == switched + 1,
        (company, desk),
    );

    if checks.failed > 0 {
        return Err(format!("{} check(s) failed", checks.failed).into());
    }
    Ok(())
}
