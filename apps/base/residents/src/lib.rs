//! Resident readers: long-lived workers that hold a live connection, run
//! outside the queue's job slots and are owned by one server process per key
//! (a lease and a heartbeat in PostgreSQL). See `README.md` for the contract
//! and the running proof.
mod reading;
mod registry;
mod store;
mod supervisor;
mod web;

pub use reading::{Ended, ReadFuture, ReaderService, Reading, Resident};
pub use registry::{Reader, Readers, RegisteredReader, READERS};
pub use store::{health_status, Filter, Page, State, Status};
pub use supervisor::{Config, Supervisor};

use bilibi_authz::{CapabilityDefinition, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_database::{Change, Db, Error, MutationTransaction, Result};
use bilibi_module_registry::{
    contributions::{Access, CompanyScope, Method, Route, ROUTES},
    Catalog, Contribution, Layer, Module, Provenance, Service,
};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::GenericClient;

pub const SERVICE: &str = "base/residents.store";
/// See every reading on the server: installation-wide, so the platform
/// operator identity is required beside the grant.
pub const VIEW: &str = "admin.system.readers.view";
const PAGES: &str = "base/residents.pages";
/// The page sizes `Residents::list` accepts.
pub const PAGE_SIZES: [u64; 3] = [25, 50, 100];

pub struct Residents {
    readers: Readers,
}
impl Residents {
    pub fn readers(&self) -> &Readers {
        &self.readers
    }
    /// Makes sure `reader` is kept reading `key` in the scope's tenant, in
    /// the caller's audited transaction: the reading commits or rolls back
    /// with the owner's write. Idempotent; `true` when this call added it.
    /// A key is the owner's reference (a record ID), never a person's data.
    /// This grants nothing: the owner authorizes its own command first, and
    /// the reading runs only as the reader's declared principal, proven
    /// again before every start.
    pub async fn keep(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        reader: &str,
        key: &str,
    ) -> Result<bool> {
        let reader = self.readers.get(reader)?;
        if !registry::valid_key(key) {
            return Err(Error::Invalid);
        }
        let tenant = tenant_of(tx, scope)?;
        if tx.sql().query_opt("SELECT id FROM base_resident_readings WHERE tenant_id=$1 AND reader_id=$2 AND key=$3", &[&tenant, &reader.id, &key]).await?.is_some() {
            return Ok(false);
        }
        let key = key.to_owned();
        tx.mutate(move |sql| Box::pin(async move {
            let new = json!({"reader": reader.id, "key": key, "kept": true});
            let (id, old) = match sql.query_opt("INSERT INTO base_resident_readings(tenant_id,reader_id,reader_module,key) VALUES($1,$2,$3,$4) ON CONFLICT (tenant_id,reader_id,key) DO NOTHING RETURNING id", &[&tenant, &reader.id, &reader.module, &key]).await? {
                Some(row) => (row.get(0), Value::Null),
                // Another transaction kept the same key first: nothing changed.
                None => (sql.query_one("SELECT id FROM base_resident_readings WHERE tenant_id=$1 AND reader_id=$2 AND key=$3", &[&tenant, &reader.id, &key]).await?.get(0), new.clone()),
            };
            let added = old.is_null();
            Ok((added, vec![Change { kind: "base/residents.reader".into(), id, old, new }]))
        })).await
    }
    /// Stops reading `key`: removes the reading in the caller's audited
    /// transaction. A running reader is cancelled at its owner's next
    /// heartbeat, and `Reading::begin` refuses its writes from the commit on.
    /// Idempotent; `true` when a reading was removed. Not limited to readers
    /// this composition declares, so an owner can clear what it left behind.
    pub async fn release(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        reader: &str,
        key: &str,
    ) -> Result<bool> {
        if reader.is_empty() || reader.len() > 128 || !registry::valid_key(key) {
            return Err(Error::Invalid);
        }
        let tenant = tenant_of(tx, scope)?;
        // The row lock waits for a reader's fenced write in flight.
        let Some(row) = tx.sql().query_opt("SELECT id FROM base_resident_readings WHERE tenant_id=$1 AND reader_id=$2 AND key=$3 FOR UPDATE", &[&tenant, &reader, &key]).await? else {
            return Ok(false);
        };
        let id: i64 = row.get(0);
        let old = json!({"reader": reader, "key": key, "kept": true});
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "DELETE FROM base_resident_readings WHERE tenant_id=$1 AND id=$2",
                    &[&tenant, &id],
                )
                .await?;
                Ok((
                    true,
                    vec![Change {
                        kind: "base/residents.reader".into(),
                        id,
                        old,
                        new: Value::Null,
                    }],
                ))
            })
        })
        .await
    }
    /// Every tenant's readings, for the platform operator holding [`VIEW`]:
    /// which process owns each, its state, last heartbeat and last error.
    /// No lease token is returned, and keys are references by contract.
    pub async fn list(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        filter: &Filter,
        page: u64,
        page_size: u64,
    ) -> Result<Page> {
        bilibi_authz::authorize_operator(client, scope, VIEW).await?;
        if page < 1 || !PAGE_SIZES.contains(&page_size) || filter.search.len() > 200 {
            return Err(Error::Invalid);
        }
        let mut found = store::list(client, filter, page as i64, page_size as i64).await?;
        for status in &mut found.entries {
            status.mounted = self.readers.get(&status.reader).is_ok();
        }
        Ok(found)
    }
}
/// The tenant both the sealed scope and the audited transaction name.
fn tenant_of(tx: &MutationTransaction<'_>, scope: &Scope) -> Result<i64> {
    if tx.context().tenant != scope.tenant() {
        return Err(Error::Forbidden);
    }
    Ok(scope.tenant())
}

macro_rules! provenance {
    () => {
        Provenance {
            module: "base/residents",
            source: file!(),
            line: line!(),
        }
    };
}
inventory::submit! { Contribution::new(VOCABULARY, "base/residents", provenance!(), &VocabularyContribution {
    domains: &[],
    verbs: &[],
    capabilities: &[CapabilityDefinition::platform(VIEW)],
    roles: &[RoleDefinition { code: "system_viewer", name: "System Viewer", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: &[VIEW] }],
    system_principals: &[],
}) }
inventory::submit! { Contribution::new(ROUTES, "base/residents.index", provenance!(), &Route { company_scope: CompanyScope::Tenant,
    method: Method::Get, path: web::PATH, service: PAGES, operation: "index",
    access: Access::OperatorCapability(VIEW), framed: true,
}) }
bilibi_menu::contribute!(
    "base/residents",
    bilibi_menu::MenuItem::page("admin.system.readers", "Resident readers", web::PATH)
        .icon("signal")
        .under("admin.system")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(22)
);
// The page reads only: it has no write event.
inventory::submit! { Contribution::new(bilibi_module_registry::contributions::PAGE_WRITES, PAGES, provenance!(), &bilibi_module_registry::contributions::PageWrites {
    service: PAGES,
    contracts: &[],
    pending: bilibi_module_registry::contributions::Pending::None,
}) }
inventory::submit! {
    Module {
        id: "base/residents", layer: Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/tenancy", "base/authz", "base/ui", "base/menu", "base/datetime", "base/locale"],
        migrations: &[("base-residents-001-readings", include_str!("../migrations/001-readings.sql"))],
        services: &[
            Service { id: SERVICE, factory: |_| {
                let catalog = Catalog::discover(&bilibi_module_registry::discover()?)?;
                Ok(Arc::new(Residents { readers: Readers::from_catalog(&catalog)? }))
            } },
            Service { id: PAGES, factory: |services| {
                Ok(Arc::new(bilibi_ui::page::PageService::<Scope, Error>(Arc::new(web::Pages {
                    db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
                    residents: services.get(SERVICE)?,
                    tenants: services.get("base/tenancy.tenants")?,
                    display: services.get("base/datetime.display")?,
                    locales: services.get("base/locale.locales")?,
                }))))
            } },
        ],
    }
}
