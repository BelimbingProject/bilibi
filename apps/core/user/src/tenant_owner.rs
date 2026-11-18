//! Deployment bootstrap and invitation settlement for a customer tenant.
use crate::{
    accounts::{field_error, validate_email, FieldError},
    memberships::{digest, token},
    Invitation, UserService,
};
use bilibi_affiliation::{AffiliationInput, EffectiveDates, OrganisationId};
use bilibi_audit::{record_action, Attribution};
use bilibi_authz::grants::Grants;
use bilibi_company::{NewCompany, ProvisionOutcome};
use bilibi_database::{Change, Context, Error, MutationTransaction, Result};
use bilibi_mail::{Address, Message};
use bilibi_person::{validate_person, PersonInput, PersonKind};
use bilibi_settings::Setting;
use serde_json::json;

pub const TENANT_OWNER_PRINCIPAL: &str = "tenant-provision";
const TENANT_OWNER_ACTION: &str = "user.tenant_owner.bootstrap";
const TENANT_OWNER_INVITED: &str = "user.tenant_owner.invited";
const TENANT_OWNER_REISSUED: &str = "user.tenant_owner.reissued";

#[derive(Debug, thiserror::Error)]
pub enum FirstTenantOwnerError {
    #[error("a matching tenant already exists without a matching owner bootstrap receipt")]
    TenantExists,
    #[error("the bootstrap owner has no pending invitation for this address")]
    OwnerNotPending,
    #[error("Invalid owner fields")]
    Validation(Vec<FieldError>),
    #[error("Invalid company fields")]
    CompanyValidation(Vec<bilibi_company::FieldError>),
    #[error("APP_ORIGIN must be the installation's HTTP or HTTPS origin")]
    Origin,
    #[error("Cannot prepare the owner invitation: {0}")]
    Mail(#[from] bilibi_mail::Error),
    #[error(
        "Owner invitation delivery failed after commit. Rerun the same command to reissue it."
    )]
    Delivery,
    #[error(transparent)]
    Access(#[from] Error),
}

impl From<tokio_postgres::Error> for FirstTenantOwnerError {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}

impl UserService {
    pub async fn provision_tenant_owner(
        &self,
        grants: &Grants,
        tenant_name: &str,
        company: &NewCompany,
        owner_email: &str,
        origin: &str,
    ) -> std::result::Result<Invitation, FirstTenantOwnerError> {
        let tenant_name = tenant_name.trim();
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
        let message = Message::text(
            Address::new(Some(&name), &email)?,
            format!("You're invited to {tenant_name}"),
            format!(
                "You are invited to become the owner of {tenant_name}.\n\nOpen this link to set up your sign-in:\n\n{origin}/accept/{raw_token}\n\nThe link works once and expires in {days} days. If you did not expect this, you can ignore this email.\n"
            ),
        );
        let rendered = self.mailer.render(&message).await?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let sql = tokio_postgres::Client::transaction(&mut client).await?;
        sql.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended('tenant-owner:' || $1,0))",
            &[&tenant_name],
        )
        .await?;
        let candidates = sql
            .query(
                "SELECT id FROM tenants WHERE name=$1 AND deleted_at IS NULL ORDER BY id LIMIT 2 FOR SHARE",
                &[&tenant_name],
            )
            .await?;
        let (tenant, company, retry_invitation) = match candidates.as_slice() {
            [] => match bilibi_company::provision_tenant_in(&sql, tenant_name, company).await? {
                ProvisionOutcome::Provisioned(ready) => (ready.tenant.id, ready.company.id, None),
                ProvisionOutcome::Refused(errors) => {
                    return Err(FirstTenantOwnerError::CompanyValidation(errors));
                }
            },
            [row] => {
                let tenant: i64 = row.get(0);
                let receipt = sql
                    .query_opt(
                        "SELECT b.company_id,b.invitation_id FROM user_tenant_owner_bootstraps b
                         JOIN tenants t ON t.id=b.tenant_id
                         JOIN tenant_primary_companies p ON p.tenant_id=b.tenant_id AND p.company_id=b.company_id
                         JOIN companies c ON c.tenant_id=b.tenant_id AND c.id=b.company_id
                         LEFT JOIN platform_operator o ON o.tenant_id=b.tenant_id
                         WHERE b.tenant_id=$1 AND t.status='active' AND o.tenant_id IS NULL
                           AND c.deleted_at IS NULL AND c.name=$2 AND c.code=$3
                         FOR SHARE OF p,c",
                        &[&tenant, &company.name.trim(), &company.code.as_deref().map(str::trim)],
                    )
                    .await?
                    .ok_or(FirstTenantOwnerError::TenantExists)?;
                (tenant, receipt.get(0), Some(receipt.get::<_, i64>(1)))
            }
            _ => return Err(FirstTenantOwnerError::TenantExists),
        };
        let mut tx = self.mutations.adopt_deployment(
            sql,
            Context::system(tenant, TENANT_OWNER_PRINCIPAL, TENANT_OWNER_ACTION),
        )?;
        let (invitation, event) = if let Some(id) = retry_invitation {
            let user = tx
                .sql()
                .query_opt(
                    "SELECT i.user_id FROM user_tenant_owner_bootstraps b
                     JOIN user_invitations i ON i.tenant_id=b.tenant_id AND i.user_id=b.user_id AND i.id=b.invitation_id
                     JOIN users u ON u.tenant_id=b.tenant_id AND u.id=b.user_id
                     WHERE b.tenant_id=$1 AND b.affiliation_settled_at IS NULL AND i.id=$2
                       AND i.email=$3 AND i.accepted_at IS NULL AND i.revoked_at IS NULL
                       AND u.status='invited' AND u.identity_id IS NULL AND u.deleted_at IS NULL
                     FOR UPDATE OF i",
                    &[&tenant, &id, &email],
                )
                .await?
                .ok_or(FirstTenantOwnerError::OwnerNotPending)?
                .get(0);
            tx.mutate(move |sql| {
                Box::pin(async move {
                    sql.execute(
                        "UPDATE user_invitations SET token_hash=$3,expires_at=clock_timestamp()+make_interval(days=>$4::int) WHERE tenant_id=$1 AND id=$2",
                        &[&tenant, &id, &token_hash, &days],
                    )
                    .await?;
                    Ok(((), vec![Change {
                        kind: "user.invitation".into(),
                        id,
                        old: json!({"user_id": user, "status": "pending"}),
                        new: json!({"user_id": user, "status": "pending", "reissued": true}),
                    }]))
                })
            })
            .await?;
            (Invitation { id, user, email }, TENANT_OWNER_REISSUED)
        } else {
            (
                Self::invite_tenant_owner_in(
                    &mut tx, grants, company, name, email, token_hash, days,
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
        drop(client);
        self.mailer
            .deliver_rendered(rendered)
            .await
            .map_err(|_| FirstTenantOwnerError::Delivery)?;
        Ok(invitation)
    }

    async fn invite_tenant_owner_in(
        tx: &mut MutationTransaction<'_>,
        grants: &Grants,
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
        let role = grants
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
