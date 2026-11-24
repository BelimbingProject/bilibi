//! Server-owner commands for the portable identity lifecycle. They are not
//! routes or agent operations: a customer tenant can never invoke them.
use bilibi_database::{Db, Error, Result};
use bilibi_user::{IdentityLifecycleError, IdentityRecovery};

const RECOVER_USAGE: &str = "usage: bilibi-web identity-recover --email OLD --new-email NEW\nRecovers a portable sign-in address, verifies the operator-attested new mailbox and signs every membership out.";
const DELETE_USAGE: &str = "usage: bilibi-web identity-delete --email ADDRESS --confirm ADDRESS\nPermanently anonymises a portable identity only after its last membership was removed.";
const RETENTION_USAGE: &str = "usage: bilibi-web identity-retention\nAnonymises up to 100 unused portable identities older than the operator's Unused sign-in retention setting.";

fn pair<'a>(args: &'a [String], first: &str, second: &str) -> Option<(&'a str, &'a str)> {
    match args {
        [a, one, b, two] if a == first && b == second => Some((one.trim(), two.trim())),
        [b, two, a, one] if a == first && b == second => Some((one.trim(), two.trim())),
        _ => None,
    }
}

fn report(error: IdentityLifecycleError) -> Error {
    eprintln!("{error}");
    match error {
        IdentityLifecycleError::NotFound => Error::NotFound,
        IdentityLifecycleError::Invalid => Error::Invalid,
        IdentityLifecycleError::Conflict | IdentityLifecycleError::HasMembership => Error::Conflict,
        IdentityLifecycleError::Other(error) => error,
    }
}

pub async fn recover(db: &Db, args: &[String]) -> Result<()> {
    let Some((old, new)) = pair(args, "--email", "--new-email") else {
        eprintln!("{RECOVER_USAGE}");
        return Err(Error::Invalid);
    };
    let app = crate::App::new(
        db.clone(),
        &std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost:3000".into()),
    )?;
    match app.user.recover_identity_email(old, new).await.map_err(report)? {
        IdentityRecovery::Recovered => println!("Recovered the portable sign-in from {old} to {new}, signed it out everywhere and sent a notice to the new address."),
        IdentityRecovery::Unchanged => println!("The portable sign-in already uses {new}. Nothing changed."),
    }
    Ok(())
}

pub async fn delete(db: &Db, args: &[String]) -> Result<()> {
    let Some((email, confirm)) = pair(args, "--email", "--confirm") else {
        eprintln!("{DELETE_USAGE}");
        return Err(Error::Invalid);
    };
    if email != confirm {
        eprintln!("Confirmation must exactly match the address.\n{DELETE_USAGE}");
        return Err(Error::Invalid);
    }
    let app = crate::App::new(db.clone(), "http://localhost")?;
    app.user
        .delete_orphan_identity(email)
        .await
        .map_err(report)?;
    println!("Deleted the unused portable identity for {email}.");
    Ok(())
}

pub async fn retention(db: &Db, args: &[String]) -> Result<()> {
    if !args.is_empty() {
        eprintln!("{RETENTION_USAGE}");
        return Err(Error::Invalid);
    }
    let app = crate::App::new(db.clone(), "http://localhost")?;
    let count = app.user.sweep_orphan_identities().await.map_err(report)?;
    println!("Deleted {count} unused portable identity/identities whose retention period ended.");
    Ok(())
}
