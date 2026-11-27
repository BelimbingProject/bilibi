//! Running-database proof that a team's roles are grants (#681): members hold
//! the team's roles while they are in it, through Authz; giving a team a role
//! and adding someone to a team that gives roles pass role-update
//! permission, the subset rule and step-up; a team never gives a full-access
//! role; a team with a company gives its roles there only; leaving or losing
//! the role applies at the next check; a direct deny wins; and the role
//! display and the decision log say "via team <name>". Explicit disposable DB
//! with the walkthrough fixture only; fails the executable when a check does
//! not hold.
use bilibi_authz::{
    grants::Grants, CapabilityDefinition as C, RoleDefinition, VocabularyContribution, VOCABULARY,
};
use bilibi_database::{pool, Db, Error, MutationBoundary};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_team::{MemberChange, TeamInput, Teams, SERVICE};
use bilibi_tenancy::Scope;
use serde_json::json;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

/// A company-scoped key and a tenant-only key, each in its own role.
const RECORD: &str = "teamproof.record.view";
const REPORT: &str = "teamproof.report.view";
inventory::submit! { Contribution::new(VOCABULARY, "team-roles.proof", Provenance { module: "core/team", source: file!(), line: line!() }, &VocabularyContribution {
    domains: &["teamproof"], verbs: &["view"],
    capabilities: &[C::company(RECORD), C::tenant(REPORT)],
    roles: &[
        RoleDefinition { code: "team_proof_records", name: "Line records", description: "Disposable proof", grant_all: false, capabilities: &[RECORD] },
        RoleDefinition { code: "team_proof_reports", name: "Line reports", description: "Disposable proof", grant_all: false, capabilities: &[REPORT] },
    ],
    system_principals: &[],
}) }

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
    boundary: std::sync::Arc<MutationBoundary>,
    teams: std::sync::Arc<Teams>,
    parties: std::sync::Arc<bilibi_tenancy::PartyProofs>,
}
type Refusal = Result<(), String>;
/// Runs one team write in its own audited transaction: committed when the
/// owner accepts it, rolled back on a field refusal or a step-up refusal.
macro_rules! in_tx {
    ($proof:expr, $scope:expr, |$tx:ident| $body:expr) => {{
        let mut client = $proof.db.get().await?;
        let mut $tx = $proof
            .boundary
            .begin(
                &mut client,
                $scope.mutation_context("walkthrough.team-roles"),
            )
            .await?;
        let result: bilibi_database::Result<Refusal> = async { $body }.await;
        match result {
            Ok(Ok(())) => {
                $tx.commit().await?;
                Ok(Ok(()))
            }
            Ok(Err(refusal)) => Ok(Err(refusal)),
            Err(Error::StepUpRequired { operation, reason }) => {
                Ok(Err(format!("step-up {operation} {reason}")))
            }
            Err(error) => Err(Failure::from(error)),
        }
    }};
}
impl Proof {
    async fn give(
        &self,
        scope: &Scope,
        team: i64,
        role: i64,
        attach: bool,
    ) -> Result<Refusal, Failure> {
        in_tx!(self, scope, |tx| Ok(self
            .teams
            .change_role(&mut tx, scope, team, role, attach)
            .await?
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))))
    }
    async fn members(
        &self,
        scope: &Scope,
        team: i64,
        change: MemberChange,
    ) -> Result<Refusal, Failure> {
        in_tx!(self, scope, |tx| Ok(self
            .teams
            .change_members(&mut tx, scope, team, &change)
            .await?
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))))
    }
    async fn update(
        &self,
        scope: &Scope,
        team: i64,
        patch: serde_json::Value,
    ) -> Result<Refusal, Failure> {
        let version = self
            .teams
            .get(&**self.db.get().await?, scope, team)
            .await?
            .version;
        in_tx!(self, scope, |tx| Ok(self
            .teams
            .update(&mut tx, scope, team, version, &patch)
            .await?
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))))
    }
    async fn create(
        &self,
        scope: &Scope,
        name: &str,
        company: Option<i64>,
        lead: i64,
    ) -> Result<i64, Failure> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                scope.mutation_context("walkthrough.team-roles"),
            )
            .await?;
        let team = self
            .teams
            .create(
                &mut tx,
                scope,
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
        Ok(team.id)
    }
    async fn allowed(
        &self,
        scope: &Scope,
        key: &str,
        company: Option<i64>,
    ) -> Result<(bool, &'static str), Failure> {
        let client = self.db.get().await?;
        let decision = bilibi_authz::decide_in_company(&**client, scope, key, company).await?;
        Ok((decision.allowed, decision.reason))
    }
    /// What the decision log page shows `reader` as the source of `user`'s
    /// latest allow of `key`.
    async fn last_via(
        &self,
        reader: &Scope,
        user: i64,
        key: &str,
    ) -> Result<Option<String>, Failure> {
        let client = self.db.get().await?;
        let reach: Option<Vec<i64>> = Some(
            self.parties
                .reachable_organisations(&**client, reader)
                .await?
                .into_iter()
                .map(|company| company.id.get())
                .collect(),
        );
        let query = bilibi_authz::decision_logs::Query {
            search: key.into(),
            allowed: Some(true),
            sort: bilibi_authz::decision_logs::Sort::OccurredAt,
            descending: true,
            page: 1,
            page_size: 50,
        };
        let page =
            bilibi_authz::decision_logs::list(&**client, reader, &query, reach.as_deref()).await?;
        Ok(page
            .entries
            .into_iter()
            .find(|log| log.actor_id == user && log.capability == key)
            .and_then(|log| log.via))
    }
    async fn instance_role(&self, role: &str) -> Result<(), Failure> {
        Ok(bilibi_walkthrough_fixture::set_instance_role(&self.db, role).await?)
    }
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_TEAM_ROLES").as_deref() != Ok("disposable") {
        return Err("set BILIBI_TEAM_ROLES=disposable on a freshly migrated disposable DB".into());
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    // The admin (grant-all) works in companies 1 and 2; the manager works
    // in company 1 only (the fixture's desk); Sam Staff in both.
    db.get().await?.batch_execute(
        "INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human'),(10,1,'sam_staff','Sam Staff','human'),(11,1,'lee_viewer','Lee Viewer','human') ON CONFLICT DO NOTHING; \
         UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; UPDATE users SET person_id=10 WHERE tenant_id=1 AND id=3; UPDATE users SET person_id=11 WHERE tenant_id=1 AND id=2; \
         INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,4,2),(1,10,1),(1,10,2),(1,11,1),(1,11,2) ON CONFLICT DO NOTHING; \
         SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:43233")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    // The manager manages teams and may change roles, but holds neither
    // proof key: the case the subset rule exists for.
    db.get()
        .await?
        .batch_execute(
            "INSERT INTO role_capabilities(tenant_id,role_id,capability) VALUES(1,1,'teams.view'),(1,1,'teams.manage'),(1,1,'admin.authz.role.update'),(1,2,'teams.view') ON CONFLICT DO NOTHING",
        )
        .await?;
    let role = |code: &'static str| {
        let db = db.clone();
        async move {
            Ok::<i64, Failure>(
                db.get()
                    .await?
                    .query_one(
                        "SELECT id FROM roles WHERE tenant_id=1 AND code=$1",
                        &[&code],
                    )
                    .await?
                    .get(0),
            )
        }
    };
    let (records, reports, full) = (
        role("team_proof_records").await?,
        role("team_proof_reports").await?,
        role("admin").await?,
    );
    let admin = login(&app, "admin@example.test").await?;
    let manager = login(&app, "manager@example.test").await?;
    let staff = login(&app, "staff@example.test").await?;
    let viewer = login(&app, "viewer@example.test").await?;
    let proof = Proof {
        db: db.clone(),
        boundary: app
            .services
            .get::<MutationBoundary>("base/database.mutations")?,
        teams: app.services.get::<Teams>(SERVICE)?,
        parties: app.services.get("base/tenancy.parties")?,
    };

    // A tenant-wide team led by Sam Staff. Ada Manager manages teams but is
    // not in this one: a member would hold its roles and could pass them on,
    // as the subset rule allows anyone holding a role's permissions.
    let line = proof.create(&admin, "Line 2 crew", None, 3).await?;
    prove(
        "before the team gives a role, Sam Staff does not hold it",
        !proof.allowed(&staff, REPORT, None).await?.0,
    )?;

    let refused = proof.give(&admin, line, full, true).await?;
    prove(
        "a team never carries a full-access role, even from a full-access administrator",
        refused.as_ref().is_err_and(|e| e.contains("full access")),
    )?;
    let refused = proof.give(&manager, line, reports, true).await?;
    prove(
        "a team manager who does not hold a role's permissions cannot give the team that role",
        refused
            .as_ref()
            .is_err_and(|e| e.contains("every permission the role gives")),
    )?;

    // Step-up: outside a development instance the grant asks for a second
    // factor (nobody here enrolled one), and nothing is written.
    proof.instance_role("staging").await?;
    let refused = proof.give(&admin, line, reports, true).await?;
    proof.instance_role("development").await?;
    prove(
        "giving a team a role asks for step-up (base.authz.principal_role.set)",
        refused == Err("step-up base.authz.principal_role.set not_enrolled".into()),
    )?;
    prove(
        "the refused step-up wrote nothing",
        proof
            .teams
            .roles(&**db.get().await?, &admin, line)
            .await?
            .is_empty(),
    )?;

    proof
        .give(&admin, line, reports, true)
        .await?
        .map_err(|e| format!("give reports: {e}"))?;
    let (allowed, reason) = proof.allowed(&staff, REPORT, None).await?;
    prove(
        "a member holds the team's role at the next check",
        allowed && reason == "group_role_capability",
    )?;
    prove(
        "the decision log says where the role came from",
        proof.last_via(&admin, 3, REPORT).await?.as_deref() == Some("team Line 2 crew"),
    )?;
    let held = bilibi_authz::roles::group_assignments(&**db.get().await?, &admin, 3).await?;
    prove(
        "the account's role display says the role is via team Line 2 crew",
        held.iter().any(|h| {
            h.role.id == reports
                && h.source == "team"
                && h.label == "Line 2 crew"
                && h.company_id.is_none()
        }),
    )?;

    // Adding someone to a team that gives roles is a grant too.
    let refused = proof
        .members(&manager, line, MemberChange::Add(vec![2]))
        .await?;
    prove(
        "a team manager without the role cannot add a member to a team that gives it",
        refused
            .as_ref()
            .is_err_and(|e| e.contains("Adding people to it needs permission")),
    )?;
    prove(
        "the refused addition gave Lee Viewer nothing",
        !proof.allowed(&viewer, REPORT, None).await?.0,
    )?;
    proof.instance_role("staging").await?;
    let refused = proof
        .members(&admin, line, MemberChange::Add(vec![2]))
        .await?;
    proof.instance_role("development").await?;
    prove(
        "adding a member to a team that gives roles asks for step-up",
        refused == Err("step-up base.authz.principal_role.set not_enrolled".into()),
    )?;
    proof
        .members(&admin, line, MemberChange::Add(vec![2]))
        .await?
        .map_err(|e| format!("add Lee: {e}"))?;
    prove(
        "a new member holds the team's role at once",
        proof.allowed(&viewer, REPORT, None).await?.0,
    )?;
    // Removing only narrows: the same manager may always do it.
    proof
        .members(&manager, line, MemberChange::Remove(vec![2]))
        .await?
        .map_err(|e| format!("remove Lee: {e}"))?;
    prove(
        "leaving the team removes the role at the next check, with no job and no sign-out",
        !proof.allowed(&viewer, REPORT, None).await?.0,
    )?;

    // A direct deny wins over the team's role.
    grants
        .set_direct(&admin, 3, REPORT, Some(bilibi_authz::grants::Direct::Deny))
        .await?;
    let (allowed, reason) = proof.allowed(&staff, REPORT, None).await?;
    prove(
        "a direct deny wins over a role the team gives",
        !allowed && reason == "denied_explicitly",
    )?;
    grants.set_direct(&admin, 3, REPORT, None).await?;
    prove(
        "clearing the deny restores the team's role",
        proof.allowed(&staff, REPORT, None).await?.0,
    )?;

    // A rename changes the name the source shows.
    proof
        .update(&admin, line, json!({"name":"Line 2 day crew"}))
        .await?
        .map_err(|e| format!("rename: {e}"))?;
    proof.allowed(&staff, REPORT, None).await?;
    prove(
        "after a rename the decision log names the team by its new name",
        proof.last_via(&admin, 3, REPORT).await?.as_deref() == Some("team Line 2 day crew"),
    )?;
    proof
        .give(&admin, line, reports, false)
        .await?
        .map_err(|e| format!("take reports: {e}"))?;
    prove(
        "taking the role away from the team removes it at the next check",
        !proof.allowed(&staff, REPORT, None).await?.0,
    )?;

    // A team with a company gives its roles in that company only (ADR 20).
    let crew = proof.create(&admin, "Company 1 crew", Some(1), 3).await?;
    let refused = proof.give(&admin, crew, reports, true).await?;
    prove(
        "a team with a company refuses a role with tenant-wide permissions",
        refused
            .as_ref()
            .is_err_and(|e| e.contains("apply across the organisation")),
    )?;
    proof
        .give(&admin, crew, records, true)
        .await?
        .map_err(|e| format!("give records: {e}"))?;
    prove(
        "the team's role holds in the team's company",
        proof.allowed(&staff, RECORD, Some(1)).await?.0,
    )?;
    prove(
        "the team's role does not hold in another company the member works in",
        !proof.allowed(&staff, RECORD, Some(2)).await?.0,
    )?;
    proof
        .update(&admin, crew, json!({"company_id":2}))
        .await?
        .map_err(|e| format!("move crew: {e}"))?;
    prove(
        "moving the team moves its role to the new company at the next check",
        proof.allowed(&staff, RECORD, Some(2)).await?.0
            && !proof.allowed(&staff, RECORD, Some(1)).await?.0,
    )?;

    // Step 1's visibility rule: a hidden team's roles stay hidden with it.
    prove(
        "a reader with no desk in the team's company cannot read its roles",
        matches!(
            proof.teams.roles(&**db.get().await?, &manager, crew).await,
            Err(Error::NotFound)
        ),
    )?;
    prove(
        "a write to that hidden team answers NotFound, so it reveals nothing",
        matches!(
            proof
                .teams
                .plan_role(&**db.get().await?, &manager, crew, records, false, false)
                .await,
            Err(Error::NotFound)
        ),
    )?;
    proof.allowed(&staff, RECORD, Some(2)).await?;
    prove(
        "a log reader who works in the team's company sees its name",
        proof.last_via(&admin, 3, RECORD).await?.as_deref() == Some("team Company 1 crew"),
    )?;
    let hidden = proof.last_via(&manager, 3, RECORD).await?;
    prove(
        "a log reader without a desk in the team's company does not see its name",
        hidden.as_deref() == Some("a team in a company you do not work in"),
    )?;
    let held = bilibi_authz::roles::group_assignments(&**db.get().await?, &admin, 3).await?;
    prove(
        "the display lists the company team's role with its company",
        held.iter()
            .any(|h| h.role.id == records && h.company_id == Some(2)),
    )?;
    println!("Team roles are grants, follow live membership, and say where they came from.");
    Ok(())
}
