//! Customer-hosted scoring through Integration's one outbound HTTP door.
//!
//! A reviewed module contributes a [`Scorer`] whose event fields and result
//! choices are fixed in code. Tenant settings hold consent, destination and a
//! signing secret; the installation-wide destination policy is checked on
//! every delivery. The remote reply can select only one contributed result,
//! so it never supplies an operation name or operation input.
use crate::{Call, Failure, Integration, Origin, Outgoing};
use bilibi_module_registry::{Catalog, Composition, Consumer, ExtensionCode, RegistryError};
use bilibi_settings::{ScopeKind, Setting, Target, ValueType};
use bilibi_tenancy::Scope;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Registry of customer-hosted scorer contracts contributed by reviewed code.
pub const SCORERS: &str = "base/integration.scorers";
/// Exact public HTTPS destinations the installation owner admits.
pub const CONNECTOR_DESTINATIONS: &str = "integration.connector_destinations";

inventory::submit! { Consumer::new::<Scorer>(SCORERS, "base/integration", |entries| {
    entries.iter().try_for_each(|entry| entry.get::<Scorer>().map(|_| ()))
}) }

/// One result a customer-hosted scorer may choose.
pub struct ScorerResult {
    /// Stable reply value.
    pub value: &'static str,
    /// Fixed operation key the reviewed connector may invoke.
    pub action: &'static str,
    /// Fixed JSON object passed to that operation. The remote service cannot
    /// add to or replace it.
    pub input: &'static str,
}

/// A reviewed connector's fixed scoring contract.
pub struct Scorer {
    /// The one consented event name this scorer accepts.
    pub event: &'static str,
    /// Exact event field keys sent to the customer service.
    pub fields: &'static [&'static str],
    /// Tenant boolean setting. `true` is the tenant's live consent.
    pub consent_setting: &'static str,
    /// Tenant string setting containing the destination URL.
    pub destination_setting: &'static str,
    /// Tenant encrypted string setting containing the request-signing secret.
    pub secret_setting: &'static str,
    /// Whole request timeout. Bounded at registry load.
    pub timeout_ms: u64,
    /// How long a signed request and its answer remain valid.
    pub expires_seconds: u64,
    /// Typed result values and their fixed actions.
    pub results: &'static [ScorerResult],
}

#[derive(Clone)]
pub(crate) struct BoundResult {
    action: &'static str,
    input: Value,
}

#[derive(Clone)]
pub(crate) struct BoundScorer {
    pub(crate) id: &'static str,
    contract: &'static Scorer,
    results: BTreeMap<&'static str, BoundResult>,
}

pub(crate) struct Scorers(BTreeMap<&'static str, BoundScorer>);

impl Scorers {
    pub(crate) fn from_catalog(
        catalog: &Catalog,
        composition: &Composition,
    ) -> bilibi_module_registry::Result<Self> {
        let settings: BTreeMap<_, _> = catalog
            .entries::<Setting>(bilibi_settings::DEFINITIONS)?
            .iter()
            .map(|entry| Ok((entry.id, entry.get::<Setting>()?)))
            .collect::<bilibi_module_registry::Result<_>>()?;
        let customer_modules: BTreeSet<_> = composition
            .containers
            .iter()
            .filter(|container| container.extension_code == Some(ExtensionCode::Customer))
            .flat_map(|container| container.modules.iter().copied())
            .collect();
        let mut scorers = BTreeMap::new();
        for entry in catalog.entries::<Scorer>(SCORERS)? {
            let contract = entry.get::<Scorer>()?;
            let valid_id = !entry.id.is_empty() && entry.id.len() <= 200;
            let fields: BTreeSet<_> = contract.fields.iter().copied().collect();
            let mut results = BTreeMap::new();
            for result in contract.results {
                let input: Value = serde_json::from_str(result.input).map_err(|_| {
                    RegistryError::Contribution(format!(
                        "scorer {} result {} at {}:{} has invalid JSON input",
                        entry.id, result.value, entry.provenance.source, entry.provenance.line
                    ))
                })?;
                if result.value.is_empty()
                    || result.value.len() > 120
                    || result.action.is_empty()
                    || result.action.len() > 200
                    || !input.is_object()
                    || results
                        .insert(
                            result.value,
                            BoundResult {
                                action: result.action,
                                input,
                            },
                        )
                        .is_some()
                {
                    return Err(RegistryError::Contribution(format!(
                        "scorer {} at {}:{} has an invalid or duplicate result",
                        entry.id, entry.provenance.source, entry.provenance.line
                    )));
                }
            }
            if !valid_id
                || contract.event.is_empty()
                || contract.event.len() > 200
                || fields.len() != contract.fields.len()
                || fields.is_empty()
                || fields.len() > 50
                || fields
                    .iter()
                    .any(|field| field.is_empty() || field.len() > 120)
                || contract.consent_setting.is_empty()
                || contract.destination_setting.is_empty()
                || contract.secret_setting.is_empty()
                || customer_modules.contains(entry.provenance.module)
                || !setting_matches(
                    settings.get(contract.consent_setting).copied(),
                    ValueType::Boolean,
                    false,
                )
                || !setting_matches(
                    settings.get(contract.destination_setting).copied(),
                    ValueType::String,
                    false,
                )
                || !setting_matches(
                    settings.get(contract.secret_setting).copied(),
                    ValueType::String,
                    true,
                )
                || !(100..=10_000).contains(&contract.timeout_ms)
                || !(1..=300).contains(&contract.expires_seconds)
                || results.is_empty()
                || scorers
                    .insert(
                        entry.id,
                        BoundScorer {
                            id: entry.id,
                            contract,
                            results,
                        },
                    )
                    .is_some()
            {
                return Err(RegistryError::Contribution(format!(
                    "scorer {} at {}:{} has an invalid contract",
                    entry.id, entry.provenance.source, entry.provenance.line
                )));
            }
        }
        Ok(Self(scorers))
    }

    fn get(&self, id: &str) -> Option<&BoundScorer> {
        self.0.get(id)
    }
}

fn setting_matches(setting: Option<&Setting>, value_type: ValueType, encrypted: bool) -> bool {
    matches!(setting, Some(Setting::Definition(definition))
        if definition.value_type == value_type
            && definition.scopes.contains(&ScopeKind::Tenant)
            && definition.encrypted == encrypted
            && definition.editor.is_some())
}

/// One consented event offered to a scorer.
pub struct ScoreEvent<'a> {
    /// Stable owner key used for deduplication (for example the Base event id).
    pub key: &'a str,
    pub name: &'a str,
    /// Exactly the keys declared by [`Scorer::fields`].
    pub fields: Map<String, Value>,
}

/// The only trusted output of a scorer reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScoreDecision {
    pub delivery_id: String,
    pub result: String,
    pub action: &'static str,
    pub input: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ScoreFailure {
    #[error("Invalid scorer request")]
    Invalid,
    #[error("Scorer is not configured or consented")]
    NotConfigured,
    #[error("Scorer was revoked")]
    Revoked,
    #[error("This delivery already has an uncertain or completed outcome")]
    Duplicate,
    #[error("Scorer request failed")]
    Transport(#[from] Failure),
    #[error("Scorer returned an unsuccessful HTTP status")]
    HttpStatus,
    #[error("Scorer decision expired")]
    Expired,
    #[error("Scorer returned a reply outside its typed result schema")]
    InvalidReply,
    #[error("Scorer delivery receipt could not be recorded")]
    Unrecorded,
}

#[derive(PartialEq)]
struct Configuration {
    destination: String,
    secret: String,
}

impl Integration {
    /// Send a consented event to a customer-hosted scorer and return only its
    /// contributed action choice. The caller invokes that action through its
    /// owner contract and can correlate that action with `delivery_id`.
    pub async fn score(
        &self,
        origin: Origin<'_>,
        scope: &Scope,
        scorer: &str,
        event: ScoreEvent<'_>,
    ) -> std::result::Result<ScoreDecision, ScoreFailure> {
        if origin.tenant() != Some(scope.tenant()) || event.key.is_empty() || event.key.len() > 255
        {
            return Err(ScoreFailure::Invalid);
        }
        let contract = self.scorers.get(scorer).ok_or(ScoreFailure::Invalid)?;
        if event.name != contract.contract.event
            || event.fields.len() != contract.contract.fields.len()
            || !contract
                .contract
                .fields
                .iter()
                .all(|key| event.fields.contains_key(*key))
        {
            return Err(ScoreFailure::Invalid);
        }
        let configuration = self.configuration(scope, contract).await?;
        let delivery_id = delivery_id(&configuration.secret, scope.tenant(), scorer, event.key)?;
        if !self
            .begin_delivery(scope.tenant(), scorer, event.key, &delivery_id)
            .await?
        {
            return Err(ScoreFailure::Duplicate);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ScoreFailure::Invalid)?
            .as_secs();
        let expires_at = now + contract.contract.expires_seconds;
        let body = json!({
            "delivery_id": delivery_id,
            "expires_at": expires_at,
            "event": event.name,
            "fields": event.fields,
        });
        let bytes = serde_json::to_vec(&body).map_err(|_| ScoreFailure::Invalid)?;
        if bytes.len() > 65_536 {
            self.finish_delivery(
                scope.tenant(),
                &delivery_id,
                "refused",
                None,
                None,
                None,
                Some("request_too_large"),
            )
            .await?;
            return Err(ScoreFailure::Invalid);
        }
        let signature = signature(&configuration.secret, expires_at, &bytes)?;
        let call = Call {
            system: contract.id.to_owned(),
            operation: event.name.to_owned(),
            correlation_id: Some(delivery_id.clone()),
            ..Call::default()
        };
        let outgoing = Outgoing::post(configuration.destination.clone())
            .header("content-type", "application/json")
            .header("bilibi-delivery", delivery_id.clone())
            .header("bilibi-expires", expires_at.to_string())
            .header("bilibi-signature", format!("v1={signature}"))
            .bytes(bytes)
            .public_https()
            .no_redirects()
            .timeout(Duration::from_millis(contract.contract.timeout_ms))
            .max_body_bytes(16_384);
        if self.configuration(scope, contract).await.as_ref().ok() != Some(&configuration) {
            self.finish_delivery(
                scope.tenant(),
                &delivery_id,
                "revoked",
                None,
                None,
                None,
                Some("revoked"),
            )
            .await?;
            return Err(ScoreFailure::Revoked);
        }
        let reply = match self.send(origin, &call, outgoing).await {
            Ok(reply) => reply,
            Err(error) => {
                let (exchange_id, status) = match &error {
                    Failure::Transport {
                        exchange_id,
                        status,
                        ..
                    } => (Some(exchange_id.as_str()), *status),
                    _ => (None, None),
                };
                self.finish_delivery(
                    scope.tenant(),
                    &delivery_id,
                    "failed",
                    exchange_id,
                    status,
                    None,
                    Some("transport"),
                )
                .await?;
                return Err(error.into());
            }
        };
        if !(200..300).contains(&reply.status) {
            self.finish_delivery(
                scope.tenant(),
                &delivery_id,
                "refused",
                Some(&reply.exchange_id),
                Some(reply.status),
                None,
                Some("http_status"),
            )
            .await?;
            return Err(ScoreFailure::HttpStatus);
        }
        let deadline = UNIX_EPOCH + Duration::from_secs(expires_at);
        if SystemTime::now() >= deadline {
            self.finish_delivery(
                scope.tenant(),
                &delivery_id,
                "refused",
                Some(&reply.exchange_id),
                Some(reply.status),
                None,
                Some("expired"),
            )
            .await?;
            return Err(ScoreFailure::Expired);
        }
        let parsed: Value = match serde_json::from_slice(&reply.body) {
            Ok(parsed) => parsed,
            Err(_) => {
                self.finish_delivery(
                    scope.tenant(),
                    &delivery_id,
                    "refused",
                    Some(&reply.exchange_id),
                    Some(reply.status),
                    None,
                    Some("invalid_reply"),
                )
                .await?;
                return Err(ScoreFailure::InvalidReply);
            }
        };
        let result = parsed
            .as_object()
            .and_then(|object| {
                (object.get("delivery_id")?.as_str()? == delivery_id)
                    .then(|| object.get("result")?.as_str())
                    .flatten()
            })
            .and_then(|result| contract.results.get_key_value(result));
        let Some((result, choice)) = result else {
            self.finish_delivery(
                scope.tenant(),
                &delivery_id,
                "refused",
                Some(&reply.exchange_id),
                Some(reply.status),
                None,
                Some("invalid_reply"),
            )
            .await?;
            return Err(ScoreFailure::InvalidReply);
        };
        // Consent, destination policy and secret are live. A revocation or
        // rotation while the request was in flight withholds the action.
        if self.configuration(scope, contract).await.as_ref().ok() != Some(&configuration) {
            self.finish_delivery(
                scope.tenant(),
                &delivery_id,
                "revoked",
                Some(&reply.exchange_id),
                Some(reply.status),
                Some(result),
                Some("revoked"),
            )
            .await?;
            return Err(ScoreFailure::Revoked);
        }
        let expired = SystemTime::now() >= deadline;
        self.finish_delivery(
            scope.tenant(),
            &delivery_id,
            if expired { "refused" } else { "done" },
            Some(&reply.exchange_id),
            Some(reply.status),
            (!expired).then_some(*result),
            expired.then_some("expired"),
        )
        .await?;
        if expired || SystemTime::now() >= deadline {
            return Err(ScoreFailure::Expired);
        }
        Ok(ScoreDecision {
            delivery_id,
            result: (*result).to_owned(),
            action: choice.action,
            input: choice.input.clone(),
        })
    }

    async fn configuration(
        &self,
        scope: &Scope,
        scorer: &BoundScorer,
    ) -> std::result::Result<Configuration, ScoreFailure> {
        let client = self
            .db
            .get()
            .await
            .map_err(|_| ScoreFailure::NotConfigured)?;
        let keys = [
            scorer.contract.consent_setting,
            scorer.contract.destination_setting,
            scorer.contract.secret_setting,
        ];
        let values = self
            .settings
            .resolve_many(&**client, scope, Target::Tenant, &keys)
            .await
            .map_err(|_| ScoreFailure::NotConfigured)?;
        let consent = values
            .get(scorer.contract.consent_setting)
            .filter(|value| {
                value.source.is_some_and(|source| {
                    source.kind == ScopeKind::Tenant && source.tenant == scope.tenant()
                })
            })
            .and_then(|value| value.value.as_bool())
            .unwrap_or(false);
        let destination = values
            .get(scorer.contract.destination_setting)
            .and_then(|value| value.value.as_str())
            .map(str::to_owned);
        let secret = values
            .get(scorer.contract.secret_setting)
            .and_then(|value| value.value.as_str())
            .filter(|secret| secret.len() >= 32)
            .map(str::to_owned);
        let policy = self
            .settings
            .resolve_many(&**client, scope, Target::Global, &[CONNECTOR_DESTINATIONS])
            .await
            .map_err(|_| ScoreFailure::NotConfigured)?;
        let allowed = policy
            .get(CONNECTOR_DESTINATIONS)
            .and_then(|value| value.value.as_array());
        match (consent, destination, secret, allowed) {
            (true, Some(destination), Some(secret), Some(allowed))
                if allowed
                    .iter()
                    .any(|value| value.as_str() == Some(&destination))
                    && public_https(&destination) =>
            {
                Ok(Configuration {
                    destination,
                    secret,
                })
            }
            _ => Err(ScoreFailure::NotConfigured),
        }
    }

    async fn begin_delivery(
        &self,
        tenant: i64,
        scorer: &str,
        event_key: &str,
        delivery_id: &str,
    ) -> std::result::Result<bool, ScoreFailure> {
        let client = self.db.get().await.map_err(|_| ScoreFailure::Unrecorded)?;
        let event_digest = event_digest(scorer, event_key);
        let changed = client
            .execute(
                "INSERT INTO base_integration_scorer_deliveries (tenant_id,scorer,event_digest,delivery_id,state) VALUES ($1,$2,$3,$4,'sending') ON CONFLICT (tenant_id,scorer,event_digest) DO NOTHING",
                &[&tenant, &scorer, &event_digest, &delivery_id],
            )
            .await
            .map_err(|_| ScoreFailure::Unrecorded)?;
        Ok(changed == 1)
    }

    async fn finish_delivery(
        &self,
        tenant: i64,
        delivery_id: &str,
        state: &str,
        exchange_id: Option<&str>,
        response_status: Option<u16>,
        result: Option<&str>,
        reason: Option<&str>,
    ) -> std::result::Result<(), ScoreFailure> {
        let client = self.db.get().await.map_err(|_| ScoreFailure::Unrecorded)?;
        let response_status = response_status
            .filter(|status| (100..=599).contains(status))
            .map(|status| status as i16);
        let changed = client
            .execute(
                "UPDATE base_integration_scorer_deliveries SET state=$3,exchange_id=$4,response_status=$5,result=$6,reason=$7,finished_at=now() WHERE tenant_id=$1 AND delivery_id=$2 AND state='sending'",
                &[&tenant, &delivery_id, &state, &exchange_id, &response_status, &result, &reason],
            )
            .await
            .map_err(|_| ScoreFailure::Unrecorded)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(ScoreFailure::Unrecorded)
        }
    }
}

fn public_https(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let literal_is_public = url
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse().ok())
        .is_none_or(crate::transport::public_address);
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && literal_is_public
}

pub(crate) fn valid_destination_policy(value: &Value) -> bool {
    value.as_array().is_some_and(|destinations| {
        let distinct: BTreeSet<_> = destinations.iter().filter_map(Value::as_str).collect();
        destinations.len() <= 200
            && distinct.len() == destinations.len()
            && destinations.iter().all(|value| {
                value.as_str().is_some_and(|destination| {
                    destination.len() <= 4096 && public_https(destination)
                })
            })
    })
}

fn delivery_id(
    secret: &str,
    tenant: i64,
    scorer: &str,
    event_key: &str,
) -> std::result::Result<String, ScoreFailure> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| ScoreFailure::Invalid)?;
    mac.update(
        json!(["bilibi-scorer-delivery-v1", tenant, scorer, event_key])
            .to_string()
            .as_bytes(),
    );
    let digest = mac.finalize().into_bytes();
    Ok(format!(
        "sd_{}",
        data_encoding::HEXLOWER.encode(&digest[..16])
    ))
}

fn event_digest(scorer: &str, event_key: &str) -> Vec<u8> {
    Sha256::digest(json!(["bilibi-scorer-event-v1", scorer, event_key]).to_string()).to_vec()
}

fn signature(
    secret: &str,
    expires_at: u64,
    body: &[u8],
) -> std::result::Result<String, ScoreFailure> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| ScoreFailure::Invalid)?;
    mac.update(expires_at.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(data_encoding::HEXLOWER.encode(&mac.finalize().into_bytes()))
}
