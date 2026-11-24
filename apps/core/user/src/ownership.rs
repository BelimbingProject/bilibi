//! Reviewed transfer of the installation's one owner receipt.
use crate::{staff_choices::StaffChoice, UserService};
use bilibi_database::{Change, Error, Result};
use bilibi_notifications::Notice;
use bilibi_security::Scope;
use serde_json::json;

pub const START: &str = "core.user.ownership_transfer.start";
pub const ACCEPT: &str = "core.user.ownership_transfer.accept";
pub const OFFER_MINUTES: i64 = 15;

bilibi_step_up::declare!(
    "core/user",
    bilibi_step_up::Operation::new(START, "Offer installation ownership").always()
);
bilibi_step_up::declare!(
    "core/user",
    bilibi_step_up::Operation::new(ACCEPT, "Accept installation ownership").always()
);

#[derive(Clone, Debug)]
pub struct OwnershipOffer {
    pub id: i64,
    pub from_user: i64,
    pub from_name: String,
    pub to_user: i64,
    pub to_name: String,
}

#[derive(Clone, Debug)]
pub struct OwnershipState {
    pub owner_user: i64,
    pub owner_name: String,
    pub offer: Option<OwnershipOffer>,
}

#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    #[error("only a direct browser session can transfer ownership")]
    BrowserOnly,
    #[error("choose another active staff account in the operator tenant")]
    Target,
    #[error("another ownership offer is still waiting")]
    Pending,
    #[error("the ownership offer is no longer waiting")]
    Unavailable,
    #[error("the ownership offer expired")]
    Expired,
    #[error(transparent)]
    Access(#[from] Error),
}

impl From<tokio_postgres::Error> for OwnershipError {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}

fn direct_browser(scope: &Scope) -> std::result::Result<(), OwnershipError> {
    if scope.impersonator().is_some() || scope.agent().is_some() || scope.principal().is_some() {
        Err(OwnershipError::BrowserOnly)
    } else {
        Ok(())
    }
}

const ACTIVE_STAFF: &str = "u.kind='staff' AND u.status='active' AND u.deleted_at IS NULL AND i.status='active' AND i.deleted_at IS NULL AND p.retired_at IS NULL AND p.system_key IS NULL";

impl UserService {
    pub async fn ownership_state(&self, scope: &Scope) -> Result<OwnershipState> {
        let client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        let Some(owner) = client
            .query_opt(
                "SELECT b.user_id,u.name FROM user_bootstrap b
                 JOIN platform_operator o ON o.tenant_id=b.tenant_id AND o.singleton
                 JOIN users u ON u.tenant_id=b.tenant_id AND u.id=b.user_id
                 WHERE b.id=1 AND b.tenant_id=$1",
                &[&scope.tenant()],
            )
            .await?
        else {
            return Ok(OwnershipState {
                owner_user: 0,
                owner_name: "The platform operator".into(),
                offer: None,
            });
        };
        let offer = client
            .query_opt(
                "SELECT x.id,x.from_user_id,f.name,x.to_user_id,t.name
                 FROM user_ownership_transfers x
                 JOIN users f ON f.tenant_id=x.tenant_id AND f.id=x.from_user_id
                 JOIN users t ON t.tenant_id=x.tenant_id AND t.id=x.to_user_id
                 WHERE x.tenant_id=$1 AND x.status='pending' AND x.expires_at>clock_timestamp()
                   AND (x.from_user_id=$2 OR x.to_user_id=$2)
                 ORDER BY x.id DESC LIMIT 1",
                &[&scope.tenant(), &scope.user()],
            )
            .await?
            .map(|row| OwnershipOffer {
                id: row.get(0),
                from_user: row.get(1),
                from_name: row.get(2),
                to_user: row.get(3),
                to_name: row.get(4),
            });
        Ok(OwnershipState {
            owner_user: owner.get(0),
            owner_name: owner.get(1),
            offer,
        })
    }

    pub async fn ownership_candidates(
        &self,
        scope: &Scope,
        text: &str,
    ) -> std::result::Result<Vec<StaffChoice>, OwnershipError> {
        direct_browser(scope)?;
        let text = text.trim();
        if text.len() > crate::staff_choices::STAFF_SEARCH_MAX {
            return Err(Error::Invalid.into());
        }
        let client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        bilibi_tenancy::prove_owner(&**client, &scope).await?;
        let sql = format!(
            "SELECT u.id,u.name FROM users u
             JOIN user_identities i ON i.id=u.identity_id
             JOIN persons p ON p.tenant_id=u.tenant_id AND p.id=u.person_id
             JOIN platform_operator o ON o.tenant_id=u.tenant_id AND o.singleton
             WHERE u.tenant_id=$1 AND {ACTIVE_STAFF} AND u.id<>$2
               AND ($3='' OR starts_with(lower(u.name),$3) OR position(' '||$3 in lower(u.name))>0)
             ORDER BY lower(u.name),u.id LIMIT $4"
        );
        Ok(client
            .query(
                &sql,
                &[
                    &scope.tenant(),
                    &scope.user(),
                    &text.to_lowercase(),
                    &(crate::staff_choices::STAFF_CHOICE_MAX as i64),
                ],
            )
            .await?
            .into_iter()
            .map(|row| StaffChoice {
                id: row.get(0),
                name: row.get(1),
            })
            .collect())
    }

    pub async fn offer_ownership(
        &self,
        scope: &Scope,
        target: i64,
    ) -> std::result::Result<i64, OwnershipError> {
        direct_browser(scope)?;
        self.step_up
            .require_code(scope, START, &target.to_string())
            .await?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.ownership_transfer.offer"),
            )
            .await?;
        let owner = bilibi_tenancy::prove_owner(tx.sql(), scope).await?;
        let scope = owner.scope();
        direct_browser(scope)?;
        tx.sql()
            .execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                &[&"core/user.ownership-transfer"],
            )
            .await?;
        tx.sql()
            .query_opt(
                &format!(
                    "SELECT u.id FROM users u
                     JOIN user_identities i ON i.id=u.identity_id
                     JOIN persons p ON p.tenant_id=u.tenant_id AND p.id=u.person_id
                     JOIN platform_operator o ON o.tenant_id=u.tenant_id AND o.singleton
                     WHERE u.tenant_id=$1 AND u.id=$2 AND {ACTIVE_STAFF} AND u.id<>$3"
                ),
                &[&scope.tenant(), &target, &scope.user()],
            )
            .await?
            .ok_or(OwnershipError::Target)?;
        let expired = tx
            .sql()
            .query(
                "UPDATE user_ownership_transfers SET status='expired',resolved_at=clock_timestamp()
                 WHERE tenant_id=$1 AND status='pending' AND expires_at<=clock_timestamp()
                 RETURNING id,tenant_id,from_user_id,to_user_id",
                &[&scope.tenant()],
            )
            .await?;
        for row in expired {
            tx.record(Change {
                kind: "user.ownership_transfer".into(),
                id: row.get(0),
                old: json!({"tenant_id":row.get::<_,i64>(1),"from_user_id":row.get::<_,i64>(2),"to_user_id":row.get::<_,i64>(3),"status":"pending"}),
                new: json!({"status":"expired"}),
            })
            .await?;
        }
        let pending: bool = tx
            .sql()
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM user_ownership_transfers WHERE tenant_id=$1 AND status='pending')",
                &[&scope.tenant()],
            )
            .await?
            .get(0);
        if pending {
            return Err(OwnershipError::Pending);
        }
        let tenant = scope.tenant();
        let from = scope.user();
        let id: i64 = tx
            .sql()
            .query_one(
                "INSERT INTO user_ownership_transfers(tenant_id,from_user_id,to_user_id,expires_at)
                 VALUES($1,$2,$3,clock_timestamp()+make_interval(mins=>$4::int)) RETURNING id",
                &[&tenant, &from, &target, &(OFFER_MINUTES as i32)],
            )
            .await?
            .get(0);
        tx.record(Change {
            kind: "user.ownership_transfer".into(),
            id,
            old: json!(null),
            new: json!({"tenant_id":tenant,"from_user_id":from,"to_user_id":target,"status":"pending","expires_in_minutes":OFFER_MINUTES}),
        })
        .await?;
        self.notification_boundary
            .notify(
                &mut tx,
                scope,
                &[target],
                &Notice::new("Installation ownership offered")
                    .kind("ownership_transfer")
                    .body(format!(
                        "You have {OFFER_MINUTES} minutes to accept ownership from {}.",
                        scope.user_name
                    ))
                    .url("/settings/ownership")
                    .icon("shield-check"),
            )
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn accept_ownership(
        &self,
        scope: &Scope,
        offer: i64,
    ) -> std::result::Result<(), OwnershipError> {
        direct_browser(scope)?;
        self.step_up
            .require_code(scope, ACCEPT, &offer.to_string())
            .await?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.ownership_transfer.accept"),
            )
            .await?;
        let scope = self.fresh(tx.sql(), scope).await?;
        direct_browser(&scope)?;
        tx.sql()
            .execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                &[&"core/user.ownership-transfer"],
            )
            .await?;
        let row = tx
            .sql()
            .query_opt(
                &format!(
                    "SELECT x.from_user_id,x.to_user_id,x.expires_at>clock_timestamp(),i.email
                     FROM user_ownership_transfers x
                     JOIN users u ON u.tenant_id=x.tenant_id AND u.id=x.to_user_id
                     JOIN user_identities i ON i.id=u.identity_id
                     JOIN persons p ON p.tenant_id=u.tenant_id AND p.id=u.person_id
                     JOIN platform_operator o ON o.tenant_id=u.tenant_id AND o.singleton
                     WHERE x.id=$1 AND x.tenant_id=$2 AND x.to_user_id=$3 AND x.status='pending'
                       AND {ACTIVE_STAFF} FOR UPDATE OF x"
                ),
                &[&offer, &scope.tenant(), &scope.user()],
            )
            .await?
            .ok_or(OwnershipError::Unavailable)?;
        if !row.get::<_, bool>(2) {
            return Err(OwnershipError::Expired);
        }
        let from: i64 = row.get(0);
        let to: i64 = row.get(1);
        let email: String = row.get(3);
        let receipt = tx
            .sql()
            .query_opt(
                "SELECT user_id,admin_email FROM user_bootstrap
                 WHERE id=1 AND tenant_id=$1 FOR UPDATE",
                &[&scope.tenant()],
            )
            .await?
            .ok_or(OwnershipError::Unavailable)?;
        if receipt.get::<_, i64>(0) != from {
            return Err(OwnershipError::Unavailable);
        }
        let old_email: String = receipt.get(1);
        let tenant = scope.tenant();
        tx.sql()
            .execute(
                "UPDATE user_bootstrap SET user_id=$2,admin_email=$3 WHERE id=1 AND tenant_id=$1",
                &[&tenant, &to, &email],
            )
            .await?;
        tx.sql()
            .execute(
                "UPDATE user_ownership_transfers SET status='accepted',resolved_at=clock_timestamp() WHERE id=$1 AND tenant_id=$2",
                &[&offer, &tenant],
            )
            .await?;
        tx.record(Change {
            kind: "user.bootstrap".into(),
            id: 1,
            old: json!({"user_id":from,"admin_email":old_email}),
            new: json!({"user_id":to,"admin_email":email}),
        })
        .await?;
        tx.record(Change {
            kind: "user.ownership_transfer".into(),
            id: offer,
            old: json!({"status":"pending"}),
            new: json!({"status":"accepted"}),
        })
        .await?;
        self.notification_boundary
            .notify(
                &mut tx,
                &scope,
                &[from, to],
                &Notice::new("Installation ownership transferred")
                    .kind("ownership_transfer")
                    .body("The installation owner has changed.")
                    .url("/settings/ownership")
                    .icon("shield-check"),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn decline_ownership(
        &self,
        scope: &Scope,
        offer: i64,
    ) -> std::result::Result<(), OwnershipError> {
        direct_browser(scope)?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.ownership_transfer.decline"),
            )
            .await?;
        let scope = self.fresh(tx.sql(), scope).await?;
        direct_browser(&scope)?;
        let row = tx
            .sql()
            .query_opt(
                "SELECT expires_at>clock_timestamp() FROM user_ownership_transfers
                 WHERE id=$1 AND tenant_id=$2 AND to_user_id=$3 AND status='pending' FOR UPDATE",
                &[&offer, &scope.tenant(), &scope.user()],
            )
            .await?
            .ok_or(OwnershipError::Unavailable)?;
        if !row.get::<_, bool>(0) {
            return Err(OwnershipError::Expired);
        }
        tx.sql()
            .execute(
                "UPDATE user_ownership_transfers SET status='declined',resolved_at=clock_timestamp() WHERE id=$1 AND tenant_id=$2",
                &[&offer, &scope.tenant()],
            )
            .await?;
        tx.record(Change {
            kind: "user.ownership_transfer".into(),
            id: offer,
            old: json!({"status":"pending"}),
            new: json!({"status":"declined"}),
        })
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
