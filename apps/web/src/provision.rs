//! Server-side tenant provisioning. Supplying an owner keeps this one command
//! safe to retry across an interrupted first-day setup: Company resolves the
//! exact tenant/company pair and User refuses once any account exists.
use bilibi_authz::grants::Grants;
use bilibi_company::{self as primary, NewCompany, ProvisionOutcome};
use bilibi_database::{Db, Error, Result};
use bilibi_user::{
    validate_first_tenant_owner, FirstTenantOwner, FirstTenantOwnerError, TenantCompany,
};

const TENANT_USAGE: &str = "usage: bilibi-web tenant-provision --tenant-name NAME --company-name NAME --company-code CODE [--owner-email ADDRESS [--owner-name NAME]]";
const PLATFORM_USAGE: &str = "usage: bilibi-web platform-provision [--tenant-name NAME] --company-name NAME --company-code CODE";

#[derive(Default)]
struct Options {
    tenant_name: Option<String>,
    company_name: Option<String>,
    company_code: Option<String>,
    owner_email: Option<String>,
    owner_name: Option<String>,
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

fn parse(command: &str, args: &[String]) -> Option<Options> {
    let mut options = Options::default();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let slot = match flag.as_str() {
            "--tenant-name" => &mut options.tenant_name,
            "--company-name" => &mut options.company_name,
            "--company-code" => &mut options.company_code,
            "--owner-email" if command == "tenant-provision" => &mut options.owner_email,
            "--owner-name" if command == "tenant-provision" => &mut options.owner_name,
            _ => return None,
        };
        let value = rest.next().map(|value| value.trim().to_owned())?;
        if value.is_empty() || value.starts_with("--") || slot.replace(value).is_some() {
            return None;
        }
    }
    Some(options)
}

/// `tenant-provision` creates a customer tenant and, when `--owner-email` is
/// present, prepares its first owner through the ordinary invitation flow.
/// `platform-provision` retains its existing company-only contract.
pub async fn provision(db: &Db, command: &str, args: &[String]) -> Result<()> {
    let options = parse(command, args).ok_or_else(|| usage(command))?;
    let company = NewCompany {
        name: options.company_name.unwrap_or_default(),
        code: options.company_code,
        ..NewCompany::default()
    };
    let tenant_name = options.tenant_name.unwrap_or_default();
    let owner = match (options.owner_email, options.owner_name) {
        (Some(email), name) => {
            let owner = FirstTenantOwner {
                name: name.unwrap_or_else(|| email.split('@').next().unwrap_or_default().into()),
                email,
            };
            let errors = validate_first_tenant_owner(&owner);
            if !errors.is_empty() {
                for error in errors {
                    eprintln!("{}: {}", error.field, error.message);
                }
                return Err(Error::Invalid);
            }
            Some(owner)
        }
        (None, Some(_)) => return Err(usage(command)),
        (None, None) => None,
    };
    if company.name.is_empty()
        || company.code.is_none()
        || (command == "tenant-provision" && tenant_name.is_empty())
    {
        return Err(usage(command));
    }
    let outcome = if command == "tenant-provision" {
        if owner.is_some() {
            primary::provision_tenant_once(db, &tenant_name, &company).await?
        } else {
            primary::provision_tenant(db, &tenant_name, &company).await?
        }
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
    let Some(owner) = owner else {
        return Ok(());
    };
    let origin = std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost:3000".into());
    let app = crate::App::new(db.clone(), &origin)?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    match app
        .user
        .create_first_tenant_owner(
            &grants,
            TenantCompany {
                tenant: ready.tenant.id,
                company: ready.company.id,
            },
            &owner,
        )
        .await
    {
        Ok(created) => {
            println!(
                "Owner invitation {} prepared for {} with the tenant owner role. The owner must use the emailed link to set up sign-in.",
                created.invitation, created.email
            );
            Ok(())
        }
        Err(FirstTenantOwnerError::AccountsExist { count, first }) => {
            eprintln!(
                "Owner not changed: this tenant already has {count} account(s); the first is {first}."
            );
            Err(Error::Invalid)
        }
        Err(FirstTenantOwnerError::Validation(errors)) => {
            for error in errors {
                eprintln!("{}: {}", error.field, error.message);
            }
            Err(Error::Invalid)
        }
        Err(FirstTenantOwnerError::NotCustomer) => {
            eprintln!("Owner not changed: the resolved tenant is not a live customer tenant with this primary company.");
            Err(Error::Invalid)
        }
        Err(FirstTenantOwnerError::Access(error)) => Err(error),
    }
}
