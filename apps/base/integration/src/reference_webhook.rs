//! Non-business specimen of a signed webhook, registered as
//! `/webhooks/integration-reference`. It shows what a module owes the host
//! boundary: a signature over the exact bytes, a freshness window and a
//! single-use nonce, all decided here, never in the host.
//!
//! Signature: `X-Bilibi-Signature` is lowercase hex HMAC-SHA256 with the shared
//! secret over `"{X-Bilibi-Timestamp}.{X-Bilibi-Nonce}." + body`. The timestamp
//! is Unix seconds within ±300 s of the host clock. The nonce is claimed
//! atomically in `handle`, so a replay of a valid delivery is refused and a
//! delivery the host rate-limited after verification may be retried unchanged.
//!
//! The secret comes from `BILIBI_REFERENCE_WEBHOOK_SECRET` at boot: a webhook
//! has no actor, and Settings deliberately releases an encrypted value only to
//! an authorized scope. Without it every delivery is refused. Production
//! owners resolve their credentials through their own definitions.
use crate::webhooks::{
    Delivery, Refused, Verified, Webhook, WebhookHandler, WebhookService, WEBHOOKS,
};
use bilibi_database::Db;
use bilibi_module_registry::{Contribution, Provenance};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub const IDENTIFIER: &str = "integration-reference";
pub const SERVICE: &str = "base/integration.reference-webhook";
pub const SECRET_ENV: &str = "BILIBI_REFERENCE_WEBHOOK_SECRET";
pub const SIGNATURE_HEADER: &str = "x-bilibi-signature";
pub const TIMESTAMP_HEADER: &str = "x-bilibi-timestamp";
pub const NONCE_HEADER: &str = "x-bilibi-nonce";
pub const TOLERANCE_SECONDS: i64 = 300;
const NONCE_RETENTION: &str = "1 day";

pub struct Reference {
    db: Db,
    secret: Option<Vec<u8>>,
}
impl Reference {
    pub fn from_env(db: Db) -> Self {
        let secret = std::env::var(SECRET_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .map(String::into_bytes);
        if secret.is_none() {
            bilibi_module_registry::server_log!(
                Warning,
                "warning: {SECRET_ENV} is unset; /webhooks/{IDENTIFIER} refuses every delivery"
            );
        }
        Self { db, secret }
    }
}
/// What `verify` proved; `handle` claims the nonce.
struct Claim {
    nonce: String,
    signed_at: i64,
}
pub fn valid_nonce(nonce: &str) -> bool {
    (1..=128).contains(&nonce.len())
        && nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
/// The signature the sender must present for these inputs.
pub fn sign(secret: &[u8], timestamp: i64, nonce: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(format!("{timestamp}.{nonce}.").as_bytes());
    mac.update(body);
    data_encoding::HEXLOWER.encode(&mac.finalize().into_bytes())
}
impl WebhookHandler for Reference {
    fn verify<'a>(
        &'a self,
        delivery: &'a Delivery<'a>,
    ) -> crate::webhooks::WebhookFuture<'a, Verified> {
        Box::pin(async move {
            let secret = self.secret.as_deref().ok_or(Refused)?;
            let timestamp: i64 = delivery
                .header_str(TIMESTAMP_HEADER)
                .and_then(|t| t.parse().ok())
                .ok_or(Refused)?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if (now - timestamp).abs() > TOLERANCE_SECONDS {
                return Err(Refused);
            }
            let nonce = delivery.header_str(NONCE_HEADER).ok_or(Refused)?;
            if !valid_nonce(nonce) {
                return Err(Refused);
            }
            let supplied = data_encoding::HEXLOWER
                .decode(delivery.header(SIGNATURE_HEADER).ok_or(Refused)?)
                .map_err(|_| Refused)?;
            let mut mac = Hmac::<Sha256>::new_from_slice(secret).map_err(|_| Refused)?;
            mac.update(format!("{timestamp}.{nonce}.").as_bytes());
            mac.update(delivery.body);
            // Constant-time comparison of the full tag.
            mac.verify_slice(&supplied).map_err(|_| Refused)?;
            Ok(Box::new(Claim {
                nonce: nonce.to_owned(),
                signed_at: timestamp,
            }) as Verified)
        })
    }
    fn handle<'a>(
        &'a self,
        delivery: &'a Delivery<'a>,
        verified: Verified,
    ) -> crate::webhooks::WebhookFuture<'a, ()> {
        Box::pin(async move {
            let claim = verified.downcast::<Claim>().map_err(|_| Refused)?;
            let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(delivery.body));
            let bytes = i32::try_from(delivery.body.len()).map_err(|_| Refused)?;
            let client = self.db.get().await.map_err(|_| Refused)?;
            client
                .execute(
                    &format!("DELETE FROM base_integration_reference_webhook WHERE received_at < now() - interval '{NONCE_RETENTION}'"),
                    &[],
                )
                .await
                .map_err(|_| Refused)?;
            // A nonce is spent once: the second delivery with it changes nothing and is refused.
            let claimed = client
                .execute(
                    "INSERT INTO base_integration_reference_webhook(nonce,signed_at,body_bytes,body_sha256) VALUES($1,$2,$3,$4) ON CONFLICT (nonce) DO NOTHING",
                    &[&claim.nonce, &claim.signed_at, &bytes, &digest],
                )
                .await
                .map_err(|_| Refused)?;
            if claimed == 1 {
                Ok(())
            } else {
                Err(Refused)
            }
        })
    }
}
inventory::submit! { Contribution::new(WEBHOOKS, "base/integration.reference", Provenance { module: "base/integration", source: file!(), line: line!() }, &Webhook {
    identifier: IDENTIFIER, service: SERVICE,
}) }
pub(crate) fn service(db: Db) -> Arc<WebhookService> {
    Arc::new(WebhookService(Arc::new(Reference::from_env(db))))
}
