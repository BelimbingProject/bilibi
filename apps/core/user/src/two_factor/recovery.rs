//! Password-proved 2FA reset requests and the reviewed administrator reset.
use super::agent::{clear_confirmed, lock_reset_target, reset_mail, reset_notice, wake, Account};
use crate::{notifications, UserService};
use askama::Template;
use bilibi_agent_api::{
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, WriteHandler,
    DEFINITIONS,
};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_mail::{Address, Message};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_notifications::Notice;
use bilibi_security::Scope;
use bilibi_templates::Value as TemplateValue;
use bilibi_ui::Html;
use serde_json::{json, Value};
use std::{
    sync::{Arc, OnceLock},
    time::SystemTime,
};

pub const RESET: &str = "core.user.two_factor.reset";
pub const CAPABILITY: &str = "admin.user.two-factor-reset";
const MAX_RECIPIENTS: usize = 500;

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
    scope: &Scope,
) -> Result<bool> {
    match bilibi_tenancy::prove_owner(client, scope).await {
        Ok(_) => Ok(true),
        Err(Error::Forbidden | Error::Unauthorized) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) async fn administrators(
    client: &impl tokio_postgres::GenericClient,
    tenant: i64,
) -> Result<Vec<i64>> {
    bilibi_authz::holders(client, tenant, CAPABILITY, i64::MAX).await
}

pub(crate) const OWNER_ONLY: &str = "(u.kind='customer' OR EXISTS(SELECT 1 FROM users owner JOIN user_bootstrap b ON b.tenant_id=owner.tenant_id AND b.user_id=owner.id WHERE owner.identity_id=u.identity_id) OR EXISTS(SELECT 1 FROM users other WHERE other.identity_id=u.identity_id AND other.tenant_id<>u.tenant_id AND other.deleted_at IS NULL))";

async fn owner_only(
    client: &impl tokio_postgres::GenericClient,
    tenant: i64,
    user: i64,
) -> Result<bool> {
    Ok(client
        .query_one(
            &format!("SELECT {OWNER_ONLY} FROM users u WHERE u.tenant_id=$1 AND u.id=$2"),
            &[&tenant, &user],
        )
        .await?
        .get(0))
}

async fn recipient_rows(
    tx: &MutationTransaction<'_>,
    scope: &Scope,
    owner: bool,
    admins: &[i64],
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
        admins
            .iter()
            .copied()
            .filter(|user| *user != scope.user())
            .map(|user| (scope.tenant(), user))
            .collect()
    };
    let mut recipients = Vec::new();
    for (tenant, user) in pairs {
        if let Some(row) = tx.sql().query_opt("SELECT u.name,i.email FROM users u JOIN user_identities i ON i.id=u.identity_id AND i.status='active' AND i.deleted_at IS NULL WHERE u.tenant_id=$1 AND u.id=$2 AND u.kind='staff' AND u.status='active' AND u.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM user_scim_users scim WHERE scim.tenant_id=u.tenant_id AND scim.user_id=u.id AND NOT scim.active)", &[&tenant,&user]).await? {
            recipients.push(Recipient { tenant, user, name: row.get(0), email: row.get(1) });
            if recipients.len() == MAX_RECIPIENTS { break; }
        }
    }
    Ok(recipients)
}

fn request_url(scope: &Scope, email: &str, user: i64) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("recoveryTenant", &scope.tenant().to_string());
    if scope.actor().kind() == bilibi_tenancy::AccountKind::Customer {
        query.append_pair("recoveryAccount", &user.to_string());
        format!(
            "/customer-portal?{}#customer-account-{user}",
            query.finish()
        )
    } else {
        query.append_pair("search", email);
        query.append_pair("resetRequested", "open");
        format!("/users?{}", query.finish())
    }
}

fn request_mail(
    service: &UserService,
    recipient: &Recipient,
    account: &str,
    company: &str,
    time: &str,
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

pub(crate) async fn request_in(
    service: &UserService,
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
) -> Result<RequestOutcome> {
    let row = tx.sql().query_one("SELECT u.identity_id,u.name,i.email,u.organisation_id FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1 AND u.id=$2", &[&scope.tenant(),&scope.user()]).await?;
    let identity: i64 = row.get(0);
    let name: String = row.get(1);
    let email: String = row.get(2);
    let account = format!("{name} <{email}>");
    if let Some(row) = tx.sql().query_opt("SELECT requested_at FROM user_two_factor_reset_requests WHERE identity_id=$1 AND closed_at IS NULL AND requested_at>clock_timestamp()-interval '24 hours' ORDER BY requested_at DESC LIMIT 1", &[&identity]).await? {
        return Ok(RequestOutcome { created: false, requested_at: row.get(0), recipient: String::new() });
    }
    let admins = administrators(tx.sql(), scope.tenant()).await?;
    let mut owner =
        owner_only(tx.sql(), scope.tenant(), scope.user()).await? || admins.contains(&scope.user());
    let mut recipients = if owner {
        Vec::new()
    } else {
        recipient_rows(tx, scope, false, &admins).await?
    };
    if recipients.is_empty() {
        owner = true;
        recipients = recipient_rows(tx, scope, true, &[]).await?;
    }
    let recipient = if owner {
        recipients.first().ok_or(Error::Conflict)?.name.clone()
    } else {
        "your administrators".into()
    };
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
    let (requested_at, time): (SystemTime, String) = tx.mutate(move |sql| Box::pin(async move {
        let row = sql.query_one("INSERT INTO user_two_factor_reset_requests(tenant_id,user_id,identity_id,company_id,company_name,recipient_kind) VALUES($1,$2,$3,$4,$5,$6) RETURNING id,requested_at,to_char(requested_at AT TIME ZONE 'UTC','YYYY-MM-DD HH24:MI UTC')", &[&tenant,&user,&identity,&company_id,&company_for_change,&recipient_kind]).await?;
        Ok(((row.get(1),row.get(2)),vec![Change { kind:"user_two_factor_reset_request".into(), id:row.get(0), old:json!({}), new:json!({"user_id":user,"company":company_for_change,"requested_at":row.get::<_,SystemTime>(1),"recipient":recipient_kind}) }]))
    })).await?;
    let url = request_url(scope, &email, user);
    let notice = Notice::new("2FA reset requested")
        .kind("two_factor_reset_request")
        .icon("shield-exclamation")
        .body(format!(
            "{account} requested a 2FA reset for {company} at {time}."
        ))
        .url(&url);
    let recipient_ids: Vec<_> = recipients.iter().map(|recipient| recipient.user).collect();
    if owner && recipients[0].tenant != tenant {
        service
            .notifications
            .notify_bootstrap_owner(tx, &notice)
            .await?;
    } else {
        service
            .notification_boundary
            .notify(tx, scope, &recipient_ids, &notice)
            .await?;
    }
    for recipient in recipients {
        if let Some(message) = request_mail(service, &recipient, &account, &company, &time, &url) {
            let mailer = service.mailer.clone();
            tx.after_commit(move || {
                tokio::spawn(async move {
                    if let Err(error) = mailer.deliver_for(recipient.tenant, &message).await {
                        eprintln!("mail: 2FA reset request was not delivered: {error}");
                    }
                });
            });
        }
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
    let row = client.query_opt("SELECT u.id,u.tenant_id,u.name,i.email,u.status='active' FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1 AND u.id=$2 AND u.kind IN ('staff','customer') AND u.deleted_at IS NULL", &[&scope.tenant(),&user]).await?.ok_or(Error::NotFound)?;
    let actor_owner = installation_owner(client, scope).await?;
    if !actor_owner
        && (owner_only(client, scope.tenant(), user).await?
            || administrators(client, scope.tenant())
                .await?
                .contains(&user))
    {
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
    lock_reset_target(tx, user).await?;
    let (account, owner) = account(tx.sql(), scope, user).await?;
    let (ended, rows) = clear_confirmed(service, tx, &account, json!({"reset_by":if owner {"installation_owner"} else {"tenant_administrator"},"reset_by_user_id":scope.user()}), Some(scope.user())).await?;
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
    identity: i64,
    actor: Option<i64>,
) -> Result<usize> {
    let open = tx.sql().query_one("SELECT EXISTS(SELECT 1 FROM user_two_factor_reset_requests WHERE identity_id=$1 AND closed_at IS NULL)", &[&identity]).await?.get::<_,bool>(0);
    if !open {
        return Ok(0);
    }
    tx.mutate(move |sql| Box::pin(async move {
        let rows = sql.query("UPDATE user_two_factor_reset_requests SET closed_at=now(),closed_by_user_id=$2 WHERE identity_id=$1 AND closed_at IS NULL RETURNING id,tenant_id", &[&identity,&actor]).await?;
        let changes = rows.iter().map(|row| Change {kind:"user_two_factor_reset_request".into(),id:row.get(0),old:json!({"status":"open","tenant_id":row.get::<_,i64>(1)}),new:json!({"status":"closed","tenant_id":row.get::<_,i64>(1),"closed_by_user_id":actor})}).collect();
        Ok((rows.len(), changes))
    })).await
}

#[derive(Template)]
#[template(
    source = r#"{% import "dialog.html" as dialog %}{% call dialog::confirm_dialog(id="reset-two-factor-confirm", consequence=consequence, detail="They will be signed out everywhere and emailed. They must set up 2FA again after signing in.", confirm="Reset 2FA", working="Resetting…", confirm_event="reset_two_factor", cancel_event="cancel_reset_two_factor", body=body) %}{% endcall %}"#,
    ext = "html"
)]
struct ResetDialog<'a> {
    consequence: &'a str,
    body: Html,
}
impl UserService {
    pub async fn two_factor_reset_dialog(
        &self,
        scope: &Scope,
        name: &str,
        error: Option<&str>,
    ) -> Result<Html> {
        let methods = self
            .step_up
            .enrolled_methods(scope)
            .await?
            .into_iter()
            .map(|method| {
                bilibi_ui::ProofMethod::new(
                    method.id,
                    method.label,
                    method.code_length,
                    method.numeric,
                )
            })
            .collect();
        let mut body = Html::EMPTY;
        if let Some(message) = error {
            body.push(
                &Html::render(&bilibi_ui::Alert {
                    kind: bilibi_ui::StatusKind::Error,
                    id: "reset-two-factor-error",
                    class: "mb-3",
                    body: Html::text(message),
                })
                .map_err(|_| Error::Internal)?,
            );
        }
        body.push(
            &Html::render(&bilibi_ui::ProofInput::new(
                "reset-two-factor-code",
                "proof",
                methods,
            ))
            .map_err(|_| Error::Internal)?,
        );
        Html::render(&ResetDialog {
            consequence: &format!("{name}'s 2FA methods will be turned off."),
            body,
        })
        .map_err(|_| Error::Internal)
    }
}

pub async fn customer_reset_requests(
    client: &(impl tokio_postgres::GenericClient + Sync),
    scope: &Scope,
    accounts: &[i64],
) -> Result<std::collections::HashMap<i64, SystemTime>> {
    if accounts.is_empty()
        || !installation_owner(client, scope).await?
        || !bilibi_authz::decide(client, scope, CAPABILITY)
            .await?
            .allowed
    {
        return Ok(std::collections::HashMap::new());
    }
    Ok(client.query("SELECT r.user_id,max(r.requested_at) FROM user_two_factor_reset_requests r JOIN users u ON u.tenant_id=r.tenant_id AND u.id=r.user_id WHERE r.tenant_id=$1 AND r.user_id=ANY($2) AND r.closed_at IS NULL AND u.kind='customer' AND u.deleted_at IS NULL AND u.id<>$3 GROUP BY r.user_id", &[&scope.tenant(), &accounts, &scope.user()]).await?.iter().map(|row| (row.get(0),row.get(1))).collect())
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
