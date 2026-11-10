//! The calendar feed: a read-only iCalendar link per person, and its key.
//!
//! **The key is one more key to the same door.** The feed reads through the
//! same `Activities::list` and visibility rule as the Activities page and the
//! agent API, acting as the key's owner under their current permissions:
//! when they lose `core.activity.view` or are deactivated, the feed stops
//! showing what they can no longer see, and a deleted or deactivated account
//! gets the one refusal. Only the SHA-256 of the key is stored; the key is
//! shown once, when it is made. README, Calendar feed.
use crate::{
    ics, Activities, Filter, Mine, Sort, State, WriteError, WriteResult, CALENDAR_LIMIT,
    MANAGE_FEEDS, VIEW,
};
use bilibi_authz::authorize_proof;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_tenancy::{ReadClient, Scope};
use bilibi_ui::endpoint::{Cache, Endpoint, EndpointFuture, EndpointRequest, Reply};
use jiff::{SignedDuration, Timestamp};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::SystemTime};

/// The audit kind of a person's feed key.
pub const FEED_RECORD: &str = "core.activity.feed_key";
/// Making or replacing a feed key asks for a fresh second factor on every
/// action, on every channel, development included: a durable bearer link
/// that outlives the session. An agent presents the operator's code as
/// `step_up_code`; the page asks for it in its dialog.
pub const STEP_UP_FEED_KEY: &str = "core.activity.feed_key.create";
bilibi_step_up::declare!(
    "core/activity",
    bilibi_step_up::Operation::new(STEP_UP_FEED_KEY, "Make a calendar feed link").always()
);
/// The feed's address; the key is `{token}`.
pub const FEED_PATH: &str = "/feeds/activities/{token}";
/// How far back the feed reaches, and how far ahead.
const PAST_DAYS: i64 = 30;
const AHEAD_DAYS: i64 = 365;
/// Admin list size.
const KEY_LIMIT: i64 = 200;

/// A person's key as it is shown: when it was made and last read. Never the
/// key itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedKey {
    pub user: i64,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
}

fn timestamp(at: SystemTime) -> Result<Timestamp> {
    Timestamp::try_from(at).map_err(|_| Error::Internal)
}

fn digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// 256 random bits as 64 hex characters.
fn new_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::Internal)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn token_shaped(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The reader holds `capability`, and is not an operator or system
/// principal acting for someone else.
async fn own(client: &dyn ReadClient, scope: &Scope, capability: &str) -> WriteResult<()> {
    authorize_proof(client, scope, capability).await?;
    if scope.impersonator().is_some() || scope.principal().is_some() {
        return Err(WriteError::Forbidden);
    }
    Ok(())
}

impl Activities {
    /// The reader's own feed key, if they have one.
    pub async fn feed_key(&self, scope: &Scope) -> Result<Option<FeedKey>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        authorize_proof(&**client, &scope, VIEW).await?;
        let row = client
            .query_opt(
                "SELECT created_at,last_used_at FROM core_activity_feed_keys WHERE tenant_id=$1 AND user_id=$2",
                &[&scope.tenant(), &scope.user()],
            )
            .await?;
        row.map(|row| {
            Ok(FeedKey {
                user: scope.user(),
                created_at: timestamp(row.get(0))?,
                last_used_at: row
                    .get::<_, Option<SystemTime>>(1)
                    .map(timestamp)
                    .transpose()?,
            })
        })
        .transpose()
    }

    /// Every feed key of the tenant, newest first, for an administrator who
    /// holds `core.activity.feed.manage`.
    pub async fn feed_keys(&self, scope: &Scope) -> Result<Vec<FeedKey>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        authorize_proof(&**client, &scope, MANAGE_FEEDS).await?;
        client
            .query(
                "SELECT user_id,created_at,last_used_at FROM core_activity_feed_keys WHERE tenant_id=$1 ORDER BY created_at DESC,user_id LIMIT $2",
                &[&scope.tenant(), &KEY_LIMIT],
            )
            .await?
            .iter()
            .map(|row| {
                Ok(FeedKey {
                    user: row.get(0),
                    created_at: timestamp(row.get(1))?,
                    last_used_at: row.get::<_, Option<SystemTime>>(2).map(timestamp).transpose()?,
                })
            })
            .collect()
    }

    /// The checks `create_feed_key` makes, with no write.
    pub async fn preview_create_feed_key(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
    ) -> WriteResult<()> {
        own(client, scope, VIEW).await
    }

    /// Makes the reader's feed key, replacing the one they had (its link
    /// stops working at once), in the caller's transaction. Returns the key
    /// and its text, which is shown once and never stored.
    pub async fn create_feed_key(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
    ) -> WriteResult<(FeedKey, String)> {
        let scope = self.fresh(tx.sql(), scope).await?;
        own(tx.sql(), &scope, VIEW).await?;
        // A link anyone holding it can read: ask for the second factor first.
        self.step_up
            .require(
                &scope,
                STEP_UP_FEED_KEY,
                &format!("feed_key:{}", scope.user()),
            )
            .await?;
        let token = new_token()?;
        let hash = digest(&token);
        let (tenant, user) = (scope.tenant(), scope.user());
        let created = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let old = sql
                        .query_opt(
                            "SELECT created_at FROM core_activity_feed_keys WHERE tenant_id=$1 AND user_id=$2 FOR UPDATE",
                            &[&tenant, &user],
                        )
                        .await?
                        .map(|row| row.get::<_, SystemTime>(0));
                    let row = sql
                        .query_one(
                            "INSERT INTO core_activity_feed_keys(tenant_id,user_id,key_hash) VALUES ($1,$2,$3)
                             ON CONFLICT (tenant_id,user_id) DO UPDATE SET key_hash=EXCLUDED.key_hash,created_at=now(),last_used_at=NULL
                             RETURNING created_at",
                            &[&tenant, &user, &hash],
                        )
                        .await?;
                    let created: SystemTime = row.get(0);
                    let at = |t: SystemTime| timestamp(t).map(|t| t.to_string());
                    let change = Change {
                        kind: FEED_RECORD.into(),
                        id: user,
                        old: match old {
                            Some(old) => json!({"active": true, "created_at": at(old)?}),
                            None => json!({"active": false}),
                        },
                        new: json!({"active": true, "created_at": at(created)?}),
                    };
                    Ok((created, vec![change]))
                })
            })
            .await?;
        Ok((
            FeedKey {
                user,
                created_at: timestamp(created)?,
                last_used_at: None,
            },
            token,
        ))
    }

    /// The checks `revoke_feed_key` makes, with no write.
    pub async fn preview_revoke_feed_key(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        user: Option<i64>,
    ) -> WriteResult<i64> {
        let target = user.unwrap_or_else(|| scope.user());
        if target == scope.user() {
            own(client, scope, VIEW).await?;
        } else {
            own(client, scope, MANAGE_FEEDS).await?;
        }
        let found = client
            .query_opt(
                "SELECT 1 FROM core_activity_feed_keys WHERE tenant_id=$1 AND user_id=$2",
                &[&scope.tenant(), &target],
            )
            .await?;
        found.map(|_| target).ok_or(WriteError::NotFound)
    }

    /// Revokes a feed key, in the caller's transaction: the reader's own, or
    /// any person's in the tenant for one who holds
    /// `core.activity.feed.manage`. The link stops working at once.
    pub async fn revoke_feed_key(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        user: Option<i64>,
    ) -> WriteResult<i64> {
        let scope = self.fresh(tx.sql(), scope).await?;
        let target = self.preview_revoke_feed_key(tx.sql(), &scope, user).await?;
        let tenant = scope.tenant();
        tx.mutate(move |sql| {
            Box::pin(async move {
                let rows = sql
                    .query(
                        "DELETE FROM core_activity_feed_keys WHERE tenant_id=$1 AND user_id=$2 RETURNING created_at",
                        &[&tenant, &target],
                    )
                    .await?;
                let changes = rows
                    .iter()
                    .map(|row| {
                        Ok(Change {
                            kind: FEED_RECORD.into(),
                            id: target,
                            old: json!({"active": true, "created_at": timestamp(row.get(0))?.to_string()}),
                            new: json!({"active": false}),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(((), changes))
            })
        })
        .await?;
        Ok(target)
    }

    /// The iCalendar text the key opens, or `None` for a key that does not
    /// exist, was revoked or replaced, or whose owner can no longer be acted
    /// as (deactivated, or without `core.activity.view`). The calendar is the
    /// owner's own `Activities::list` view: the same rule as the page.
    pub async fn feed(&self, token: &str) -> Result<Option<String>> {
        if !token_shaped(token) {
            return Ok(None);
        }
        let client = self.db.get().await?;
        let hash = digest(token);
        let Some(row) = client
            .query_opt(
                "SELECT tenant_id,user_id FROM core_activity_feed_keys WHERE key_hash=$1",
                &[&hash],
            )
            .await?
        else {
            return Ok(None);
        };
        let (tenant, user): (i64, i64) = (row.get(0), row.get(1));
        let scope = match self
            .authentication
            .authenticate_job(&**client, tenant, user, None)
            .await
        {
            Ok(scope) => scope,
            Err(Error::Unauthorized | Error::Forbidden | Error::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        drop(client);
        let now = Timestamp::now();
        // The owner's own calendar: what they take part in, never the
        // tenant-wide oversight set, which would put every private activity
        // on a third-party calendar server and crowd out their own at 501.
        let filter = Filter {
            state: State::All,
            mine: Mine::TakingPart,
            between: Some((
                now - SignedDuration::from_hours(24 * PAST_DAYS),
                now + SignedDuration::from_hours(24 * AHEAD_DAYS),
            )),
            sort: Sort::When,
            page: 1,
            page_size: CALENDAR_LIMIT,
            ..Filter::default()
        };
        let page = match self.list(&scope, &filter).await {
            Ok(page) => page,
            Err(Error::Unauthorized | Error::Forbidden | Error::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        // When it was last read: a usage stamp on the key, at most once a
        // minute, not a business change.
        let client = self.db.get().await?;
        client
            .execute(
                "UPDATE core_activity_feed_keys SET last_used_at=now() WHERE key_hash=$1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
                &[&hash],
            )
            .await?;
        Ok(Some(ics::render(&page.entries)))
    }
}

/// The public endpoint's owner (`core/activity.feed`): the token in the path
/// is the only credential, and any refusal is the one 404.
pub(crate) struct FeedEndpoint(pub Arc<Activities>);
impl Endpoint<Error> for FeedEndpoint {
    fn answer<'a>(&'a self, request: EndpointRequest<'a>) -> EndpointFuture<'a, Error> {
        Box::pin(async move {
            match self.0.feed(request.token()).await? {
                Some(body) => Ok(Reply::Bytes {
                    bytes: body.into_bytes(),
                    content_type: "text/calendar; charset=utf-8".into(),
                    cache: Cache::NoStore,
                }),
                None => Err(Error::NotFound),
            }
        })
    }
}
