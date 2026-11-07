//! Real boundary/sink proof: a principal has no own bell, while account and
//! explicitly addressed notifications still commit, roll back and validate.
use super::*;
use bilibi_authz::{SystemAuthentication, SystemScope};
use bilibi_database::MutationBoundary;
use bilibi_notifications::{Notice, NotificationBoundary};
use bilibi_tenancy::Scope;

async fn unread(db: &Db, scope: &Scope) -> Result<i64> {
    let client = db.get().await?;
    bilibi_user::unread(&**client, scope).await
}

pub(super) async fn prove(app: &bilibi_web::App, db: &Db, admin: &Scope) -> Result<()> {
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let notifications = app
        .services
        .get::<NotificationBoundary>("base/notifications.sink")?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let principal = {
        let client = db.get().await?;
        let identity = SystemScope::rehydrate(&**client, admin.tenant(), Some(PRINCIPAL)).await?;
        authentication.for_system(&**client, &identity).await?
    };
    assert_eq!(principal.user(), 0);
    assert_eq!(principal.principal(), Some(PRINCIPAL));
    let initial = unread(db, admin).await?;
    let mut client = db.get().await?;
    let mut tx = boundary
        .begin(
            &mut client,
            principal.mutation_context("scheduled-reference.principal-self-notice"),
        )
        .await?;
    bilibi_authz::authorize_key(tx.sql(), &principal, bilibi_company::UPDATE).await?;
    notifications
        .record(&mut tx, &principal, "Scheduled principal self notice proof")
        .await?;
    tx.commit().await?;
    assert_eq!(unread(db, &principal).await?, 0);
    assert_eq!(unread(db, admin).await?, initial);
    println!("principal self-record commits with no notification and no borrowed account");

    let mut tx = boundary
        .begin(
            &mut client,
            principal.mutation_context("scheduled-reference.invalid-notice"),
        )
        .await?;
    assert!(matches!(
        notifications.record(&mut tx, &principal, "").await,
        Err(Error::Invalid)
    ));
    assert!(matches!(
        notifications
            .notify(
                &mut tx,
                &principal,
                &[0],
                &Notice::new("Invalid recipient proof")
            )
            .await,
        Err(Error::Invalid)
    ));
    assert!(matches!(
        notifications
            .notify(
                &mut tx,
                &principal,
                &[],
                &Notice::new("Empty recipients proof")
            )
            .await,
        Err(Error::Invalid)
    ));
    tx.rollback().await?;
    assert_eq!(unread(db, admin).await?, initial);
    println!("principal self-record still validates the title; explicit notify still refuses zero/empty recipients");

    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("scheduled-reference.account-self-notice"),
        )
        .await?;
    notifications
        .record(&mut tx, admin, "Account self notice proof")
        .await?;
    tx.commit().await?;
    assert_eq!(unread(db, admin).await?, initial + 1);
    println!("ordinary account self-record still delivers exactly one notification");

    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("scheduled-reference.rolled-back-notice"),
        )
        .await?;
    notifications
        .record(&mut tx, admin, "Rolled-back self notice proof")
        .await?;
    tx.rollback().await?;
    assert_eq!(unread(db, admin).await?, initial + 1);
    println!("rolled-back account self-record leaves no notification");

    let mut tx = boundary
        .begin(
            &mut client,
            principal.mutation_context("scheduled-reference.explicit-notice"),
        )
        .await?;
    notifications
        .notify(
            &mut tx,
            &principal,
            &[admin.user()],
            &Notice::new("Explicit principal notice proof"),
        )
        .await?;
    tx.commit().await?;
    assert_eq!(unread(db, admin).await?, initial + 2);
    assert_eq!(unread(db, &principal).await?, 0);
    println!("principal explicit notify still delivers to the named real account");
    Ok(())
}
