//! Executable boot-contract proof through the public Catalog and Schedule APIs.
//! No database connection, source inspection or unit-test harness.
use super::*;
use bilibi_authz::StandingGrant;
use bilibi_database::MutationBoundary;
use bilibi_module_registry::{Catalog, Consumer, RegistryError};

fn boot(contributions: Vec<&Contribution>) -> bilibi_module_registry::Result<Schedule> {
    let modules =
        bilibi_web::modules().map_err(|error| RegistryError::Contribution(error.to_string()))?;
    let catalog = Catalog::build(
        &modules,
        inventory::iter::<Consumer>.into_iter().collect(),
        contributions,
    )?;
    let db = bilibi_database::pool("postgres://127.0.0.1/unused")
        .map_err(|error| RegistryError::Contribution(error.to_string()))?;
    Schedule::from_catalog(
        &catalog,
        Arc::new(bilibi_queue::Queue::new(
            bilibi_queue::Workers::from_catalog(&catalog)?,
        )),
        Arc::new(bilibi_tenancy::Tenants::new(db.clone(), Arc::default())),
        Arc::new(MutationBoundary::default()),
        db,
    )
}

fn contributions() -> Vec<&'static Contribution> {
    inventory::iter::<Contribution>.into_iter().collect()
}

fn replaced(replacement: &Contribution) -> Vec<&Contribution> {
    let mut all: Vec<_> = contributions()
        .into_iter()
        .filter(|entry| entry.consumer != replacement.consumer || entry.id != replacement.id)
        .collect();
    all.push(replacement);
    all
}

fn refusal(name: &str, all: Vec<&Contribution>, expected: &str) {
    match boot(all) {
        Err(error) if error.to_string().contains(expected) => println!("refused {name}: {error}"),
        Err(error) => panic!("unexpected refusal for {name}: {error}"),
        Ok(_) => panic!("accepted invalid {name}"),
    }
}

pub(super) fn prove() -> Result<()> {
    let original = boot(contributions())?;
    let item = original.definition(KEY).ok_or(Error::NotFound)?;
    assert_eq!(item.execution_principal(), PRINCIPAL);
    let fingerprint = item.fingerprint().to_owned();
    println!("accepted owner principal and bounded definition: {fingerprint}");

    let missing = Contribution::new(
        schedule::EXECUTION_PRINCIPALS,
        "missing/definition",
        provenance!(),
        &ExecutionPrincipal {
            principal: PRINCIPAL,
        },
    );
    let mut all = contributions();
    all.push(&missing);
    refusal("missing definition", all, "definition is not installed");

    let foreign = Contribution::new(
        schedule::EXECUTION_PRINCIPALS,
        KEY,
        Provenance {
            module: "base/schedule",
            ..provenance!()
        },
        &ExecutionPrincipal {
            principal: PRINCIPAL,
        },
    );
    refusal(
        "foreign definition",
        replaced(&foreign),
        "owned by this module",
    );

    for (principal, reason) in [
        ("missing-principal", "principal is not declared"),
        (schedule::PRINCIPAL, "principal must be declared by"),
    ] {
        let payload = Box::leak(Box::new(ExecutionPrincipal { principal }));
        let selection =
            Contribution::new(schedule::EXECUTION_PRINCIPALS, KEY, provenance!(), payload);
        refusal(principal, replaced(&selection), reason);
    }

    let installation = Box::leak(Box::new(Definition {
        reach: Reach::Installation,
        ..*item.definition()
    }));
    let definition = Contribution::new(schedule::DEFINITIONS, KEY, provenance!(), installation);
    refusal(
        "installation definition",
        replaced(&definition),
        "each-tenant definition",
    );

    let binding = Contribution::new(
        bilibi_queue::HANDLERS,
        WORKER,
        provenance!(),
        &HandlerBinding {
            service: HANDLER,
            capability: bilibi_company::UPDATE,
        },
    );
    refusal(
        "wrong operator consent gate",
        replaced(&binding),
        "worker must bind EXECUTE",
    );

    let vocabulary = contributions()
        .into_iter()
        .find(|entry| entry.consumer == VOCABULARY && entry.provenance.module == MODULE)
        .ok_or(Error::Invalid)?;
    let base = *vocabulary.get::<VocabularyContribution>()?;
    for (name, capabilities, accepted) in [
        (
            "missing schedule gate",
            &[bilibi_company::VIEW, bilibi_company::UPDATE][..],
            false,
        ),
        (
            "reordered bound",
            &[
                bilibi_company::UPDATE,
                schedule::TENANT_WORK,
                schedule::EXECUTE,
                bilibi_company::VIEW,
            ][..],
            true,
        ),
        (
            "widened bound",
            &[
                schedule::EXECUTE,
                schedule::TENANT_WORK,
                bilibi_company::VIEW,
                bilibi_company::UPDATE,
                bilibi_company::DELETE,
            ][..],
            true,
        ),
    ] {
        let principals = Box::leak(Box::new([SystemPrincipalDefinition {
            name: PRINCIPAL,
            capabilities,
        }]));
        let changed = Box::leak(Box::new(VocabularyContribution {
            system_principals: principals,
            ..base
        }));
        let declaration = Contribution::new(VOCABULARY, vocabulary.id, provenance!(), changed);
        if !accepted {
            refusal(
                name,
                replaced(&declaration),
                "principal must declare EXECUTE and TENANT_WORK",
            );
        } else {
            let changed = boot(replaced(&declaration))?;
            let changed = changed
                .definition(KEY)
                .ok_or(Error::NotFound)?
                .fingerprint();
            if name == "reordered bound" {
                assert_eq!(fingerprint, changed);
            } else {
                assert_ne!(fingerprint, changed);
            }
            println!("accepted {name}; review fingerprint checked");
        }
    }

    let standing = Contribution::new(
        bilibi_authz::STANDING_GRANTS,
        PRINCIPAL,
        provenance!(),
        &StandingGrant {
            principal: PRINCIPAL,
            capabilities: &[bilibi_company::UPDATE],
        },
    );
    let mut all = contributions();
    all.push(&standing);
    refusal(
        "standing business grant",
        all,
        "cannot have standing grants",
    );
    Ok(())
}
