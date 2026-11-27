//! Terminal entry point for installation owner setup and recovery. See
//! `docs/server-install.md` for usage and `apps/core/user/README.md` for the
//! bootstrap and ownership contracts.
use bilibi_company::{self as primary, NewCompany, ProvisionOutcome};
use bilibi_database::{Db, Error, Result};
use bilibi_user::{
    validate_first_administrator, BootstrapAffiliation, FieldError, FirstAdministrator,
    FirstAdministratorError, OperatorCompany, OwnerClaim, OwnerClaimError,
};

const USAGE: &str = "usage: bilibi-web create-owner --email ADDRESS [--name NAME] [--recover]
                               [--company-name NAME [--company-code CODE] [--tenant-name NAME]]
Creates the first owner account of this install (the platform operator's administrator) and
emails the owner a link to claim it: they choose their password there, then set up 2FA.
Safe to repeat: once an owner exists it creates no other, finishes that owner's unfinished
Person, business affiliation and first desk, resends the claim link while no password has
been chosen, and reports the claim. --company-name is needed only while no
platform-operator company exists yet. Pass --recover with the replacement owner’s email to
recover an existing installation.";

#[derive(Default)]
struct Options {
    recover: bool,
    email: Option<String>,
    name: Option<String>,
    company_name: Option<String>,
    company_code: Option<String>,
    tenant_name: Option<String>,
}

fn parse(args: &[String]) -> Option<Options> {
    let mut options = Options::default();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        if flag == "--recover" && !options.recover {
            options.recover = true;
            continue;
        }
        let slot = match flag.as_str() {
            "--email" => &mut options.email,
            "--name" => &mut options.name,
            "--company-name" => &mut options.company_name,
            "--company-code" => &mut options.company_code,
            "--tenant-name" => &mut options.tenant_name,
            _ => return None,
        };
        let value = rest.next().map(|value| value.trim().to_owned())?;
        if value.is_empty() || value.starts_with("--") || slot.replace(value).is_some() {
            return None;
        }
    }
    Some(options)
}

fn usage() -> Error {
    eprintln!("{USAGE}");
    Error::Invalid
}

pub async fn create_owner(db: &Db, args: &[String]) -> Result<()> {
    let options = parse(args).ok_or_else(usage)?;
    let origin = std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost:3000".into());
    let app = crate::App::new(db.clone(), &origin)?;
    let grants = app
        .services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?;

    let existing = existing_operator(db).await?;
    if let Some(operator) = &existing {
        if !options.recover {
            if let Some(email) = app.user.recorded_owner_email(operator.tenant).await? {
                println!("Recorded installation owner: {email}.");
                settle(&app).await?;
                return claim(&app, &origin).await;
            }
        }
    } else if options.recover {
        return Err(usage());
    }
    let input = FirstAdministrator {
        email: options.email.clone().ok_or_else(usage)?,
        name: options.name.clone().unwrap_or_else(|| {
            let email = options.email.as_deref().unwrap_or_default();
            email.split('@').next().unwrap_or_default().to_owned()
        }),
    };
    let errors = validate_first_administrator(&input);
    if !errors.is_empty() {
        return Err(report(&errors));
    }
    let operator = match existing {
        Some(operator) => {
            if options.company_name.is_some() || options.company_code.is_some() {
                println!("The platform operator's company already exists; the company flags are not used.");
            }
            operator
        }
        None => {
            let Some(name) = options.company_name.clone() else {
                eprintln!("No platform-operator company exists yet: pass --company-name.");
                println!("Next: rerun with --company-name \"<your organisation>\".");
                return Err(usage());
            };
            let code = options
                .company_code
                .clone()
                .unwrap_or_else(|| suggest_code(&name));
            provision_operator(
                db,
                &NewOperator {
                    name,
                    code,
                    tenant_name: options.tenant_name.clone().unwrap_or_default(),
                },
            )
            .await?
        }
    };
    match app
        .user
        .create_first_administrator(&grants, operator, &input, options.recover)
        .await
    {
        Ok(created) => {
            println!(
                "Recorded installation owner: {} (account {}).",
                created.email, created.user
            );
        }
        Err(FirstAdministratorError::AccountsExist) => {
            let Some(email) = app.user.recorded_owner_email(operator.tenant).await? else {
                eprintln!("The operator tenant already has accounts and has no recorded owner.");
                return Err(Error::Invalid);
            };
            println!("Recorded installation owner: {email}.");
        }
        Err(FirstAdministratorError::Validation(errors)) => return Err(report(&errors)),
        Err(FirstAdministratorError::NotOperator) => {
            eprintln!("The provisioned tenant is not the platform operator; run `bilibi-web platform-provision` and try again.");
            return Err(Error::Invalid);
        }
        Err(FirstAdministratorError::Access(error)) => return Err(error),
    }
    settle(&app).await?;
    claim(&app, &origin).await
}

/// The owner's Person, business affiliation and first desk, finished when an
/// earlier run left them unfinished.
async fn settle(app: &crate::App) -> Result<()> {
    match app.user.settle_bootstrap_affiliation().await {
        Ok(BootstrapAffiliation::Linked { .. }) => {
            println!(
                "The owner now has a Person, business affiliation and desk in the platform operator's company."
            );
            Ok(())
        }
        Ok(BootstrapAffiliation::Deferred(reason)) => {
            eprintln!("The owner's Person, business affiliation and first desk are not ready yet: {reason}.");
            println!("Next: fix that, then rerun the same command.");
            Err(Error::Invalid)
        }
        Ok(_) => Ok(()),
        Err(error) => {
            eprintln!(
                "The owner's Person, business affiliation and first desk could not be set up."
            );
            println!("Next: rerun the same command.");
            Err(error)
        }
    }
}

/// Sends or reports the owner's claim. The last line names its state.
async fn claim(app: &crate::App, origin: &str) -> Result<()> {
    match app.user.claim_owner().await {
        Ok(OwnerClaim::Sent { email }) => {
            println!("A claim link was emailed to {email}. The owner opens it, chooses a password, signs in at {origin}/login and sets up 2FA.");
            println!("Next: when the owner has chosen a password and set up 2FA, rerun the same command to confirm.");
            println!("Owner claim: sent");
            Ok(())
        }
        Ok(OwnerClaim::RecentlySent { email }) => {
            println!("A claim link went to {email} less than a minute ago; it still works.");
            println!(
                "Next: ask the owner to open it, or wait a minute and rerun to send a new one."
            );
            println!("Owner claim: recently-sent");
            Ok(())
        }
        Ok(OwnerClaim::PasswordChosen { email }) => {
            println!("{email} chose a password but has not set up 2FA yet.");
            println!("Next: the owner signs in at {origin}/login and sets up 2FA at {origin}/settings/two-factor; then rerun to confirm.");
            println!("Owner claim: password-chosen");
            Ok(())
        }
        Ok(OwnerClaim::Claimed { email }) => {
            println!("{email} holds this server: password chosen and 2FA set up. Sign in at {origin}/login.");
            println!("Owner claim: claimed");
            Ok(())
        }
        Err(OwnerClaimError::MailNotReady(detail)) => {
            eprintln!("The claim link cannot be emailed: {detail}");
            println!("Next: set up outgoing mail with the server installer's mail command, then rerun the same command.");
            println!("Code: MAIL_NOT_READY");
            println!("Owner claim: mail-not-ready");
            Err(Error::Invalid)
        }
        Err(OwnerClaimError::Delivery(detail)) => {
            eprintln!("The claim link was not delivered: {detail}");
            println!(
                "Next: check the mail relay settings and the address, then rerun the same command."
            );
            println!("Code: MAIL_NOT_DELIVERED");
            println!("Owner claim: not-delivered");
            Err(Error::Invalid)
        }
        Err(OwnerClaimError::NoOwner) => {
            eprintln!("This install has no owner account to claim.");
            println!("Next: rerun the same command.");
            Err(Error::Invalid)
        }
        Err(OwnerClaimError::Access(error)) => Err(error),
    }
}

/// The operator company to provision when the database has none yet.
struct NewOperator {
    name: String,
    code: String,
    tenant_name: String,
}

/// The marked operator tenant with its primary company, when both exist.
async fn existing_operator(db: &Db) -> Result<Option<OperatorCompany>> {
    let client = db.get().await?;
    let Some(tenant) = bilibi_tenancy::platform_operator(&**client).await? else {
        return Ok(None);
    };
    Ok(primary::primary_company(&**client, tenant.id)
        .await?
        .map(|company| OperatorCompany {
            tenant: tenant.id,
            company: company.id.get(),
        }))
}

/// The operator tenant and company, provisioned through Company as
/// `platform-provision` does (an operator tenant without a company gets one).
async fn provision_operator(db: &Db, new: &NewOperator) -> Result<OperatorCompany> {
    let company = NewCompany {
        name: new.name.clone(),
        code: Some(new.code.clone()),
        legal_name: Some(new.name.clone()),
        ..NewCompany::default()
    };
    match primary::provision_platform_operator(db, &new.tenant_name, &company).await? {
        ProvisionOutcome::Provisioned(result) => {
            println!(
                "Platform operator ready: tenant {} “{}”, company {} “{}”.",
                result.tenant.id, result.tenant.name, result.company.id, result.company.name
            );
            Ok(OperatorCompany {
                tenant: result.tenant.id,
                company: result.company.id,
            })
        }
        ProvisionOutcome::Refused(errors) => {
            for error in errors {
                eprintln!("company {}: {}", error.field, error.message);
            }
            Err(Error::Invalid)
        }
    }
}

/// `Example Operations` → `example_operations`, the shape the company code
/// rule accepts; the operator may type another.
fn suggest_code(name: &str) -> String {
    let mut code = String::new();
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            code.push(character);
        } else if !code.is_empty() && !code.ends_with('_') {
            code.push('_');
        }
    }
    code.trim_end_matches('_').to_owned()
}

fn report(errors: &[FieldError]) -> Error {
    for error in errors {
        eprintln!("{}: {}", error.field, error.message);
    }
    Error::Invalid
}
