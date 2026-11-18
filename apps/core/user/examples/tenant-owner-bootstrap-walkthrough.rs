//! Disposable proof of the server-side customer-tenant first-owner path.
//! Exercises the operator command and ordinary invitation acceptance against a fresh database.
use bilibi_database::{pool, Error, Result};
use bilibi_mail::Mailbox;
use bilibi_user::{AccountInput, InvitationError};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

const OWNER: &str = "owner@customer.example.test";
const PASSWORD: &str = "owner-proof-password";

fn mailbox_dir() -> PathBuf {
    std::env::var("BILIBI_MAIL_DIR")
        .unwrap_or_else(|_| ".local/mailbox".into())
        .into()
}

fn count(mailbox: &Mailbox) -> Result<usize> {
    mailbox
        .entries()
        .map(|entries| entries.len())
        .map_err(|_| Error::Internal)
}

fn invitation_token(
    mailbox: &Mailbox,
    before: usize,
    origin: &str,
    recipient: &str,
    tenant: &str,
) -> Result<String> {
    if count(mailbox)? != before + 1 {
        return Err(Error::Internal);
    }
    let newest = mailbox
        .entries()
        .map_err(|_| Error::Internal)?
        .into_iter()
        .next()
        .ok_or(Error::Internal)?;
    let message = mailbox.read(&newest.id).map_err(|_| Error::Internal)?;
    let recipient = format!("<{recipient}>");
    if !message
        .lines()
        .any(|line| line.starts_with("To: ") && line.ends_with(&recipient))
        || !message.contains(&format!("You are invited to become the owner of {tenant}."))
    {
        return Err(Error::Internal);
    }
    let prefix = format!("{origin}/accept/");
    message
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .filter(|token| token.len() == 64)
        .map(str::to_owned)
        .ok_or(Error::Internal)
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
                'bootstrap_state',(SELECT jsonb_agg(jsonb_build_array(to_jsonb(b),to_jsonb(u),to_jsonb(i),identity.email,grants.roles) ORDER BY b.tenant_id)
                    FROM user_tenant_owner_bootstraps b
                    JOIN users u ON u.tenant_id=b.tenant_id AND u.id=b.user_id
                    JOIN user_invitations i ON i.tenant_id=b.tenant_id AND i.id=b.invitation_id
                    LEFT JOIN user_identities identity ON identity.id=u.identity_id
                    CROSS JOIN LATERAL (SELECT jsonb_agg(jsonb_build_array(r.id,r.code,r.grant_all) ORDER BY r.id) AS roles
                        FROM principal_roles pr JOIN roles r ON r.tenant_id=pr.tenant_id AND r.id=pr.role_id
                        WHERE pr.tenant_id=u.tenant_id AND pr.user_id=u.id) grants),
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

fn web_command() -> Result<Command> {
    let executable = std::env::current_exe().map_err(|_| Error::Internal)?;
    let web = executable
        .parent()
        .and_then(|examples| examples.parent())
        .map(|debug| debug.join("bilibi-web"))
        .ok_or(Error::Internal)?;
    Ok(Command::new(web))
}

fn provisioning_command(tenant: &str) -> Result<Command> {
    let mut command = web_command()?;
    command.args([
        "tenant-provision",
        "--tenant-name",
        tenant,
        "--company-name",
        "Customer Company Sdn. Bhd.",
        "--company-code",
        "customer",
    ]);
    Ok(command)
}

fn provision(tenant: &str, email: Option<&str>, mailbox: Option<&Path>) -> Result<Output> {
    let mut command = provisioning_command(tenant)?;
    if let Some(email) = email {
        command.args(["--owner-email", email]);
    }
    if let Some(mailbox) = mailbox {
        command.env("BILIBI_MAIL_DIR", mailbox);
    }
    command.output().map_err(|_| Error::Internal)
}

fn expect_output(output: Output, refusal: Option<&str>) -> Result<()> {
    let matches = match refusal {
        None => output.status.success(),
        Some(message) => {
            output.status.code() == Some(1)
                && String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .any(|line| line == message)
        }
    };
    if !matches {
        return Err(Error::Refused(format!(
            "Unexpected result from tenant-provision: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

struct RetryState {
    tenant: i64,
    user: i64,
    invitation: i64,
    digest: Vec<u8>,
    unexpired: bool,
    reissues: i64,
}

impl RetryState {
    fn same_invitation(&self, other: &Self) -> bool {
        (self.tenant, self.user, self.invitation) == (other.tenant, other.user, other.invitation)
    }
}

async fn retry_state(client: &tokio_postgres::Client) -> Result<RetryState> {
    let row = client
        .query_one(
            "SELECT b.tenant_id,b.user_id,i.id,i.token_hash,i.expires_at>clock_timestamp(),
             (SELECT count(*) FROM base_audit_actions a WHERE a.tenant_id=b.tenant_id
                AND a.system_principal='tenant-provision' AND a.event='user.tenant_owner.reissued')
         FROM tenants t
         JOIN user_tenant_owner_bootstraps b ON b.tenant_id=t.id
         JOIN user_invitations i ON i.tenant_id=b.tenant_id AND i.id=b.invitation_id
         WHERE t.name='Delivery retry workspace'",
            &[],
        )
        .await?;
    Ok(RetryState {
        tenant: row.get(0),
        user: row.get(1),
        invitation: row.get(2),
        digest: row.get(3),
        unexpired: row.get(4),
        reissues: row.get(5),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_TENANT_OWNER_DEMO").as_deref() != Ok("disposable") {
        return Err(Error::Invalid);
    }
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    assert_eq!(db.status().max_size, 1, "run with DATABASE_POOL_SIZE=1");
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let directory = mailbox_dir();
    let mailbox = Mailbox::new(directory.clone(), 100);
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let before_mail = count(&mailbox)?;
    let client = db.get().await?;
    let before_preflight = snapshot(&client).await?;
    let invalid_address = format!("{}@customer.example.test", "a".repeat(65));
    expect_output(
        provision("Invalid address workspace", Some(&invalid_address), None)?,
        Some("Cannot prepare the owner invitation: Invalid email message"),
    )?;
    for (flag, value) in [
        ("--tenant-name", "Different workspace"),
        ("--company-name", "Different company"),
        ("--company-code", "different"),
        ("--owner-email", "different@customer.example.test"),
    ] {
        let output = provisioning_command("Duplicate options workspace")?
            .args(["--owner-email", OWNER, flag, value])
            .output()
            .map_err(|_| Error::Internal)?;
        expect_output(output, Some(&format!("Repeated option: {flag}")))?;
    }
    if snapshot(&client).await? != before_preflight || count(&mailbox)? != before_mail {
        return Err(Error::Internal);
    }
    drop(client);
    let workspace = "--Customer workspace ".repeat(10) + "headquarters";
    expect_output(provision(&workspace, Some(OWNER), None)?, None)?;
    let token = invitation_token(&mailbox, before_mail, &origin, OWNER, &workspace)?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    let invitation = app
        .user
        .member_invitation(&token)
        .await
        .map_err(|_| Error::Internal)?;
    let user = app
        .user
        .accept_new_member_invitation(&token, PASSWORD)
        .await
        .map_err(|_| Error::Internal)?;
    let login = app
        .user
        .login(OWNER, PASSWORD)
        .await
        .map_err(|_| Error::Unauthorized)?;
    let owner = app.security.guard(&login.token).await?;
    let retry_email = "retry@customer.example.test";
    let retry_account = app
        .user
        .create_account(
            &owner,
            &AccountInput {
                name: "Second workspace invitee".into(),
                email: retry_email.into(),
                password: PASSWORD.into(),
                ..AccountInput::default()
            },
        )
        .await
        .map_err(|_| Error::Internal)?;
    let retry_login = app
        .user
        .login(retry_email, PASSWORD)
        .await
        .map_err(|_| Error::Unauthorized)?;
    let revoked = app.security.guard(&retry_login.token).await?;
    let client = db.get_detached("tenant owner walkthrough checks").await?;
    let row = client
        .query_one(
            "SELECT t.id,c.id,u.status,i.email,p.kind,
                EXISTS(SELECT 1 FROM principal_roles x JOIN roles r ON r.tenant_id=x.tenant_id AND r.id=x.role_id WHERE x.tenant_id=t.id AND x.user_id=u.id AND r.code='tenant_owner' AND r.is_system AND NOT r.grant_all)
                AND (SELECT count(*) FROM principal_roles x WHERE x.tenant_id=t.id AND x.user_id=u.id)=1,
                EXISTS(SELECT 1 FROM person_affiliations a WHERE a.tenant_id=t.id AND a.person_id=u.person_id AND a.organisation_id=c.id AND a.unlinked_at IS NULL),
                EXISTS(SELECT 1 FROM user_desks d WHERE d.tenant_id=t.id AND d.person_id=u.person_id AND d.company_id=c.id),
                b.invitation_id=(SELECT accepted.id FROM user_invitations accepted WHERE accepted.tenant_id=t.id AND accepted.id=b.invitation_id AND accepted.accepted_at IS NOT NULL)
             FROM tenants t
             JOIN tenant_primary_companies x ON x.tenant_id=t.id
             JOIN companies c ON c.tenant_id=t.id AND c.id=x.company_id
             JOIN users u ON u.tenant_id=t.id AND u.id=$1
             JOIN user_identities i ON i.id=u.identity_id
             JOIN persons p ON p.tenant_id=t.id AND p.id=u.person_id
             JOIN user_tenant_owner_bootstraps b ON b.tenant_id=t.id AND b.user_id=u.id
             WHERE t.name=$2 AND c.name='Customer Company Sdn. Bhd.' AND c.code='customer'",
            &[&user, &workspace],
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
                EXISTS(SELECT 1 FROM base_audit_mutations WHERE tenant_id=$1 AND actor_type='user' AND actor_id=$2 AND event='updated' AND auditable_type='user.invitation')",
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
    if invitation.tenant_name != workspace || !owner_ready || !action_audit || !mutation_audit {
        return Err(Error::Internal);
    }

    let before_retry = snapshot(&client).await?;
    let mail_before_retry = count(&mailbox)?;
    expect_output(
        provision(&workspace, Some(OWNER), None)?,
        Some("the bootstrap owner has no pending invitation for this address"),
    )?;
    if snapshot(&client).await? != before_retry || count(&mailbox)? != mail_before_retry {
        return Err(Error::Internal);
    }

    expect_output(provision("Independent workspace", None, None)?, None)?;
    let independent = snapshot(&client).await?;
    expect_output(
        provision("Independent workspace", Some(OWNER), None)?,
        Some("a matching tenant already exists without a matching owner bootstrap receipt"),
    )?;
    if snapshot(&client).await? != independent || count(&mailbox)? != mail_before_retry {
        return Err(Error::Internal);
    }

    let blocked = directory.join("blocked-delivery");
    std::fs::write(&blocked, b"file in place of mailbox").map_err(|_| Error::Internal)?;
    let retry_workspace = "Delivery retry workspace";
    expect_output(
        provision(retry_workspace, Some(retry_email), Some(&blocked))?,
        Some(
            "Owner invitation delivery failed after commit. Rerun the same command to reissue it.",
        ),
    )?;
    let original = retry_state(&client).await?;
    if !original.unexpired || original.reissues != 0 || count(&mailbox)? != mail_before_retry {
        return Err(Error::Internal);
    }
    let before_wrong_address = snapshot(&client).await?;
    expect_output(
        provision(retry_workspace, Some(OWNER), None)?,
        Some("the bootstrap owner has no pending invitation for this address"),
    )?;
    if snapshot(&client).await? != before_wrong_address || count(&mailbox)? != mail_before_retry {
        return Err(Error::Internal);
    }
    expect_output(provision(retry_workspace, Some(retry_email), None)?, None)?;
    let first_token = invitation_token(
        &mailbox,
        mail_before_retry,
        &origin,
        retry_email,
        retry_workspace,
    )?;
    let delivered = retry_state(&client).await?;
    if !delivered.same_invitation(&original)
        || delivered.digest == original.digest
        || !delivered.unexpired
        || delivered.reissues != 1
    {
        return Err(Error::Internal);
    }
    client.execute(
        "UPDATE user_invitations SET expires_at=clock_timestamp()-interval '1 minute' WHERE tenant_id=$1 AND id=$2",
        &[&original.tenant, &original.invitation],
    ).await?;
    expect_output(provision(retry_workspace, Some(retry_email), None)?, None)?;
    let new_token = invitation_token(
        &mailbox,
        mail_before_retry + 1,
        &origin,
        retry_email,
        retry_workspace,
    )?;
    let reissued = retry_state(&client).await?;
    expect_output(provision(retry_workspace, None, None)?, None)?;
    let ambiguous = snapshot(&client).await?;
    expect_output(
        provision(retry_workspace, Some(retry_email), None)?,
        Some("more than one tenant matches the supplied tenant and primary company"),
    )?;
    if snapshot(&client).await? != ambiguous || count(&mailbox)? != mail_before_retry + 2 {
        return Err(Error::Internal);
    }
    for (command, name) in [
        ("tenant-provision", "Legacy tenant"),
        ("platform-provision", "Legacy platform"),
    ] {
        let output = web_command()?
            .args([
                command,
                "--tenant-name",
                name,
                "--company-name",
                "Original company",
                "--company-code",
                "studio",
                "--company-name",
                "--Studio",
            ])
            .output()
            .map_err(|_| Error::Internal)?;
        if !String::from_utf8_lossy(&output.stdout).contains("“--Studio”") {
            return Err(Error::Refused(format!(
                "{command} did not use the last company name"
            )));
        }
        expect_output(output, None)?;
    }
    let auth = app
        .services
        .get::<bilibi_tenancy::Authentication>("base/tenancy.authentication")?;
    let viewing = auth
        .authenticate_session(
            &**client,
            tenant,
            retry_account.id,
            Some(user),
            None,
            String::new(),
        )
        .await?;
    drop(client);
    app.user
        .logout(&retry_login.token, &retry_login.csrf)
        .await?;
    let client = db.get().await?;
    let mut expected: serde_json::Value =
        serde_json::from_str(&snapshot(&client).await?).map_err(|_| Error::Internal)?;
    let actions = expected["actions"].as_i64().ok_or(Error::Internal)?;
    expected["actions"] = serde_json::json!(actions + 2);
    drop(client);
    assert!(matches!(
        app.user
            .accept_member_invitation(&viewing, &new_token)
            .await,
        Err(InvitationError::Access(Error::Forbidden))
    ));
    assert!(matches!(
        app.user
            .accept_member_invitation(&revoked, &new_token)
            .await,
        Err(InvitationError::Access(Error::Unauthorized))
    ));
    let client = db.get().await?;
    let after: serde_json::Value =
        serde_json::from_str(&snapshot(&client).await?).map_err(|_| Error::Internal)?;
    let audited: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM base_audit_actions WHERE tenant_id=$1 AND actor_id=$2
         AND impersonator_id=$3 AND event='user.invitation.accept.refused'
         AND payload->>'reason'='impersonating')
         AND EXISTS(SELECT 1 FROM base_audit_actions WHERE tenant_id=$1 AND actor_id=$2
         AND impersonator_id IS NULL AND event='user.invitation.accept.refused'
         AND payload->>'reason'='source_session_unavailable')",
            &[&tenant, &retry_account.id, &user],
        )
        .await?
        .get(0);
    if !audited || after != expected {
        return Err(Error::Internal);
    }
    drop(client);
    let retry_login = app
        .user
        .login(retry_email, PASSWORD)
        .await
        .map_err(|_| Error::Unauthorized)?;
    let signed_in = app.security.guard(&retry_login.token).await?;
    if !reissued.same_invitation(&original)
        || reissued.digest == delivered.digest
        || !reissued.unexpired
        || reissued.reissues != 2
        || !matches!(
            app.user.member_invitation(&first_token).await,
            Err(InvitationError::Link)
        )
        || app
            .user
            .accept_member_invitation(&signed_in, &new_token)
            .await
            .map_err(|_| Error::Internal)?
            != original.user
    {
        return Err(Error::Internal);
    }
    println!("Verified delivery recovery, expired link replacement and refusal to change or adopt an existing owner.");
    Ok(())
}
