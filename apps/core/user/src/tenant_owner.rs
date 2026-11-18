//! The first owner of a customer tenant, prepared by a deployment operator.
//!
//! The server command carries authority only into this narrow bootstrap: a
//! system-attributed transaction proves a live customer tenant and its primary
//! company, proves that it has no account or earlier owner receipt, creates an
//! ordinary membership invitation, and grants the contributed `core_admin`
//! role. The emailed link remains the normal credential setup path.
//!
//! Acceptance settles the receipt in the invitation transaction by creating
//! the owner's Person, business affiliation and first desk through their
//! owners. A regular invitation has no receipt and keeps its ordinary path.
use crate::{
    accounts::{validate_email, validate_name, FieldError},
    memberships::{digest, token},
    Invitation, UserService,
};
use bilibi_affiliation::{AffiliationInput, EffectiveDates, OrganisationId};
use bilibi_audit::{record_action, Attribution};
use bilibi_authz::grants::Grants;
use bilibi_database::{Change, Context, Error, MutationTransaction, Result};
use bilibi_mail::{Address, Message};
use bilibi_person::{PersonInput, PersonKind};
use bilibi_settings::Setting;
use serde_json::json;
use tokio_postgres::GenericClient;

pub const TENANT_OWNER_PRINCIPAL: &str = "tenant-provision";
const TENANT_OWNER_ACTION: &str = "user.tenant_owner.bootstrap";
const TENANT_OWNER_INVITED: &str = "user.tenant_owner.invited";

/// The customer tenant and primary company reported by Company provisioning.
#[derive(Clone, Copy, Debug)]
pub struct TenantCompany {
    pub tenant: i64,
    pub company: i64,
}

/// The runtime details of a customer tenant's first owner. Credentials are
/// deliberately absent: the owner chooses them through the emailed invitation.
#[derive(Clone, Debug, Default)]
pub struct FirstTenantOwner {
    pub name: String,
    pub email: String,
}

/// What the deployment bootstrap prepared.
#[derive(Clone, Debug)]
pub struct FirstTenantOwnerCreated {
    pub invitation: i64,
    pub user: i64,
    pub email: String,
    pub role: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum FirstTenantOwnerError {
    #[error("the tenant already has {count} account(s), the first being {first}")]
    AccountsExist { count: i64, first: String },
    #[error("Invalid owner fields")]
    Validation(Vec<FieldError>),
    #[error("not a live customer tenant and its primary company")]
    NotCustomer,
    #[error(transparent)]
    Access(#[from] Error),
}

impl From<tokio_postgres::Error> for FirstTenantOwnerError {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}

pub fn validate_first_tenant_owner(input: &FirstTenantOwner) -> Vec<FieldError> {
    let mut errors = Vec::new();
    errors.extend(validate_name(&input.name));
    errors.extend(validate_email(&input.email).err());
    errors
}

async fn accounts(
    client: &(impl GenericClient + Sync),
    tenant: i64,
) -> Result<Option<(i64, String)>> {
    let row = client
        .query_one(
            "SELECT count(*), (SELECT COALESCE(i.email,v.email,'') FROM users a LEFT JOIN user_identities i ON i.id=a.identity_id LEFT JOIN user_invitations v ON v.tenant_id=a.tenant_id AND v.user_id=a.id WHERE a.tenant_id=$1 AND a.deleted_at IS NULL ORDER BY a.id LIMIT 1) FROM users WHERE tenant_id=$1 AND deleted_at IS NULL",
            &[&tenant],
        )
        .await?;
    let count: i64 = row.get(0);
    Ok((count > 0).then(|| (count, row.get::<_, Option<String>>(1).unwrap_or_default())))
}

impl UserService {
    /// Prepares the first owner of a customer tenant. The caller is a trusted
    /// server command, not a request handler. It refuses any existing account
    /// or earlier receipt and never promotes or replaces one.
    pub async fn create_first_tenant_owner(
        &self,
        grants: &Grants,
        target: TenantCompany,
        input: &FirstTenantOwner,
    ) -> std::result::Result<FirstTenantOwnerCreated, FirstTenantOwnerError> {
        let errors = validate_first_tenant_owner(input);
        if !errors.is_empty() {
            return Err(FirstTenantOwnerError::Validation(errors));
        }
        let email = validate_email(&input.email)
            .map_err(|error| FirstTenantOwnerError::Validation(vec![error]))?;
        let name = input.name.trim().to_owned();
        // A fresh tenant has nobody who could have changed its tenant-scoped
        // invitation setting yet. Read the contributed default so bootstrap
        // follows the same contract without duplicating the value here.
        let invitation_days = match self
            .settings
            .definitions()
            .lookup(crate::INVITATION_DAYS_KEY)?
            .setting
        {
            Setting::Definition(definition) => (definition.default)()
                .as_i64()
                .and_then(|days| i32::try_from(days).ok())
                .filter(|days| (1..=60).contains(days))
                .ok_or(Error::Internal)?,
            Setting::RuntimeClaim => return Err(Error::Internal.into()),
        };
        let raw_token = token()?;
        let token_hash = digest(&raw_token);
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(target.tenant, TENANT_OWNER_PRINCIPAL, TENANT_OWNER_ACTION),
            )
            .await?;
        tx.sql()
            .batch_execute("LOCK TABLE users IN SHARE ROW EXCLUSIVE MODE")
            .await?;
        let Some(tenant_name) = tx
            .sql()
            .query_opt(
                "SELECT t.name FROM tenants t JOIN tenant_primary_companies p ON p.tenant_id=t.id JOIN companies c ON c.tenant_id=t.id AND c.id=p.company_id AND c.deleted_at IS NULL LEFT JOIN platform_operator o ON o.tenant_id=t.id WHERE t.id=$1 AND t.status='active' AND t.deleted_at IS NULL AND p.company_id=$2 AND o.tenant_id IS NULL",
                &[&target.tenant, &target.company],
            )
            .await?
            .map(|row| row.get::<_, String>(0))
        else {
            return Err(FirstTenantOwnerError::NotCustomer);
        };
        if tx
            .sql()
            .query_opt(
                "SELECT 1 FROM user_tenant_owner_bootstraps WHERE tenant_id=$1",
                &[&target.tenant],
            )
            .await?
            .is_some()
        {
            let (count, first) = accounts(tx.sql(), target.tenant)
                .await?
                .unwrap_or((1, String::new()));
            return Err(FirstTenantOwnerError::AccountsExist { count, first });
        }
        if let Some((count, first)) = accounts(tx.sql(), target.tenant).await? {
            return Err(FirstTenantOwnerError::AccountsExist { count, first });
        }

        let tenant = target.tenant;
        let company = target.company;
        let days = invitation_days;
        let (insert_name, insert_email) = (name.clone(), email.clone());
        let invitation: Invitation = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let user: i64 = sql
                        .query_one(
                            "INSERT INTO users(tenant_id,name,status) VALUES($1,$2,'invited') RETURNING id",
                            &[&tenant, &insert_name],
                        )
                        .await?
                        .get(0);
                    let id: i64 = sql
                        .query_one(
                            "INSERT INTO user_invitations(tenant_id,user_id,email,token_hash,invited_by,expires_at) VALUES($1,$2,$3,$4,NULL,now()+make_interval(days=>$5::int)) RETURNING id",
                            &[&tenant, &user, &insert_email, &token_hash, &days],
                        )
                        .await?
                        .get(0);
                    Ok((
                        Invitation {
                            id,
                            user,
                            email: insert_email.clone(),
                        },
                        vec![
                            Change {
                                kind: "user".into(),
                                id: user,
                                old: json!(null),
                                new: json!({"name": insert_name, "status": "invited"}),
                            },
                            Change {
                                kind: "user.invitation".into(),
                                id,
                                old: json!(null),
                                new: json!({"user_id": user, "email": insert_email, "status": "pending", "source": "tenant_owner_bootstrap"}),
                            },
                        ],
                    ))
                })
            })
            .await?;
        let role = grants
            .grant_first_administrator_in(&mut tx, tenant, invitation.user)
            .await?;
        let user = invitation.user;
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "INSERT INTO user_tenant_owner_bootstraps(tenant_id,user_id,company_id) VALUES($1,$2,$3)",
                    &[&tenant, &user, &company],
                )
                .await?;
                Ok((
                    (),
                    vec![Change {
                        kind: "user.tenant_owner_bootstrap".into(),
                        id: tenant,
                        old: json!(null),
                        new: json!({"user_id": user, "company_id": company, "role_id": role, "status": "invited"}),
                    }],
                ))
            })
        })
        .await?;
        let mut who = Attribution::system(Some(TENANT_OWNER_PRINCIPAL))?;
        who.tenant = Some(tenant);
        record_action(
            tx.sql(),
            &who,
            None,
            TENANT_OWNER_INVITED,
            json!({"user_id": invitation.user, "role": "core_admin", "invitation_id": invitation.id}),
            true,
        )
        .await?;
        let message = Message::text(
            Address::new(Some(&name), &email).map_err(|_| Error::Invalid)?,
            format!("You're invited to {tenant_name}"),
            format!(
                "You are invited to become the owner of {tenant_name}.\n\nOpen this link to set up your sign-in:\n\n{}/accept/{raw_token}\n\nThe link works once and expires in {invitation_days} days. If you did not expect this, you can ignore this email.\n",
                self.origin
            ),
        );
        tx.commit().await?;
        drop(client);
        // This facade is called by a short-lived server command, so wait for
        // its ordinary invitation delivery after commit. A background task
        // could be cancelled when the command's runtime exits.
        if let Err(error) = self.mailer.deliver(&message).await {
            eprintln!(
                "mail: tenant owner invitation {} was not delivered: {error}",
                invitation.id
            );
        }
        Ok(FirstTenantOwnerCreated {
            invitation: invitation.id,
            user: invitation.user,
            email,
            role,
        })
    }

    /// Completes a customer-owner receipt while accepting its ordinary
    /// invitation. Everything stays on the invitation's mutation transaction.
    pub(crate) async fn settle_tenant_owner_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        tenant: i64,
        user: i64,
    ) -> Result<()> {
        let Some(row) = tx
            .sql()
            .query_opt(
                "SELECT company_id,affiliation_settled_at IS NOT NULL FROM user_tenant_owner_bootstraps WHERE tenant_id=$1 AND user_id=$2 FOR UPDATE",
                &[&tenant, &user],
            )
            .await?
        else {
            return Ok(());
        };
        if row.get::<_, bool>(1) {
            return Ok(());
        }
        let company: i64 = row.get(0);
        let scope = self
            .authentication
            .authenticate(tx.sql(), tenant, user, String::new())
            .await?;
        if scope.actor().person().is_none() {
            let person = bilibi_person::create_person(
                tx,
                &scope,
                &PersonInput {
                    name: scope.user_name.clone(),
                    kind: PersonKind::Human,
                    email: Some(scope.email.clone()),
                    phone: None,
                },
            )
            .await
            .map_err(|error| match error {
                bilibi_person::WriteError::Backend(error) => error,
                _ => Error::Invalid,
            })?
            .id;
            let organisation = OrganisationId::new(company).map_err(|_| Error::Internal)?;
            bilibi_affiliation::identity::create(
                tx,
                &scope,
                &AffiliationInput {
                    person_id: person,
                    organisation_id: organisation,
                    title: None,
                    work_email: None,
                    work_phone: None,
                    dates: EffectiveDates::new(None, None).map_err(|_| Error::Internal)?,
                },
            )
            .await?;
            self.initialize_desk_in(tx, &scope, user, person, organisation)
                .await?;
        }
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "UPDATE user_tenant_owner_bootstraps SET affiliation_settled_at=now() WHERE tenant_id=$1 AND user_id=$2 AND affiliation_settled_at IS NULL",
                    &[&tenant, &user],
                )
                .await?;
                Ok((
                    (),
                    vec![Change {
                        kind: "user.tenant_owner_bootstrap".into(),
                        id: tenant,
                        old: json!({"user_id": user, "company_id": company, "status": "invited"}),
                        new: json!({"user_id": user, "company_id": company, "status": "accepted"}),
                    }],
                ))
            })
        })
        .await
    }
}
