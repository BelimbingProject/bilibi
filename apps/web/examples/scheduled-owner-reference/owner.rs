//! An archived owner record stays read-only even for a granted principal.
use super::*;
use bilibi_authz::{SystemAuthentication, SystemScope};
use bilibi_tenancy::Scope;

pub(super) async fn locked(app: &bilibi_web::App, db: &Db, admin: &Scope, id: i64) -> Result<()> {
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let client = db.get().await?;
    let identity = SystemScope::rehydrate(&**client, admin.tenant(), Some(PRINCIPAL)).await?;
    let principal = authentication.for_system(&**client, &identity).await?;
    bilibi_authz::authorize_key(&**client, &principal, bilibi_company::UPDATE).await?;
    let (before, _) = bilibi_company::detail(&**client, &principal, id).await?;
    assert!(
        before.archived(),
        "archive this disposable record through its owner first"
    );
    drop(client);
    let companies = app
        .services
        .get::<CompanyService>(bilibi_company::SERVICE)?;
    assert!(matches!(
        companies
            .save_as(
                &principal,
                &Edit {
                    id,
                    field: "legal_name".into(),
                    value: "Must not be saved".into(),
                    version: before.version,
                }
            )
            .await,
        Err(Error::Forbidden)
    ));
    let client = db.get().await?;
    let (after, _) = bilibi_company::detail(&**client, &principal, id).await?;
    assert_eq!(after.version, before.version);
    assert_eq!(after.legal_name, before.legal_name);
    assert!(after.archived());
    println!("granted principal ordinary owner facade refused archived company {id}; value/version unchanged; user={}", principal.user());
    Ok(())
}
