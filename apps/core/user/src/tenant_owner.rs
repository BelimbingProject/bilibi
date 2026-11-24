//! Deployment bootstrap and invitation settlement for a customer tenant.
use crate::{
    accounts::{email_taken, field_error, validate_email, FieldError},
    memberships::{digest, token},
    Invitation, UserService,
};
use bilibi_affiliation::{AffiliationInput, EffectiveDates, OrganisationId};
use bilibi_audit::{record_action, Attribution};
use bilibi_company::{NewCompany, ProvisionOutcome};
use bilibi_database::{Change, Connection, Context, Error, MutationTransaction, Result};
use bilibi_mail::{Address, Message};
use bilibi_person::{validate_person, PersonInput, PersonKind};
use bilibi_settings::Setting;
use bilibi_tenancy::tenant_name_lock;
use serde_json::json;

pub const TENANT_OWNER_PRINCIPAL: &str = "tenant-provision";
const TENANT_OWNER_ACTION: &str = "user.tenant_owner.bootstrap";
const TENANT_OWNER_INVITED: &str = "user.tenant_owner.invited";
const TENANT_OWNER_REISSUED: &str = "user.tenant_owner.reissued";
pub(crate) const DEDICATED_OWNER_SIGN_IN: &str =
    "A tenant owner needs a new sign-in. Use a separate email address.";

pub enum TenantOwnerTarget<'a> {
    Create {
        name: &'a str,
        company: &'a NewCompany,
    },
    Reissue {
        tenant: i64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum FirstTenantOwnerError {
    #[error("a tenant with that name already exists")]
    TenantExists,
    #[error("the bootstrap owner has no pending invitation")]
    OwnerNotPending,
    #[error("{DEDICATED_OWNER_SIGN_IN}")]
    DedicatedSignIn,
    #[error("Invalid owner fields")]
    Validation(Vec<FieldError>),
    #[error("Invalid company fields")]
    CompanyValidation(Vec<bilibi_company::FieldError>),
    #[error("APP_ORIGIN must be the installation's HTTP or HTTPS origin")]
    Origin,
    #[error("Cannot prepare the owner invitation: {0}")]
    Mail(#[from] bilibi_mail::Error),
    #[error(
        "Owner invitation delivery failed after commit for tenant {tenant}. Reissue with --tenant-id {tenant} and --owner-email."
    )]
    Delivery { tenant: i64 },
    #[error(transparent)]
    Access(#[from] Error),
}

impl From<tokio_postgres::Error> for FirstTenantOwnerError {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}

fn tenant_owner_lock(tenant: i64) -> String {
    format!("tenant-owner-id:{tenant}")
}

impl UserService {
    pub async fn provision_tenant_owner(
        &self,
        target: TenantOwnerTarget<'_>,
        owner_email: &str,
    ) -> std::result::Result<(i64, Invitation), FirstTenantOwnerError> {
        let email = validate_email(owner_email)
            .map_err(|error| FirstTenantOwnerError::Validation(vec![error]))?;
        let name = email
            .split_once('@')
            .map_or("", |(local, _)| local)
            .to_owned();
        let person = validate_person(&PersonInput {
            name,
            kind: PersonKind::Human,
            email: Some(email.clone()),
            phone: None,
        })
        .map_err(|errors| {
            FirstTenantOwnerError::Validation(
                errors
                    .into_iter()
                    .map(|error| field_error("owner_email", error.message))
                    .collect(),
            )
        })?;
        let name = person.name;
        let origin = self.origin.as_str();
        if !url::Url::parse(origin).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https") && url.origin().ascii_serialization() == origin
        }) {
            return Err(FirstTenantOwnerError::Origin);
        }
        let days = match self
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
        self.mailer.ready().await?;
        let mut client = Connection::take(self.db.get().await.map_err(Error::from)?);
        let (tenant, invitation, rendered) = {
            let sql = tokio_postgres::Client::transaction(&mut client).await?;
            let (tenant, created) = match target {
                TenantOwnerTarget::Create { name, company } => {
                    let name = name.trim();
                    sql.execute(
                        "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                        &[&tenant_name_lock(name)],
                    )
                    .await?;
                    let exists: bool = sql
                        .query_one(
                            "SELECT EXISTS(SELECT 1 FROM tenants WHERE name=$1 AND deleted_at IS NULL)",
                            &[&name],
                        )
                        .await?
                        .get(0);
                    if exists {
                        return Err(FirstTenantOwnerError::TenantExists);
                    }
                    if email_taken(&sql, &email, None).await? {
                        return Err(FirstTenantOwnerError::DedicatedSignIn);
                    }
                    match bilibi_company::provision_tenant_in(&sql, name, company).await? {
                        ProvisionOutcome::Provisioned(ready) => {
                            (ready.tenant.id, Some((ready.tenant.name, ready.company.id)))
                        }
                        ProvisionOutcome::Refused(errors) => {
                            return Err(FirstTenantOwnerError::CompanyValidation(errors));
                        }
                    }
                }
                TenantOwnerTarget::Reissue { tenant } if tenant > 0 => (tenant, None),
                TenantOwnerTarget::Reissue { .. } => return Err(Error::Invalid.into()),
            };
            sql.execute(
                "SELECT pg_advisory_lock(hashtextextended($1,0))",
                &[&tenant_owner_lock(tenant)],
            )
            .await?;
            let pending = if created.is_none() {
                Some(sql
                    .query_opt(
                        "SELECT i.id,i.user_id,i.email,u.name,t.name FROM user_tenant_owner_bootstraps b
                         JOIN user_invitations i ON i.tenant_id=b.tenant_id AND i.user_id=b.user_id AND i.id=b.invitation_id
                         JOIN users u ON u.tenant_id=b.tenant_id AND u.id=b.user_id
                         JOIN tenants t ON t.id=b.tenant_id
                         JOIN tenant_primary_companies p ON p.tenant_id=b.tenant_id AND p.company_id=b.company_id
                         JOIN companies c ON c.tenant_id=p.tenant_id AND c.id=p.company_id
                         WHERE b.tenant_id=$1
                           AND t.deleted_at IS NULL AND t.status='active' AND c.deleted_at IS NULL
                           AND NOT EXISTS (SELECT 1 FROM platform_operator o WHERE o.tenant_id=t.id)
                           AND i.accepted_at IS NULL AND i.revoked_at IS NULL
                           AND u.status='invited' AND u.identity_id IS NULL AND u.deleted_at IS NULL
                         FOR UPDATE OF i,u FOR SHARE OF t,p,c",
                        &[&tenant],
                    )
                    .await?
                    .ok_or(FirstTenantOwnerError::OwnerNotPending)?)
            } else {
                None
            };
            let tenant_name: &str = if let Some((name, _)) = &created {
                name
            } else {
                pending.as_ref().ok_or(Error::Internal)?.get(4)
            };
            let message = Message::text(
                Address::new(Some(&name), &email)?,
                "You're invited to become a tenant owner",
                format!(
                    "You are invited to become the owner of {tenant_name}.\n\nOpen this link to set up your sign-in:\n\n{origin}/accept/{raw_token}\n\nThe link works once and expires in {days} days. If you did not expect this, you can ignore this email.\n"
                ),
            );
            let rendered = self.mailer.render(&message).await?;
            let mut tx = self.mutations.adopt_deployment(
                sql,
                Context::system(tenant, TENANT_OWNER_PRINCIPAL, TENANT_OWNER_ACTION),
            )?;
            let (invitation, event) = if let Some(pending) = pending {
                let (id, user, old_email, old_name): (i64, i64, String, String) = (
                    pending.get(0),
                    pending.get(1),
                    pending.get(2),
                    pending.get(3),
                );
                if email_taken(tx.sql(), &email, None).await? {
                    return Err(FirstTenantOwnerError::DedicatedSignIn);
                }
                let invitation = tx.mutate(move |sql| {
                    Box::pin(async move {
                        sql.execute(
                            "UPDATE user_invitations SET email=$3,token_hash=$4,expires_at=clock_timestamp()+make_interval(days=>$5::int) WHERE tenant_id=$1 AND id=$2",
                            &[&tenant, &id, &email, &token_hash, &days],
                        )
                        .await?;
                        let mut changes = vec![Change {
                            kind: "user.invitation".into(),
                            id,
                            old: json!({"user_id": user, "email": old_email, "status": "pending"}),
                            new: json!({"user_id": user, "email": email, "status": "pending", "reissued": true}),
                        }];
                        if old_email != email && old_name != name {
                            sql.execute(
                                "UPDATE users SET name=$3,version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                                &[&tenant, &user, &name],
                            )
                            .await?;
                            changes.push(Change {
                                kind: "user".into(),
                                id: user,
                                old: json!({"name": old_name}),
                                new: json!({"name": name}),
                            });
                        }
                        Ok((Invitation { id, user, email }, changes))
                    })
                })
                .await?;
                (invitation, TENANT_OWNER_REISSUED)
            } else {
                (
                    self.invite_tenant_owner_in(
                        &mut tx,
                        created.ok_or(Error::Internal)?.1,
                        name,
                        email,
                        token_hash,
                        days,
                    )
                    .await?,
                    TENANT_OWNER_INVITED,
                )
            };
            let mut who = Attribution::system(Some(TENANT_OWNER_PRINCIPAL))?;
            who.tenant = Some(tenant);
            record_action(
                tx.sql(),
                &who,
                None,
                event,
                if event == TENANT_OWNER_INVITED {
                    json!({"user_id": invitation.user, "invitation_id": invitation.id, "role": "tenant_owner"})
                } else {
                    json!({"user_id": invitation.user, "invitation_id": invitation.id})
                },
                true,
            )
            .await?;
            tx.commit().await?;
            (tenant, invitation, rendered)
        };
        self.mailer
            .deliver_rendered(rendered)
            .await
            .map_err(|_| FirstTenantOwnerError::Delivery { tenant })?;
        Ok((tenant, invitation))
    }

    async fn invite_tenant_owner_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        company: i64,
        name: String,
        email: String,
        token_hash: Vec<u8>,
        days: i32,
    ) -> Result<Invitation> {
        let tenant = tx.context().tenant;
        let invitation: Invitation = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let user: i64 = sql
                        .query_one(
                            "INSERT INTO users(tenant_id,name,status) VALUES($1,$2,'invited') RETURNING id",
                            &[&tenant, &name],
                        )
                        .await?
                        .get(0);
                    let id: i64 = sql
                        .query_one(
                            "INSERT INTO user_invitations(tenant_id,user_id,email,token_hash,invited_by,expires_at) VALUES($1,$2,$3,$4,NULL,clock_timestamp()+make_interval(days=>$5::int)) RETURNING id",
                            &[&tenant, &user, &email, &token_hash, &days],
                        )
                        .await?
                        .get(0);
                    Ok((
                        Invitation {
                            id,
                            user,
                            email: email.clone(),
                        },
                        vec![
                            Change {
                                kind: "user".into(),
                                id: user,
                                old: json!(null),
                                new: json!({"name": name, "status": "invited"}),
                            },
                            Change {
                                kind: "user.invitation".into(),
                                id,
                                old: json!(null),
                                new: json!({"user_id": user, "email": email, "status": "pending", "source": "tenant_owner_bootstrap"}),
                            },
                        ],
                    ))
                })
            })
            .await?;
        let role = self
            .sso_grants
            .grant_first_tenant_owner_in(tx, tenant, invitation.user)
            .await?;
        let user = invitation.user;
        let invitation_id = invitation.id;
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "INSERT INTO user_tenant_owner_bootstraps(tenant_id,user_id,company_id,invitation_id) VALUES($1,$2,$3,$4)",
                    &[&tenant, &user, &company, &invitation_id],
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
        Ok(invitation)
    }

    /// Serializes owner-link acceptance with deployment reissue before either
    /// path locks the invitation or the primary company.
    pub(crate) async fn lock_tenant_owner_in(
        &self,
        tx: &MutationTransaction<'_>,
        tenant: i64,
        user: i64,
    ) -> Result<()> {
        let sql = tx.sql();
        if sql
            .query_opt(
                "SELECT 1 FROM user_tenant_owner_bootstraps WHERE tenant_id=$1 AND user_id=$2",
                &[&tenant, &user],
            )
            .await?
            .is_some()
        {
            sql.execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                &[&tenant_owner_lock(tenant)],
            )
            .await?;
        }
        Ok(())
    }

    /// Creates the owner Person, affiliation and desk while accepting the
    /// exact invitation recorded by the deployment receipt.
    pub(crate) async fn settle_tenant_owner_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        tenant: i64,
        user: i64,
        invitation: i64,
    ) -> Result<()> {
        let Some(row) = tx
            .sql()
            .query_opt(
                "SELECT b.company_id FROM user_tenant_owner_bootstraps b
                 JOIN user_invitations i ON i.tenant_id=b.tenant_id AND i.id=b.invitation_id
                 WHERE b.tenant_id=$1 AND b.user_id=$2 AND b.invitation_id=$3
                   AND i.accepted_at IS NOT NULL
                 FOR UPDATE OF b",
                &[&tenant, &user, &invitation],
            )
            .await?
        else {
            return Ok(());
        };
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
        Ok(())
    }
}
