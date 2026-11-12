//! Platform sign-ins, owning account credentials while
//! keeping Bilibi's tenant account as the actor. A staff identity may later
//! own several tenant memberships; a customer identity remains tenant-bound.
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_tenancy::AccountKind;
use serde_json::json;
use tokio_postgres::GenericClient;

pub(crate) struct Created {
    pub identity: i64,
    pub user: i64,
    pub change: Change,
}

/// Reserves the home account id and creates its credential identity. The
/// caller inserts the account in the same transaction; both circular foreign
/// keys are deferred until commit, so neither half can survive alone.
pub(crate) async fn create(
    sql: &(impl GenericClient + Sync),
    tenant: i64,
    kind: AccountKind,
    email: &str,
    password_hash: &str,
    verified: bool,
) -> Result<Created> {
    let user: i64 = sql
        .query_one(
            "SELECT nextval(pg_get_serial_sequence('users','id'))::bigint",
            &[],
        )
        .await?
        .get(0);
    let identity: i64 = sql
        .query_one(
            "INSERT INTO user_identities(kind,tenant_id,email,password_hash,email_verified_at,home_tenant_id,home_user_id,last_tenant_id) VALUES($1,CASE WHEN $1='customer' THEN $2::bigint END,$3,$4,CASE WHEN $5 THEN now() END,$2,$6,$2) RETURNING id",
            &[&kind.as_str(), &tenant, &email, &password_hash, &verified, &user],
        )
        .await?
        .get(0);
    Ok(Created {
        identity,
        user,
        change: Change {
            kind: "user_identity".into(),
            id: identity,
            old: json!(null),
            new: json!({
                "kind": kind.as_str(),
                "email": email,
                "password_hash": "set",
                "home_tenant_id": tenant,
                "home_user_id": user,
            }),
        },
    })
}

pub(crate) async fn id(sql: &(impl GenericClient + Sync), tenant: i64, user: i64) -> Result<i64> {
    sql.query_opt(
        "SELECT identity_id FROM users WHERE tenant_id=$1 AND id=$2",
        &[&tenant, &user],
    )
    .await?
    .map(|row| row.get(0))
    .ok_or(Error::NotFound)
}

pub(crate) async fn identity_and_home(
    sql: &(impl GenericClient + Sync),
    tenant: i64,
    user: i64,
) -> Result<(i64, bool)> {
    sql.query_opt(
        "SELECT i.id,i.home_tenant_id=$1 AND i.home_user_id=$2 FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1 AND u.id=$2",
        &[&tenant, &user],
    )
    .await?
    .map(|row| (row.get(0), row.get(1)))
    .ok_or(Error::NotFound)
}

/// Ends what the sign-in's shared password let in, in every tenant: the
/// account's own sessions except the one the cookie token `keep` opens, and the sign-in's accounts in other
/// tenants with their pending sign-ins, sessions and AI connections. Called in
/// the password change, reset and administrator set transactions; the caller
/// announces the returned session ids after its commit.
pub(crate) async fn end_everywhere(
    sessions: &bilibi_session::Sessions,
    tx: &mut MutationTransaction<'_>,
    identity: i64,
    user: i64,
    keep: Option<&str>,
) -> Result<Vec<String>> {
    let tenant = tx.context().tenant;
    let elsewhere: Vec<(i64, i64)> = tx
        .sql()
        .query(
            "SELECT tenant_id,id FROM users WHERE identity_id=$1 AND deleted_at IS NULL AND NOT (tenant_id=$2 AND id=$3) ORDER BY tenant_id,id",
            &[&identity, &tenant, &user],
        )
        .await?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    if !elsewhere.is_empty() {
        let accounts = elsewhere.clone();
        tx.mutate(move |sql| {
            Box::pin(async move {
                let mut changes = vec![Change {
                    kind: "user_identity".into(),
                    id: identity,
                    old: json!({}),
                    new: json!({"other_accounts_signed_out": accounts.len()}),
                }];
                for (tenant, user) in accounts {
                    changes.extend(crate::sign_in::revoke(sql, tenant, user).await?);
                }
                Ok(((), changes))
            })
        })
        .await?;
    }
    sessions.revoke_sign_in(tx, user, &elsewhere, keep).await
}
