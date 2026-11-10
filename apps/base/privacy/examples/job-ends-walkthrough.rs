//! Manual running-service walkthrough of a request whose background job ends
//! without finishing (Queue's ending hook, `base/queue.endings`): an access
//! request whose approver is denied before its store jobs run, and an erasure
//! whose store fails until its job runs out of attempts. Each becomes
//! `failed`, recorded by `System · privacy-monitor`; a person cannot mark a
//! request failed; an access export of the erasure's person is no longer held
//! back; another person runs each again and it completes. Explicit disposable
//! DB only (`seed` first, same variables as `requests-walkthrough`); prints
//! real outcomes for inspection. No assertions or test harness.
use bilibi_authz::grants::Grants;
use bilibi_base::workflow::{Engine, Request};
use bilibi_database::{pool, MutationBoundary};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_personal_data::Identifier;
use bilibi_privacy::{NewRequest, Privacy, RequestKind, RequestView, SERVICE};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[allow(dead_code)]
#[path = "support/erasure.rs"]
mod erasure_approval;

fn now_step() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        / 30
}
/// The authenticator code for `key` (base32) at the current 30-second step.
fn code(key: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut secret = Vec::new();
    let (mut buffer, mut bits) = (0u64, 0u32);
    for c in key.chars().filter(|c| c.is_ascii_alphanumeric()) {
        let value = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .find(c.to_ascii_uppercase())
            .expect("base32") as u64;
        buffer = buffer << 5 | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            secret.push((buffer >> bits) as u8);
        }
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).expect("key");
    mac.update(&(now_step() as u64).to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let at = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[at] & 0x7f,
        digest[at + 1],
        digest[at + 2],
        digest[at + 3],
    ]);
    format!("{:06}", binary % 1_000_000)
}

/// One signed-in account, its session token and its authenticator.
struct Account {
    scope: Scope,
    token: String,
    key: String,
    used: i64,
}
impl Account {
    async fn enroll(app: &bilibi_web::App, email: &str) -> Result<Self> {
        let login = app
            .user
            .login(email, bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        let scope = app.security.guard(&login.token).await?;
        let enrollment = app
            .user
            .begin_two_factor(&scope, bilibi_walkthrough_fixture::PASSWORD)
            .await
            .map_err(|e| format!("begin two-factor: {e:?}"))?;
        let mut account = Self {
            scope,
            token: login.token,
            key: enrollment.key,
            used: 0,
        };
        let first = account.code().await;
        app.user
            .confirm_two_factor(&account.scope, &first)
            .await
            .map_err(|e| format!("confirm two-factor: {e:?}"))?;
        Ok(account)
    }
    async fn code(&mut self) -> String {
        while now_step() <= self.used {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.used = now_step();
        code(&self.key)
    }
    async fn step_up(&mut self) -> Scope {
        let code = self.code().await;
        self.scope.clone().presenting_step_up(&code)
    }
}

// The browser-bound write authority the page uses: a write through the
// operation contract, as the Run again button and an agent reach it.
struct Browser<'a> {
    app: &'a bilibi_web::App,
    token: &'a str,
}
impl bilibi_base::agent_api::plans::PlanAuthority for Browser<'_> {
    fn verify<'a>(
        &'a self,
        tx: &'a tokio_postgres::Transaction<'_>,
    ) -> bilibi_tenancy::ProofFuture<'a, bilibi_base::agent_api::plans::PlanIdentity> {
        Box::pin(async move {
            tx.query_opt(
                "SELECT id FROM sessions WHERE id=$1 AND expires_at>clock_timestamp() FOR UPDATE",
                &[&self.token],
            )
            .await?
            .ok_or(bilibi_database::Error::Unauthorized)?;
            Ok(bilibi_base::agent_api::plans::PlanIdentity {
                scope: self.app.security.sessions.check(tx, self.token).await?,
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}
async fn write(app: &bilibi_web::App, account: &Account, key: &str, input: Value) -> String {
    use bilibi_base::agent_api::plans::{PlannedOperation, Plans};
    let plans = match app.services.get::<Plans>("base/agent_api.plans") {
        Ok(plans) => plans,
        Err(error) => return format!("refused — {error}"),
    };
    match plans
        .submit(
            &account.scope,
            &Browser {
                app,
                token: &account.token,
            },
            &PlannedOperation {
                key: key.into(),
                input,
            },
        )
        .await
    {
        Ok(answer) => format!("ok status={}", answer["result"]["status"]),
        Err(error) => format!("refused — {error:?}"),
    }
}

fn summary(request: &RequestView) {
    println!(
        "request {} {:?} status={} late={} export={} certificate={}",
        request.id,
        request.kind,
        request.status,
        request.late,
        request.export_artifact.is_some(),
        request.certificate_artifact.is_some()
    );
    for store in &request.stores {
        println!(
            "  store {:<32} {:<8} attempts={} error={:?}",
            store.store, store.state, store.attempts, store.last_error
        );
    }
}
/// Waits up to `seconds` while the request is `approved` or `running`.
async fn settle(
    privacy: &Privacy,
    client: &Client,
    scope: &Scope,
    id: i64,
    seconds: u64,
) -> Result<RequestView> {
    for _ in 0..seconds {
        let request = privacy.request(client, scope, id).await?;
        if !matches!(request.status.as_str(), "running" | "approved") {
            return Ok(request);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(privacy.request(client, scope, id).await?)
}
async fn rows(client: &Client, label: &str, sql: &str) -> Result<()> {
    for row in client.query(sql, &[]).await? {
        println!("{label}: {}", row.get::<_, String>(0));
    }
    Ok(())
}
/// The request's jobs as Queue holds them (whether its owner was told, once
/// the column exists).
async fn jobs(client: &Client, id: i64) -> Result<()> {
    let told: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='base_queue_jobs' AND column_name='ending')",
            &[],
        )
        .await?
        .get(0);
    rows(
        client,
        "  job",
        &format!(
            "SELECT worker_id||' '||coalesce(args->>'store','')||' state='||state||' attempt='||attempt||'/'||max_attempts||' code='||coalesce(failure_code,'-'){} \
             FROM base_queue_jobs WHERE args->>'request_id'='{id}' ORDER BY id",
            if told { "||' ending='||coalesce(ending,'-')" } else { "" }
        ),
    )
    .await
}
/// Who moved the request and to what, from Workflow history.
async fn history(client: &Client, id: i64) -> Result<()> {
    rows(
        client,
        "  history",
        &format!(
            "SELECT h.from_status||' -> '||h.status||' by '||h.actor_type||' '||h.actor_id FROM base_workflow_status_history h \
             JOIN base_workflow_bindings b ON b.id=h.binding_id WHERE b.subject_type='privacy.request' AND b.subject_id={id} ORDER BY h.id"
        ),
    )
    .await
}
async fn new_person(client: &Client, name: &str, email: &str) -> Result<PersonId> {
    let id: i64 = client
        .query_one(
            "INSERT INTO persons(tenant_id,code,name,kind,email) \
             SELECT 1, trim(both '_' from lower(regexp_replace($1,'[^A-Za-z0-9]+','_','g'))) || '_' || (coalesce(max(id),0)+1), $1, 'human', $2 FROM persons RETURNING id",
            &[&name, &email],
        )
        .await?
        .get(0);
    Ok(PersonId::new(id)?)
}
/// Receives and verifies a request as `receiver`.
async fn received(
    privacy: &Privacy,
    client: &mut Client,
    receiver: &Scope,
    kind: RequestKind,
    person: PersonId,
    email: &str,
) -> Result<RequestView> {
    let request = privacy
        .receive(
            client,
            receiver,
            NewRequest {
                kind,
                person,
                identifiers: vec![Identifier::Email(email.into())],
                company: OrganisationId::new(1)?,
            },
        )
        .await?;
    Ok(privacy
        .verify(
            client,
            receiver,
            request.id,
            request.version,
            "Identity checked at the counter",
            &[],
        )
        .await?)
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_PRIVACY_DEMO").as_deref() != Ok("yes") {
        return Err("explicit disposable DB opt-in required".into());
    }
    let root = std::env::var("BILIBI_PRIVACY_DEMO_ROOT")?;
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    if std::env::args().nth(1).as_deref() == Some("seed") {
        return Ok(bilibi_web::seed::production(&db).await?);
    }
    // ADR 19: the file handlers need explicit desks, not an unlinked admin.
    db.get().await?.batch_execute("INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,3,1),(1,4,2),(1,3,2) ON CONFLICT DO NOTHING; \
        SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35574")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let mut admin = Account::enroll(&app, "admin@example.test").await?;
    let second = Account::enroll(&app, "ai@agent.my").await?;
    let privacy = app.services.get::<Privacy>(SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let engine = app.services.get::<Engine>("base/workflow.engine")?;
    let mut client = db.get().await?;
    println!(
        "erasure approval rules published for company 1: policy {}",
        erasure_approval::publish_rules(&app, &mut client, &admin.scope, 1).await?
    );
    {
        let mut tx = boundary
            .begin(
                &mut client,
                admin
                    .scope
                    .mutation_context("privacy.walkthrough.configure"),
            )
            .await?;
        let chromium =
            std::env::var("BILIBI_PRIVACY_DEMO_CHROMIUM").unwrap_or_else(|_| "chromium".into());
        for (key, value) in [
            ("media.disks", json!({"local": root})),
            ("media.attachment_disk", json!("local")),
            ("artifacts.storage_root", json!(root)),
            ("pdf.chromium.binary", json!(chromium)),
        ] {
            settings
                .put(&mut tx, &admin.scope, Target::Global, key, value)
                .await?;
        }
        tx.commit().await?;
    }

    println!("== A: the approver loses the right to run it before its jobs run");
    let ada = new_person(&client, "Ada Denied", "ada-denied@example.test").await?;
    let access = received(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        ada,
        "ada-denied@example.test",
    )
    .await?;
    // No worker runs yet: approved now, run after the deny below.
    let access = privacy
        .approve(&mut client, &admin.scope, access.id, access.version)
        .await?;
    println!(
        "approved by user {} with no worker running",
        admin.scope.user()
    );
    let cy = new_person(&client, "Cy Stale", "cy-stale@example.test").await?;
    let stale = received(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        cy,
        "cy-stale@example.test",
    )
    .await?;
    let stale = privacy
        .approve(&mut client, &admin.scope, stale.id, stale.version)
        .await?;
    // Hold back the last store job's ending (disposable database only): it is
    // told only after the request has failed and run again.
    let held: i64 = client
        .query_one(
            &format!("UPDATE base_queue_jobs SET ending_after=clock_timestamp()+interval '1 hour' WHERE id=(SELECT max(id) FROM base_queue_jobs WHERE worker_id='base/privacy-request-store' AND args->>'request_id'='{}') RETURNING id", stale.id),
            &[],
        )
        .await?
        .get(0);
    println!(
        "request {} approved; the ending of job {held} is held back",
        stale.id
    );
    grants
        .set_direct(
            &second.scope,
            admin.scope.user(),
            bilibi_privacy::REQUEST_APPROVE,
            Some(false),
        )
        .await?;
    println!(
        "user {} directly denied {}",
        admin.scope.user(),
        bilibi_privacy::REQUEST_APPROVE
    );
    // Even granted the key, a person cannot take the monitor's edge.
    grants
        .set_direct(
            &admin.scope,
            second.scope.user(),
            bilibi_privacy::REQUEST_FAIL,
            Some(true),
        )
        .await?;
    match engine
        .transition(
            &second.scope,
            "privacy.request",
            access.id,
            "failed",
            Request {
                expected_version: Some(access.version),
                input: json!({"failure": "cancelled_by_hand"}),
                ..Request::default()
            },
        )
        .await
    {
        Ok(_) => println!(
            "user {} (granted {}) marks the running request failed: accepted",
            second.scope.user(),
            bilibi_privacy::REQUEST_FAIL
        ),
        Err(error) => println!(
            "user {} (granted {}) marks the running request failed: refused — {error}",
            second.scope.user(),
            bilibi_privacy::REQUEST_FAIL
        ),
    }
    grants
        .set_direct(
            &admin.scope,
            second.scope.user(),
            bilibi_privacy::REQUEST_FAIL,
            None,
        )
        .await?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));
    println!("worker started; waiting 20 s");
    tokio::time::sleep(Duration::from_secs(20)).await;
    summary(&privacy.request(&**client, &second.scope, access.id).await?);
    jobs(&client, access.id).await?;
    history(&client, access.id).await?;
    let current = privacy.request(&**client, &second.scope, access.id).await?;
    println!(
        "user {} runs it again: {}",
        second.scope.user(),
        write(
            &app,
            &second,
            "base.privacy.request.run_again",
            json!({"id": access.id, "version": current.version}),
        )
        .await
    );
    summary(&settle(&privacy, &client, &second.scope, access.id, 60).await?);
    history(&client, access.id).await?;
    grants
        .set_direct(
            &second.scope,
            admin.scope.user(),
            bilibi_privacy::REQUEST_APPROVE,
            None,
        )
        .await?;
    println!("user {} deny removed", admin.scope.user());

    println!("== C: an ending told late for a job of an earlier run");
    summary(&privacy.request(&**client, &second.scope, stale.id).await?);
    jobs(&client, stale.id).await?;
    let current = privacy.request(&**client, &second.scope, stale.id).await?;
    println!(
        "user {} runs it again: {}",
        second.scope.user(),
        write(
            &app,
            &second,
            "base.privacy.request.run_again",
            json!({"id": stale.id, "version": current.version}),
        )
        .await
    );
    client
        .execute(
            "UPDATE base_queue_jobs SET ending_after=clock_timestamp() WHERE id=$1",
            &[&held],
        )
        .await?;
    println!("the held ending of job {held} is released; waiting 20 s");
    tokio::time::sleep(Duration::from_secs(20)).await;
    jobs(&client, stale.id).await?;
    let after = settle(&privacy, &client, &second.scope, stale.id, 60).await?;
    summary(&after);
    println!(
        "the new run was {} by the old ending",
        if after.status == "failed" {
            "failed"
        } else {
            "not failed"
        }
    );
    history(&client, stale.id).await?;

    println!("== B: an erasure's store fails until its job runs out of attempts");
    let bea = new_person(&client, "Bea Stuck", "bea-stuck@example.test").await?;
    client
        .batch_execute(&format!(
            "CREATE OR REPLACE FUNCTION walkthrough_stuck() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id = {} THEN RAISE EXCEPTION 'walkthrough: the person store fails'; END IF; RETURN NEW; END $$; \
             DROP TRIGGER IF EXISTS walkthrough_stuck ON persons; \
             CREATE TRIGGER walkthrough_stuck BEFORE UPDATE ON persons FOR EACH ROW EXECUTE FUNCTION walkthrough_stuck();",
            bea.get()
        ))
        .await?;
    let erasure = received(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Erasure,
        bea,
        "bea-stuck@example.test",
    )
    .await?;
    let erasure = erasure_approval::decide(
        &app,
        &privacy,
        &mut client,
        &second.scope,
        &admin.step_up().await,
        &erasure,
        true,
    )
    .await?;
    println!("erasure {} approved: {}", erasure.id, erasure.status);
    // Twenty-five attempts take hours of backoff: wait for the first failed
    // attempt, then make the next one the last (disposable database only).
    for attempt in 0..60 {
        let failed: i64 = client
            .query_one(
                &format!("SELECT count(*) FROM base_queue_jobs WHERE worker_id='base/privacy-request-store' AND args->>'request_id'='{}' AND state='retryable'", erasure.id),
                &[],
            )
            .await?
            .get(0);
        if failed > 0 {
            break;
        }
        if attempt == 59 {
            return Err("the person store did not fail".into());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    client
        .execute(
            &format!("UPDATE base_queue_jobs SET max_attempts=attempt+1,available_at=clock_timestamp() WHERE worker_id='base/privacy-request-store' AND args->>'request_id'='{}' AND state='retryable'", erasure.id),
            &[],
        )
        .await?;
    println!("next attempt is the last; waiting 20 s");
    tokio::time::sleep(Duration::from_secs(20)).await;
    summary(
        &privacy
            .request(&**client, &second.scope, erasure.id)
            .await?,
    );
    jobs(&client, erasure.id).await?;
    history(&client, erasure.id).await?;

    println!("== B: an access request for the same person meanwhile");
    let later = received(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        bea,
        "bea-stuck@example.test",
    )
    .await?;
    let later = privacy
        .approve(&mut client, &admin.scope, later.id, later.version)
        .await?;
    summary(&settle(&privacy, &client, &second.scope, later.id, 45).await?);

    println!("== B: the store is fixed and the erasure runs again");
    client
        .batch_execute(
            "DROP TRIGGER walkthrough_stuck ON persons; DROP FUNCTION walkthrough_stuck();",
        )
        .await?;
    let current = privacy
        .request(&**client, &second.scope, erasure.id)
        .await?;
    println!(
        "user {} runs it again: {}",
        second.scope.user(),
        write(
            &app,
            &second,
            "base.privacy.request.run_again",
            json!({"id": erasure.id, "version": current.version}),
        )
        .await
    );
    summary(&settle(&privacy, &client, &second.scope, erasure.id, 120).await?);
    history(&client, erasure.id).await?;
    rows(
        &client,
        "  person",
        &format!(
            "SELECT row_to_json(p)::text FROM (SELECT id,name,email,erased_at IS NOT NULL AS erased FROM persons WHERE id={}) p",
            bea.get()
        ),
    )
    .await?;
    rows(
        &client,
        "audit by the monitor",
        "SELECT auditable_type||' '||auditable_id||' '||event||' '||coalesce(new_values::text,'') FROM base_audit_mutations WHERE system_principal='privacy-monitor' ORDER BY id",
    )
    .await?;

    stop.send(true)?;
    workers.await??;
    Ok(())
}
