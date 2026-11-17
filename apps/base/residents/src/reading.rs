//! What a reader is handed for one run under one lease, and what it answers.
use bilibi_authz::SystemScope;
use bilibi_database::{Error, MutationBoundary, MutationTransaction, Result};
use bilibi_tenancy::Scope;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::watch;

pub type ReadFuture<'a> = Pin<Box<dyn Future<Output = Ended> + Send + 'a>>;
/// Owner services implement this and contribute a matching [`crate::Reader`].
/// `read` holds the connection and handles events until the connection
/// closes, it fails, or [`Reading::cancelled`] resolves. A takeover may hand a
/// new owner an event the old one was handling, so effects must be idempotent.
pub trait Resident: Send + Sync {
    fn read<'a>(&'a self, reading: &'a Reading) -> ReadFuture<'a>;
}
pub struct ReaderService(pub Arc<dyn Resident>);

/// How one run ended. Either way the reading starts again; a key stops being
/// read only when its owner releases it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ended {
    /// The connection ended in the ordinary way, or the reader was cancelled.
    Closed,
    /// It failed. The code, lowercase `[a-z][a-z0-9_]{0,63}`, is what
    /// operators see; error text, payloads and addresses are never stored.
    Failed(&'static str),
}

/// One run of a reader for one key, under one lease.
pub struct Reading {
    pub(crate) id: i64,
    pub(crate) key: String,
    pub(crate) reader: &'static str,
    pub(crate) system: SystemScope,
    pub(crate) scope: Box<Scope>,
    pub(crate) token: String,
    pub(crate) capability: &'static str,
    pub(crate) boundary: Arc<MutationBoundary>,
    pub(crate) cancel: watch::Receiver<bool>,
}
impl Reading {
    /// The key this reading holds: the owner's reference, such as a record ID.
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn reader(&self) -> &'static str {
        self.reader
    }
    pub fn tenant(&self) -> i64 {
        self.system.tenant()
    }
    /// The reader's system principal, sealed in the key's tenant when this
    /// run started. Hand it to owner facades: each decides by the principal's
    /// name and its live grants.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
    /// Whether this reading was told to stop.
    pub fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }
    /// Resolves when the reader must stop: the process shuts down, the lease
    /// was lost, the key was released or the principal was refused. Select on
    /// it beside the connection wait and return [`Ended::Closed`]. A reader
    /// that has not returned within the grace period is dropped.
    pub async fn cancelled(&self) {
        let mut cancel = self.cancel.clone();
        // A dropped sender means the supervisor is gone: stop as well.
        let _ = cancel.wait_for(|stop| *stop).await;
    }
    /// Opens the audited transaction for a write of this reading, attributed
    /// to `System · <principal>`. It locks the reading's live lease, so a
    /// reader whose lease expired, moved to another process or was released
    /// gets `Error::Conflict` and writes nothing, and it proves the tenant,
    /// the principal's declaration and its grant again. The lease row stays
    /// locked until commit or rollback: keep the transaction short and never
    /// hold it over external IO. Reauthorize target-specific operations
    /// inside it too.
    pub async fn begin<'a>(
        &self,
        client: &'a mut tokio_postgres::Client,
        action: impl Into<String>,
    ) -> Result<MutationTransaction<'a>> {
        let tx = self
            .boundary
            .begin(client, self.system.mutation_context(action))
            .await?;
        let alive = tx.sql().query_opt("SELECT id FROM base_resident_readings WHERE id=$1 AND tenant_id=$2 AND state='running' AND lease_token=$3 AND lease_expires_at>clock_timestamp() FOR UPDATE", &[&self.id, &self.tenant(), &self.token]).await?.is_some();
        if !alive {
            return Err(Error::Conflict);
        }
        let system =
            SystemScope::rehydrate(tx.sql(), self.system.tenant(), self.system.principal()).await?;
        bilibi_authz::authorize_system(tx.sql(), &system, self.capability).await?;
        Ok(tx)
    }
}
