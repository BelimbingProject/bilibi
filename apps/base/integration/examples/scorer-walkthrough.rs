//! Fresh-database proof for a reviewed customer-hosted scorer contract.
//!
//! Route a public HTTPS endpoint with a trusted certificate to the receiver
//! on 127.0.0.1:18464, preserving the request body and headers. Run:
//!
//! ```text
//! BILIBI_DISPOSABLE_SCORER_DEMO=yes BILIBI_SETTINGS_KEY=<64-hex-character-key> \
//! BILIBI_SCORER_FIXTURE_URL=https://.../score \
//! DATABASE_URL=postgres://... cargo run -p bilibi-integration --example scorer-walkthrough
//! ```
use axum::{body::Bytes, extract::State, http::HeaderMap, routing::post, Json, Router};
use bilibi_authz::{
    grants::Grants, SystemAuthentication, SystemPrincipalDefinition, SystemScope,
    VocabularyContribution, VOCABULARY,
};
use bilibi_base::picklists::{self, NewValue, PickLists};
use bilibi_database::{pool, Db, Error, MutationBoundary, Result};
use bilibi_integration::{
    Integration, Origin, ScoreDecision, ScoreEvent, ScoreFailure, Scorer, ScorerResult,
    CONNECTOR_DESTINATIONS, SCORERS, SERVICE,
};
use bilibi_module_registry::{Contribution, Provenance, Services};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, Settings, Target, ValueType};
use bilibi_tenancy::{Authentication, Scope};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Map, Value};
use sha2::Sha256;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

const CONSENT: &str = "walkthrough.scorer.consent";
const DESTINATION: &str = "walkthrough.scorer.destination";
const SECRET: &str = "walkthrough.scorer.secret";
const SCORER: &str = "walkthrough.case-score";
const PRINCIPAL: &str = "scorer-walkthrough";
const QUEUE_LIST: &str = "core.team.kind";
const ACTION: &str = "base.picklists.value.create";
const EVENT: &str = "walkthrough.case.opened";
const SIGNING_SECRET: &str = "0123456789abcdef0123456789abcdef";

const PROVENANCE: Provenance = Provenance {
    module: "base/integration",
    source: file!(),
    line: line!(),
};

inventory::submit! { Contribution::new(VOCABULARY, PRINCIPAL, PROVENANCE, &VocabularyContribution {
    domains: &[], verbs: &[], capabilities: &[], roles: &[],
    system_principals: &[SystemPrincipalDefinition { name: PRINCIPAL, capabilities: &[picklists::MANAGE] }],
}) }

inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, CONSENT, PROVENANCE, &Setting::Definition(Definition {
    value_type: ValueType::Boolean, scopes: &[ScopeKind::Global, ScopeKind::Tenant], default: || json!(true), nullable: false, encrypted: false,
    minimum: None, maximum: None, validator: None,
    editor: Some(Editor { group: "Walkthrough scorer", label: "Send case events", help: "Send the approved case fields to the configured scorer.", capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, DESTINATION, PROVENANCE, &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: &[ScopeKind::Tenant], default: || Value::Null, nullable: true, encrypted: false,
    minimum: None, maximum: None, validator: None,
    editor: Some(Editor { group: "Walkthrough scorer", label: "Scorer destination", help: "The admitted public HTTPS address that scores case events.", capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, SECRET, PROVENANCE, &Setting::Definition(Definition {
    value_type: ValueType::String, scopes: &[ScopeKind::Tenant], default: || Value::Null, nullable: true, encrypted: true,
    minimum: None, maximum: None, validator: None,
    editor: Some(Editor { group: "Walkthrough scorer", label: "Scorer signing secret", help: "Signs each scorer delivery; changing or clearing it revokes in-flight answers.", capability: None, reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(SCORERS, SCORER, PROVENANCE, &Scorer {
    event: EVENT,
    fields: &["amount", "priority"],
    consent_setting: CONSENT,
    destination_setting: DESTINATION,
    secret_setting: SECRET,
    timeout_ms: 10_000,
    expires_seconds: 30,
    action: ACTION,
    results: &[
        ScorerResult { value: "priority", input: r#"{"code":"priority","label":"Priority review"}"# },
        ScorerResult { value: "standard", input: r#"{"code":"standard","label":"Standard review"}"# },
    ],
}) }

#[derive(Default)]
struct Receiver {
    received: Notify,
    release: Notify,
}

async fn receive(
    State(receiver): State<Arc<Receiver>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Json<Value> {
    let body: Value = serde_json::from_slice(&bytes).expect("scorer JSON envelope");
    let id = headers["bilibi-delivery"]
        .to_str()
        .expect("delivery header");
    let opaque = id.strip_prefix("sd_").expect("opaque delivery prefix");
    assert_eq!(opaque.len(), 32);
    assert!(opaque
        .bytes()
        .all(|byte| b"0123456789abcdef".contains(&byte)));
    assert_eq!(headers["content-type"], "application/json");
    let expiry_header = headers["bilibi-expires"].to_str().expect("expiry header");
    let expiry = expiry_header.parse::<u64>().expect("Unix seconds");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Unix clock");
    assert!(Duration::from_secs(expiry) > now);
    assert!(Duration::from_secs(expiry) <= now + Duration::from_secs(30));
    let priority = body["fields"]["priority"].as_str().expect("priority field");
    let result = match priority {
        "urgent" => "priority",
        "normal" | "unexpected" => "standard",
        _ => panic!("unexpected priority"),
    };
    assert_eq!(
        body,
        json!({"delivery_id": id, "expires_at": expiry, "event": "walkthrough.case.opened",
               "fields": {"amount": "1500.00", "priority": priority}})
    );
    let signature = headers["bilibi-signature"]
        .to_str()
        .expect("signature header")
        .strip_prefix("v1=")
        .expect("signature version");
    let digest = data_encoding::HEXLOWER
        .decode(signature.as_bytes())
        .expect("hex signature");
    let signed = [expiry_header.as_bytes(), b".", bytes.as_ref()].concat();
    let mut verifier =
        Hmac::<Sha256>::new_from_slice(SIGNING_SECRET.as_bytes()).expect("fixture key");
    verifier.update(&signed);
    verifier.verify_slice(&digest).expect("request signature");
    if priority == "unexpected" {
        return Json(json!({"delivery_id": id, "result": result, "input": {}}));
    }
    if priority == "normal" {
        receiver.received.notify_one();
        receiver.release.notified().await;
    }
    Json(json!({"delivery_id": id, "result": result}))
}

#[derive(Debug, thiserror::Error)]
enum ConnectorError {
    #[error(transparent)]
    Score(#[from] ScoreFailure),
    #[error(transparent)]
    Action(#[from] Error),
}

struct Connector {
    integration: Arc<Integration>,
    owner: Arc<PickLists>,
    boundary: Arc<MutationBoundary>,
    authentication: Arc<Authentication>,
    db: Db,
}

impl Connector {
    async fn run(
        &self,
        scope: &Scope,
        key: &str,
        fields: Map<String, Value>,
    ) -> std::result::Result<ScoreDecision, ConnectorError> {
        let decision = self
            .integration
            .score(
                Origin::User(scope),
                scope,
                SCORER,
                ScoreEvent {
                    key,
                    name: EVENT,
                    fields,
                },
            )
            .await?;
        if decision.action != ACTION {
            return Err(Error::Invalid.into());
        }
        let input: NewValue =
            serde_json::from_value(decision.input.clone()).map_err(|_| Error::Invalid)?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let system = SystemScope::rehydrate(&**client, scope.tenant(), Some(PRINCIPAL)).await?;
        let actor = self.authentication.for_system(&**client, &system).await?;
        let mut context = actor.mutation_context(ACTION);
        context.trace_id.clone_from(&decision.delivery_id);
        let mut tx = self.boundary.begin(&mut client, context).await?;
        self.owner
            .create(&mut tx, &actor, QUEUE_LIST, &input)
            .await?
            .map_err(|_| Error::Invalid)?;
        tx.commit().await?;
        Ok(decision)
    }
}

async fn configure(
    boundary: &MutationBoundary,
    settings: &Settings,
    client: &mut tokio_postgres::Client,
    scope: &bilibi_tenancy::Scope,
    destination: &str,
) -> Result<()> {
    let mut tx = boundary
        .begin(
            client,
            scope.mutation_context("integration.scorer.walkthrough"),
        )
        .await?;
    settings
        .put(
            &mut tx,
            scope,
            Target::Global,
            CONNECTOR_DESTINATIONS,
            json!([destination]),
        )
        .await?;
    settings
        .put(
            &mut tx,
            scope,
            Target::Tenant,
            DESTINATION,
            json!(destination),
        )
        .await?;
    settings
        .put(
            &mut tx,
            scope,
            Target::Tenant,
            SECRET,
            json!(SIGNING_SECRET),
        )
        .await?;
    tx.commit().await
}

fn fields(priority: &str) -> Map<String, Value> {
    Map::from_iter([
        ("amount".into(), json!("1500.00")),
        ("priority".into(), json!(priority)),
    ])
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_SCORER_DEMO").as_deref() != Ok("yes") {
        return Err(bilibi_database::Error::Invalid);
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| bilibi_database::Error::Invalid)?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    let integration = services.get::<bilibi_integration::Integration>(SERVICE)?;
    let settings = services.get::<Settings>("base/settings.store")?;
    let boundary = services.get::<MutationBoundary>("base/database.mutations")?;
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    let mut client = db
        .get_detached("the scorer walkthrough's checking connection")
        .await?;
    let admin = authentication
        .authenticate(&**client, 1, 4, "scorer-walkthrough".into())
        .await?;

    let destination = std::env::var("BILIBI_SCORER_FIXTURE_URL").map_err(|_| Error::Invalid)?;
    let receiver = Arc::new(Receiver::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18464")
        .await
        .map_err(|_| Error::Internal)?;
    let app = Router::new()
        .route("/score", post(receive))
        .with_state(receiver.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.expect("fixture receiver") });

    let owner = services.get::<PickLists>(picklists::SERVICE)?;
    let grants = services.get::<Grants>("base/authz.grants")?;
    grants
        .set_system(&admin, PRINCIPAL, picklists::MANAGE, Some(true))
        .await?;
    configure(&boundary, &settings, &mut client, &admin, &destination).await?;
    let connector = Connector {
        integration,
        owner: owner.clone(),
        boundary: boundary.clone(),
        authentication,
        db: db.clone(),
    };
    let first_key = "case-opened-41";
    assert!(matches!(
        connector.run(&admin, first_key, fields("urgent")).await,
        Err(ConnectorError::Score(ScoreFailure::NotConfigured))
    ));
    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("integration.scorer.walkthrough"),
        )
        .await?;
    settings
        .put(&mut tx, &admin, Target::Global, CONSENT, json!(true))
        .await?;
    tx.commit().await?;
    assert!(matches!(
        connector.run(&admin, first_key, fields("urgent")).await,
        Err(ConnectorError::Score(ScoreFailure::NotConfigured))
    ));
    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("integration.scorer.walkthrough"),
        )
        .await?;
    settings
        .put(&mut tx, &admin, Target::Tenant, CONSENT, json!(true))
        .await?;
    tx.commit().await?;

    println!("== Approved fields and signed request drive the fixed owner action");
    let initial_values = owner.values(&**client, &admin, QUEUE_LIST, false).await?;
    assert!(initial_values
        .iter()
        .all(|value| &*value.code != "priority"));
    let decision = connector
        .run(&admin, first_key, fields("urgent"))
        .await
        .expect("signed scoring and owner action");
    let first_delivery = decision.delivery_id;
    assert_eq!(decision.result, "priority");
    let values = owner.values(&**client, &admin, QUEUE_LIST, false).await?;
    assert_eq!(values.len(), initial_values.len() + 1);
    let priority = values
        .iter()
        .find(|value| &*value.code == "priority")
        .expect("fixed owner action created the priority value");
    assert_eq!(&*priority.label, "Priority review");

    println!("== Deduplication withholds a second delivery and action");
    assert!(matches!(
        connector.run(&admin, first_key, fields("urgent")).await,
        Err(ConnectorError::Score(ScoreFailure::Duplicate))
    ));

    println!("== An extra outbound field is refused");
    let mut extra = fields("urgent");
    extra.insert("private_note".into(), json!("must stay here"));
    assert!(matches!(
        connector.run(&admin, "case-opened-42", extra).await,
        Err(ConnectorError::Score(ScoreFailure::Invalid))
    ));

    println!("== An extra reply field is refused");
    assert!(matches!(
        connector
            .run(&admin, "case-opened-extra-reply", fields("unexpected"))
            .await,
        Err(ConnectorError::Score(ScoreFailure::InvalidReply))
    ));

    println!("== Clearing the tenant override revokes an in-flight valid reply");
    let delivery = connector.run(&admin, "case-opened-43", fields("normal"));
    tokio::pin!(delivery);
    tokio::select! {
        outcome = &mut delivery => panic!("delivery ended before revocation: {outcome:?}"),
        reached = tokio::time::timeout(Duration::from_secs(8), receiver.received.notified()) => {
            reached.expect("customer receiver saw and verified the request");
        }
    }
    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("integration.scorer.walkthrough"),
        )
        .await?;
    settings
        .delete(&mut tx, &admin, Target::Tenant, CONSENT)
        .await?;
    tx.commit().await?;
    receiver.release.notify_one();
    assert!(matches!(
        delivery.await,
        Err(ConnectorError::Score(ScoreFailure::Revoked))
    ));
    assert!(matches!(
        connector
            .run(&admin, "case-opened-44", fields("normal"))
            .await,
        Err(ConnectorError::Score(ScoreFailure::NotConfigured))
    ));
    assert_eq!(
        owner.values(&**client, &admin, QUEUE_LIST, false).await?,
        values
    );

    println!("== Receipts name the exchange and typed result without storing the payload");
    let receipt = client
        .query_one(
            "SELECT state,result,reason,exchange_id IS NOT NULL FROM base_integration_scorer_deliveries WHERE tenant_id=$1 AND delivery_id=$2",
            &[&admin.tenant(), &first_delivery],
        )
        .await?;
    assert_eq!(receipt.get::<_, &str>(0), "done");
    assert_eq!(receipt.get::<_, Option<&str>>(1), Some("priority"));
    assert_eq!(receipt.get::<_, Option<&str>>(2), None);
    assert!(receipt.get::<_, bool>(3));
    let counts = client
        .query_one(
            "SELECT count(*),count(request_body),count(request_headers) FROM base_integration_exchanges WHERE tenant_id=$1 AND correlation_id=$2",
            &[&admin.tenant(), &first_delivery],
        )
        .await?;
    assert_eq!(counts.get::<_, i64>(0), 1);
    assert_eq!(counts.get::<_, i64>(1), 0);
    assert_eq!(counts.get::<_, i64>(2), 0);
    println!("PASS: the HTTPS receiver verified the signed envelope, the approved owner action created its value, and duplicate, extra-field and revoked deliveries made no additional owner changes.");
    Ok(())
}
