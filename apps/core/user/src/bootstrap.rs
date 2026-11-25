//! The bootstrap administrator's first workplace (the seed
//! creates `ai@agent.my` under the operator's primary company). The platform's
//! first administrator gets an agent Person, a business affiliation for that
//! Person and an explicit desk in the company named by the bootstrap receipt.
//! The staff account stores only the Person.
//!
//! This happens once per receipt: `affiliation_settled_at` records it, and a
//! settled receipt is never revisited, so an operator who later unlinks,
//! re-links or retires that identity keeps the change. A receipt whose
//! account already names a Person is settled without a write. The writes
//! are the administrator's own (the receipt names the account, and nobody
//! else exists yet), on one audited mutation transaction.
//!
//! `create_first_administrator` is the production counterpart of the seed's
//! bootstrap (with the operator's own
//! attributes): the install's first owner account in the platform-operator
//! tenant, on one audited transaction as the `create-owner` system
//! principal, since no account exists yet to act as. It refuses once the
//! operator tenant holds any account, so it can never promote or replace one.
use crate::{
    accounts::{email_taken, field_error, validate_email, validate_name, FieldError, EMAIL_TAKEN},
    password,
    reset::{random_token, ClaimMail},
    UserService,
};
use bilibi_affiliation::{AffiliationInput, EffectiveDates, OrganisationId};
use bilibi_audit::{record_action, Attribution};
use bilibi_authz::grants::Grants;
use bilibi_database::{Change, Context, Error, Result};
use bilibi_person::{PersonInput, PersonKind};
use bilibi_tenancy::AccountKind;
use serde_json::json;
use tokio_postgres::GenericClient;

/// The system principal the first administrator's bootstrap is recorded as.
pub const CREATE_OWNER_PRINCIPAL: &str = "create-owner";
/// The bootstrap transaction's action; its retained
/// `user.bootstrap.completed` action (the seed's) carries the
/// account and role. It carries no email: an action payload
/// is never erased (ADR 05), and the receipt's email is.
pub const BOOTSTRAP_ACTION: &str = "user.bootstrap";
pub const BOOTSTRAP_COMPLETED: &str = "user.bootstrap.completed";

/// What [`UserService::settle_bootstrap_affiliation`] did.
#[derive(Debug)]
pub enum BootstrapAffiliation {
    /// No receipt yet: nothing was bootstrapped.
    NoReceipt,
    /// The receipt was settled before; nothing was read further.
    Settled,
    /// The account already named a Person; the receipt is now settled.
    AlreadyLinked,
    /// The account named nothing and still cannot be bound; left unsettled
    /// so a later run tries again. The reason is for the operator.
    Deferred(&'static str),
    /// A Person, business affiliation and first desk were created for the
    /// receipt's company.
    Linked { person: i64, affiliation: i64 },
}

async fn settled(client: &(impl GenericClient + Sync)) -> Result<Option<(i64, i64, i64, bool)>> {
    Ok(client
        .query_opt(
            "SELECT tenant_id, user_id, company_id, affiliation_settled_at IS NOT NULL FROM user_bootstrap",
            &[],
        )
        .await?
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3))))
}

impl UserService {
    /// Gives the bootstrap administrator its initial Person, affiliation and
    /// desk once; see the module documentation. Concurrent runs serialize on
    /// the receipt.
    pub async fn settle_bootstrap_affiliation(&self) -> Result<BootstrapAffiliation> {
        let mut client = self.db.get().await?;
        let Some((tenant, user, company, done)) = settled(&**client).await? else {
            return Ok(BootstrapAffiliation::NoReceipt);
        };
        if done {
            return Ok(BootstrapAffiliation::Settled);
        }
        let scope = match self
            .authentication
            .authenticate(&**client, tenant, user, String::new())
            .await
        {
            Ok(scope) => scope,
            Err(Error::Unauthorized) => {
                return Ok(BootstrapAffiliation::Deferred(
                    "the bootstrap administrator cannot sign in",
                ))
            }
            Err(error) => return Err(error),
        };
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.bootstrap.affiliation"),
            )
            .await?;
        let locked = tx
            .sql()
            .query_one(
                "SELECT affiliation_settled_at IS NOT NULL FROM user_bootstrap FOR UPDATE",
                &[],
            )
            .await?;
        if locked.get::<_, bool>(0) {
            tx.rollback().await?;
            return Ok(BootstrapAffiliation::Settled);
        }
        if scope.actor().person().is_some() {
            mark_settled(tx.sql()).await?;
            tx.commit().await?;
            return Ok(BootstrapAffiliation::AlreadyLinked);
        }
        let person = match bilibi_person::create_person(
            &mut tx,
            &scope,
            &PersonInput {
                name: scope.user_name.clone(),
                kind: PersonKind::Agent,
                email: None,
                phone: None,
            },
        )
        .await
        {
            Ok(person) => person.id,
            Err(bilibi_person::WriteError::Backend(Error::Forbidden)) => {
                return Ok(BootstrapAffiliation::Deferred(
                    "the bootstrap administrator may no longer create contacts",
                ))
            }
            Err(bilibi_person::WriteError::Backend(error)) => return Err(error),
            Err(_) => return Err(Error::Invalid),
        };
        let organisation = OrganisationId::new(company).map_err(|_| Error::Internal)?;
        let affiliation = match bilibi_affiliation::identity::create(
            &mut tx,
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
        .await
        {
            Ok(affiliation) => affiliation.id,
            Err(Error::Forbidden | Error::NotFound) => {
                return Ok(BootstrapAffiliation::Deferred(
                    "the bootstrap company is gone or read-only, or the administrator may no longer affiliate contacts",
                ))
            }
            Err(error) => return Err(error),
        };
        match self
            .initialize_desk_in(&mut tx, &scope, user, person, organisation)
            .await
        {
            Ok(_) => {}
            Err(Error::Unauthorized) => return Err(Error::Unauthorized),
            Err(error @ (Error::Database(_) | Error::Pool(_))) => return Err(error),
            Err(_) => {
                return Ok(BootstrapAffiliation::Deferred(
                    "the bootstrap administrator's account cannot be linked",
                ))
            }
        }
        mark_settled(tx.sql()).await?;
        tx.commit().await?;
        Ok(BootstrapAffiliation::Linked {
            person: person.get(),
            affiliation: affiliation.get(),
        })
    }
}

async fn mark_settled(sql: &(impl GenericClient + Sync)) -> Result<()> {
    sql.execute(
        "UPDATE user_bootstrap SET affiliation_settled_at = now()",
        &[],
    )
    .await?;
    Ok(())
}

/// What the first administrator is created from: `admin_name` and
/// `admin_email`. There is no password: the account gets a random one nobody
/// knows, and the owner chooses their own from the emailed claim link
/// ([`UserService::claim_owner`]), so whoever runs the server command never
/// holds the owner's sign-in.
#[derive(Clone, Debug, Default)]
pub struct FirstAdministrator {
    pub name: String,
    pub email: String,
}

/// The platform-operator tenant and its primary company, as Company's
/// provisioning reported them; the receipt names both and the owner's first
/// desk is created in the company.
#[derive(Clone, Copy, Debug)]
pub struct OperatorCompany {
    pub tenant: i64,
    pub company: i64,
}

/// Why no first administrator was created.
#[derive(Debug, thiserror::Error)]
pub enum FirstAdministratorError {
    #[error("the platform operator already has accounts")]
    AccountsExist,
    /// A field refused: the same wording the administrator form uses.
    #[error("Invalid account fields")]
    Validation(Vec<FieldError>),
    /// The named tenant is not the marked platform operator, or the company
    /// is not that tenant's.
    #[error("not the platform operator's tenant and company")]
    NotOperator,
    #[error(transparent)]
    Access(#[from] Error),
}
impl From<tokio_postgres::Error> for FirstAdministratorError {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}

/// The accounts a platform-operator tenant already holds (deleted ones
/// excepted): how many, and the earliest one's email.
#[derive(Clone, Debug)]
pub struct OperatorAccounts {
    pub count: i64,
    pub first: String,
}

/// The account named by the one-time bootstrap receipt: the installation
/// owner whose fresh factor may authorize a root-invoked software command.
#[derive(Clone, Debug)]
pub struct InstallationOwner {
    pub tenant: i64,
    pub user: i64,
}

async fn operator_accounts(
    client: &(impl GenericClient + Sync),
    tenant: i64,
) -> Result<Option<OperatorAccounts>> {
    let row = client
        .query_one(
            "SELECT count(*), (SELECT i.email FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1 AND u.deleted_at IS NULL ORDER BY u.id LIMIT 1) \
             FROM users WHERE tenant_id=$1 AND deleted_at IS NULL",
            &[&tenant],
        )
        .await?;
    let count: i64 = row.get(0);
    Ok((count > 0).then(|| OperatorAccounts {
        count,
        first: row.get::<_, Option<String>>(1).unwrap_or_default(),
    }))
}

/// What [`UserService::create_first_administrator`] wrote.
#[derive(Clone, Debug)]
pub struct FirstAdministratorCreated {
    pub user: i64,
    pub email: String,
    /// The grant-all role the account holds.
    pub role: Option<i64>,
    /// A receipt from an earlier bootstrap named an account that no longer
    /// exists and now names this one.
    pub receipt_replaced: bool,
}

/// The create command's validation, without touching the database: the
/// administrator form's rules for name and email.
pub fn validate_first_administrator(input: &FirstAdministrator) -> Vec<FieldError> {
    let mut errors = Vec::new();
    errors.extend(validate_name(&input.name));
    errors.extend(validate_email(&input.email).err());
    errors
}

/// Where the server owner's claim stands. The owner proves the address by
/// choosing a password from the emailed link, then sets up 2FA; until both
/// are done the install is not finished.
#[derive(Debug)]
pub enum OwnerClaim {
    /// A claim link was mailed now.
    Sent { email: String },
    /// A link went out within the last minute; that link stands.
    RecentlySent { email: String },
    /// The owner chose a password; 2FA is not set up yet.
    PasswordChosen { email: String },
    /// Password chosen and 2FA set up: the owner holds the server.
    Claimed { email: String },
}

/// Why [`UserService::claim_owner`] could not report or send a claim.
#[derive(Debug, thiserror::Error)]
pub enum OwnerClaimError {
    #[error("this install has no owner yet")]
    NoOwner,
    /// Mail cannot be sent from this install; nothing was issued.
    #[error("{0}")]
    MailNotReady(String),
    /// The transport refused; the link was withdrawn.
    #[error("{0}")]
    Delivery(String),
    #[error(transparent)]
    Access(#[from] Error),
}
impl From<tokio_postgres::Error> for OwnerClaimError {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}

impl UserService {
    /// The installation owner, if bootstrap has completed. Authentication
    /// still re-proves the account live before a caller may use it.
    pub async fn installation_owner(&self) -> Result<Option<InstallationOwner>> {
        let client = self.db.get().await?;
        Ok(client
            .query_opt(
                "SELECT tenant_id, user_id FROM user_bootstrap WHERE id=1",
                &[],
            )
            .await?
            .map(|row| InstallationOwner {
                tenant: row.get(0),
                user: row.get(1),
            }))
    }

    /// The accounts the operator tenant already holds, `None` while it is
    /// empty: whether an owner exists, decided before any question is asked.
    pub async fn operator_accounts(&self, tenant: i64) -> Result<Option<OperatorAccounts>> {
        let client = self.db.get().await?;
        operator_accounts(&**client, tenant).await
    }

    /// The owner's sign-in address as the bootstrap receipt records it.
    pub async fn recorded_owner_email(&self, tenant: i64) -> Result<Option<String>> {
        let client = self.db.get().await?;
        Ok(client
            .query_opt(
                "SELECT admin_email FROM user_bootstrap WHERE id=1 AND tenant_id=$1",
                &[&tenant],
            )
            .await?
            .map(|row| row.get(0)))
    }

    /// Creates the install's first administrator: the account in the
    /// platform-operator tenant, the grant-all role through Authz's bootstrap
    /// seam, and the one-time bootstrap receipt that makes it the
    /// installation owner, on one audited transaction as the `create-owner`
    /// system principal. With recovery it assigns the receipt to active operator staff or creates a claim.
    /// `settle_bootstrap_affiliation` then creates its Person, business
    /// affiliation and first desk.
    pub async fn create_first_administrator(
        &self,
        grants: &Grants,
        operator: OperatorCompany,
        input: &FirstAdministrator,
        recover: bool,
    ) -> std::result::Result<FirstAdministratorCreated, FirstAdministratorError> {
        let errors = validate_first_administrator(input);
        if !errors.is_empty() {
            return Err(FirstAdministratorError::Validation(errors));
        }
        let email = validate_email(&input.email)
            .map_err(|error| FirstAdministratorError::Validation(vec![error]))?;
        let name = input.name.trim().to_owned();
        // Unusable until the owner chooses their own from the claim link.
        let hash = password::hash(&random_token()?).await?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                Context::system(
                    operator.tenant,
                    CREATE_OWNER_PRINCIPAL,
                    if recover {
                        "user.bootstrap.recover"
                    } else {
                        BOOTSTRAP_ACTION
                    },
                ),
            )
            .await?;
        let Some(names) = tx
            .sql()
            .query_opt(
                "SELECT t.name, c.name, c.code FROM platform_operator o \
                 JOIN tenants t ON t.id=o.tenant_id AND t.deleted_at IS NULL \
                 JOIN tenant_primary_companies p ON p.tenant_id=t.id \
                 JOIN companies c ON c.tenant_id=t.id AND c.id=p.company_id AND c.deleted_at IS NULL \
                 WHERE o.singleton AND o.tenant_id=$1 AND p.company_id=$2",
                &[&operator.tenant, &operator.company],
            )
            .await?
        else {
            return Err(FirstAdministratorError::NotOperator);
        };
        let tenant = operator.tenant;
        let previous: Option<(i64, String)> = tx.sql().query_opt(
            "SELECT user_id,admin_email FROM user_bootstrap WHERE id=1 AND tenant_id=$1 FOR UPDATE",
            &[&tenant],
        ).await?.map(|row| (row.get(0), row.get(1)));
        // Lock: serializes this bootstrap with every other
        // account insert, so the emptiness proof below holds until commit.
        tx.sql()
            .batch_execute("LOCK TABLE users IN SHARE ROW EXCLUSIVE MODE")
            .await?;
        if !recover {
            if tx
                .sql()
                .query_opt(
                    "SELECT id FROM users WHERE tenant_id=$1 AND deleted_at IS NULL LIMIT 1",
                    &[&tenant],
                )
                .await?
                .is_some()
            {
                return Err(FirstAdministratorError::AccountsExist);
            }
        } else if previous.is_none() {
            return Err(Error::Invalid.into());
        }

        if recover {
            if let Some(row) = tx
                .sql()
                .query_opt(
                    "SELECT u.id FROM users u JOIN user_identities i ON i.id=u.identity_id
                 WHERE u.tenant_id=$1 AND u.kind='staff' AND u.deleted_at IS NULL
                   AND i.deleted_at IS NULL AND lower(i.email)=lower($2)",
                    &[&tenant, &email],
                )
                .await?
            {
                let user = row.get(0);
                let email = crate::ownership::active_staff(tx.sql(), tenant, user)
                    .await?
                    .filter(|identity| identity.email.eq_ignore_ascii_case(&email))
                    .ok_or_else(|| {
                        FirstAdministratorError::Validation(vec![field_error(
                            "email",
                            "Choose an active staff account in the operator tenant.",
                        )])
                    })?
                    .email;
                let (old_user, old_email) = previous.ok_or(Error::Internal)?;
                let receipt_replaced = old_user != user;
                if receipt_replaced {
                    tx.sql().execute(
                        "UPDATE user_bootstrap SET user_id=$2,admin_email=$3,affiliation_settled_at=now()
                         WHERE id=1 AND tenant_id=$1",
                        &[&tenant, &user, &email],
                    ).await?;
                    tx.record(Change {
                        kind: "user.bootstrap".into(),
                        id: 1,
                        old: json!({"user_id": old_user, "admin_email": old_email}),
                        new: json!({"user_id": user, "admin_email": email}),
                    })
                    .await?;
                    crate::ownership::expire_offers(&mut tx, tenant, user).await?;
                    tx.commit().await?;
                    drop(client);
                    self.email_recovered_owner(tenant, &old_email).await;
                }
                return Ok(FirstAdministratorCreated {
                    user,
                    email,
                    role: None,
                    receipt_replaced,
                });
            }
        }
        if email_taken(tx.sql(), &email, None).await? {
            return Err(FirstAdministratorError::Validation(vec![field_error(
                "email",
                EMAIL_TAKEN,
            )]));
        }
        let (insert_name, insert_email) = (name, email.clone());
        let user: i64 = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let created = crate::sign_in_identity::create(
                        sql,
                        tenant,
                        AccountKind::Staff,
                        &insert_email,
                        &hash,
                        false,
                    )
                    .await?;
                    let id = created.user;
                    sql.execute(
                        "INSERT INTO users(id,tenant_id,identity_id,name) VALUES($1,$2,$3,$4)",
                        &[&id, &tenant, &created.identity, &insert_name],
                    )
                    .await?;
                    Ok((
                        id,
                        vec![
                            created.change,
                            Change {
                                kind: "user".into(),
                                id,
                                old: json!({}),
                                new: json!({"name": insert_name, "identity_id": created.identity}),
                            },
                        ],
                    ))
                })
            })
            .await?;
        let role = if recover {
            None
        } else {
            Some(
                grants
                    .grant_first_administrator_in(&mut tx, tenant, user)
                    .await?,
            )
        };
        let (tenant_name, company_name, company_code): (String, String, String) =
            (names.get(0), names.get(1), names.get(2));
        let receipt_replaced = previous.is_some();
        let receipt_email = email.clone();
        let former_email = previous.as_ref().map(|(_, email)| email.clone());
        tx.mutate(move |sql| {
            Box::pin(async move {
                let old = match &previous {
                    Some((old_user, old_email)) => {
                        sql.execute(
                            "UPDATE user_bootstrap SET tenant_name=$1,company_name=$2,company_code=$3,admin_email=$4,tenant_id=$5,user_id=$6,company_id=$7,completed_at=now(),affiliation_settled_at=CASE WHEN $8 THEN now() END WHERE id=1 AND tenant_id=$5",
                            &[&tenant_name, &company_name, &company_code, &receipt_email, &tenant, &user, &operator.company, &recover],
                        )
                        .await?;
                        json!({"user_id": old_user, "admin_email": old_email})
                    }
                    None => {
                        sql.execute(
                            "INSERT INTO user_bootstrap(id,tenant_name,company_name,company_code,admin_email,tenant_id,user_id,company_id) VALUES(1,$1,$2,$3,$4,$5,$6,$7)",
                            &[&tenant_name, &company_name, &company_code, &receipt_email, &tenant, &user, &operator.company],
                        )
                        .await?;
                        json!({})
                    }
                };
                Ok((
                    (),
                    vec![Change {
                        kind: "user.bootstrap".into(),
                        id: 1,
                        old,
                        new: json!({"user_id": user, "admin_email": receipt_email, "tenant_id": tenant, "company_id": operator.company, "role_id": role}),
                    }],
                ))
            })
        })
        .await?;
        if recover {
            crate::ownership::expire_offers(&mut tx, tenant, user).await?;
        } else {
            let mut who = Attribution::system(Some(CREATE_OWNER_PRINCIPAL))?;
            who.tenant = Some(tenant);
            record_action(
                tx.sql(),
                &who,
                None,
                BOOTSTRAP_COMPLETED,
                json!({"user_id": user, "role": "core_admin", "created": true}),
                true,
            )
            .await?;
        }
        tx.commit().await?;
        drop(client);
        if recover {
            if let Some(old_email) = former_email {
                self.email_recovered_owner(tenant, &old_email).await;
            }
        }
        Ok(FirstAdministratorCreated {
            user,
            email,
            role,
            receipt_replaced,
        })
    }

    async fn email_recovered_owner(&self, tenant: i64, email: &str) {
        use bilibi_mail::{Address, Message};
        let Ok(address) = Address::new(None, email) else {
            eprintln!("Former owner notice could not be sent: invalid address.");
            return;
        };
        let message = Message::text(
            address,
            "Bilibi installation ownership recovered",
            format!(
                "Installation ownership at {} was reassigned through server recovery.",
                self.origin
            ),
        );
        if self.mailer.deliver_for(tenant, &message).await.is_err() {
            eprintln!("Former owner notice could not be delivered.");
        }
    }

    /// The server owner's claim, read from the bootstrap receipt's account,
    /// never from an address typed at the server. While the owner has not
    /// chosen a password it mails the claim link (again), so the command is
    /// safe to repeat; afterwards it only reports, and the owner finishes by
    /// setting up 2FA. Root can still do anything on its own server: this
    /// keeps the person running the install from holding the owner's sign-in
    /// by mistake or shortcut, not from a malicious administrator.
    pub async fn claim_owner(&self) -> std::result::Result<OwnerClaim, OwnerClaimError> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let Some(row) = client
            .query_opt(
                "SELECT b.tenant_id,b.user_id,u.name,i.email,i.password_version>1 \
                 FROM user_bootstrap b \
                 JOIN users u ON u.tenant_id=b.tenant_id AND u.id=b.user_id AND u.deleted_at IS NULL AND u.status='active' \
                 JOIN user_identities i ON i.id=u.identity_id AND i.deleted_at IS NULL AND i.status='active' \
                 WHERE b.id=1",
                &[],
            )
            .await?
        else {
            return Err(OwnerClaimError::NoOwner);
        };
        let (tenant, user, name, email, chosen): (i64, i64, String, String, bool) =
            (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
        if chosen {
            let tx = self
                .mutations
                .begin(
                    &mut client,
                    Context::system(tenant, CREATE_OWNER_PRINCIPAL, "user.bootstrap.claim"),
                )
                .await?;
            let enrolled = self.step_up.account_enrolled(&tx, user).await?;
            tx.rollback().await?;
            return Ok(if enrolled {
                OwnerClaim::Claimed { email }
            } else {
                OwnerClaim::PasswordChosen { email }
            });
        }
        drop(client);
        self.mailer
            .ready()
            .await
            .map_err(|error| OwnerClaimError::MailNotReady(error.to_string()))?;
        match self
            .reset_mail()
            .mail_owner_claim(tenant, user, &name, &email)
            .await?
        {
            ClaimMail::Sent => Ok(OwnerClaim::Sent { email }),
            ClaimMail::RecentlySent => Ok(OwnerClaim::RecentlySent { email }),
            ClaimMail::NotDelivered(detail) => Err(OwnerClaimError::Delivery(detail)),
        }
    }
}
