//! Emailing a link to a document. Mailing a name out of Bilibi is a
//! disclosure, so a request is never a fire-and-forget task:
//!
//! 1. **Request** (in the share's own transaction): the per-person, per-
//!    recipient-and-document and per-tenant limits are counted under a lock,
//!    one audited `core_document_emails` row records the request (ids only,
//!    never an address, a name or a link) as `queued` or `rate_limited`, and a
//!    `queued` one enqueues a durable `core/document-email` job.
//! 2. **Delivery** (the job, as the sender): the document and the recipient's
//!    live grant are read again. The mail goes only when the sender may still
//!    share the document, the document is not being deleted and the recipient
//!    still holds an unended view grant and the view capability; the message
//!    then carries the document's *current* name. Anything else settles the
//!    row `suppressed` and sends nothing.
//! 3. **Outcome** (an audited write): `sent`, `failed` or `suppressed`, with a
//!    reason code. A crash between sending and settling sends once more.
use crate::store::Store;
use crate::{Document, EmailOutcome, WriteResult, MANAGE, VIEW};
use bilibi_database::{Change, Db, Error, MutationTransaction, Result};
use bilibi_mail::{Address, Mailer};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_queue::{
    Execution, Handler, HandlerBinding, HandlerFuture, Outcome, Worker, HANDLERS, WORKERS,
};
use bilibi_settings::{Setting, Target};
use bilibi_tenancy::{Authentication, Scope};
use serde_json::{json, Value};
use std::sync::Arc;

/// The audit kind of one email request.
pub const EMAIL_RECORD: &str = "core.document.email";
pub(crate) const WORKER: &str = "core/document-email";
pub(crate) const HANDLER: &str = "core/document.email";

/// Most emails one person may request an hour.
pub const ACTOR_PER_HOUR: &str = "core.document.email_actor_per_hour";
/// Most emails about one document to one recipient a day.
pub const RECIPIENT_PER_DAY: &str = "core.document.email_recipient_per_day";
/// Most emails the tenant may request an hour.
pub const TENANT_PER_HOUR: &str = "core.document.email_tenant_per_hour";

macro_rules! provenance {
    () => {
        Provenance {
            module: crate::MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

/// `{email}` (the request's id) and nothing else.
fn arguments(args: Value) -> Result<Value> {
    let object = args.as_object().ok_or(Error::Invalid)?;
    if object.len() != 1
        || object
            .get("email")
            .and_then(Value::as_i64)
            .is_none_or(|id| id <= 0)
    {
        return Err(Error::Invalid);
    }
    Ok(args)
}
inventory::submit! { Contribution::new(WORKERS, WORKER, provenance!(), &Worker { max_attempts: 5, validate: arguments }) }
inventory::submit! { Contribution::new(HANDLERS, WORKER, provenance!(), &HandlerBinding { service: HANDLER, capability: MANAGE }) }

macro_rules! limit {
    ($key:expr, $default:expr, $label:expr, $help:expr) => {
        inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, $key, provenance!(), &Setting::Definition(bilibi_settings::Definition {
            value_type: bilibi_settings::ValueType::Integer,
            scopes: &[bilibi_settings::ScopeKind::Global, bilibi_settings::ScopeKind::Tenant],
            default: || json!($default),
            nullable: false, encrypted: false, minimum: Some(1.0), maximum: Some(10_000.0), validator: None,
            editor: Some(bilibi_settings::Editor { group: "Documents", label: $label, help: $help, capability: None, reveal_duration_ms: 5_000 }),
        })) }
    };
}
limit!(ACTOR_PER_HOUR, 20, "Document emails per person an hour", "The most emails with a link to a document one person may request in an hour. More are shared without an email.");
limit!(
    RECIPIENT_PER_DAY,
    3,
    "Emails about one document to one person a day",
    "The most emails about the same document to the same person in a day."
);
limit!(
    TENANT_PER_HOUR,
    200,
    "Document emails per tenant an hour",
    "The most emails with a link to a document the whole tenant may request in an hour."
);

async fn limit(
    store: &Store<'_>,
    tx: &MutationTransaction<'_>,
    scope: &Scope,
    key: &str,
    default: i64,
) -> Result<i64> {
    Ok(store
        .settings
        .get(tx.sql(), scope, Target::Tenant, key)
        .await?
        .value
        .as_i64()
        .unwrap_or(default))
}

impl Store<'_> {
    /// Records the request for emailing `document` to `recipient` and, when
    /// the limits allow, queues the delivery. Call it in the share's own
    /// transaction.
    pub(crate) async fn request_email(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        document: &Document,
        recipient: i64,
    ) -> WriteResult<EmailOutcome> {
        let (actor_max, pair_max, tenant_max) = (
            limit(self, tx, scope, ACTOR_PER_HOUR, 20).await?,
            limit(self, tx, scope, RECIPIENT_PER_DAY, 3).await?,
            limit(self, tx, scope, TENANT_PER_HOUR, 200).await?,
        );
        let (tenant, actor, id) = (scope.tenant(), scope.user(), document.id);
        let (request, refused) = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    // One request at a time per tenant: the counts below see
                    // every earlier request's row.
                    sql.execute(
                        "SELECT pg_advisory_xact_lock(hashtextextended('core.document.email:' || $1::bigint::text, 0))",
                        &[&tenant],
                    )
                    .await?;
                    let counts = sql
                        .query_one(
                            "SELECT
                               (SELECT count(*) FROM core_document_emails WHERE tenant_id=$1 AND requested_by=$2 AND requested_at > now() - interval '1 hour' AND state IN ('queued','sent')),
                               (SELECT count(*) FROM core_document_emails WHERE tenant_id=$1 AND document_id=$3 AND recipient_id=$4 AND requested_at > now() - interval '1 day' AND state IN ('queued','sent')),
                               (SELECT count(*) FROM core_document_emails WHERE tenant_id=$1 AND requested_at > now() - interval '1 hour' AND state IN ('queued','sent'))",
                            &[&tenant, &actor, &id, &recipient],
                        )
                        .await?;
                    let (by_actor, by_pair, by_tenant): (i64, i64, i64) =
                        (counts.get(0), counts.get(1), counts.get(2));
                    let refused = if by_actor >= actor_max {
                        Some("actor_limit")
                    } else if by_pair >= pair_max {
                        Some("recipient_limit")
                    } else if by_tenant >= tenant_max {
                        Some("tenant_limit")
                    } else {
                        None
                    };
                    let state = if refused.is_some() { "rate_limited" } else { "queued" };
                    let row = sql
                        .query_one(
                            "INSERT INTO core_document_emails(tenant_id,document_id,recipient_id,requested_by,state,reason,settled_at) VALUES($1,$2,$3,$4,$5,$6,CASE WHEN $5 = 'rate_limited' THEN now() END) RETURNING id",
                            &[&tenant, &id, &recipient, &actor, &state, &refused],
                        )
                        .await?;
                    let request: i64 = row.get(0);
                    let change = Change {
                        kind: EMAIL_RECORD.into(),
                        id: request,
                        old: json!({}),
                        new: json!({"document_id": id, "recipient_id": recipient, "requested_by": actor, "state": state, "reason": refused}),
                    };
                    Ok(((request, refused), vec![change]))
                })
            })
            .await
            .map_err(crate::store::write_error)?;
        match refused {
            Some(reason) => Ok(EmailOutcome::RateLimited(reason)),
            None => {
                self.queue
                    .enqueue_for(tx, scope, WORKER, json!({"email": request}))
                    .await?;
                Ok(EmailOutcome::Queued)
            }
        }
    }
}

/// The job that delivers one queued request.
pub(crate) struct EmailJob {
    pub db: Db,
    pub mailer: Arc<Mailer>,
    pub authentication: Arc<Authentication>,
    /// The public origin the link points at (`APP_ORIGIN`).
    pub origin: String,
}
impl Handler for EmailJob {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            match self.run(execution, &args).await {
                Ok(Done::Settled) => Outcome::Completed,
                Ok(Done::Retry) => Outcome::Retry("email_delivery_failed"),
                // The sender lost their account or the capability.
                Err(Error::Forbidden | Error::NotFound | Error::Unauthorized) => {
                    Outcome::Cancel("email_refused")
                }
                Err(error) => {
                    eprintln!("error: document email: {error}");
                    Outcome::Retry("email_failed")
                }
            }
        })
    }
}
enum Done {
    Settled,
    Retry,
}
impl EmailJob {
    async fn run(&self, execution: &Execution, args: &Value) -> Result<Done> {
        let scope = execution.scope().ok_or(Error::Unauthorized)?.clone();
        let request = args["email"].as_i64().ok_or(Error::Invalid)?;
        let mut client = self.db.get().await?;
        let tenant = scope.tenant();
        let Some(row) = client
            .query_opt(
                "SELECT document_id,recipient_id FROM core_document_emails WHERE tenant_id=$1 AND id=$2 AND requested_by=$3 AND state='queued'",
                &[&tenant, &request, &scope.user()],
            )
            .await?
        else {
            // Already settled: a retry after the outcome was recorded.
            return Ok(Done::Settled);
        };
        let (document, recipient): (i64, i64) = (row.get(0), row.get(1));
        // The sender must still be allowed to share this document.
        let may_share = bilibi_authz::holds_proof(&**client, &scope, MANAGE).await?
            && crate::store::permission(&**client, &scope, document, "share").await?;
        // The recipient's live grant, read now: the name goes out only to
        // someone who could open the document this minute.
        let current = client
            .query_opt(
                "SELECT d.name,a.expires_at FROM core_documents d JOIN core_document_access a ON a.tenant_id=d.tenant_id AND a.document_id=d.id AND a.user_id=$3
                 WHERE d.tenant_id=$1 AND d.id=$2 AND d.deleting_at IS NULL AND a.can_view AND (a.expires_at IS NULL OR a.expires_at > now())",
                &[&tenant, &document, &recipient],
            )
            .await?;
        let to = self
            .authentication
            .authenticate(&**client, tenant, recipient, String::new())
            .await
            .ok();
        let reason = match (&current, &to) {
            _ if !may_share => Some("sender_access_ended"),
            (None, _) => Some("access_ended"),
            (_, None) => Some("recipient_unavailable"),
            (Some(_), Some(to)) => {
                if bilibi_authz::holds_proof(&**client, to, VIEW).await? {
                    None
                } else {
                    Some("recipient_cannot_view")
                }
            }
        };
        let (Some(current), Some(to), None) = (current, to, reason) else {
            self.settle(execution, &mut client, request, "suppressed", reason)
                .await?;
            return Ok(Done::Settled);
        };
        let from_name = scope.user_name.clone();
        let name: String = current.get::<_, String>(0).chars().take(150).collect();
        let ends = current
            .get::<_, Option<std::time::SystemTime>>(1)
            .and_then(|t| jiff::Timestamp::try_from(t).ok())
            .map(|t| format!("\nYour access ends {}.", t.strftime("%Y-%m-%d %H:%M UTC")))
            .unwrap_or_default();
        let address = Address::new(Some(&to.user_name), &to.email);
        let delivered = match address {
            Ok(address) => {
                let message = bilibi_mail::Message::text(
                    address,
                    format!("{from_name} shared a document with you: {name}"),
                    format!(
                        "{from_name} shared “{name}” with you.{ends}\n\nOpen it, signed in:\n{}/documents/{document}\n\nThe link works only for your account and only while your access lasts.\n",
                        self.origin
                    ),
                );
                self.mailer.deliver(&message).await.is_ok()
            }
            Err(_) => false,
        };
        if delivered {
            self.settle(execution, &mut client, request, "sent", None)
                .await?;
            return Ok(Done::Settled);
        }
        if execution.attempt >= execution.max_attempts {
            self.settle(
                execution,
                &mut client,
                request,
                "failed",
                Some("delivery_failed"),
            )
            .await?;
            return Ok(Done::Settled);
        }
        Ok(Done::Retry)
    }

    /// Records the outcome, audited: ids and a reason code only.
    async fn settle(
        &self,
        execution: &Execution,
        client: &mut tokio_postgres::Client,
        request: i64,
        state: &'static str,
        reason: Option<&'static str>,
    ) -> Result<()> {
        let tenant = execution.scope().ok_or(Error::Unauthorized)?.tenant();
        let mut tx = execution.begin(client, "core.document.email").await?;
        tx.mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_opt(
                        "UPDATE core_document_emails SET state=$3,reason=$4,settled_at=now() WHERE tenant_id=$1 AND id=$2 AND state='queued' RETURNING document_id,recipient_id,requested_by",
                        &[&tenant, &request, &state, &reason],
                    )
                    .await?;
                let Some(row) = row else { return Ok(((), Vec::new())) };
                let change = Change {
                    kind: EMAIL_RECORD.into(),
                    id: request,
                    old: json!({"state": "queued"}),
                    new: json!({"document_id": row.get::<_, i64>(0), "recipient_id": row.get::<_, i64>(1), "requested_by": row.get::<_, i64>(2), "state": state, "reason": reason}),
                };
                Ok(((), vec![change]))
            })
        })
        .await?;
        tx.commit().await
    }
}
