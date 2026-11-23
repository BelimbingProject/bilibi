//! `availability` and `generate`: the switches, the provider, the deadline,
//! the OpenAI-compatible request and the one interaction row. A call holds
//! no pooled connection while the provider works: settings are read on a
//! short connection that is released first, Integration records the
//! exchange on its own, and the row is written on another afterwards.
use crate::{
    contributions::{
        AUTOMATION_ENABLED, COMPANY_ENABLED, ENABLED, PROVIDERS, PROVIDER_KEYS, TIMEOUT_MAX,
        TIMEOUT_MIN, TIMEOUT_SECONDS,
    },
    providers::{self, Provider},
    store, Ai, Answer, Availability, Generated, Reason, Request,
};
use bilibi_database::{Error, Result};
use bilibi_integration::{Call, Failure, Origin, Outgoing};
use bilibi_module_registry::Module;
use bilibi_settings::Target;
use bilibi_tenancy::{ReadClient, Scope};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// The longest instructions one call may carry, in characters.
pub const INSTRUCTIONS_CHARACTERS_MAX: usize = 20_000;
/// The longest text one call may carry, in characters.
pub const TEXT_CHARACTERS_MAX: usize = 200_000;
/// The longest answer passed on, in characters; a longer one is a provider
/// error.
pub const ANSWER_CHARACTERS_MAX: usize = 200_000;
/// How long past the deadline the gateway waits for a call that ignores it
/// before cutting it off.
pub const CUT_OFF_GRACE: Duration = Duration::from_secs(2);
/// The largest reply body read from a provider.
const REPLY_BYTES_MAX: u64 = 4 << 20;

/// What the settings allow, before any call.
enum Configured {
    Ready {
        provider: Provider,
        timeout: Duration,
    },
    Unavailable(Reason),
}

/// The bounds on a call; a call outside them is the caller's mistake, never
/// an unavailable model.
fn check(request: &Request<'_>) -> Result<()> {
    let registered = inventory::iter::<Module>
        .into_iter()
        .any(|module| module.id == request.module);
    let instructions = request.instructions.chars().count();
    if !registered
        || request.instructions.trim().is_empty()
        || instructions > INSTRUCTIONS_CHARACTERS_MAX
        || request.text.chars().count() > TEXT_CHARACTERS_MAX
        || request.provider.is_some_and(|name| !providers::name(name))
        || request.deadline.is_zero()
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

fn interaction_id() -> String {
    format!(
        "ai_{}",
        data_encoding::BASE32_NOPAD.encode(uuid::Uuid::new_v4().as_bytes())
    )
}

/// The first choice's message text of a chat-completions reply, if it is a
/// non-empty string within [`ANSWER_CHARACTERS_MAX`].
fn answer_text(reply: &Value) -> Option<String> {
    let text = reply
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()?;
    (!text.trim().is_empty() && text.chars().count() <= ANSWER_CHARACTERS_MAX)
        .then(|| text.to_owned())
}

impl Ai {
    async fn configured(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        requested: Option<&str>,
    ) -> Result<Configured> {
        let (target, switch) = match (scope.principal(), scope.current_company()) {
            (Some("watchers"), _) => (Target::Tenant, AUTOMATION_ENABLED),
            (Some(_), _) => return Ok(Configured::Unavailable(Reason::Disabled)),
            (None, Some(company)) => (Target::Company(company), COMPANY_ENABLED),
            (None, None) => (Target::Tenant, COMPANY_ENABLED),
        };
        let values = self
            .settings
            .resolve_many(
                client,
                scope,
                target,
                &[ENABLED, switch, PROVIDERS, TIMEOUT_SECONDS],
            )
            .await?;
        let read = |key: &str| values.get(key).map(|resolved| &resolved.value);
        let on = |key: &str| read(key).and_then(Value::as_bool) == Some(true);
        if !on(ENABLED) || !on(switch) {
            return Ok(Configured::Unavailable(Reason::Disabled));
        }
        let listed = read(PROVIDERS)
            .and_then(providers::parse)
            .ok_or(Error::Internal)?;
        let chosen = match requested {
            Some(name) => listed.into_iter().find(|provider| provider.name == name),
            None => listed.into_iter().next(),
        };
        let Some(provider) = chosen else {
            return Ok(Configured::Unavailable(Reason::NotConfigured));
        };
        let seconds = read(TIMEOUT_SECONDS)
            .and_then(Value::as_i64)
            .filter(|seconds| (TIMEOUT_MIN..=TIMEOUT_MAX).contains(seconds))
            .ok_or(Error::Internal)?;
        Ok(Configured::Ready {
            provider,
            timeout: Duration::from_secs(seconds as u64),
        })
    }

    /// Whether a call to `provider` (or the default) can be made for this
    /// scope now, from settings alone: no model call, no key read and no
    /// write. It answers only `Disabled` or `NotConfigured` when it cannot,
    /// so a page can hide its AI action. Reads on the handed client.
    pub async fn availability(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        provider: Option<&str>,
    ) -> Result<Availability> {
        if provider.is_some_and(|name| !providers::name(name)) {
            return Err(Error::Invalid);
        }
        Ok(match self.configured(client, scope, provider).await? {
            Configured::Ready { provider, .. } => Availability::Available {
                provider: provider.name,
                model: provider.model,
            },
            Configured::Unavailable(reason) => Availability::Unavailable(reason),
        })
    }

    /// One call. Never call it while holding a pooled connection or a
    /// transaction: it takes short connections of its own.
    ///
    /// A switch that is off or a missing provider answers `Unavailable` at
    /// once, with no model call. Otherwise it sends one chat-completions
    /// request through Integration with the deadline (the smaller of the
    /// request's and `ai.timeout_seconds`) and cuts the call off two seconds
    /// after it. Every call that passes the checks writes exactly one
    /// metadata row. `Err` is an invalid request (an unregistered module, an
    /// empty or oversized instruction or text, a malformed provider name, a
    /// zero deadline) or an infrastructure failure, including a row that
    /// could not be written.
    pub async fn generate(&self, scope: &Scope, request: Request<'_>) -> Result<Answer> {
        check(&request)?;
        let started = Instant::now();
        let (configured, key) = {
            let client = self.db.get().await?;
            let configured = if scope.principal().is_some() && request.module != "base/watchers" {
                Configured::Unavailable(Reason::Disabled)
            } else {
                self.configured(&**client, scope, request.provider).await?
            };
            let key = match &configured {
                Configured::Ready { provider, .. } => {
                    let stored = self
                        .settings
                        .get(&**client, scope, Target::Tenant, PROVIDER_KEYS)
                        .await?;
                    providers::keys(&stored.value)
                        .ok_or(Error::Internal)?
                        .remove(&provider.name)
                }
                Configured::Unavailable(_) => None,
            };
            (configured, key)
        };
        let id = interaction_id();
        let input_characters = request.instructions.chars().count() + request.text.chars().count();
        let (provider, timeout) = match configured {
            Configured::Ready { provider, timeout } => (provider, timeout),
            Configured::Unavailable(reason) => {
                store::record(
                    &self.db,
                    store::Record {
                        public_id: &id,
                        scope,
                        module: request.module,
                        provider: request.provider,
                        model: None,
                        input_characters,
                        output_characters: None,
                        reason: Some(reason),
                        duration: started.elapsed(),
                        exchange_id: None,
                    },
                )
                .await?;
                return Ok(Answer::Unavailable { reason });
            }
        };
        let deadline = request.deadline.min(timeout);
        let body = json!({
            "model": provider.model,
            "messages": [
                {"role": "system", "content": request.instructions},
                {"role": "user", "content": request.text},
            ],
            "stream": false,
        });
        // A redirect is returned rather than followed, so the key never
        // goes elsewhere. A hosted provider is public HTTPS: its name is
        // resolved once, a non-public address is refused and the checked
        // addresses are pinned, so a hostname cannot resolve or rebind to an
        // internal service. Only a provider the operator marked `local` may
        // reach a private address, with the server's own trust store.
        let outgoing = Outgoing::post(provider.endpoint())
            .json(body)
            .timeout(deadline)
            .max_body_bytes(REPLY_BYTES_MAX)
            .no_redirects();
        let mut outgoing = if provider.local {
            outgoing.trust_system_roots()
        } else {
            outgoing.public_https()
        };
        if let Some(key) = &key {
            outgoing = outgoing.header("Authorization", format!("Bearer {key}"));
        }
        let call = Call {
            system: "ai".into(),
            operation: "chat.completions".into(),
            provider: Some(provider.name.clone()),
            correlation_id: Some(id.clone()),
            ..Call::default()
        };
        let sent = tokio::time::timeout(
            deadline + CUT_OFF_GRACE,
            self.integration.send(Origin::User(scope), &call, outgoing),
        )
        .await;
        let (outcome, exchange_id) = match sent {
            Err(_) => (Err(Reason::Timeout), None),
            Ok(Err(Failure::Transport {
                class: "timeout",
                exchange_id,
                ..
            })) => (Err(Reason::Timeout), Some(exchange_id)),
            Ok(Err(Failure::Transport { exchange_id, .. })) => {
                (Err(Reason::ProviderError), Some(exchange_id))
            }
            Ok(Err(Failure::Invalid | Failure::Unrecorded)) => (Err(Reason::ProviderError), None),
            Ok(Ok(reply)) => {
                let text = (200..300)
                    .contains(&reply.status)
                    .then(|| reply.json())
                    .flatten()
                    .as_ref()
                    .and_then(answer_text);
                (text.ok_or(Reason::ProviderError), Some(reply.exchange_id))
            }
        };
        store::record(
            &self.db,
            store::Record {
                public_id: &id,
                scope,
                module: request.module,
                provider: Some(&provider.name),
                model: Some(&provider.model),
                input_characters,
                output_characters: outcome.as_ref().ok().map(|text| text.chars().count()),
                reason: outcome.as_ref().err().copied(),
                duration: started.elapsed(),
                exchange_id: exchange_id.as_deref(),
            },
        )
        .await?;
        Ok(match outcome {
            Ok(text) => Answer::Generated(Generated {
                text,
                provider: provider.name,
                model: provider.model,
                interaction_id: id,
            }),
            Err(reason) => Answer::Unavailable { reason },
        })
    }
}
