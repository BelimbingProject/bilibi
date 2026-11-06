//! Proves that a company sign-in client secret crosses an agent review only in
//! memory: the durable review and audit omit it, while commit receives the
//! value entered after the review opens.
use bilibi_agent_api::{
    mcp::{self, Outcome, Session},
    plans::{PlanAuthority, PlanIdentity, Plans},
    Registry, Reviews,
};
use bilibi_core::agent::{AgentAccess, AgentClaims, Decision, OAuthError, CLIENT_ID};
use bilibi_database::pool;
use bilibi_tenancy::{ProofFuture, Scope};
use bilibi_user::IdentityConfiguration;
use serde_json::{json, Value};
use tokio_postgres::Transaction;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

inventory::submit! {bilibi_module_registry::Contribution::new(bilibi_authz::VOCABULARY,"walkthrough.sso-agent-review",bilibi_module_registry::Provenance{module:"base/authz",source:file!(),line:line!()},&bilibi_authz::VocabularyContribution{domains:&[],verbs:&[],capabilities:&[],roles:&[bilibi_authz::RoleDefinition{code:"manager",name:"manager",description:"Walkthrough fixture role",grant_all:false,capabilities:&["admin.user.sso.manage","admin.authz.role.update","admin.authz.role.list"]}],system_principals:&[]})}

const OPERATION: &str = "core.user.sso.configure";

struct AgentAuthority<'a> {
    access: &'a AgentAccess,
    claims: AgentClaims,
}

impl PlanAuthority for AgentAuthority<'_> {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            let scope = self
                .access
                .prove(tx, &self.claims, true, "sso-agent-review-walkthrough", None)
                .await
                .map_err(|error| match error {
                    OAuthError::Other(error) => error,
                    _ => bilibi_database::Error::Unauthorized,
                })?;
            Ok(PlanIdentity {
                scope,
                credential_binding: format!("agent-connection:{}", self.claims.connection),
            })
        })
    }
}

struct ReviewAgent<'a> {
    registry: &'a Registry,
    plans: &'a Plans,
    reviews: &'a Reviews,
    step_up: &'a bilibi_step_up::StepUp,
    db: &'a bilibi_database::Db,
    scope: &'a Scope,
    authority: &'a dyn PlanAuthority,
}

impl ReviewAgent<'_> {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        match mcp::dispatch(
            self.registry,
            self.plans,
            Some(Session {
                db: self.db,
                scope: self.scope,
                authority: self.authority,
                step_up: self.step_up,
                reviews: self.reviews,
            }),
            &message,
        )
        .await
        {
            Outcome::Reply(reply)
                if reply.get("error").is_none()
                    && reply.pointer("/result/isError") == Some(&Value::Bool(false)) =>
            {
                reply
                    .pointer("/result/structuredContent")
                    .cloned()
                    .ok_or_else(|| "tool returned no structured content".into())
            }
            Outcome::Reply(reply) => {
                let body = reply
                    .pointer("/result/structuredContent")
                    .and_then(Value::as_object);
                let error = body
                    .and_then(|body| body.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let reason = body
                    .and_then(|body| body.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("unspecified");
                Err(format!("{name} was refused ({error}/{reason})").into())
            }
            Outcome::Accepted => Err(format!("{name} returned no reply").into()),
            Outcome::Unauthorized => Err(format!("{name} lost its agent authority").into()),
            Outcome::Unavailable(_) => Err(format!("{name} could not reach storage").into()),
        }
    }
}

fn contains(value: &Value, secret: &str) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| contains(value, secret)),
        Value::Object(values) => values.values().any(|value| contains(value, secret)),
        Value::String(value) => value.contains(secret),
        _ => false,
    }
}

fn prove(label: &'static str, condition: bool) -> Result<(), Box<dyn std::error::Error>> {
    if condition {
        println!("PASS {label}");
        Ok(())
    } else {
        Err(label.into())
    }
}

fn authenticator_code(key: &str, offset: i64) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut secret = Vec::new();
    let (mut buffer, mut bits) = (0u64, 0u32);
    for character in key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
    {
        let value = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .find(character.to_ascii_uppercase())
            .expect("base32") as u64;
        buffer = buffer << 5 | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            secret.push((buffer >> bits) as u8);
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).expect("key");
    mac.update(&((now / 30 + offset) as u64).to_be_bytes());
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

async fn enroll_authenticator(
    app: &bilibi_web::App,
    scope: &Scope,
) -> Result<String, Box<dyn std::error::Error>> {
    let enrollment = app
        .user
        .begin_two_factor(scope, bilibi_walkthrough_fixture::PASSWORD)
        .await
        .map_err(|error| format!("begin 2FA: {error:?}"))?;
    app.user
        .confirm_two_factor(scope, &authenticator_code(&enrollment.key, -1))
        .await
        .map_err(|error| format!("confirm 2FA: {error:?}"))?;
    Ok(enrollment.key)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_SSO_AGENT_REVIEW_WALKTHROUGH").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_SSO_AGENT_REVIEW_WALKTHROUGH=disposable with a fresh migrated DATABASE_URL"
                .into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &std::env::var("APP_ORIGIN")?)?;
    let grants = app
        .services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let login = app
        .user
        .login("manager@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    prove("walkthrough login is admitted", !login.pending)?;
    let browser_scope = app.security.guard(&login.token).await?;
    let authenticator = enroll_authenticator(&app, &browser_scope).await?;
    let settings = app
        .services
        .get::<bilibi_settings::Settings>("base/settings.store")?;
    let access = app.services.get::<AgentAccess>("core/agent.grants")?;
    let device = access
        .start_device(CLIENT_ID, "SSO review walkthrough")
        .await?;
    access
        .decide(&browser_scope, &device.user_code, Decision::Approve)
        .await?;
    let connection = access
        .list_connections(&browser_scope)
        .await?
        .into_iter()
        .next()
        .ok_or("device approval created no agent connection")?;
    let claims = access.review_claims(&browser_scope, connection.id).await?;
    let agent_scope = access
        .seal(&claims, "sso-agent-review-walkthrough", None)
        .await?;
    let authority = AgentAuthority {
        access: &access,
        claims,
    };
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let reviews = app.services.get::<Reviews>("base/agent_api.reviews")?;
    let agent = ReviewAgent {
        registry: &registry,
        plans: &plans,
        reviews: &reviews,
        step_up: &app.step_up,
        db: &db,
        scope: &agent_scope,
        authority: &authority,
    };

    prove(
        "client_secret is write-only in the operation schema",
        registry.operations[OPERATION].input["properties"]["client_secret"]["writeOnly"] == true,
    )?;

    let draft_secret = format!("draft-{}", uuid::Uuid::new_v4().simple());
    let entered_secret = format!("entered-{}", uuid::Uuid::new_v4().simple());
    let configuration = serde_json::to_string(&IdentityConfiguration {
        label: "Agent review walkthrough".into(),
        ..Default::default()
    })?;
    let draft = json!({
        "company_id": 1,
        "configuration": configuration,
        "client_secret": draft_secret,
    });
    let opened = agent
        .call(
            "review_entries",
            json!({"key": OPERATION, "entries": [draft]}),
        )
        .await?;
    prove(
        "the in-memory draft reaches the editor",
        opened
            .pointer("/entries/0/client_secret")
            .and_then(Value::as_str)
            == Some(draft_secret.as_str()),
    )?;
    let review_id = opened["review_id"]
        .as_str()
        .ok_or("review did not return an id")?;
    let client = db.get().await?;
    let stored = client
        .query_one(
            "SELECT card,original_operations FROM base_agent_api_reviews WHERE id=$1",
            &[&review_id],
        )
        .await?;
    let stored_card: Value = stored.get(0);
    let stored_operations: Value = stored.get(1);
    prove(
        "stored review card omits the draft secret",
        !contains(&stored_card, &draft_secret),
    )?;
    prove(
        "stored review operations omit the draft secret",
        !contains(&stored_operations, &draft_secret),
    )?;
    drop(client);

    let entered = json!({
        "company_id": 1,
        "configuration": configuration,
        "client_secret": entered_secret,
    });
    let preview = agent
        .call(
            "plan",
            json!({"operations": [{"key": OPERATION, "input": entered.clone()}]}),
        )
        .await?;
    prove(
        "freshly entered input passes the dry run",
        preview["valid"] == true,
    )?;
    let committed = agent
        .call(
            "commit",
            json!({
                "operations": [{"key": OPERATION, "input": entered}],
                "step_up_code": format!(
                    "core.user.authenticator:{}",
                    authenticator_code(&authenticator, 0)
                ),
            }),
        )
        .await?;
    prove(
        "commit output omits both secret values",
        !contains(&committed, &draft_secret) && !contains(&committed, &entered_secret),
    )?;

    let client = db.get().await?;
    let connection = client
        .query_one(
            "SELECT id,sealed_runtime FROM user_identity_providers WHERE tenant_id=$1 AND company_id=$2 ORDER BY id DESC LIMIT 1",
            &[&agent_scope.tenant(), &1_i64],
        )
        .await?;
    let connection_id: i64 = connection.get(0);
    let sealed: Value = connection.get(1);
    let opened_secret = settings.open_owned(
        "core/user.sso.connection",
        agent_scope.tenant(),
        connection_id,
        &sealed,
    )?;
    prove(
        "commit receives the freshly entered secret",
        opened_secret["client_secret"].as_str() == Some(entered_secret.as_str())
            && !contains(&opened_secret, &draft_secret),
    )?;

    let mutations = client
        .query(
            "SELECT old_values,new_values FROM base_audit_mutations WHERE trace_id='sso-agent-review-walkthrough' AND auditable_type='user.identity_provider' ORDER BY id",
            &[],
        )
        .await?;
    let actions = client
        .query(
            "SELECT payload FROM base_audit_actions WHERE trace_id='sso-agent-review-walkthrough' AND event='base.agent_api.write' ORDER BY id",
            &[],
        )
        .await?;
    prove(
        "agent commit writes mutation audit evidence",
        !mutations.is_empty(),
    )?;
    prove(
        "agent commit writes action audit evidence",
        !actions.is_empty(),
    )?;
    prove(
        "audit evidence omits both secret values",
        mutations.iter().all(|row| {
            let old: Option<Value> = row.get(0);
            let new: Option<Value> = row.get(1);
            old.as_ref().is_none_or(|value| {
                !contains(value, &draft_secret) && !contains(value, &entered_secret)
            }) && new.as_ref().is_none_or(|value| {
                !contains(value, &draft_secret) && !contains(value, &entered_secret)
            })
        }) && actions.iter().all(|row| {
            let payload: Option<Value> = row.get(0);
            payload.as_ref().is_none_or(|value| {
                !contains(value, &draft_secret) && !contains(value, &entered_secret)
            })
        }),
    )?;
    drop(client);

    let final_operations = json!([{
        "key": OPERATION,
        "input": {"company_id": 1, "configuration": configuration},
    }]);
    agent
        .call(
            "review_outcome",
            json!({
                "review_id": review_id,
                "status": "saved",
                "outcome": {"final_operations": final_operations, "results": committed["results"]},
            }),
        )
        .await?;
    let status = agent
        .call("review_status", json!({"review_id": review_id}))
        .await?;
    prove(
        "review status reports the saved outcome",
        status["status"] == "saved",
    )?;
    prove(
        "review status outcome omits both secret values",
        !contains(&status, &draft_secret) && !contains(&status, &entered_secret),
    )?;

    println!("Walkthrough complete; no secret value was printed.");
    Ok(())
}
