//! Installation-owner lifecycle for portable staff identities. Customer
//! membership writes may only start or cancel the orphan clock; recovery and
//! final disposal are server commands with fixed system principals.
use crate::{reset::normalize_email, sign_in_identity, UserService};
use bilibi_database::{Change, Context, Error, Result};
use bilibi_mail::{Address, Message};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, ValueType, DEFINITIONS};
use serde_json::json;
use tokio_postgres::GenericClient;

pub const RETENTION_DAYS_KEY: &str = "user.identity_retention_days";
const RECOVER_PRINCIPAL: &str = "identity-recover";
const DELETE_PRINCIPAL: &str = "identity-delete";
const RETENTION_PRINCIPAL: &str = "identity-retention";
const GLOBAL: &[ScopeKind] = &[ScopeKind::Global];

inventory::submit! {
    Contribution::new(DEFINITIONS, RETENTION_DAYS_KEY, Provenance { module: "core/user", source: file!(), line: line!() }, &Setting::Definition(Definition {
        value_type: ValueType::Integer, scopes: GLOBAL, default: || json!(90), nullable: false, encrypted: false,
        minimum: Some(1.0), maximum: Some(3_650.0), validator: None,
        editor: Some(Editor { group: "Session", label: "Unused sign-in retention", help: "Days an installation sign-in is kept after its last membership is removed.", capability: Some(crate::contributions::UPDATE), reveal_duration_ms: 5_000 }),
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityRecovery {
    Recovered,
    Unchanged,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityLifecycleError {
    #[error("No active portable identity uses that email.")]
    NotFound,
    #[error("That email address is not valid.")]
    Invalid,
    #[error("The new email address is already used by another portable identity.")]
    Conflict,
    #[error("The portable identity still has a live membership.")]
    HasMembership,
    #[error(transparent)]
    Other(#[from] Error),
}
impl From<tokio_postgres::Error> for IdentityLifecycleError {
    fn from(error: tokio_postgres::Error) -> Self {
        Self::Other(Error::from(error))
    }
}

pub(crate) async fn mark_orphaned(
    sql: &(impl GenericClient + Sync),
    identity: i64,
) -> Result<Option<Change>> {
    let changed = sql
        .execute(
            "UPDATE user_identities i SET orphaned_at=now(),updated_at=now()
             WHERE i.id=$1 AND i.kind='staff' AND i.status='active' AND i.deleted_at IS NULL
               AND i.orphaned_at IS NULL
               AND NOT EXISTS(SELECT 1 FROM users u WHERE u.identity_id=i.id AND u.deleted_at IS NULL)",
            &[&identity],
        )
        .await?;
    Ok((changed == 1).then(|| Change {
        kind: "user_identity".into(),
        id: identity,
        old: json!({"membership_state":"in_use"}),
        new: json!({"membership_state":"unused","retention_started":true}),
    }))
}

pub(crate) async fn clear_orphaned(
    sql: &(impl GenericClient + Sync),
    identity: i64,
) -> Result<Option<Change>> {
    let changed = sql
        .execute(
            "UPDATE user_identities SET orphaned_at=NULL,updated_at=now() WHERE id=$1 AND orphaned_at IS NOT NULL",
            &[&identity],
        )
        .await?;
    Ok((changed == 1).then(|| Change {
        kind: "user_identity".into(),
        id: identity,
        old: json!({"membership_state":"unused"}),
        new: json!({"membership_state":"in_use","retention_cancelled":true}),
    }))
}

struct Target {
    identity: i64,
    tenant: i64,
    user: i64,
    name: String,
    email: String,
}

impl UserService {
    pub async fn recover_identity_email(
        &self,
        old_email: &str,
        new_email: &str,
    ) -> std::result::Result<IdentityRecovery, IdentityLifecycleError> {
        let old_email = normalize_email(old_email).map_err(|_| IdentityLifecycleError::Invalid)?;
        let new_email = normalize_email(new_email).map_err(|_| IdentityLifecycleError::Invalid)?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let row = client
            .query_opt(
                "SELECT i.id,u.tenant_id,u.id,u.name,i.email FROM user_identities i
                 JOIN LATERAL (
                   SELECT u.tenant_id,u.id,u.name FROM users u WHERE u.identity_id=i.id
                   ORDER BY (u.tenant_id=i.last_tenant_id) DESC,(u.deleted_at IS NULL) DESC,u.tenant_id,u.id LIMIT 1
                 ) u ON true
                 WHERE i.kind='staff' AND i.status='active' AND i.deleted_at IS NULL AND lower(i.email)=$1
                 LIMIT 1",
                &[&old_email],
            )
            .await
            .map_err(Error::from)?
            .ok_or(IdentityLifecycleError::NotFound)?;
        let target = Target {
            identity: row.get(0),
            tenant: row.get(1),
            user: row.get(2),
            name: row.get(3),
            email: row.get(4),
        };
        if target.email == new_email {
            return Ok(IdentityRecovery::Unchanged);
        }
        if client.query_opt("SELECT 1 FROM user_identities WHERE kind='staff' AND deleted_at IS NULL AND lower(email)=$1 AND id<>$2", &[&new_email, &target.identity]).await.map_err(Error::from)?.is_some() {
            return Err(IdentityLifecycleError::Conflict);
        }
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(
                    target.tenant,
                    RECOVER_PRINCIPAL,
                    "user.identity.email_recovered",
                ),
            )
            .await?;
        tx.sql().query_one("SELECT id FROM user_identities WHERE id=$1 AND status='active' AND deleted_at IS NULL FOR UPDATE", &[&target.identity]).await?;
        let identity = target.identity;
        let before = target.email.clone();
        let recovered = new_email.clone();
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute("UPDATE user_identities SET email=$2,email_verified_at=now(),updated_at=now() WHERE id=$1", &[&identity,&recovered]).await?;
            sql.execute("DELETE FROM password_reset_tokens WHERE identity_id=$1", &[&identity]).await?;
            Ok(((), vec![Change { kind:"user_identity".into(), id:identity, old:json!({"email":before}), new:json!({"email":recovered,"recovered_by":RECOVER_PRINCIPAL}) }]))
        })).await?;
        let ended = sign_in_identity::end_everywhere(
            &self.sessions,
            &mut tx,
            target.identity,
            target.user,
            None,
        )
        .await?;
        let sessions = self.sessions.clone();
        let mailer = self.mailer.clone();
        let message = Message::text(
            Address::new(Some(&target.name), &new_email).map_err(|_| IdentityLifecycleError::Invalid)?,
            "Your Bilibi sign-in email was recovered",
            format!("A server administrator changed your Bilibi sign-in email to this address. Every existing session was signed out.\n\n{}/login\n", self.origin),
        );
        tx.after_commit(move || {
            sessions.announce(ended);
            tokio::spawn(async move {
                if let Err(error) = mailer.deliver(&message).await {
                    bilibi_module_registry::server_log!(
                        Warning,
                        "mail: identity recovery notice was not delivered: {error}"
                    );
                }
            });
        });
        tx.commit().await?;
        Ok(IdentityRecovery::Recovered)
    }

    pub async fn delete_orphan_identity(
        &self,
        email: &str,
    ) -> std::result::Result<(), IdentityLifecycleError> {
        let email = normalize_email(email).map_err(|_| IdentityLifecycleError::Invalid)?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let row = client.query_opt("SELECT i.id,coalesce(i.last_tenant_id,(SELECT min(u.tenant_id) FROM users u WHERE u.identity_id=i.id)) FROM user_identities i WHERE i.kind='staff' AND i.status='active' AND i.deleted_at IS NULL AND lower(i.email)=$1", &[&email]).await.map_err(Error::from)?.ok_or(IdentityLifecycleError::NotFound)?;
        let identity: i64 = row.get(0);
        let tenant: Option<i64> = row.get(1);
        let tenant = tenant.ok_or(IdentityLifecycleError::NotFound)?;
        self.dispose_identity(&mut client, tenant, identity, DELETE_PRINCIPAL, false)
            .await
    }

    pub async fn sweep_orphan_identities(
        &self,
    ) -> std::result::Result<u64, IdentityLifecycleError> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let days = self
            .settings
            .get_global(&**client, RETENTION_DAYS_KEY)
            .await?
            .as_i64()
            .filter(|days| *days > 0)
            .ok_or(Error::Internal)?;
        let rows = client.query("SELECT i.id,coalesce(i.last_tenant_id,(SELECT min(u.tenant_id) FROM users u WHERE u.identity_id=i.id)) FROM user_identities i WHERE i.kind='staff' AND i.status='active' AND i.deleted_at IS NULL AND i.orphaned_at < now()-make_interval(days=>$1::int) ORDER BY i.orphaned_at,i.id LIMIT 100", &[&(days as i32)]).await.map_err(Error::from)?;
        let targets: Vec<(i64, i64)> = rows
            .into_iter()
            .filter_map(|row| {
                row.get::<_, Option<i64>>(1)
                    .map(|tenant| (row.get(0), tenant))
            })
            .collect();
        let mut disposed = 0;
        for (identity, tenant) in targets {
            self.dispose_identity(&mut client, tenant, identity, RETENTION_PRINCIPAL, true)
                .await?;
            disposed += 1;
        }
        Ok(disposed)
    }

    async fn dispose_identity(
        &self,
        client: &mut tokio_postgres::Client,
        tenant: i64,
        identity: i64,
        principal: &'static str,
        due: bool,
    ) -> std::result::Result<(), IdentityLifecycleError> {
        let mut tx = self
            .mutations
            .begin(
                client,
                Context::system(tenant, principal, "user.identity.deleted"),
            )
            .await?;
        let row = tx.sql().query_opt("SELECT email,orphaned_at IS NOT NULL FROM user_identities WHERE id=$1 AND kind='staff' AND status='active' AND deleted_at IS NULL FOR UPDATE", &[&identity]).await?.ok_or(IdentityLifecycleError::NotFound)?;
        let old_email: String = row.get(0);
        if !row.get::<_, bool>(1)
            || tx
                .sql()
                .query_opt(
                    "SELECT 1 FROM users WHERE identity_id=$1 AND deleted_at IS NULL LIMIT 1",
                    &[&identity],
                )
                .await?
                .is_some()
        {
            return Err(IdentityLifecycleError::HasMembership);
        }
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute("DELETE FROM password_reset_tokens WHERE identity_id=$1", &[&identity]).await?;
            sql.execute("DELETE FROM user_credential_attempts WHERE identity_id=$1", &[&identity]).await?;
            sql.execute("DELETE FROM user_two_factor WHERE identity_id=$1", &[&identity]).await?;
            sql.execute("DELETE FROM user_sign_ins WHERE identity_id=$1", &[&identity]).await?;
            sql.execute("UPDATE user_identities SET email='deleted-identity-'||id||'@erased.invalid',password_hash='!deleted',email_verified_at=NULL,status='deleted',deleted_at=now(),orphaned_at=NULL,updated_at=now() WHERE id=$1", &[&identity]).await?;
            Ok(((), vec![Change { kind:"user_identity".into(), id:identity, old:json!({"email":old_email,"status":"active"}), new:json!({"email":"erased","status":"deleted","retention_due":due}) }]))
        })).await?;
        tx.commit().await?;
        Ok(())
    }
}
