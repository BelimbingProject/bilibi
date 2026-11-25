//! Reviewed transfer of the installation's one owner receipt.
use crate::{privacy::OWNERSHIP_TRANSFER_KIND, staff_choices::StaffChoice, UserService};
use bilibi_database::{Change, Error, Result};
use bilibi_notifications::Notice;
use bilibi_security::Scope;
use bilibi_tenancy::AccountKind;
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
    pub from_name: String,
    pub to_user: i64,
    pub to_name: String,
}

#[derive(Clone, Debug)]
pub struct OwnershipState {
    pub is_owner: bool,
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

pub(crate) async fn active_staff(
    client: &tokio_postgres::Transaction<'_>,
    tenant: i64,
    user: i64,
) -> Result<Option<bilibi_security::AccountIdentity>> {
    if client
        .query_opt(
            "SELECT u.id FROM users u JOIN user_identities i ON i.id=u.identity_id
         WHERE u.tenant_id=$1 AND u.id=$2 FOR SHARE OF u,i",
            &[&tenant, &user],
        )
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let crate::identity::Standing::Live(identity) =
        crate::identity::standing(client, tenant, user).await?
    else {
        return Ok(None);
    };
    if identity.kind != AccountKind::Staff {
        return Ok(None);
    }
    if let Some(person) = identity.person {
        if !bilibi_person::hold_account_person(client, tenant, person).await? {
            return Ok(None);
        }
    }
    Ok(Some(identity))
}

pub(crate) async fn expire_offers(
    tx: &mut bilibi_database::MutationTransaction<'_>,
    tenant: i64,
    owner: i64,
) -> Result<()> {
    let expired = tx
            .sql()
            .query(
                "UPDATE user_ownership_transfers SET status='expired',resolved_at=clock_timestamp()
                 WHERE tenant_id=$1 AND status='pending' AND (expires_at<=clock_timestamp() OR from_user_id<>$2)
                 RETURNING id,tenant_id,from_user_id,to_user_id",
                &[&tenant, &owner],
            )
            .await?;
    for row in expired {
        tx.record(Change {
                kind: OWNERSHIP_TRANSFER_KIND.into(),
                id: row.get(0),
                old: json!({"tenant_id":row.get::<_,i64>(1),"from_user_id":row.get::<_,i64>(2),"to_user_id":row.get::<_,i64>(3),"status":"pending"}),
                new: json!({"status":"expired"}),
            })
            .await?;
    }
    Ok(())
}

impl UserService {
    pub async fn ownership_state(&self, scope: &Scope) -> Result<OwnershipState> {
        let client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        if direct_browser(&scope).is_err() {
            return Ok(OwnershipState {
                is_owner: false,
                offer: None,
            });
        }
        let is_owner = match bilibi_tenancy::prove_owner(&**client, &scope).await {
            Ok(_) => true,
            Err(Error::Forbidden) => false,
            Err(error) => return Err(error),
        };
        let offer = client
            .query_opt(
                "SELECT x.id,f.name,x.to_user_id,
                        CASE WHEN x.from_user_id=$2 THEN concat(t.name,' (',i.email,', account #',t.id,')') ELSE t.name END
                 FROM user_ownership_transfers x
                 JOIN user_bootstrap b ON b.id=1 AND b.tenant_id=x.tenant_id AND b.user_id=x.from_user_id
                 JOIN users f ON f.tenant_id=x.tenant_id AND f.id=x.from_user_id
                 JOIN users t ON t.tenant_id=x.tenant_id AND t.id=x.to_user_id
                 LEFT JOIN user_identities i ON i.id=t.identity_id AND x.from_user_id=$2
                 WHERE x.tenant_id=$1 AND x.status='pending' AND x.expires_at>clock_timestamp()
                   AND (x.from_user_id=$2 OR x.to_user_id=$2)
                 ORDER BY x.id DESC LIMIT 1",
                &[&scope.tenant(), &scope.user()],
            )
            .await?
            .map(|row| OwnershipOffer {
                id: row.get(0),
                from_name: row.get(1),
                to_user: row.get(2),
                to_name: row.get(3),
            });
        Ok(OwnershipState { is_owner, offer })
    }

    pub(crate) async fn ownership_candidates(
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
        Ok(crate::staff_choices::search_live_staff(
            &**client,
            scope.tenant(),
            text,
            Some(scope.user()),
        )
        .await?)
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
        if target == scope.user()
            || active_staff(tx.sql(), scope.tenant(), target)
                .await?
                .is_none()
        {
            return Err(OwnershipError::Target);
        }
        expire_offers(&mut tx, scope.tenant(), scope.user()).await?;
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
            kind: OWNERSHIP_TRANSFER_KIND.into(),
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
        let receipt = tx
            .sql()
            .query_opt(
                "SELECT b.user_id,b.admin_email FROM user_bootstrap b
             JOIN platform_operator o ON o.tenant_id=b.tenant_id AND o.singleton
             WHERE b.id=1 AND b.tenant_id=$1 FOR UPDATE OF b",
                &[&scope.tenant()],
            )
            .await?
            .ok_or(OwnershipError::Unavailable)?;
        let from: i64 = receipt.get(0);
        let to = scope.user();
        let email = active_staff(tx.sql(), scope.tenant(), to)
            .await?
            .ok_or(OwnershipError::Target)?
            .email;
        let row = tx.sql().query_opt(
            "SELECT expires_at>clock_timestamp() FROM user_ownership_transfers
             WHERE id=$1 AND tenant_id=$2 AND to_user_id=$3 AND from_user_id=$4 AND status='pending'",
            &[&offer, &scope.tenant(), &to, &from],
        ).await?.ok_or(OwnershipError::Unavailable)?;
        if !row.get::<_, bool>(0) {
            return Err(OwnershipError::Expired);
        }
        let old_email: String = receipt.get(1);
        let tenant = scope.tenant();
        tx.sql()
            .execute(
                "UPDATE user_bootstrap SET user_id=$2,admin_email=$3,
                 affiliation_settled_at=COALESCE(affiliation_settled_at,clock_timestamp())
                 WHERE id=1 AND tenant_id=$1",
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
            kind: OWNERSHIP_TRANSFER_KIND.into(),
            id: offer,
            old: json!({"status":"pending"}),
            new: json!({"status":"accepted"}),
        })
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
