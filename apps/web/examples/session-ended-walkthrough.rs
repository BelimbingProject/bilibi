//! Running proof of the session-ended notice (`core/user.session_ended`), with
//! the reference subscriber `fixtures/containers/session_listener` mounted. On
//! a fresh disposable DATABASE_URL, run the two phases in two processes:
//!
//! ```text
//! cp -r fixtures/containers/session_listener apps/domains/ && python3 scripts/discover-modules.py
//! cargo run -p bilibi-web --example session-ended-walkthrough -- end      # 1. sessions end; nothing is delivered yet
//! cargo run -p bilibi-web --example session-ended-walkthrough -- deliver  # 2. a new process delivers every notice
//! rm -r apps/domains/session_listener && python3 scripts/discover-modules.py && git checkout -- Cargo.lock
//! ```
//!
//! Phase 1 ends sessions of the fixture's tenant-1 accounts by each reason (a
//! sign-out through Core User's logout, an operator's termination, idle
//! expiry through the sweep) and an operator in tenant 1 ending a tenant-2
//! session, printing `stands` and `signed_in` before and after each. It
//! checks that each ending recorded exactly one notice in the session's own
//! tenant, and that no notice's subject, key or payload holds a token, its
//! digest, the operator list's row key or a CSRF proof. No queue worker runs,
//! so every delivery is still pending when the process exits. Phase 2 is a
//! new process: it starts the queue worker, and the subscriber receives each
//! notice once, in the tenant it belongs to.
use bilibi_authz::grants::Grants;
use bilibi_base::events::{DELIVER, PRINCIPAL};
use bilibi_base::queue::{Config, Executor};
use bilibi_base::session::{session_id, Sessions};
use bilibi_database::{pool, Db, Error, MutationBoundary, Result};
use bilibi_module_registry::{verify_composition, Services};
use bilibi_tenancy::{Authentication, Scope};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const ENDED: &str = "core/user.session_ended";
const SUBSCRIBER: &str = "session_listener/presence.sessions";

struct Proof {
    db: Db,
    boundary: Arc<MutationBoundary>,
    sessions: Arc<Sessions>,
    users: Arc<bilibi_user::UserService>,
    /// Every credential this run handed out, to search the notices for.
    secrets: Vec<String>,
}
struct Signed {
    token: String,
    /// The stored row id: the token's digest.
    id: String,
    scope: Scope,
}
impl Proof {
    /// A browser session for the account, as sign-in stores it.
    async fn sign_in(&mut self, tenant: i64, user: i64) -> Result<Signed> {
        self.sign_in_as(tenant, user, None).await
    }
    /// With `impersonator`, the operator's session viewing `user`.
    async fn sign_in_as(
        &mut self,
        tenant: i64,
        user: i64,
        impersonator: Option<i64>,
    ) -> Result<Signed> {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let csrf = uuid::Uuid::new_v4().simple().to_string();
        let id = session_id(&token);
        let client = self.db.get().await?;
        client.execute(
            "INSERT INTO sessions(id,tenant_id,user_id,csrf,impersonator_id,expires_at) VALUES($1,$2,$3,$4,$5,now()+interval '24 hours')",
            &[&id, &tenant, &user, &csrf, &impersonator],
        ).await?;
        let scope = self.sessions.check(&**client, &token).await?;
        // The token, its stored digest, the row key the lists render.
        let key = Sha256::digest(id.as_bytes());
        self.secrets.push(token.clone());
        self.secrets.push(id.clone());
        self.secrets
            .push(key.iter().take(12).map(|b| format!("{b:02x}")).collect());
        self.secrets.push(csrf);
        Ok(Signed { token, id, scope })
    }
    async fn show(&self, label: &str, signed: &Signed, proof: &Scope, account: i64) -> Result<()> {
        let client = self.db.get().await?;
        println!(
            "   {label}: stands {}, account {account} signed in (tenant {}) {}",
            self.sessions.stands(&**client, &signed.scope).await?,
            proof.tenant(),
            self.sessions.signed_in(&**client, proof, account).await?,
        );
        Ok(())
    }
    async fn notices(&self, heading: &str) -> Result<()> {
        let client = self.db.get().await?;
        let rows = client
            .query(
                "SELECT tenant_id,subject,payload FROM base_events WHERE name=$1 ORDER BY id",
                &[&ENDED],
            )
            .await?;
        println!("-- {heading}: {} notice(s)", rows.len());
        for row in rows {
            println!(
                "   tenant {} {} {}",
                row.get::<_, i64>(0),
                row.get::<_, String>(1),
                row.get::<_, Value>(2)
            );
        }
        Ok(())
    }
    async fn terminate(&self, operator: &Signed, target: &Signed) -> Result<()> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                operator.scope.mutation_context("session.terminate"),
            )
            .await?;
        let outcome = self
            .sessions
            .terminate(&mut tx, &target.id, &operator.id)
            .await?;
        tx.commit().await?;
        println!("   operator termination: {outcome:?}");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    bilibi_base::link();
    bilibi_core::link();
    bilibi_domains::link();
    bilibi_extensions::link();
    let phase = std::env::args().nth(1).unwrap_or_default();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_module_registry::discover()?;
    verify_composition(
        &modules,
        bilibi_domains::MODULES,
        bilibi_extensions::MODULES,
    )?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    let grants = services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let events = services.get::<bilibi_base::events::Events>(bilibi_base::events::SERVICE)?;
    println!(
        "mounted subscribers: {:?}",
        events.subscribers().collect::<Vec<_>>()
    );
    let proof = Proof {
        db: db.clone(),
        boundary: services.get("base/database.mutations")?,
        sessions: services.get("base/session.sessions")?,
        users: services.get("core/user.accounts")?,
        secrets: Vec::new(),
    };
    match phase.as_str() {
        "end" => end(proof, &services, &grants).await,
        "deliver" => deliver(&proof, &services).await,
        _ => {
            println!("name a phase: end, then deliver");
            Err(Error::Invalid)
        }
    }
}

async fn end(mut proof: Proof, services: &Services, grants: &Grants) -> Result<()> {
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    let client = proof.db.get().await?;
    let other_admin = authentication
        .authenticate(&**client, 2, 5, "walkthrough".into())
        .await?;
    drop(client);
    // Declaration grants nothing: each tenant's administrator grants delivery
    // on Automation access (`Grants::set_system`), behind a step-up code this
    // walkthrough does not enrol, so it writes the grant row that page writes.
    for tenant in [1_i64, 2] {
        proof.db.get().await?.execute(
            "INSERT INTO authz_system_grants(tenant_id,principal,capability,is_allowed) VALUES($1,$2,$3,true) ON CONFLICT(tenant_id,principal,capability) DO UPDATE SET is_allowed=true",
            &[&tenant, &PRINCIPAL, &DELIVER],
        ).await?;
    }
    grants.reconcile().await?;
    let operator = proof.sign_in(1, 4).await?;

    println!("signed out: staff@example.test (account 3) signs out through Core User");
    let staff = proof.sign_in(1, 3).await?;
    proof.show("before", &staff, &staff.scope, 3).await?;
    proof
        .show("before, asked from tenant 2", &staff, &other_admin, 3)
        .await?;
    proof.users.logout(&staff.token, &staff.scope.csrf).await?;
    proof.show("after", &staff, &staff.scope, 3).await?;

    println!("terminated: an operator ends one of viewer@example.test's (account 2) two sessions");
    let first = proof.sign_in(1, 2).await?;
    let second = proof.sign_in(1, 2).await?;
    proof.terminate(&operator, &first).await?;
    proof
        .show("terminated session", &first, &first.scope, 2)
        .await?;
    proof
        .show("other session", &second, &first.scope, 2)
        .await?;

    println!("expired: the other session idles past the lifetime; the sweep removes it");
    proof
        .db
        .get()
        .await?
        .execute(
            "UPDATE sessions SET last_activity=now()-interval '1 day' WHERE id=$1",
            &[&second.id],
        )
        .await?;
    proof
        .show("idle, before the sweep", &second, &first.scope, 2)
        .await?;
    proof.notices("before the sweep").await?;
    let pruned = proof
        .sessions
        .prune_expired(&proof.db, &proof.boundary)
        .await?;
    println!("   sweep pruned {pruned} session(s)");

    println!("impersonated: ai@agent.my (account 6) signs out while viewing as contractor@example.test (account 7)");
    let viewing = proof.sign_in_as(1, 7, Some(6)).await?;
    proof.show("before", &viewing, &viewing.scope, 6).await?;
    proof
        .show("the viewed account", &viewing, &viewing.scope, 7)
        .await?;
    proof
        .users
        .logout(&viewing.token, &viewing.scope.csrf)
        .await?;
    proof.show("after", &viewing, &viewing.scope, 6).await?;
    proof
        .notices("an impersonated write may not enqueue: marked for the sweep")
        .await?;

    println!("another tenant: the tenant-1 operator ends manager@other.example.test's (tenant 2, account 5) session");
    let foreign = proof.sign_in(2, 5).await?;
    proof.show("before", &foreign, &other_admin, 5).await?;
    proof.terminate(&operator, &foreign).await?;
    proof.show("after", &foreign, &other_admin, 5).await?;
    proof
        .notices("marked, before the sweep (nothing in tenant 2 yet)")
        .await?;
    let pruned = proof
        .sessions
        .prune_expired(&proof.db, &proof.boundary)
        .await?;
    println!("   sweep pruned {pruned} session(s)");
    proof.notices("after every ending").await?;

    let client = proof.db.get().await?;
    let mut leaks = 0;
    for secret in &proof.secrets {
        leaks += client.query_one(
            "SELECT count(*) FROM base_events WHERE name=$1 AND strpos(subject||' '||event_key||' '||payload::text, $2)>0",
            &[&ENDED, secret],
        ).await?.get::<_, i64>(0);
    }
    println!(
        "credentials searched: {}, found in a notice: {leaks}",
        proof.secrets.len()
    );
    for row in client.query(
        "SELECT tenant_id,payload->>'account',payload->>'reason',count(*) FROM base_events WHERE name=$1 GROUP BY 1,2,3 ORDER BY 1,2,3",
        &[&ENDED],
    ).await? {
        println!(
            "   tenant {} account {} {}: {} notice(s)",
            row.get::<_, i64>(0),
            row.get::<_, String>(1),
            row.get::<_, String>(2),
            row.get::<_, i64>(3)
        );
    }
    let keys: Vec<String> = client
        .query(
            "SELECT DISTINCT jsonb_object_keys(payload) FROM base_events WHERE name=$1 ORDER BY 1",
            &[&ENDED],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    println!("payload keys: {keys:?}");
    let pending: i64 = client.query_one(
        "SELECT count(*) FROM base_events_deliveries WHERE subscriber=$1 AND delivered_at IS NULL",
        &[&SUBSCRIBER],
    ).await?.get(0);
    println!("no queue worker ran: {pending} delivery(ies) pending for {SUBSCRIBER}");
    Ok(())
}

async fn deliver(proof: &Proof, services: &Services) -> Result<()> {
    let executor = Executor::from_services(
        services,
        Config {
            concurrency: 4,
            poll_interval: Duration::from_millis(100),
            lease_seconds: 30,
            max_runtime: Duration::from_secs(60),
        },
    )?;
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(executor.run(shutdown));
    let mut pending: i64 = -1;
    for _ in 0..600 {
        pending = proof.db.get().await?.query_one(
            "SELECT count(*) FROM base_events_deliveries WHERE subscriber=$1 AND delivered_at IS NULL",
            &[&SUBSCRIBER],
        ).await?.get(0);
        if pending == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = stop.send(true);
    let _ = worker.await;
    println!("after the restart: {pending} delivery(ies) pending for {SUBSCRIBER}");
    let client = proof.db.get().await?;
    for row in client.query(
        "SELECT tenant_id,account,reason,sessions,still_signed_in,signed_in_now FROM session_listener_notices ORDER BY received_at,account",
        &[],
    ).await? {
        println!(
            "   subscriber received in tenant {}: account {} {} ({} session(s)), still signed in then {}, signed in when handled {}",
            row.get::<_, i64>(0),
            row.get::<_, i64>(1),
            row.get::<_, String>(2),
            row.get::<_, i64>(3),
            row.get::<_, bool>(4),
            row.get::<_, bool>(5)
        );
    }
    Ok(())
}
