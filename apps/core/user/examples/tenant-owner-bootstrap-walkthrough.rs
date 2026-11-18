//! Disposable proof of the server-side customer-tenant first-owner path.
//! It runs the one operator command, follows the dev-mailbox invitation,
//! proves the role and workplace setup, then reruns the command and proves the
//! existing owner was refused without another write or message.
use bilibi_database::{pool, Error, Result};
use bilibi_mail::Mailbox;
use std::{path::PathBuf, process::ExitStatus, time::Duration};

const OWNER: &str = "owner@customer.example.test";

fn mailbox_dir() -> PathBuf {
    std::env::var("BILIBI_MAIL_DIR")
        .unwrap_or_else(|_| ".local/mailbox".into())
        .into()
}

fn count(mailbox: &Mailbox) -> usize {
    mailbox.entries().map(|entries| entries.len()).unwrap_or(0)
}

async fn invitation_token(mailbox: &Mailbox, before: usize) -> Result<String> {
    for _ in 0..50 {
        if count(mailbox) > before {
            let newest = mailbox
                .entries()
                .map_err(|_| Error::Internal)?
                .into_iter()
                .next()
                .ok_or(Error::Internal)?;
            let message = mailbox.read(&newest.id).map_err(|_| Error::Internal)?;
            return message
                .lines()
                .find_map(|line| line.split("/accept/").nth(1))
                .and_then(|rest| rest.split_whitespace().next())
                .filter(|token| token.len() == 64)
                .map(str::to_owned)
                .ok_or(Error::Internal);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Error::Internal)
}

async fn snapshot(client: &tokio_postgres::Client) -> Result<String> {
    Ok(client
        .query_one(
            "SELECT jsonb_build_object(
                'tenants',(SELECT count(*) FROM tenants),
                'companies',(SELECT count(*) FROM companies),
                'users',(SELECT count(*) FROM users),
                'invitations',(SELECT count(*) FROM user_invitations),
                'owner_receipts',(SELECT count(*) FROM user_tenant_owner_bootstraps),
                'roles',(SELECT count(*) FROM principal_roles),
                'people',(SELECT count(*) FROM persons),
                'affiliations',(SELECT count(*) FROM person_affiliations),
                'desks',(SELECT count(*) FROM user_desks),
                'actions',(SELECT count(*) FROM base_audit_actions),
                'mutations',(SELECT count(*) FROM base_audit_mutations)
            )::text",
            &[],
        )
        .await?
        .get(0))
}

fn command() -> Vec<String> {
    [
        "--tenant-name",
        "Customer workspace",
        "--company-name",
        "Customer Company Sdn. Bhd.",
        "--company-code",
        "customer",
        "--owner-email",
        OWNER,
        "--owner-name",
        "Customer Owner",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Run the actual server command in its own process, as an installation
/// operator does. A process owns one module-service binding; the walkthrough
/// then opens a separate app process to follow the invitation.
fn provision() -> Result<ExitStatus> {
    let executable = std::env::current_exe().map_err(|_| Error::Internal)?;
    let web = executable
        .parent()
        .and_then(|examples| examples.parent())
        .map(|debug| debug.join("bilibi-web"))
        .ok_or(Error::Internal)?;
    std::process::Command::new(web)
        .arg("tenant-provision")
        .args(command())
        .status()
        .map_err(|_| Error::Internal)
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_TENANT_OWNER_DEMO").as_deref() != Ok("disposable") {
        return Err(Error::Invalid);
    }
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let mailbox = Mailbox::new(mailbox_dir(), 100);
    let before_mail = count(&mailbox);
    if !provision()?.success() {
        return Err(Error::Internal);
    }
    let token = invitation_token(&mailbox, before_mail).await?;
    let app = bilibi_web::App::new(
        db.clone(),
        &std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?,
    )?;
    let invitation = app
        .user
        .member_invitation(&token)
        .await
        .map_err(|_| Error::Internal)?;
    let user = app
        .user
        .accept_new_member_invitation(&token, "owner-proof-password")
        .await
        .map_err(|_| Error::Internal)?;
    let client = db.get_detached("tenant owner walkthrough checks").await?;
    let row = client
        .query_one(
            "SELECT t.id,c.id,u.status,i.email,p.kind,
                EXISTS(SELECT 1 FROM principal_roles x JOIN roles r ON r.tenant_id=x.tenant_id AND r.id=x.role_id WHERE x.tenant_id=t.id AND x.user_id=u.id AND r.code='core_admin' AND r.is_system AND r.grant_all),
                EXISTS(SELECT 1 FROM person_affiliations a WHERE a.tenant_id=t.id AND a.person_id=u.person_id AND a.organisation_id=c.id AND a.unlinked_at IS NULL),
                EXISTS(SELECT 1 FROM user_desks d WHERE d.tenant_id=t.id AND d.person_id=u.person_id AND d.company_id=c.id),
                b.affiliation_settled_at IS NOT NULL
             FROM tenants t
             JOIN tenant_primary_companies x ON x.tenant_id=t.id
             JOIN companies c ON c.tenant_id=t.id AND c.id=x.company_id
             JOIN users u ON u.tenant_id=t.id AND u.id=$1
             JOIN user_identities i ON i.id=u.identity_id
             JOIN persons p ON p.tenant_id=t.id AND p.id=u.person_id
             JOIN user_tenant_owner_bootstraps b ON b.tenant_id=t.id AND b.user_id=u.id
             WHERE t.name='Customer workspace' AND c.name='Customer Company Sdn. Bhd.' AND c.code='customer'",
            &[&user],
        )
        .await?;
    let tenant: i64 = row.get(0);
    let company: i64 = row.get(1);
    let owner_ready = row.get::<_, String>(2) == "active"
        && row.get::<_, String>(3) == OWNER
        && row.get::<_, String>(4) == "human"
        && row.get::<_, bool>(5)
        && row.get::<_, bool>(6)
        && row.get::<_, bool>(7)
        && row.get::<_, bool>(8);
    let action_row = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM base_audit_actions WHERE tenant_id=$1 AND system_principal='provision' AND event='company.tenant.provisioned'),
                EXISTS(SELECT 1 FROM base_audit_actions WHERE tenant_id=$1 AND system_principal='tenant-provision' AND event='user.tenant_owner.invited')",
            &[&tenant],
        )
        .await?;
    let action_audit = action_row.get::<_, bool>(0) && action_row.get::<_, bool>(1);
    let mutation_row = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM base_audit_mutations WHERE tenant_id=$1 AND actor_type='system' AND system_principal='tenant-provision' AND event='updated' AND auditable_type='authz.principal_role'),
                EXISTS(SELECT 1 FROM base_audit_mutations WHERE tenant_id=$1 AND actor_type='user' AND actor_id=$2 AND event='updated' AND auditable_type='user.tenant_owner_bootstrap')",
            &[&tenant, &user],
        )
        .await?;
    let mutation_audit = mutation_row.get::<_, bool>(0) && mutation_row.get::<_, bool>(1);
    println!(
        "Customer owner accepted: tenant={tenant} company={company} user={user} invitation_tenant={} owner_ready={owner_ready}",
        invitation.tenant_name
    );
    println!(
        "Provision actions audited: company={} owner={}; mutations audited: owner_role={} acceptance={}",
        action_row.get::<_, bool>(0),
        action_row.get::<_, bool>(1),
        mutation_row.get::<_, bool>(0),
        mutation_row.get::<_, bool>(1)
    );
    if invitation.tenant_name != "Customer workspace"
        || !owner_ready
        || !action_audit
        || !mutation_audit
    {
        return Err(Error::Internal);
    }

    let before_retry = snapshot(&client).await?;
    let mail_before_retry = count(&mailbox);
    let refused = !provision()?.success();
    let after_retry = snapshot(&client).await?;
    let unchanged = before_retry == after_retry && mail_before_retry == count(&mailbox);
    println!("Existing owner refused: {refused}; no retry writes or mail: {unchanged}");
    if !refused || !unchanged {
        return Err(Error::Internal);
    }
    Ok(())
}
