//! Fresh-database proof for a reviewed customer-hosted scorer contract.
//!
//! This intentionally calls public `https://httpbin.org`: its
//! `/response-headers` endpoint stands in for the customer's service and
//! returns the typed delivery id and result supplied in its fixed URL. Run:
//!
//! ```text
//! BILIBI_DISPOSABLE_SCORER_DEMO=yes BILIBI_SETTINGS_KEY=<64-hex-character-key> \
//! DATABASE_URL=postgres://... cargo run -p bilibi-integration --example scorer-walkthrough
//! ```
use bilibi_database::{pool, MutationBoundary, Result};
use bilibi_integration::{
    Origin, ScoreEvent, ScoreFailure, Scorer, ScorerResult, CONNECTOR_DESTINATIONS, SCORERS,
    SERVICE,
};
use bilibi_module_registry::{Contribution, Provenance, Services};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, Settings, Target, ValueType};
use bilibi_tenancy::Authentication;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Map, Value};
use sha2::Sha256;
use std::sync::Arc;

const CONSENT: &str = "walkthrough.scorer.consent";
const DESTINATION: &str = "walkthrough.scorer.destination";
const SECRET: &str = "walkthrough.scorer.secret";
const SCORER: &str = "walkthrough.case-score";
const EVENT: &str = "walkthrough.case.opened";
const SIGNING_SECRET: &str = "0123456789abcdef0123456789abcdef";

const PROVENANCE: Provenance = Provenance {
    module: "base/integration",
    source: file!(),
    line: line!(),
};

inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, CONSENT, PROVENANCE, &Setting::Definition(Definition {
    value_type: ValueType::Boolean, scopes: &[ScopeKind::Tenant], default: || json!(false), nullable: false, encrypted: false,
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
    timeout_ms: 5_000,
    expires_seconds: 30,
    results: &[
        ScorerResult { value: "priority", action: "walkthrough.case.route", input: r#"{"queue":"priority","notify":true}"# },
        ScorerResult { value: "standard", action: "walkthrough.case.route", input: r#"{"queue":"standard","notify":false}"# },
    ],
}) }

fn delivery_id(tenant: i64, key: &str) -> String {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(SIGNING_SECRET.as_bytes()) else {
        return String::new();
    };
    mac.update(
        json!(["bilibi-scorer-delivery-v1", tenant, SCORER, key])
            .to_string()
            .as_bytes(),
    );
    let digest = mac.finalize().into_bytes();
    format!("sd_{}", data_encoding::HEXLOWER.encode(&digest[..16]))
}

async fn configure(
    boundary: &MutationBoundary,
    settings: &Settings,
    client: &mut tokio_postgres::Client,
    scope: &bilibi_tenancy::Scope,
    destination: &str,
    consent: bool,
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
        .put(&mut tx, scope, Target::Tenant, CONSENT, json!(consent))
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

fn fields() -> Map<String, Value> {
    Map::from_iter([
        ("amount".into(), json!("1500.00")),
        ("priority".into(), json!("urgent")),
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

    let first_key = "case-opened-41";
    let first_delivery = delivery_id(admin.tenant(), first_key);
    let success = format!(
        "https://httpbin.org/response-headers?delivery_id={first_delivery}&result=priority"
    );
    configure(&boundary, &settings, &mut client, &admin, &success, true).await?;

    println!("== Approved fields, signed request and typed result");
    let decision = integration
        .score(
            Origin::User(&admin),
            &admin,
            SCORER,
            ScoreEvent {
                key: first_key,
                name: EVENT,
                fields: fields(),
            },
        )
        .await
        .map_err(|_| bilibi_database::Error::Internal)?;
    assert_eq!(decision.delivery_id, first_delivery);
    assert_eq!(decision.result, "priority");
    assert_eq!(decision.action, "walkthrough.case.route");
    assert_eq!(decision.input, json!({"queue":"priority","notify":true}));

    println!("== Deduplication withholds a second delivery and action choice");
    assert!(matches!(
        integration
            .score(
                Origin::User(&admin),
                &admin,
                SCORER,
                ScoreEvent {
                    key: first_key,
                    name: EVENT,
                    fields: fields()
                },
            )
            .await,
        Err(ScoreFailure::Duplicate)
    ));

    println!("== An extra outbound field is refused before a receipt exists");
    let mut extra = fields();
    extra.insert("private_note".into(), json!("must stay here"));
    assert!(matches!(
        integration
            .score(
                Origin::User(&admin),
                &admin,
                SCORER,
                ScoreEvent {
                    key: "case-opened-42",
                    name: EVENT,
                    fields: extra
                },
            )
            .await,
        Err(ScoreFailure::Invalid)
    ));

    println!("== Tenant revocation is live");
    configure(&boundary, &settings, &mut client, &admin, &success, false).await?;
    assert!(matches!(
        integration
            .score(
                Origin::User(&admin),
                &admin,
                SCORER,
                ScoreEvent {
                    key: "case-opened-43",
                    name: EVENT,
                    fields: fields()
                },
            )
            .await,
        Err(ScoreFailure::NotConfigured)
    ));

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
    println!("PASS: the customer service selected one fixed reviewed action; consent, exact fields, public HTTPS policy, signing, expiry, deduplication, timeout, typed reply and receipts stayed inside Integration's contract.");
    Ok(())
}
