//! Proves the installation-owner lifecycle of a portable identity: mailbox
//! recovery, last-membership retention, immediate disposal and due sweeping.
//! Never point this at a shared database.
use bilibi_database::pool;
use bilibi_user::{IdentityLifecycleError, IdentityRecovery, UserService};
use sha2::{Digest, Sha256};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_IDENTITY_LIFECYCLE_DEMO").as_deref() != Ok("disposable") {
        return Err("use a disposable migrated database".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), &std::env::var("APP_ORIGIN")?)?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let users = app.services.get::<UserService>("core/user.accounts")?;
    let password = bilibi_walkthrough_fixture::PASSWORD;

    let old = "viewer@example.test";
    let recovered = "viewer.recovered@example.test";
    let retained = "viewer.retained@example.test";
    let viewer_login = users.login(old, password).await?;
    assert_eq!(
        users.recover_identity_email(old, recovered).await?,
        IdentityRecovery::Recovered
    );
    assert!(app.security.guard(&viewer_login.token).await.is_err());
    assert!(users.login(old, password).await.is_err());
    let recovered_login = users.login(recovered, password).await?;
    app.security.guard(&recovered_login.token).await?;
    println!("PASS: server-owner mailbox recovery moved the address and ended the old session");

    assert!(matches!(
        users.delete_orphan_identity("admin@example.test").await,
        Err(IdentityLifecycleError::HasMembership)
    ));
    let administrator_login = users.login("admin@example.test", password).await?;
    let administrator = app.security.guard(&administrator_login.token).await?;
    users.remove_membership(&administrator, 2).await?;
    let client = db.get().await?;
    let orphaned: bool = client
        .query_one(
            "SELECT orphaned_at IS NOT NULL FROM user_identities WHERE lower(email)=$1",
            &[&recovered],
        )
        .await?
        .get(0);
    assert!(orphaned);
    drop(client);
    assert_eq!(
        users.recover_identity_email(recovered, retained).await?,
        IdentityRecovery::Recovered
    );
    assert!(users.login(recovered, password).await.is_err());
    let token = "a".repeat(64);
    let token_hash = Sha256::digest(token.as_bytes()).to_vec();
    let client = db.get().await?;
    let invited: i64 = client
        .query_one(
            "INSERT INTO users(tenant_id,name,status) VALUES(1,'Reactivated viewer','invited') RETURNING id",
            &[],
        )
        .await?
        .get(0);
    client
        .execute(
            "INSERT INTO user_invitations(tenant_id,user_id,email,token_hash,expires_at) VALUES(1,$1,$2,$3,now()+interval '1 day')",
            &[&invited, &retained, &token_hash],
        )
        .await?;
    drop(client);
    assert!(!users.member_invitation(&token).await?.existing_sign_in);
    assert_eq!(
        users
            .accept_new_member_invitation(&token, "replacement-password")
            .await?,
        invited
    );
    users.login(retained, "replacement-password").await?;
    let client = db.get().await?;
    let retention_cancelled: bool = client
        .query_one(
            "SELECT orphaned_at IS NULL FROM user_identities WHERE lower(email)=$1",
            &[&retained],
        )
        .await?
        .get(0);
    assert!(retention_cancelled);
    drop(client);
    users.remove_membership(&administrator, invited).await?;
    users.delete_orphan_identity(retained).await?;
    let client = db.get().await?;
    let disposed: bool = client
        .query_one(
            "SELECT i.status='deleted' AND i.deleted_at IS NOT NULL AND i.email LIKE 'deleted-identity-%@erased.invalid' AND i.password_hash='!deleted'
             AND NOT EXISTS(SELECT 1 FROM user_two_factor f WHERE f.identity_id=i.id)
             AND NOT EXISTS(SELECT 1 FROM password_reset_tokens r WHERE r.identity_id=i.id)
             FROM user_identities i JOIN users u ON u.identity_id=i.id WHERE u.tenant_id=1 AND u.id=2",
            &[],
        )
        .await?
        .get(0);
    assert!(disposed);
    println!("PASS: a retained identity remained recoverable, a later invitation cancelled retention, and only the server owner disposed of the unused identity");

    drop(client);
    users.remove_membership(&administrator, 7).await?;
    let client = db.get().await?;
    client
        .execute(
            "UPDATE user_identities i SET orphaned_at=now()-interval '91 days' FROM users u WHERE u.identity_id=i.id AND u.tenant_id=1 AND u.id=7",
            &[],
        )
        .await?;
    drop(client);
    assert_eq!(users.sweep_orphan_identities().await?, 1);
    let client = db.get().await?;
    let retained: String = client
        .query_one(
            "SELECT i.status FROM user_identities i JOIN users u ON u.identity_id=i.id WHERE u.tenant_id=1 AND u.id=7",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(retained, "deleted");
    println!("PASS: configured final retention disposed of a due identity and left live identities untouched");
    Ok(())
}
