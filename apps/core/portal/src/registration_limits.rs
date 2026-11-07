//! Durable admission limits for public customer registration. The tenant lock
//! makes one decision across every application node; one-way subject hashes
//! avoid keeping sender addresses or another copy of a recipient address.
use crate::{facade::digest, PortalService, MANAGE, STAFF_PATH};
use bilibi_database::{Change, MutationTransaction, Result};
use bilibi_notifications::{Notice, MAX_RECIPIENTS};
use bilibi_tenancy::{AccountKind, ReadClient};
use serde_json::json;
use tokio_postgres::Transaction;

pub(crate) const SENDER_MINUTE_KEY: &str = "customer_portal.registration_sender_per_minute";
pub(crate) const SENDER_DAY_KEY: &str = "customer_portal.registration_sender_per_day";
pub(crate) const RECIPIENT_MINUTE_KEY: &str = "customer_portal.registration_recipient_per_minute";
pub(crate) const RECIPIENT_DAY_KEY: &str = "customer_portal.registration_recipient_per_day";
pub(crate) const PORTAL_MINUTE_KEY: &str = "customer_portal.registration_portal_per_minute";
pub(crate) const PORTAL_DAY_KEY: &str = "customer_portal.registration_portal_per_day";
pub(crate) const PENDING_KEY: &str = "customer_portal.registration_pending_limit";

pub(crate) const SENDER_MINUTE_DEFAULT: i64 = 5;
pub(crate) const SENDER_DAY_DEFAULT: i64 = 30;
pub(crate) const RECIPIENT_MINUTE_DEFAULT: i64 = 2;
pub(crate) const RECIPIENT_DAY_DEFAULT: i64 = 3;
pub(crate) const PORTAL_MINUTE_DEFAULT: i64 = 30;
pub(crate) const PORTAL_DAY_DEFAULT: i64 = 300;
pub(crate) const PENDING_DEFAULT: i64 = 1_000;

pub(crate) struct NewRegistration {
    pub tenant: i64,
    pub name: String,
    pub email: String,
    pub company: Option<String>,
    pub token_hash: Vec<u8>,
    pub days: i32,
    pub sender: String,
}

pub(crate) struct Admission {
    pub created: bool,
    pub changed: bool,
}

#[derive(Clone, Copy)]
struct Limits {
    sender_minute: i64,
    sender_day: i64,
    recipient_minute: i64,
    recipient_day: i64,
    portal_minute: i64,
    portal_day: i64,
    pending: i64,
}

#[derive(Clone, Copy, Default)]
struct Counter {
    minute: i64,
    day: i64,
}

impl PortalService {
    pub(crate) async fn admit_registration(
        &self,
        tx: &mut MutationTransaction<'_>,
        registration: NewRegistration,
    ) -> Result<Admission> {
        let tenant = registration.tenant;
        let limits = self.registration_limits(tx.sql()).await?;
        let sender_hash = subject_hash(registration.tenant, "sender", &registration.sender);
        let recipient_hash = subject_hash(registration.tenant, "recipient", &registration.email);
        let portal_hash = subject_hash(registration.tenant, "portal", "portal");
        let sql = tx.sql();
        sql.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended('core.portal.registration:'||$1::bigint::text,0))",
            &[&registration.tenant],
        )
        .await?;
        let none = Admission {
            created: false,
            changed: false,
        };
        if sql
            .query_opt(
                "SELECT 1 FROM customer_registrations WHERE tenant_id=$1 AND email=$2 AND decision IS NULL",
                &[&registration.tenant, &registration.email],
            )
            .await?
            .is_some()
        {
            return Ok(none);
        }
        let sender = counter(sql, registration.tenant, "sender", &sender_hash).await?;
        let recipient = counter(sql, registration.tenant, "recipient", &recipient_hash).await?;
        let portal = counter(sql, registration.tenant, "portal", &portal_hash).await?;
        let pending: i64 = sql
            .query_one(
                "SELECT count(*) FROM customer_registrations WHERE tenant_id=$1 AND decision IS NULL",
                &[&registration.tenant],
            )
            .await?
            .get(0);
        let portal_full = portal.minute >= limits.portal_minute || portal.day >= limits.portal_day;
        let pending_full = pending >= limits.pending;
        let full = portal_full
            || pending_full
            || sender.minute >= limits.sender_minute
            || sender.day >= limits.sender_day
            || recipient.minute >= limits.recipient_minute
            || recipient.day >= limits.recipient_day;
        if full {
            // A refusal writes nothing unless the daily staff alert is due;
            // `mutate` rejects an empty change list, so decide that first.
            if !(portal_full || pending_full) || !alert_due(sql, tenant, &portal_hash).await? {
                return Ok(none);
            }
            tx.mutate(move |sql| {
                Box::pin(async move {
                    let mut changes = Vec::new();
                    mark_alert(sql, tenant, &portal_hash, &mut changes).await?;
                    Ok(((), changes))
                })
            })
            .await?;
            self.notify_registration_limit(tx, tenant).await?;
            return Ok(Admission {
                created: false,
                changed: true,
            });
        }
        tx.mutate(move |sql| {
            Box::pin(async move {
                let inserted = sql
                    .query_one(
                        "INSERT INTO customer_registrations(tenant_id,name,email,company_name,token_hash,expires_at) VALUES($1,$2,$3,$4,$5,now()+make_interval(days => $6::int)) RETURNING id",
                        &[&registration.tenant, &registration.name, &registration.email, &registration.company, &registration.token_hash, &registration.days],
                    )
                    .await?;
                let registration_id: i64 = inserted.get(0);
                let mut changes = vec![Change {
                    kind: "portal.registration".into(),
                    id: registration_id,
                    old: json!(null),
                    new: json!({"status": "unverified"}),
                }];
                purge_stale(sql, registration.tenant, &mut changes).await?;
                for (dimension, hash, previous) in [
                    ("sender", &sender_hash, sender),
                    ("recipient", &recipient_hash, recipient),
                    ("portal", &portal_hash, portal),
                ] {
                    increment(sql, registration.tenant, dimension, hash, previous, &mut changes)
                        .await?;
                }
                Ok(((), changes))
            })
        })
        .await?;
        Ok(Admission {
            created: true,
            changed: true,
        })
    }

    async fn registration_limits(&self, client: &dyn ReadClient) -> Result<Limits> {
        Ok(Limits {
            sender_minute: self
                .limit(client, SENDER_MINUTE_KEY, SENDER_MINUTE_DEFAULT)
                .await?,
            sender_day: self
                .limit(client, SENDER_DAY_KEY, SENDER_DAY_DEFAULT)
                .await?,
            recipient_minute: self
                .limit(client, RECIPIENT_MINUTE_KEY, RECIPIENT_MINUTE_DEFAULT)
                .await?,
            recipient_day: self
                .limit(client, RECIPIENT_DAY_KEY, RECIPIENT_DAY_DEFAULT)
                .await?,
            portal_minute: self
                .limit(client, PORTAL_MINUTE_KEY, PORTAL_MINUTE_DEFAULT)
                .await?,
            portal_day: self
                .limit(client, PORTAL_DAY_KEY, PORTAL_DAY_DEFAULT)
                .await?,
            pending: self.limit(client, PENDING_KEY, PENDING_DEFAULT).await?,
        })
    }

    async fn limit(&self, client: &dyn ReadClient, key: &str, default: i64) -> Result<i64> {
        Ok(self
            .settings
            .get_global(client, key)
            .await?
            .as_i64()
            .unwrap_or(default)
            .clamp(1, 100_000))
    }

    async fn notify_registration_limit(
        &self,
        tx: &mut MutationTransaction<'_>,
        tenant: i64,
    ) -> Result<()> {
        let managers = bilibi_authz::holders(
            tx.sql(),
            tenant,
            MANAGE,
            i64::try_from(MAX_RECIPIENTS).unwrap_or(500),
        )
        .await?;
        let mut live = Vec::new();
        let mut tenant_scope = None;
        for user in managers {
            if let Ok(scope) = self
                .authentication
                .authenticate(tx.sql(), tenant, user, String::new())
                .await
            {
                if scope.actor().kind() == AccountKind::Staff {
                    live.push(user);
                    tenant_scope.get_or_insert(scope);
                }
            }
        }
        if let Some(scope) = tenant_scope {
            self.delivery
                .notifications
                .notify(
                    tx,
                    &scope,
                    &live,
                    &Notice::new("Customer registration reached its limit")
                        .kind("portal.registration_limited")
                        .icon("shield-exclamation")
                        .url(STAFF_PATH)
                        .body("No more account requests are being accepted for this portal right now. Review the waiting requests and the registration limits."),
                )
                .await?;
        }
        Ok(())
    }
}

fn subject_hash(tenant: i64, dimension: &str, subject: &str) -> Vec<u8> {
    digest(&format!(
        "portal-registration-limit-v1|{tenant}|{dimension}|{subject}"
    ))
}

async fn counter(
    sql: &Transaction<'_>,
    tenant: i64,
    dimension: &str,
    subject_hash: &[u8],
) -> Result<Counter> {
    let row = sql
        .query_opt(
            "SELECT (CASE WHEN minute_started_at<=now()-interval '1 minute' THEN 0 ELSE minute_count END)::bigint,(CASE WHEN day_started_at<=now()-interval '1 day' THEN 0 ELSE day_count END)::bigint FROM customer_registration_limits WHERE tenant_id=$1 AND dimension=$2 AND subject_hash=$3 FOR UPDATE",
            &[&tenant, &dimension, &subject_hash],
        )
        .await?;
    Ok(row.map_or_else(Counter::default, |row| Counter {
        minute: row.get(0),
        day: row.get(1),
    }))
}

async fn increment(
    sql: &Transaction<'_>,
    tenant: i64,
    dimension: &str,
    subject_hash: &[u8],
    previous: Counter,
    changes: &mut Vec<Change>,
) -> Result<()> {
    let row = sql
        .query_one(
            "INSERT INTO customer_registration_limits(tenant_id,dimension,subject_hash,minute_count,day_count) VALUES($1,$2,$3,1,1) ON CONFLICT(tenant_id,dimension,subject_hash) DO UPDATE SET minute_count=CASE WHEN customer_registration_limits.minute_started_at<=now()-interval '1 minute' THEN 1 ELSE customer_registration_limits.minute_count+1 END,minute_started_at=CASE WHEN customer_registration_limits.minute_started_at<=now()-interval '1 minute' THEN now() ELSE customer_registration_limits.minute_started_at END,day_count=CASE WHEN customer_registration_limits.day_started_at<=now()-interval '1 day' THEN 1 ELSE customer_registration_limits.day_count+1 END,day_started_at=CASE WHEN customer_registration_limits.day_started_at<=now()-interval '1 day' THEN now() ELSE customer_registration_limits.day_started_at END,alerted_at=CASE WHEN customer_registration_limits.day_started_at<=now()-interval '1 day' THEN NULL ELSE customer_registration_limits.alerted_at END,updated_at=now() RETURNING id",
            &[&tenant, &dimension, &subject_hash],
        )
        .await?;
    changes.push(Change {
        kind: "portal.registration_limit".into(),
        id: row.get(0),
        old: json!({"dimension": dimension, "minute": previous.minute, "day": previous.day}),
        new: json!({"dimension": dimension, "minute": previous.minute + 1, "day": previous.day + 1}),
    });
    Ok(())
}

async fn alert_due(sql: &Transaction<'_>, tenant: i64, portal_hash: &[u8]) -> Result<bool> {
    Ok(sql
        .query_opt(
            "SELECT 1 FROM customer_registration_limits WHERE tenant_id=$1 AND dimension='portal' AND subject_hash=$2 AND alerted_at>now()-interval '1 day'",
            &[&tenant, &portal_hash],
        )
        .await?
        .is_none())
}

async fn mark_alert(
    sql: &Transaction<'_>,
    tenant: i64,
    portal_hash: &[u8],
    changes: &mut Vec<Change>,
) -> Result<bool> {
    let row = sql
        .query_opt(
            "INSERT INTO customer_registration_limits(tenant_id,dimension,subject_hash,alerted_at) VALUES($1,'portal',$2,now()) ON CONFLICT(tenant_id,dimension,subject_hash) DO UPDATE SET alerted_at=now(),updated_at=now() WHERE customer_registration_limits.alerted_at IS NULL OR customer_registration_limits.alerted_at<=now()-interval '1 day' RETURNING id",
            &[&tenant, &portal_hash],
        )
        .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    changes.push(Change {
        kind: "portal.registration_limit".into(),
        id: row.get(0),
        old: json!({"alerted": false}),
        new: json!({"alerted": true}),
    });
    Ok(true)
}

async fn purge_stale(sql: &Transaction<'_>, tenant: i64, changes: &mut Vec<Change>) -> Result<()> {
    let rows = sql
        .query(
            "WITH stale AS (SELECT id FROM customer_registration_limits WHERE tenant_id=$1 AND updated_at<now()-interval '2 days' ORDER BY id LIMIT 100) DELETE FROM customer_registration_limits l USING stale WHERE l.id=stale.id RETURNING l.id,l.dimension",
            &[&tenant],
        )
        .await?;
    changes.extend(rows.iter().map(|row| Change {
        kind: "portal.registration_limit".into(),
        id: row.get(0),
        old: json!({"dimension": row.get::<_, String>(1)}),
        new: json!(null),
    }));
    Ok(())
}
