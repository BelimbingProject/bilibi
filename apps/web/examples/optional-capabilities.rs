//! Running proof of `OPTIONAL_CAPABILITIES` (#428): a module's own system
//! principal holds a capability of a module it calls only while that module
//! is mounted. Two reference Extensions play the parts: a core module whose
//! worker calls a connector, and the connector that contributes the key.
//!
//! `--catalogs` needs no database. It boots the vocabulary with the
//! connector mounted, without it, and with each contribution boot must
//! refuse, and exits non-zero on any unexpected outcome.
//!
//! With `DATABASE_URL` (a disposable migrated database whose saved instance
//! role is Dev) and `BILIBI_OPTIONAL_PROOF=disposable` it also decides the
//! keys live, as the principal and as fixture people:
//! cargo run -p bilibi-web --example optional-capabilities -- --catalogs
//! BILIBI_OPTIONAL_PROOF=disposable cargo run -p bilibi-web --example optional-capabilities
use bilibi_authz::{
    decide, decide_system, effective_system, grants::Grants, CapabilityDefinition as C,
    OptionalCapabilities, SystemPrincipalDefinition, SystemScope, Vocabulary,
    VocabularyContribution, OPTIONAL_CAPABILITIES, VOCABULARY,
};
use bilibi_module_registry::{
    contributions::Catalog, Consumer, Contribution, Layer, Module, Provenance,
};
use bilibi_tenancy::Authentication;
use std::process::ExitCode;

const CORE: &str = "extensions/optional_core_reference";
const CONNECTOR: &str = "extensions/optional_connector_reference";
const ABSENT: &str = "extensions/optional_absent_reference";
const PRINCIPAL: &str = "optref-worker";
/// A second principal of the core module, for the entries boot must refuse.
const AUDITOR: &str = "optref-auditor";
const RUN: &str = "optref.core.run";
const USE: &str = "optref.connector.use";
const ABSENT_USE: &str = "optref.absent.use";

macro_rules! provenance {
    ($module:expr) => {
        Provenance {
            module: $module,
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! { Module { id: CORE, layer: Layer::Extensions, dependencies: &["base/authz"], migrations: &[], services: &[] } }
inventory::submit! { Module { id: CONNECTOR, layer: Layer::Extensions, dependencies: &["base/authz", CORE], migrations: &[], services: &[] } }
inventory::submit! { Contribution::new(VOCABULARY, CORE, provenance!(CORE), &VocabularyContribution {
    domains: &["optref"], verbs: &["run", "use"],
    capabilities: &[C::tenant(RUN)],
    roles: &[], system_principals: &[SystemPrincipalDefinition { name: PRINCIPAL, capabilities: &[RUN] }, SystemPrincipalDefinition { name: AUDITOR, capabilities: &[RUN] }],
}) }
inventory::submit! { Contribution::new(VOCABULARY, CONNECTOR, provenance!(CONNECTOR), &VocabularyContribution {
    domains: &[], verbs: &[], capabilities: &[C::tenant(USE)], roles: &[], system_principals: &[],
}) }
// The worker calls the connector behind a port, so the core module cannot
// depend on it; it names the connector's key for its own principal.
inventory::submit! { Contribution::new(OPTIONAL_CAPABILITIES, "optref-worker:connector", provenance!(CORE), &OptionalCapabilities {
    principal: PRINCIPAL, module: CONNECTOR, capabilities: &[USE], standing: true,
}) }
// A connector this composition never mounts: ignored, its key unchecked.
inventory::submit! { Contribution::new(OPTIONAL_CAPABILITIES, "optref-worker:absent", provenance!(CORE), &OptionalCapabilities {
    principal: PRINCIPAL, module: ABSENT, capabilities: &[ABSENT_USE], standing: true,
}) }

static INTRUDER: Contribution = Contribution::new(
    OPTIONAL_CAPABILITIES,
    "optref-worker:intruder",
    provenance!(CONNECTOR),
    &OptionalCapabilities {
        principal: PRINCIPAL,
        module: ABSENT,
        capabilities: &[ABSENT_USE],
        standing: false,
    },
);
static UNKNOWN: Contribution = Contribution::new(
    OPTIONAL_CAPABILITIES,
    "optref-auditor:unknown",
    provenance!(CORE),
    &OptionalCapabilities {
        principal: AUDITOR,
        module: CONNECTOR,
        capabilities: &["optref.connector.missing"],
        standing: false,
    },
);
static FOREIGN_KEY: Contribution = Contribution::new(
    OPTIONAL_CAPABILITIES,
    "optref-auditor:foreign",
    provenance!(CORE),
    &OptionalCapabilities {
        principal: AUDITOR,
        module: CONNECTOR,
        capabilities: &["admin.authz.role.list"],
        standing: false,
    },
);
static DEPENDENCY: Contribution = Contribution::new(
    OPTIONAL_CAPABILITIES,
    "optref-auditor:dependency",
    provenance!(CORE),
    &OptionalCapabilities {
        principal: AUDITOR,
        module: "base/authz",
        capabilities: &["admin.authz.role.list"],
        standing: false,
    },
);

/// Boots the vocabulary of this composition, without the connector when
/// `connector` is false, plus `extra`.
fn vocabulary(connector: bool, extra: Option<&'static Contribution>) -> Result<Vocabulary, String> {
    let modules: Vec<_> = bilibi_web::modules()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|module| connector || module.id != CONNECTOR)
        .collect();
    let consumers = inventory::iter::<Consumer>.into_iter().collect();
    let contributions = inventory::iter::<Contribution>
        .into_iter()
        .filter(|entry| connector || entry.provenance.module != CONNECTOR)
        .chain(extra)
        .collect();
    let catalog = Catalog::build(&modules, consumers, contributions).map_err(|e| e.to_string())?;
    Vocabulary::from_catalog(&catalog).map_err(|error| error.to_string())
}

fn report(vocabulary: &Vocabulary, label: &str) -> (bool, bool) {
    let bounds = vocabulary.system_principals[PRINCIPAL].capabilities;
    let in_role = vocabulary
        .roles
        .values()
        .filter(|role| role.capabilities.contains(USE))
        .map(|role| role.definition.code)
        .collect::<Vec<_>>();
    println!(
        "{label}: bounds={bounds:?} standing({USE})={} roles holding {USE}={in_role:?}",
        vocabulary.standing(PRINCIPAL, USE)
    );
    (
        bounds.contains(&USE) && vocabulary.standing(PRINCIPAL, USE),
        in_role.is_empty() && !bounds.contains(&ABSENT_USE),
    )
}

fn catalogs() -> bool {
    let mut ok = true;
    match vocabulary(true, None) {
        Ok(v) => {
            let (held, clean) = report(&v, "connector mounted");
            ok &= held && clean;
        }
        Err(error) => {
            println!("UNEXPECTED refusal with the connector mounted: {error}");
            ok = false;
        }
    }
    match vocabulary(false, None) {
        Ok(v) => {
            let (held, clean) = report(&v, "connector absent");
            ok &= !held && clean && v.system_principals[PRINCIPAL].capabilities == [RUN];
        }
        Err(error) => {
            println!("UNEXPECTED refusal without the connector: {error}");
            ok = false;
        }
    }
    match vocabulary(false, Some(&UNKNOWN)) {
        Ok(_) => println!("connector absent, unknown key: ignored"),
        Err(error) => {
            println!("UNEXPECTED refusal of an absent connector's key: {error}");
            ok = false;
        }
    }
    for (label, extra, connector) in [
        ("unknown key, connector mounted", &UNKNOWN, true),
        ("key the connector does not own", &FOREIGN_KEY, true),
        ("module that is a dependency", &DEPENDENCY, true),
        ("another module's principal", &INTRUDER, true),
    ] {
        match vocabulary(connector, Some(extra)) {
            Err(error) => println!("{label}: refused ({error})"),
            Ok(_) => {
                println!("UNEXPECTEDLY accepted {label}");
                ok = false;
            }
        }
    }
    ok
}

#[tokio::main]
async fn main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !catalogs() {
        return Ok(ExitCode::FAILURE);
    }
    if std::env::args().any(|arg| arg == "--catalogs") {
        return Ok(ExitCode::SUCCESS);
    }
    if std::env::var("BILIBI_OPTIONAL_PROOF").as_deref() != Ok("disposable") {
        return Err("set BILIBI_OPTIONAL_PROOF=disposable for the live decisions".into());
    }
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "https://optional-proof.localhost")?;
    let auth = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let client = db.get().await?;
    let client: &tokio_postgres::Client = &client;
    let worker = SystemScope::rehydrate(client, 1, Some(PRINCIPAL)).await?;
    for key in [RUN, USE, ABSENT_USE] {
        let decision = decide_system(client, &worker, key).await?;
        println!(
            "principal {PRINCIPAL} / {key}: {} ({})",
            if decision.allowed { "allow" } else { "deny" },
            decision.reason
        );
    }
    let held = effective_system(client, 1, PRINCIPAL).await?;
    println!("effective_system: {:?}", held.allowed);
    let rows: i64 = client
        .query_one(
            "SELECT count(*) FROM role_capabilities WHERE capability=$1",
            &[&USE],
        )
        .await?
        .get(0);
    println!("role_capabilities rows for {USE} after reconcile: {rows}");
    for user in [1, 2] {
        let person = auth.authenticate(client, 1, user, String::new()).await?;
        let decision = decide(client, &person, USE).await?;
        println!(
            "{} / {USE}: {} ({})",
            person.user_name,
            if decision.allowed { "allow" } else { "deny" },
            decision.reason
        );
    }
    let admin = auth.authenticate(client, 1, 4, String::new()).await?;
    grants
        .set_system(&admin, PRINCIPAL, USE, Some(false))
        .await?;
    let denied = decide_system(client, &worker, USE).await?;
    println!(
        "after the tenant denies {USE}: {} ({})",
        if denied.allowed { "allow" } else { "deny" },
        denied.reason
    );
    grants.set_system(&admin, PRINCIPAL, USE, None).await?;
    println!(
        "set_system for {ABSENT_USE}: {:?}",
        grants
            .set_system(&admin, PRINCIPAL, ABSENT_USE, Some(true))
            .await
            .err()
    );
    Ok(ExitCode::SUCCESS)
}
