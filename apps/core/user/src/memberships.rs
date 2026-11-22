//! Tenant membership invitations for issue 147. This mirrors Bilimbi's user
//! invitation boundary while keeping the invited account tenant-owned and the
//! accepted sign-in installation-wide.
use crate::{
    accounts::{email_taken, field_error, validate_email, validate_name, FieldError},
    contributions::INVITE,
    password::{self, check_policy},
    tenant_owner::DEDICATED_OWNER_SIGN_IN,
    UserService,
};
use bilibi_audit::{record_action, Attribution};
use bilibi_database::{Change, Context, Error, Result};
use bilibi_security::Scope;
use bilibi_settings::Target;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::SystemTime;

pub const INVITATION_DAYS_KEY: &str = "user.invitation_days";

#[derive(Clone, Debug, Default)]
pub struct InvitationInput {
    pub name: String,
    pub email: String,
    /// Authorization roles to preassign to the pending tenant account.
    pub roles: Vec<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invitation {
    pub id: i64,
    pub user: i64,
    pub email: String,
}

/// The facts the emailed acceptance page may reveal to the bearer. The
/// address itself stays private; the page only needs to choose between the
/// existing sign-in and new-password flows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvitationAcceptance {
    pub tenant_name: String,
    pub existing_sign_in: bool,
}

pub(crate) fn validate_invitation_input(input: &InvitationInput) -> Vec<FieldError> {
    let mut errors = Vec::new();
    errors.extend(validate_name(&input.name));
    errors.extend(validate_email(&input.email).err());
    errors
}

#[derive(Debug, thiserror::Error)]
pub enum InvitationError {
    #[error("Invalid invitation fields")]
    Validation(Vec<FieldError>),
    #[error("This invitation is invalid or has expired")]
    Link,
    #[error("{DEDICATED_OWNER_SIGN_IN}")]
    DedicatedSignIn,
    #[error(transparent)]
    Access(#[from] Error),
}

impl From<tokio_postgres::Error> for InvitationError {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}

/// Why `invite_member` refuses an impersonated session: inviting someone is
/// account administration, which is never done while viewing as someone else.
const IMPERSONATING_INVITE: &str = "Invitations cannot be sent while viewing as someone else.";

pub(crate) fn digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub(crate) fn token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::Internal)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn token_shaped(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn renewable_invitation(
    client: &tokio_postgres::Client,
    hash: &[u8],
) -> Result<Option<tokio_postgres::Row>> {
    Ok(client.query_opt(
        "SELECT v.tenant_id,v.created_at,v.expires_at FROM user_invitations v JOIN users u ON u.tenant_id=v.tenant_id AND u.id=v.user_id JOIN tenants t ON t.id=v.tenant_id WHERE v.token_hash=$1 AND v.accepted_at IS NULL AND v.revoked_at IS NULL AND v.expires_at<=clock_timestamp() AND u.status='invited' AND u.identity_id IS NULL AND u.deleted_at IS NULL AND t.status='active' AND t.deleted_at IS NULL",
        &[&hash],
    ).await?)
}

async fn record_acceptance_refusal(
    client: &tokio_postgres::Client,
    scope: &Scope,
    reason: &str,
) -> Result<()> {
    record_action(
        client,
        &Attribution::from_scope(scope),
        None,
        "user.invitation.accept.refused",
        json!({"result": "refused", "reason": reason}),
        false,
    )
    .await?;
    Ok(())
}

impl UserService {
    /// Reads an open invitation for its emailed acceptance page. Invalid,
    /// expired, used and revoked links have the same answer.
    pub async fn member_invitation(
        &self,
        raw_token: &str,
    ) -> std::result::Result<InvitationAcceptance, InvitationError> {
        if !token_shaped(raw_token) {
            return Err(InvitationError::Link);
        }
        let client = self.db.get().await.map_err(Error::from)?;
        let row = client
            .query_opt(
                "SELECT t.name,b.invitation_id IS NOT NULL,EXISTS(SELECT 1 FROM user_identities d
                     WHERE lower(d.email)=v.email AND d.kind='staff' AND d.deleted_at IS NULL
                       AND (d.status='active' OR b.invitation_id IS NOT NULL))
                 FROM user_invitations v
                 JOIN users u ON u.tenant_id=v.tenant_id AND u.id=v.user_id
                 JOIN tenants t ON t.id=v.tenant_id AND t.status='active' AND t.deleted_at IS NULL
                 LEFT JOIN user_tenant_owner_bootstraps b
                     ON b.tenant_id=v.tenant_id AND b.user_id=v.user_id AND b.invitation_id=v.id
                 WHERE v.token_hash=$1 AND v.accepted_at IS NULL AND v.revoked_at IS NULL
                   AND v.expires_at>clock_timestamp() AND u.status='invited'
                   AND u.identity_id IS NULL AND u.deleted_at IS NULL",
                &[&digest(raw_token)],
            )
            .await?
            .ok_or(InvitationError::Link)?;
        let existing_sign_in = row.get(2);
        if row.get::<_, bool>(1) && existing_sign_in {
            return Err(InvitationError::DedicatedSignIn);
        }
        Ok(InvitationAcceptance {
            tenant_name: row.get(0),
            existing_sign_in,
        })
    }

    /// Whether the bearer can request a new link for this expired invitation.
    pub async fn member_invitation_can_be_renewed(&self, raw_token: &str) -> Result<bool> {
        if !token_shaped(raw_token) {
            return Ok(false);
        }
        let client = self.db.get().await?;
        Ok(renewable_invitation(&client, &digest(raw_token))
            .await?
            .is_some())
    }

    /// Reissues an expired, still-pending invitation identified by its bearer token.
    /// All other tokens have the same successful response and send no mail.
    pub async fn resend_member_invitation(&self, raw_token: &str) -> Result<()> {
        if !token_shaped(raw_token) {
            return Ok(());
        }
        let old_hash = digest(raw_token);
        let mut client = self.db.get().await?;
        let Some(row) = renewable_invitation(&client, &old_hash).await? else {
            return Ok(());
        };
        let tenant: i64 = row.get(0);
        let old_created_at: SystemTime = row.get(1);
        let old_expires_at: SystemTime = row.get(2);
        let new_token = token()?;
        let new_hash = digest(&new_token);
        let rollback_old_hash = old_hash.clone();
        let rollback_new_hash = new_hash.clone();
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(tenant, crate::sign_in::PRINCIPAL, "user.invitation.reissue"),
            )
            .await?;
        let issued = tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_opt(
                "UPDATE user_invitations v SET token_hash=$3, expires_at=clock_timestamp()+LEAST(GREATEST(v.expires_at-v.created_at, interval '1 day'), interval '60 days'), created_at=clock_timestamp() FROM users u, tenants t WHERE v.tenant_id=$1 AND v.token_hash=$2 AND v.accepted_at IS NULL AND v.revoked_at IS NULL AND v.expires_at<=clock_timestamp() AND u.tenant_id=v.tenant_id AND u.id=v.user_id AND u.status='invited' AND u.identity_id IS NULL AND u.deleted_at IS NULL AND t.id=v.tenant_id AND t.status='active' AND t.deleted_at IS NULL RETURNING v.id,v.email,u.name,t.name",
                &[&tenant, &old_hash, &new_hash],
            ).await?;
            let Some(row) = row else { return Err(Error::Conflict) };
            let id: i64 = row.get(0);
            let details = (id, row.get::<_, String>(1), row.get::<_, String>(2), row.get::<_, String>(3));
            Ok((details, vec![Change { kind: "user.invitation".into(), id, old: json!({"status": "pending"}), new: json!({"status": "pending", "reissued": true}) }]))
        })).await;
        let (id, email, name, tenant_name) = match issued {
            Ok(details) => details,
            Err(Error::Conflict) => {
                tx.rollback().await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let message = self.invitation_mail((&name, &email), &tenant_name, None, &new_token)?;
        let mailer = self.mailer.clone();
        let db = self.db.clone();
        let mutations = self.mutations.clone();
        tx.after_commit(move || {
            tokio::spawn(async move {
                if let Err(error) = mailer.deliver_for(tenant, &message).await {
                    eprintln!("mail: user invitation {id} was not delivered: {error}");
                    let restore = async {
                        let mut client = db.get().await?;
                        let mut tx = mutations.begin(&mut client, Context::system(tenant, crate::sign_in::PRINCIPAL, "user.invitation.reissue_withdrawn")).await?;
                        let restored = tx.mutate(move |sql| Box::pin(async move {
                            let row = sql.query_opt("UPDATE user_invitations SET token_hash=$3,expires_at=$5,created_at=$6 WHERE tenant_id=$1 AND id=$2 AND token_hash=$4 AND accepted_at IS NULL AND revoked_at IS NULL RETURNING id", &[&tenant, &id, &rollback_old_hash, &rollback_new_hash, &old_expires_at, &old_created_at]).await?;
                            if row.is_none() { return Err(Error::Conflict); }
                            Ok(((), vec![Change { kind: "user.invitation".into(), id, old: json!({"status": "pending", "reissued": true}), new: json!({"status": "pending", "delivery_failed": true}) }]))
                        })).await;
                        match restored { Ok(()) => tx.commit().await?, Err(Error::Conflict) => tx.rollback().await?, Err(error) => return Err(error) }
                        Ok::<(), Error>(())
                    }.await;
                    if let Err(error) = restore { eprintln!("mail: user invitation {id} could not be restored: {error}"); }
                }
            });
        });
        tx.commit().await?;
        Ok(())
    }

    /// Prepares one tenant-owned account and emails a one-time invitation.
    /// The response never reveals whether the address already has a sign-in.
    pub async fn invite_member(
        &self,
        scope: &Scope,
        input: &InvitationInput,
    ) -> std::result::Result<Invitation, InvitationError> {
        if scope.impersonator().is_some() {
            return Err(InvitationError::Access(Error::Refused(
                IMPERSONATING_INVITE.into(),
            )));
        }
        let mut errors = Vec::new();
        errors.extend(validate_name(&input.name));
        let email = match validate_email(&input.email) {
            Ok(email) => Some(email),
            Err(error) => {
                errors.push(error);
                None
            }
        };
        if !errors.is_empty() {
            return Err(InvitationError::Validation(errors));
        }
        if input.roles.len() > 100
            || input.roles.iter().any(|role| *role <= 0)
            || input
                .roles
                .iter()
                .enumerate()
                .any(|(at, role)| input.roles[..at].contains(role))
        {
            return Err(InvitationError::Access(Error::Invalid));
        }
        let (name, email) = (input.name.trim().to_owned(), email.expect("validated"));
        let raw_token = token()?;
        let token_hash = digest(&raw_token);
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.invitation.create"),
            )
            .await?;
        let scope = self.fresh(tx.sql(), scope).await?;
        bilibi_authz::authorize_key(tx.sql(), &scope, INVITE).await?;
        let days = self
            .settings
            .get(tx.sql(), &scope, Target::Tenant, INVITATION_DAYS_KEY)
            .await?
            .value
            .as_i64()
            .unwrap_or(7)
            .clamp(1, 60) as i32;
        let tenant = scope.tenant();
        let inviter = scope.user();
        if tx
            .sql()
            .query_opt(
                "SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id AND i.kind='staff' AND i.deleted_at IS NULL WHERE u.tenant_id=$1 AND lower(i.email)=$2 AND u.deleted_at IS NULL",
                &[&tenant, &email],
            )
            .await?
            .is_some()
        {
            return Err(InvitationError::Validation(vec![field_error(
                "email",
                "already belongs to this workspace",
            )]));
        }
        let tenant_name: String = tx
            .sql()
            .query_one(
                "SELECT name FROM tenants WHERE id=$1 AND status='active' AND deleted_at IS NULL FOR SHARE",
                &[&tenant],
            )
            .await?
            .get(0);
        let (insert_name, insert_email) = (name.clone(), email.clone());
        let invitation = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let mut changes = Vec::new();
                    for row in sql
                        .query(
                            "UPDATE user_invitations SET revoked_at=now() WHERE tenant_id=$1 AND email=$2 AND accepted_at IS NULL AND revoked_at IS NULL RETURNING id,user_id",
                            &[&tenant, &insert_email],
                        )
                        .await?
                    {
                        let invitation_id: i64 = row.get(0);
                        let old_user: i64 = row.get(1);
                        sql.execute(
                            "UPDATE users SET status='deleted',deleted_at=now(),version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 AND status='invited' AND identity_id IS NULL",
                            &[&tenant, &old_user],
                        )
                        .await?;
                        changes.push(Change {
                            kind: "user.invitation".into(),
                            id: invitation_id,
                            old: json!({"status": "pending"}),
                            new: json!({"status": "revoked", "reason": "replaced"}),
                        });
                        changes.push(Change {
                            kind: "user".into(),
                            id: old_user,
                            old: json!({"status": "invited"}),
                            new: json!({"status": "deleted", "reason": "invitation_replaced"}),
                        });
                    }
                    let user: i64 = sql
                        .query_one(
                            "INSERT INTO users(tenant_id,name,status) VALUES($1,$2,'invited') RETURNING id",
                            &[&tenant, &insert_name],
                        )
                        .await?
                        .get(0);
                    let id: i64 = sql
                        .query_one(
                            "INSERT INTO user_invitations(tenant_id,user_id,email,token_hash,invited_by,expires_at) VALUES($1,$2,$3,$4,$5,now()+make_interval(days=>$6::int)) RETURNING id",
                            &[&tenant, &user, &insert_email, &token_hash, &inviter, &days],
                        )
                        .await?
                        .get(0);
                    changes.push(Change {
                        kind: "user".into(),
                        id: user,
                        old: json!(null),
                        new: json!({"name": insert_name, "status": "invited"}),
                    });
                    changes.push(Change {
                        kind: "user.invitation".into(),
                        id,
                        old: json!(null),
                        new: json!({"user_id": user, "email": insert_email, "status": "pending"}),
                    });
                    Ok((Invitation { id, user, email: insert_email }, changes))
                })
            })
            .await?;
        for role in &input.roles {
            self.sso_grants
                .assign_role_with_in(
                    &mut tx,
                    &scope,
                    invitation.user,
                    *role,
                    true,
                    &bilibi_authz::grants::Assignment::default(),
                )
                .await?;
        }
        let tenant = scope.tenant();
        let message =
            self.invitation_mail((&name, &email), &tenant_name, Some(days), &raw_token)?;
        let (mailer, id) = (self.mailer.clone(), invitation.id);
        tx.after_commit(move || {
            tokio::spawn(async move {
                if let Err(error) = mailer.deliver_for(tenant, &message).await {
                    eprintln!("mail: user invitation {id} was not delivered: {error}");
                }
            });
        });
        tx.commit().await?;
        Ok(invitation)
    }

    /// Revokes one open invitation and removes its still-pending tenant
    /// account. Accepted, already-revoked and unknown invitations are the
    /// same not-found answer.
    pub async fn revoke_member_invitation(
        &self,
        scope: &Scope,
        invitation: i64,
    ) -> std::result::Result<(), InvitationError> {
        if invitation <= 0 {
            return Err(InvitationError::Link);
        }
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.invitation.revoke"),
            )
            .await?;
        let scope = self.fresh(tx.sql(), scope).await?;
        bilibi_authz::authorize_key(tx.sql(), &scope, INVITE).await?;
        let tenant = scope.tenant();
        tx.mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_opt(
                        "UPDATE user_invitations SET revoked_at=now() WHERE tenant_id=$1 AND id=$2 AND accepted_at IS NULL AND revoked_at IS NULL RETURNING user_id",
                        &[&tenant, &invitation],
                    )
                    .await?
                    .ok_or(Error::NotFound)?;
                let user: i64 = row.get(0);
                let changed = sql
                    .execute(
                        "UPDATE users SET status='deleted',deleted_at=now(),version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 AND status='invited' AND identity_id IS NULL AND deleted_at IS NULL",
                        &[&tenant, &user],
                    )
                    .await?;
                if changed != 1 {
                    return Err(Error::NotFound);
                }
                Ok((
                    (),
                    vec![
                        Change {
                            kind: "user.invitation".into(),
                            id: invitation,
                            old: json!({"status": "pending"}),
                            new: json!({"status": "revoked"}),
                        },
                        Change {
                            kind: "user".into(),
                            id: user,
                            old: json!({"status": "invited"}),
                            new: json!({"status": "deleted", "reason": "invitation_revoked"}),
                        },
                    ],
                ))
            })
        })
        .await
        .map_err(|error| match error {
            Error::NotFound => InvitationError::Link,
            other => InvitationError::Access(other),
        })?;
        tx.commit().await?;
        Ok(())
    }

    /// Accepts an invitation with the already authenticated platform sign-in.
    /// The address must match, but the target tenant never learns the other
    /// membership through the result or its audit payload.
    pub async fn accept_member_invitation(
        &self,
        signed_in: &Scope,
        raw_token: &str,
    ) -> std::result::Result<i64, InvitationError> {
        if signed_in.impersonator().is_some() {
            let client = self.db.get().await.map_err(Error::from)?;
            record_acceptance_refusal(&client, signed_in, "impersonating").await?;
            return Err(Error::Forbidden.into());
        }
        if !token_shaped(raw_token) {
            return Err(InvitationError::Link);
        }
        let mut client = self.db.get().await.map_err(Error::from)?;
        self.accept_with_identity(&mut client, raw_token, signed_in)
            .await
    }

    /// Accepts an invitation for a new address and creates its home sign-in.
    pub async fn accept_new_member_invitation(
        &self,
        raw_token: &str,
        password: &str,
    ) -> std::result::Result<i64, InvitationError> {
        if !token_shaped(raw_token) {
            return Err(InvitationError::Link);
        }
        if let Err(error) = check_policy(password) {
            return Err(InvitationError::Validation(vec![field_error(
                "password",
                error.to_string(),
            )]));
        }
        let hash = password::hash(password).await?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let row = client
            .query_opt(
                "SELECT i.tenant_id,i.user_id,i.email,b.invitation_id IS NOT NULL
                 FROM user_invitations i
                 JOIN users u ON u.tenant_id=i.tenant_id AND u.id=i.user_id
                 LEFT JOIN user_tenant_owner_bootstraps b
                     ON b.tenant_id=i.tenant_id AND b.user_id=i.user_id AND b.invitation_id=i.id
                 WHERE i.token_hash=$1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL
                   AND i.expires_at>clock_timestamp() AND u.status='invited'
                   AND u.identity_id IS NULL AND u.deleted_at IS NULL",
                &[&digest(raw_token)],
            )
            .await?
            .ok_or(InvitationError::Link)?;
        let (tenant, user, email): (i64, i64, String) = (row.get(0), row.get(1), row.get(2));
        let owner: bool = row.get(3);
        if email_taken(&**client, &email, None).await? {
            return Err(if owner {
                InvitationError::DedicatedSignIn
            } else {
                InvitationError::Link
            });
        }
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::user(tenant, user, "user.invitation.accept"),
            )
            .await?;
        self.lock_tenant_owner_in(&tx, tenant, user).await?;
        let invitation =
            match finish_new_acceptance(&mut tx, raw_token, tenant, user, email, hash).await {
                Ok(invitation) => invitation,
                Err(error) => {
                    let refusal = match error {
                        Error::Conflict if owner => Err(InvitationError::DedicatedSignIn),
                        Error::NotFound | Error::Conflict => Err(InvitationError::Link),
                        other => Err(other.into()),
                    };
                    tx.rollback().await?;
                    return refusal;
                }
            };
        self.settle_tenant_owner_in(&mut tx, tenant, user, invitation)
            .await?;
        tx.commit().await?;
        Ok(user)
    }

    async fn accept_with_identity(
        &self,
        client: &mut tokio_postgres::Client,
        raw_token: &str,
        signed_in: &Scope,
    ) -> std::result::Result<i64, InvitationError> {
        let row = client
            .query_opt(
                "SELECT v.tenant_id,v.user_id FROM user_invitations v JOIN users u ON u.tenant_id=v.tenant_id AND u.id=v.user_id WHERE v.token_hash=$1 AND v.accepted_at IS NULL AND v.revoked_at IS NULL AND v.expires_at>clock_timestamp() AND u.status='invited' AND u.identity_id IS NULL AND u.deleted_at IS NULL",
                &[&digest(raw_token)],
            )
            .await?
            .ok_or(InvitationError::Link)?;
        let (tenant, user): (i64, i64) = (row.get(0), row.get(1));
        let mut tx = self
            .mutations
            .begin(
                client,
                Context::user(tenant, user, "user.invitation.accept"),
            )
            .await?;
        let identity = match self.hold_invitation_sign_in(tx.sql(), signed_in).await {
            Ok(identity) => identity,
            Err(error @ (Error::Unauthorized | Error::Forbidden | Error::NotFound)) => {
                tx.rollback().await?;
                record_acceptance_refusal(client, signed_in, "source_session_unavailable").await?;
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        if tx
            .sql()
            .query_opt(
                "SELECT 1 FROM user_tenant_owner_bootstraps b
                 JOIN user_invitations i ON i.tenant_id=b.tenant_id AND i.id=b.invitation_id
                 WHERE b.tenant_id=$1 AND b.user_id=$2 AND i.user_id=b.user_id AND i.token_hash=$3",
                &[&tenant, &user, &digest(raw_token)],
            )
            .await?
            .is_some()
        {
            tx.rollback().await?;
            return Err(InvitationError::DedicatedSignIn);
        }
        if let Err(error) = finish_acceptance(&mut tx, raw_token, tenant, user, identity).await {
            let refusal = match error {
                Error::NotFound => Err(InvitationError::Link),
                other => Err(other.into()),
            };
            tx.rollback().await?;
            return refusal;
        }
        tx.commit().await?;
        Ok(user)
    }

    async fn hold_invitation_sign_in(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        scope: &Scope,
    ) -> Result<i64> {
        bilibi_person::hold_account_standing(tx, scope).await?;
        let person = scope.actor().person().map(|person| person.get());
        let row = tx
            .query_opt(
                "SELECT i.id FROM tenants t JOIN users u ON u.tenant_id=t.id
             JOIN user_identities i ON i.id=u.identity_id
             WHERE t.id=$1 AND u.id=$2 AND t.status='active' AND t.deleted_at IS NULL
               AND u.kind='staff' AND u.status='active' AND u.deleted_at IS NULL
               AND u.person_id IS NOT DISTINCT FROM $3
               AND i.kind='staff' AND i.status='active' AND i.deleted_at IS NULL
             FOR SHARE OF t,u,i",
                &[&scope.tenant(), &scope.user(), &person],
            )
            .await?
            .ok_or(Error::Unauthorized)?;
        self.fresh(tx, scope).await?;
        self.sessions.hold_current(tx, scope).await?;
        Ok(row.get(0))
    }
}

async fn finish_new_acceptance(
    tx: &mut bilibi_database::MutationTransaction<'_>,
    raw_token: &str,
    tenant: i64,
    user: i64,
    email: String,
    hash: String,
) -> Result<i64> {
    let token_hash = digest(raw_token);
    tx.mutate(move |sql| {
        Box::pin(async move {
            let invitation: i64 = sql
                .query_opt(
                    "UPDATE user_invitations SET accepted_at=now() WHERE tenant_id=$1 AND user_id=$2 AND token_hash=$3 AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at>clock_timestamp() RETURNING id",
                    &[&tenant, &user, &token_hash],
                )
                .await?
                .ok_or(Error::NotFound)?
                .get(0);
            let identity: i64 = sql
                .query_opt(
                    "INSERT INTO user_identities(kind,email,password_hash,email_verified_at,home_tenant_id,home_user_id,last_tenant_id)
                     VALUES('staff',$1,$2,now(),$3,$4,$3)
                     ON CONFLICT (lower(email)) WHERE deleted_at IS NULL AND kind='staff' DO NOTHING
                     RETURNING id",
                    &[&email, &hash, &tenant, &user],
                )
                .await?
                .ok_or(Error::Conflict)?
                .get(0);
            let changed = sql
                .execute(
                    "UPDATE users SET identity_id=$3,status='active',version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 AND status='invited' AND identity_id IS NULL AND deleted_at IS NULL",
                    &[&tenant, &user, &identity],
                )
                .await?;
            if changed != 1 {
                return Err(Error::NotFound);
            }
            Ok((
                invitation,
                vec![
                    Change {
                        kind: "user_identity".into(),
                        id: identity,
                        old: json!(null),
                        new: json!({
                            "kind": "staff",
                            "email": email,
                            "password_hash": "set",
                            "home_tenant_id": tenant,
                            "home_user_id": user,
                        }),
                    },
                    Change {
                        kind: "user".into(),
                        id: user,
                        old: json!({"status": "invited"}),
                        new: json!({"status": "active", "identity_id": identity}),
                    },
                    Change {
                        kind: "user.invitation".into(),
                        id: invitation,
                        old: json!({"status": "pending"}),
                        new: json!({"status": "accepted"}),
                    },
                ],
            ))
        })
    })
    .await
}

async fn finish_acceptance(
    tx: &mut bilibi_database::MutationTransaction<'_>,
    raw_token: &str,
    tenant: i64,
    user: i64,
    identity: i64,
) -> Result<()> {
    let token_hash = digest(raw_token);
    tx.mutate(move |sql| {
        Box::pin(async move {
            let invitation: i64 = sql
                .query_opt(
                    "UPDATE user_invitations v SET accepted_at=now()
                     WHERE v.tenant_id=$1 AND v.user_id=$2 AND v.token_hash=$3
                       AND v.accepted_at IS NULL AND v.revoked_at IS NULL AND v.expires_at>clock_timestamp()
                       AND EXISTS (SELECT 1 FROM user_identities i
                           WHERE i.id=$4 AND lower(i.email)=v.email AND i.kind='staff'
                             AND i.status='active' AND i.deleted_at IS NULL FOR SHARE OF i)
                       AND NOT EXISTS (SELECT 1 FROM users m WHERE m.tenant_id=v.tenant_id
                           AND m.identity_id=$4 AND m.deleted_at IS NULL)
                     RETURNING v.id",
                    &[&tenant, &user, &token_hash, &identity],
                )
                .await?
                .ok_or(Error::NotFound)?
                .get(0);
            let changed = sql
                .execute(
                    "UPDATE users SET identity_id=$3,status='active',version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 AND status='invited' AND identity_id IS NULL AND deleted_at IS NULL",
                    &[&tenant, &user, &identity],
                )
                .await?;
            if changed != 1 {
                return Err(Error::NotFound);
            }
            Ok((
                (),
                vec![
                    Change {
                        kind: "user".into(),
                        id: user,
                        old: json!({"status": "invited"}),
                        new: json!({"status": "active", "identity_id": identity}),
                    },
                    Change {
                        kind: "user.invitation".into(),
                        id: invitation,
                        old: json!({"status": "pending"}),
                        new: json!({"status": "accepted"}),
                    },
                ],
            ))
        })
    })
    .await
}
