//! Server-side tenant and platform provisioning commands.
use bilibi_authz::grants::Grants;
use bilibi_company::{self as primary, NewCompany, ProvisionOutcome};
use bilibi_database::{Db, Error, Result};
use bilibi_user::FirstTenantOwnerError;

const TENANT_USAGE: &str = "usage: bilibi-web tenant-provision --tenant-name NAME --company-name NAME --company-code CODE [--owner-email ADDRESS]";
const PLATFORM_USAGE: &str = "usage: bilibi-web platform-provision [--tenant-name NAME] --company-name NAME --company-code CODE";

#[derive(Default)]
struct Options {
    tenant_name: Option<String>,
    company_name: Option<String>,
    company_code: Option<String>,
    owner_email: Option<String>,
}

fn usage(command: &str) -> Error {
    eprintln!(
        "{}",
        if command == "tenant-provision" {
            TENANT_USAGE
        } else {
            PLATFORM_USAGE
        }
    );
    Error::Invalid
}

fn parse(command: &str, args: &[String]) -> Result<Options> {
    let mut options = Options::default();
    let mut repeated = None;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let slot = match flag.as_str() {
            "--tenant-name" => &mut options.tenant_name,
            "--company-name" => &mut options.company_name,
            "--company-code" => &mut options.company_code,
            "--owner-email" if command == "tenant-provision" => &mut options.owner_email,
            _ => return Err(usage(command)),
        };
        let value = rest.next().ok_or_else(|| usage(command))?.trim();
        if value.is_empty() {
            return Err(usage(command));
        }
        if slot.replace(value.to_owned()).is_some() {
            repeated = Some(flag);
        }
    }
    if let Some(flag) = repeated.filter(|_| options.owner_email.is_some()) {
        eprintln!("Repeated option: {flag}");
        return Err(usage(command));
    }
    Ok(options)
}

/// `tenant-provision` creates a customer tenant and, when `--owner-email` is
/// present, prepares its first owner through the ordinary invitation flow.
/// `platform-provision` retains its existing company-only contract.
pub async fn provision(db: &Db, command: &str, args: &[String]) -> Result<()> {
    let options = parse(command, args)?;
    let company = NewCompany {
        name: options.company_name.unwrap_or_default(),
        code: options.company_code,
        ..NewCompany::default()
    };
    let tenant_name = options.tenant_name.unwrap_or_default();
    if company.name.is_empty()
        || company.code.is_none()
        || (command == "tenant-provision" && tenant_name.is_empty())
    {
        return Err(usage(command));
    }
    if let Some(email) = options.owner_email {
        let origin = std::env::var("APP_ORIGIN").map_err(|_| {
            eprintln!("Set APP_ORIGIN to this installation's public origin.");
            Error::Invalid
        })?;
        let app = crate::App::new(db.clone(), &origin)?;
        match app.secure_origin().await {
            Ok(true) => {}
            Ok(false) | Err(Error::Invalid) => {
                eprintln!(
                    "APP_ORIGIN must use HTTPS, or loopback HTTP for a development installation."
                );
                return Err(Error::Invalid);
            }
            Err(error) => return Err(error),
        }
        let grants = app.services.get::<Grants>("base/authz.grants")?;
        return match app
            .user
            .provision_tenant_owner(&grants, &tenant_name, &company, &email, &origin)
            .await
        {
            Ok(invitation) => {
                println!(
                    "Owner invitation {} sent to {}. The owner must use the emailed link to set up sign-in.",
                    invitation.id, invitation.email
                );
                Ok(())
            }
            Err(FirstTenantOwnerError::Validation(errors)) => {
                for error in errors {
                    eprintln!("{}: {}", error.field, error.message);
                }
                Err(Error::Invalid)
            }
            Err(FirstTenantOwnerError::CompanyValidation(errors)) => {
                for error in errors {
                    eprintln!("{}: {}", error.field, error.message);
                }
                Err(Error::Invalid)
            }
            Err(FirstTenantOwnerError::Access(error)) => Err(error),
            Err(error) => {
                eprintln!("{error}");
                Err(Error::Invalid)
            }
        };
    }
    let outcome = if command == "tenant-provision" {
        primary::provision_tenant(db, &tenant_name, &company).await?
    } else {
        primary::provision_platform_operator(db, &tenant_name, &company).await?
    };
    let ready = match outcome {
        ProvisionOutcome::Provisioned(result) => result,
        ProvisionOutcome::Refused(errors) => {
            for error in errors {
                eprintln!("{}: {}", error.field, error.message);
            }
            return Err(Error::Invalid);
        }
    };
    let state = |created: bool| if created { "created" } else { "existing" };
    println!(
        "{}: tenant {} ({}), primary company {} “{}” ({}).",
        if command == "tenant-provision" {
            "Tenant ready"
        } else {
            "Platform operator ready"
        },
        ready.tenant.id,
        state(ready.tenant_created),
        ready.company.id,
        ready.company.name,
        state(ready.company_created)
    );
    Ok(())
}
