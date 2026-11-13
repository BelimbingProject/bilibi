//! Core Company's low-severity audit findings (#605), on a disposable
//! database, through the Company facade as a Domain or agent reaches it.
//!
//! 1. A tenant that is not the platform operator cannot write the global
//!    legal entity type catalog, even with `admin.company.create`; the
//!    operator still can.
//! 3. A live company can end and unlink its link to an archived company; a
//!    link between two archived companies stays frozen.
//! 6. Reading relationship roles writes nothing, and a tenant created through
//!    Core Tenant's audited path gets the default roles as audited changes.
//!
//! Exits non-zero when a check fails.
use bilibi_company::{
    relationship_role_by_code, CompanyService, CreateOutcome, NewCompany, NewType, TypeOutcome,
    WriteError, SERVICE,
};
use bilibi_database::{pool, MutationBoundary};
use bilibi_party::{EffectiveDates, PartyId, RelationshipInput};
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{Authentication, NewTenant, Scope, Tenants};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

static FAILED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Reports one check; every check runs, and the proof fails at the end.
fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        println!("  ok: {message}");
    } else {
        println!("  FAIL: {message}");
        FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

async fn company(companies: &CompanyService, scope: &Scope, name: &str) -> Result<OrganisationId> {
    let new = NewCompany {
        name: name.into(),
        ..NewCompany::default()
    };
    match companies.create_company(scope, &new).await? {
        CreateOutcome::Created(company) => Ok(OrganisationId::new(company.id)?),
        CreateOutcome::Refused(errors) => Err(format!("{name}: {errors:?}").into()),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_COMPANY_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_COMPANY_DEMO=disposable for this walkthrough".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35579")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let companies = app.services.get::<CompanyService>(SERVICE)?;
    let mut client = db.get().await?;
    // Tenant 2's manager holds the create capability in its own tenant.
    client
        .batch_execute("INSERT INTO role_capabilities(tenant_id,role_id,capability) VALUES(2,5,'admin.company.create') ON CONFLICT DO NOTHING")
        .await?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let admin = authentication
        .authenticate(&**client, 1, 4, "company-low-findings".into())
        .await?;
    let other = authentication
        .authenticate(&**client, 2, 5, "company-low-findings".into())
        .await?;

    println!("== 1. the global legal entity type catalog");
    let form = |code: &str| NewType {
        code: code.into(),
        name: format!("Form {code}"),
        ..NewType::default()
    };
    let tenant_write = companies
        .create_legal_entity_type(&other, &form("t2_form"))
        .await;
    println!("  tenant 2 manager: {tenant_write:?}");
    ensure(
        matches!(tenant_write, Err(bilibi_database::Error::Forbidden)),
        "a tenant that is not the operator cannot add a global legal entity type",
    )?;
    client
        .batch_execute("INSERT INTO platform_operator(tenant_id) VALUES(1) ON CONFLICT DO NOTHING")
        .await?;
    let operator = authentication
        .authenticate(&**client, 1, 4, "company-low-findings".into())
        .await?;
    ensure(
        matches!(
            companies
                .create_legal_entity_type(&operator, &form("op_form"))
                .await?,
            TypeOutcome::Saved(_)
        ),
        "the platform operator still adds one",
    )?;
    let stored: i64 = client
        .query_one(
            "SELECT count(*) FROM company_legal_entity_types WHERE code IN ('t2_form','op_form')",
            &[],
        )
        .await?
        .get(0);
    ensure(stored == 1, "only the operator's type is in the catalog")?;

    println!("== 3. links to an archived company");
    let buyer = company(&companies, &admin, "Low Proof Buyer").await?;
    let supplier = company(&companies, &admin, "Low Proof Supplier").await?;
    let closed = company(&companies, &admin, "Low Proof Closed Buyer").await?;
    let customer = relationship_role_by_code(&**client, &admin, "customer")
        .await?
        .ok_or("customer role missing")?;
    let mut links = Vec::new();
    for source in [buyer, closed] {
        let input = RelationshipInput {
            source_organisation_id: source,
            target: PartyId::Organisation(supplier),
            role_id: customer.id,
            dates: EffectiveDates::new(Some("2026-01-01".to_owned().try_into()?), None)?,
        };
        match companies.assign_relationship(&admin, &input).await? {
            TypeOutcome::Saved(link) => links.push(link),
            TypeOutcome::Refused(errors) => return Err(format!("assign: {errors:?}").into()),
        }
    }
    let [live_link, frozen_link] = <[_; 2]>::try_from(links).map_err(|_| "two links")?;
    for archived in [supplier, closed] {
        companies
            .archive_company(&admin, archived.get(), Some("Closed for the proof"))
            .await
            .map_err(|e| format!("archive: {e:?}"))?;
    }
    let today: bilibi_party::CalendarDate = "2026-11-13".to_owned().try_into()?;
    let ended = companies
        .end_relationship(&admin, live_link.id, live_link.version, &today)
        .await;
    println!(
        "  end buyer → archived supplier: {:?}",
        ended
            .as_ref()
            .map(|l| l.dates.to().map(|d| d.as_str().to_owned()))
    );
    ensure(
        ended.is_ok(),
        "the live buyer ends its link to the archived supplier",
    )?;
    let (id, version) = ended.map_or((live_link.id, live_link.version), |l| (l.id, l.version));
    let unlinked = companies.unlink_relationship(&admin, id, version).await;
    println!("  unlink: {:?}", unlinked.as_ref().map(|l| l.id));
    ensure(unlinked.is_ok(), "and unlinks it")?;
    let frozen = companies
        .end_relationship(&admin, frozen_link.id, frozen_link.version, &today)
        .await;
    println!(
        "  end archived buyer → archived supplier: {:?}",
        frozen.as_ref().map(|l| l.id)
    );
    ensure(
        matches!(frozen, Err(WriteError::Archived)),
        "a link between two archived companies stays frozen",
    )?;

    println!("== 6. default relationship roles");
    client
        .batch_execute("DELETE FROM company_relationship_roles WHERE tenant_id=2")
        .await?;
    let read = companies.relationship_roles(&other, false).await?;
    let stored: i64 = client
        .query_one(
            "SELECT count(*) FROM company_relationship_roles WHERE tenant_id=2",
            &[],
        )
        .await?
        .get(0);
    println!(
        "  tenant 2 with no roles: read {} roles, {stored} stored",
        read.len()
    );
    ensure(read.is_empty() && stored == 0, "a read writes no role")?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let tenants = app.services.get::<Tenants>("base/tenancy.tenants")?;
    let mut tx = boundary
        .begin(&mut client, operator.mutation_context("tenant.create"))
        .await?;
    let receipt = match tenants
        .create_in(
            &mut tx,
            &operator,
            &NewTenant {
                name: "Low Proof Tenant".into(),
                parent_id: None,
            },
        )
        .await?
    {
        bilibi_tenancy::CreateOutcome::Created(receipt) => receipt,
        bilibi_tenancy::CreateOutcome::Refused(errors) => {
            return Err(format!("tenant refused: {errors:?}").into())
        }
    };
    let tenant = receipt.identity().id;
    companies
        .provision_new_tenant_in(&mut tx, &operator, &receipt, &["Low Proof Co".into()])
        .await?;
    tx.commit().await?;
    let row = client
        .query_one(
            "SELECT (SELECT count(*) FROM company_relationship_roles WHERE tenant_id=$1), \
             (SELECT count(*) FROM base_audit_mutations m JOIN company_relationship_roles r ON r.id::text=m.auditable_id \
              WHERE r.tenant_id=$1 AND m.auditable_type='company.relationship_role' AND coalesce(m.old_values,'null'::jsonb)='null'::jsonb AND m.new_values->>'code'=r.code)",
            &[&tenant],
        )
        .await?;
    let (roles, audited): (i64, i64) = (row.get(0), row.get(1));
    println!("  new tenant {tenant}: {roles} roles, {audited} audited");
    ensure(
        roles == 4 && audited == 4,
        "a tenant created through Core Tenant gets the four default roles, each audited",
    )?;

    match FAILED.load(std::sync::atomic::Ordering::Relaxed) {
        0 => {
            println!("company low-findings proof passed");
            Ok(())
        }
        failed => Err(format!("{failed} checks failed").into()),
    }
}
