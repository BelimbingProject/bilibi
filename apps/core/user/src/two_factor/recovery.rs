//! Password-proved 2FA reset requests and the reviewed administrator reset.
use super::agent::{clear_confirmed, reset_mail, reset_notice, wake, Account};
use crate::{notifications, UserService};
use bilibi_agent_api::{
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, WriteHandler,
    DEFINITIONS,
};
use bilibi_database::{Change, Context, Error, MutationTransaction, Result};
use bilibi_mail::{Address, Message};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_notifications::Notice;
use bilibi_security::Scope;
use bilibi_templates::Value as TemplateValue;
use serde_json::{json, Value};
use std::{
    net::IpAddr,
    sync::{Arc, OnceLock},
    time::SystemTime,
};

pub const RESET: &str = "core.user.two_factor.reset";
pub const CAPABILITY: &str = "admin.user.two-factor-reset";
const MAX_RECIPIENTS: i64 = 500;

bilibi_step_up::declare!(
    "core/user",
    bilibi_step_up::Operation::new(RESET, "Reset another person's 2FA").always()
);

static USERS: OnceLock<Arc<UserService>> = OnceLock::new();

pub(crate) fn bind(users: Arc<UserService>) -> bilibi_module_registry::Result<()> {
    USERS.set(users).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate 2FA recovery binding".into())
    })
}

fn users() -> bilibi_agent_api::Result<&'static UserService> {
    USERS.get().map(Arc::as_ref).ok_or(Error::Internal.into())
}

#[derive(Clone, Debug)]
pub struct RequestOutcome {
    pub created: bool,
    pub requested_at: SystemTime,
    pub recipient: String,
}

struct Recipient {
    tenant: i64,
    user: i64,
    name: String,
    email: String,
}

pub(crate) async fn installation_owner(
    client: &(impl tokio_postgres::GenericClient + Sync),
    tenant: i64,
    user: i64,
) -> Result<bool> {
    Ok(client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM users actor JOIN users owner ON owner.identity_id=actor.identity_id JOIN user_bootstrap b ON b.tenant_id=owner.tenant_id AND b.user_id=owner.id WHERE actor.tenant_id=$1 AND actor.id=$2 AND actor.kind='staff' AND actor.status='active' AND actor.deleted_at IS NULL)",
            &[&tenant, &user],
        )
        .await?
        .get(0))
}

async fn recipient_rows(
    tx: &MutationTransaction<'_>,
    scope: &Scope,
    owner: bool,
) -> Result<Vec<Recipient>> {
    let pairs: Vec<(i64, i64)> = if owner {
        tx.sql()
            .query_opt(
                "SELECT tenant_id,user_id FROM user_bootstrap WHERE id=1",
                &[],
            )
            .await?
            .map(|row| vec![(row.get(0), row.get(1))])
            .unwrap_or_default()
    } else {
        bilibi_authz::holders(tx.sql(), scope.tenant(), CAPABILITY, MAX_RECIPIENTS)
            .await?
            .into_iter()
            .filter(|user| *user != scope.user())
            .map(|user| (scope.tenant(), user))
            .collect()
    };
    let mut recipients = Vec::new();
    for (tenant, user) in pairs {
        if let Some(row) = tx.sql().query_opt("SELECT u.name,i.email FROM users u JOIN user_identities i ON i.id=u.identity_id AND i.status='active' AND i.deleted_at IS NULL WHERE u.tenant_id=$1 AND u.id=$2 AND u.kind='staff' AND u.status='active' AND u.deleted_at IS NULL", &[&tenant,&user]).await? {
            recipients.push(Recipient { tenant, user, name: row.get(0), email: row.get(1) });
        }
    }
    Ok(recipients)
}

fn request_url(email: &str) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("search", email);
    query.append_pair("resetRequested", "open");
    format!("/users?{}", query.finish())
}

fn request_mail(
    service: &UserService,
    recipient: &Recipient,
    account: &str,
    company: &str,
    time: &str,
    ip: &str,
    url: &str,
) -> Option<Message> {
    let wording = service
        .templates
        .platform_wording(
            crate::wording::TWO_FACTOR_RESET_REQUEST,
            &[
                ("account", TemplateValue::Text(account)),
                ("company", TemplateValue::Text(company)),
                ("time", TemplateValue::Text(time)),
                ("ip_address", TemplateValue::Text(ip)),
            ],
        )
        .ok()?;
    Some(Message::text(
        Address::new(Some(&recipient.name), &recipient.email).ok()?,
        wording["subject"].clone(),
        format!(
            "{}\n\n{}\n\n{}{url}\n",
            wording["details"], wording["closing"], service.origin
        ),
    ))
}

async fn deliver_cross_tenant(
    service: Arc<UserService>,
    recipient: Recipient,
    notice: Notice,
    mail: Option<Message>,
) {
    let outcome: Result<()> = async {
        let mut client = service.db.get().await?;
        let scope = service
            .authentication
            .authenticate(&**client, recipient.tenant, recipient.user, String::new())
            .await?;
        let mut tx = service
            .mutations
            .begin(
                &mut client,
                Context::system(
                    recipient.tenant,
                    "two-factor-reset-request",
                    "user.two_factor.reset_request.notified",
                ),
            )
            .await?;
        service
            .notification_boundary
            .notify(&mut tx, &scope, &[recipient.user], &notice)
            .await?;
        tx.commit().await
    }
    .await;
    if let Err(error) = outcome {
        eprintln!("2FA reset request notice was not delivered: {error}");
    }
    if let Some(message) = mail {
        if let Err(error) = service.mailer.deliver_for(recipient.tenant, &message).await {
            eprintln!("mail: 2FA reset request was not delivered: {error}");
        }
    }
}

pub(crate) async fn request_in(
    service: &UserService,
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    ip: Option<IpAddr>,
) -> Result<RequestOutcome> {
    let row = tx.sql().query_one("SELECT u.identity_id,u.name,i.email,u.organisation_id FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1 AND u.id=$2 FOR UPDATE OF i", &[&scope.tenant(),&scope.user()]).await?;
    let identity: i64 = row.get(0);
    let name: String = row.get(1);
    let email: String = row.get(2);
    let account = format!("{name} <{email}>");
    let admins =
        bilibi_authz::holders(tx.sql(), scope.tenant(), CAPABILITY, MAX_RECIPIENTS).await?;
    let installation_owner = installation_owner(tx.sql(), scope.tenant(), scope.user()).await?;
    let owner = installation_owner || admins.contains(&scope.user());
    let recipients = recipient_rows(tx, scope, owner).await?;
    let recipient = if owner {
        recipients
            .first()
            .map(|recipient| recipient.name.clone())
            .unwrap_or_else(|| "the server owner".into())
    } else {
        "your administrators".into()
    };
    if let Some(row) = tx.sql().query_opt("SELECT requested_at FROM user_two_factor_reset_requests WHERE identity_id=$1 AND closed_at IS NULL AND requested_at>clock_timestamp()-interval '24 hours' ORDER BY requested_at DESC LIMIT 1", &[&identity]).await? {
        return Ok(RequestOutcome { created: false, requested_at: row.get(0), recipient });
    }
    let company_id = scope
        .current_company()
        .map(|id| id.get())
        .or_else(|| row.get::<_, Option<i64>>(3));
    let company = match company_id {
        Some(id) => tx
            .sql()
            .query_opt(
                "SELECT name FROM companies WHERE tenant_id=$1 AND id=$2 AND deleted_at IS NULL",
                &[&scope.tenant(), &id],
            )
            .await?
            .map(|row| row.get(0))
            .unwrap_or_else(|| scope.tenant_name.clone()),
        None => scope.tenant_name.clone(),
    };
    let recipient_kind = if owner {
        "installation_owner"
    } else {
        "tenant_administrators"
    };
    let tenant = scope.tenant();
    let user = scope.user();
    let company_for_change = company.clone();
    let ip_string = ip.map(|value| value.to_string());
    let ip_for_change = ip_string.clone();
    let ip_for_audit = ip_for_change.clone();
    let (_id, requested_at, time): (i64, SystemTime, String) = tx.mutate(move |sql| Box::pin(async move {
        let row = sql.query_one("INSERT INTO user_two_factor_reset_requests(tenant_id,user_id,identity_id,company_id,company_name,request_ip,recipient_kind) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id,requested_at,to_char(requested_at AT TIME ZONE 'UTC','YYYY-MM-DD HH24:MI UTC')", &[&tenant,&user,&identity,&company_id,&company_for_change,&ip,&recipient_kind]).await?;
        let id: i64 = row.get(0);
        Ok(((id,row.get(1),row.get(2)),vec![Change { kind:"user_two_factor_reset_request".into(), id, old:json!({}), new:json!({"user_id":user,"company":company_for_change,"requested_at":row.get::<_,SystemTime>(1),"request_ip":ip_for_audit,"recipient":recipient_kind}) }]))
    })).await?;
    let url = request_url(&email);
    let notice = Notice::new(format!("2FA reset requested by {name}"))
        .kind("two_factor_reset_request")
        .icon("shield-exclamation")
        .body(format!(
            "{account} requested a 2FA reset for {company} at {time}. Request IP: {}.",
            ip_for_change.as_deref().unwrap_or("not recorded")
        ))
        .url(&url);
    notice.validate()?;
    let mut cross = Vec::new();
    let ip_label = ip_for_change.as_deref().unwrap_or("not recorded");
    for recipient in recipients {
        let mail = request_mail(
            service, &recipient, &account, &company, &time, ip_label, &url,
        );
        if recipient.tenant == scope.tenant() {
            service
                .notification_boundary
                .notify(tx, scope, &[recipient.user], &notice)
                .await?;
            if let Some(message) = mail {
                let mailer = service.mailer.clone();
                let target_tenant = recipient.tenant;
                tx.after_commit(move || {
                    tokio::spawn(async move {
                        if let Err(error) = mailer.deliver_for(target_tenant, &message).await {
                            eprintln!("mail: 2FA reset request was not delivered: {error}");
                        }
                    });
                });
            }
        } else {
            cross.push((recipient, mail));
        }
    }
    if !cross.is_empty() {
        let service = USERS.get().cloned().ok_or(Error::Internal)?;
        tx.after_commit(move || {
            for (recipient, mail) in cross {
                let service = service.clone();
                let notice = notice.clone();
                tokio::spawn(deliver_cross_tenant(service, recipient, notice, mail));
            }
        });
    }
    Ok(RequestOutcome {
        created: true,
        requested_at,
        recipient,
    })
}

async fn account(
    client: &(impl tokio_postgres::GenericClient + Sync),
    scope: &Scope,
    user: i64,
) -> Result<(Account, bool)> {
    if scope.impersonator().is_some() || user == scope.user() {
        return Err(Error::Forbidden);
    }
    bilibi_authz::authorize_key(client, scope, CAPABILITY).await?;
    let row = client.query_opt("SELECT u.id,u.tenant_id,u.name,i.email,u.status='active',EXISTS(SELECT 1 FROM users owner JOIN user_bootstrap b ON b.tenant_id=owner.tenant_id AND b.user_id=owner.id WHERE owner.identity_id=u.identity_id) FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1 AND u.id=$2 AND u.kind IN ('staff','customer') AND u.deleted_at IS NULL", &[&scope.tenant(),&user]).await?.ok_or(Error::NotFound)?;
    let protected: bool = row.get(5);
    let actor_owner = installation_owner(client, scope.tenant(), scope.user()).await?;
    let admins = bilibi_authz::holders(client, scope.tenant(), CAPABILITY, MAX_RECIPIENTS).await?;
    if !actor_owner && (protected || admins.contains(&user)) {
        return Err(Error::Forbidden);
    }
    Ok((
        Account {
            id: row.get(0),
            tenant: row.get(1),
            name: row.get(2),
            email: row.get(3),
            active: row.get(4),
        },
        actor_owner,
    ))
}

async fn reset_in(
    service: &UserService,
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    user: i64,
) -> Result<Value> {
    let (account, owner) = account(tx.sql(), scope, user).await?;
    let ended = clear_confirmed(service, tx, &account, json!({"reset_by":if owner {"installation_owner"} else {"tenant_administrator"},"reset_by_user_id":scope.user()})).await?;
    let identity = crate::sign_in_identity::id(tx.sql(), account.tenant, account.id).await?;
    let rows = close_requests(tx, account.tenant, identity, Some(scope.user())).await?;
    if account.active {
        notifications::record_in_tenant(
            tx,
            account.tenant,
            &[account.id],
            &reset_notice(crate::wording::TWO_FACTOR_ADMIN_BODY)?,
        )
        .await?;
    }
    let mail = reset_mail(
        service,
        &account,
        if owner { "by_server_admin" } else { "by_admin" },
    );
    let tenant = account.tenant;
    let target = account.id;
    let service = USERS.get().cloned().ok_or(Error::Internal)?;
    tx.after_commit(move || wake(&service, tenant, target, ended, mail));
    Ok(json!({"user_id":user,"reset":true,"closed_requests":rows}))
}

pub(super) async fn close_requests(
    tx: &mut MutationTransaction<'_>,
    tenant: i64,
    identity: i64,
    actor: Option<i64>,
) -> Result<usize> {
    tx.mutate(move |sql| Box::pin(async move {
        let rows=sql.query("UPDATE user_two_factor_reset_requests SET closed_at=now(),closed_by_user_id=$3 WHERE tenant_id=$1 AND identity_id=$2 AND closed_at IS NULL RETURNING id", &[&tenant,&identity,&actor]).await?;
        let changes=rows.iter().map(|row|Change{kind:"user_two_factor_reset_request".into(),id:row.get(0),old:json!({"status":"open"}),new:json!({"status":"closed","closed_by_user_id":actor})}).collect();
        Ok((rows.len(),changes))
    })).await
}

struct Reset;
impl WriteHandler for Reset {
    fn owner(&self) -> &'static str {
        "core/user"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let user = input["user_id"]
                .as_i64()
                .filter(|id| *id > 0)
                .ok_or(Error::Invalid)?;
            let (account, _) = match client.transaction() {
                Some(tx) => account(tx, scope, user).await?,
                None => {
                    let pooled = users()?.db.get().await.map_err(Error::from)?;
                    account(&**pooled, scope, user).await?
                }
            };
            Ok(json!({"user_id":user,"account":account.name,"signs_out_everywhere":true,"emails_account":true}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let user = input["user_id"]
                .as_i64()
                .filter(|id| *id > 0)
                .ok_or(Error::Invalid)?;
            Ok(reset_in(users()?, tx, scope, user).await?.into())
        })
    }
}
fn schema() -> Value {
    json!({"type":"object","required":["user_id"],"properties":{"user_id":{"type":"integer","minimum":1}}})
}
fn output() -> Value {
    json!({"type":"object"})
}
inventory::submit! {Contribution::new(DEFINITIONS,"core/user.two-factor-recovery",Provenance{module:"core/user",source:file!(),line:line!()},&Definitions{operations:&[
    Operation{key:RESET,title:"Reset another person's 2FA",summary:"Turn off an eligible account's 2FA, sign it out everywhere, email the person and close its open reset request.",keywords:&["2fa","two-factor","reset","lost phone"],capability:CAPABILITY,input:schema,output,handler:Handler::SafeWrite(&Reset),channels:Channels::Every,step_up:&[RESET]},
],guides:&[]})}
