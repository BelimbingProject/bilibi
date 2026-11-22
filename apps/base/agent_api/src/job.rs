//! Queued jobs as callers of operation contracts (ADR 10): the fifth door.
//! A job a person started (`Queue::enqueue_for`) calls a contract as that
//! person under the rules the other doors keep: the contract's channels
//! (`Channel::Job`), its input schema, its capability decided live on every
//! call, the owner's preview and then its commit on the job's own
//! transaction, which `Execution::begin` opened (it fences the lease,
//! re-proves the person and keeps the enqueue's trace ID).
//!
//! No agent approval round-trip occurs: the person reviewed the job when they started it,
//! as a page's form is reviewed before `Plans::submit`, and the owner's
//! version fence recognises a repeat. A contract that declares step-up is
//! not callable: no person is present to give a code.
//!
//! A named system principal's job calls a contract only where the contract's
//! owner admits that principal by name (`PRINCIPAL_WRITES`): Base Watchers runs
//! an approved watcher's action as `watchers`. The same schema, live capability
//! (the principal's own tenant grants), preview and commit apply; system
//! scopes still cannot enter the other doors.
use crate::{Error, Handler, RegisteredOperation, Registry, Result};
use bilibi_database::MutationTransaction;
use bilibi_module_registry::{Entry, RegistryError};
use bilibi_queue::Execution;
use bilibi_tenancy::{Channel, Scope};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tokio_postgres::GenericClient;

pub const SERVICE: &str = "base/agent_api.jobs";
/// The catalog a contract's owner admits named principals' jobs in:
/// `Contribution::new(PRINCIPAL_WRITES, "<contract key>", provenance,
/// &PrincipalWrite { principals: &["watchers"] })`, from the contract's module.
pub const PRINCIPAL_WRITES: &str = "base/agent_api.principal_writes";

/// The named principals whose jobs may call one write contract. The contract
/// must be the contributor's own reviewed write, open to the job door with no
/// step-up; each principal must be declared. Admitting a principal grants it
/// nothing: it still needs the contract's capability in its bounds and a
/// tenant's grant, decided live on every call.
pub struct PrincipalWrite {
    pub principals: &'static [&'static str],
}

pub(crate) fn principal_writes(
    registry: &Registry,
    vocabulary: &bilibi_authz::Vocabulary,
    entries: &[Entry],
) -> bilibi_module_registry::Result<BTreeMap<&'static str, &'static [&'static str]>> {
    let mut admitted = BTreeMap::new();
    for entry in entries {
        let write = entry.get::<PrincipalWrite>()?;
        let refuse = |why: &str| {
            RegistryError::Contribution(format!(
                "invalid principal write {} at {}:{}: {why}",
                entry.id, entry.provenance.source, entry.provenance.line
            ))
        };
        let Some(registered) = registry.operations.get(entry.id) else {
            return Err(refuse("not a registered operation"));
        };
        let op = registered.definition;
        if registered.provenance.module != entry.provenance.module {
            return Err(refuse("the contract belongs to another module"));
        }
        if !matches!(op.handler, Handler::SafeWrite(_))
            || !op.step_up.is_empty()
            || !op.channels.allows(Channel::Job)
        {
            return Err(refuse(
                "not a reviewed write open to the job door without step-up",
            ));
        }
        if write.principals.is_empty()
            || write
                .principals
                .iter()
                .any(|name| !vocabulary.system_principals.contains_key(name))
        {
            return Err(refuse("names no principal or an undeclared one"));
        }
        if admitted.insert(entry.id, write.principals).is_some() {
            return Err(refuse("a second entry for this contract"));
        }
    }
    Ok(admitted)
}

pub struct JobContracts {
    registry: Arc<Registry>,
}

impl JobContracts {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    /// A write contract a job may call: registered, a reviewed write, open to
    /// the job door and asking no step-up. Owners check theirs at boot.
    pub fn writable(&self, key: &str) -> Result<&RegisteredOperation> {
        let registered = self
            .registry
            .operations
            .get(key)
            .ok_or(Error::UnknownOperation)?;
        let op = registered.definition;
        if !matches!(op.handler, Handler::SafeWrite(_)) || !op.step_up.is_empty() {
            return Err(Error::WritesNotEnabled);
        }
        if !op.channels.allows(Channel::Job) {
            return Err(Error::ChannelNotAllowed);
        }
        Ok(registered)
    }

    /// A read contract a job may call (a lookup), open to the job door.
    pub fn readable(&self, key: &str) -> Result<&RegisteredOperation> {
        let registered = self
            .registry
            .operations
            .get(key)
            .ok_or(Error::UnknownOperation)?;
        if !matches!(registered.definition.handler, Handler::Read(_)) {
            return Err(Error::UnknownOperation);
        }
        if !registered.definition.channels.allows(Channel::Job) {
            return Err(Error::ChannelNotAllowed);
        }
        Ok(registered)
    }

    /// A write contract `principal`'s job may call: [`Self::writable`], and
    /// its owner admits that principal (`PRINCIPAL_WRITES`).
    pub fn principal_writable(&self, key: &str, principal: &str) -> Result<&RegisteredOperation> {
        let registered = self.writable(key)?;
        if !self
            .registry
            .principal_writes
            .get(key)
            .is_some_and(|names| names.contains(&principal))
        {
            return Err(Error::ChannelNotAllowed);
        }
        Ok(registered)
    }

    /// Whether `input` passes the contract's input schema, for a caller that
    /// stores an input to commit later (an approved watcher's action).
    pub fn validate(&self, key: &str, input: &Value) -> Result<()> {
        let registered = self.writable(key)?;
        crate::schema::validate(&registered.input, input).map_err(Error::InvalidInput)
    }

    /// The person behind the job, or a named principal the contract's owner
    /// admits, sealed for the job door.
    fn scope(&self, execution: &Execution, key: &str) -> Result<Scope> {
        match execution.scope() {
            Some(scope) if scope.principal().is_none() => Ok(scope.clone().in_job()),
            Some(scope)
                if scope
                    .principal()
                    .is_some_and(|name| self.principal_writable(key, name).is_ok()) =>
            {
                Ok(scope.clone().in_job())
            }
            _ => Err(bilibi_database::Error::Forbidden.into()),
        }
    }

    /// Schema, capability and the owner's preview: what a commit would do,
    /// making no business writes. Normal refusals retain owner authorization
    /// decisions. Answers the preview's result, or the refusal typed as
    /// the other doors do (`InvalidInput`, `Forbidden`, `NotFound`,
    /// `Backend(Conflict)` …).
    pub async fn preview(
        &self,
        client: &mut (impl GenericClient + Sync),
        execution: &Execution,
        key: &str,
        input: &Value,
    ) -> Result<Value> {
        let scope = self.scope(execution, key)?;
        let registered = self.writable(key)?;
        self.check(client, &scope, registered, input).await?;
        let Handler::SafeWrite(owner) = registered.definition.handler else {
            return Err(Error::WritesNotEnabled);
        };
        let tx = client
            .transaction()
            .await
            .map_err(bilibi_database::Error::from)?;
        let reservations = crate::PreviewReservations::default();
        let output = owner
            .preview(&crate::TransactionRead(&tx, &reservations), &scope, input)
            .await;
        crate::finish_read_transaction(tx, &output).await?;
        output.map(|output| output.envelope(key))
    }

    /// Preview an admitted principal's job action inside the caller's snapshot.
    /// The caller owns transaction recovery and checks reader access before disclosure.
    pub async fn preview_principal(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        scope: &Scope,
        key: &str,
        input: &Value,
        reservations: &crate::PreviewReservations,
    ) -> Result<Value> {
        let principal = scope.principal().ok_or(bilibi_database::Error::Forbidden)?;
        let registered = self.principal_writable(key, principal)?;
        let scope = scope.clone().in_job();
        self.check(tx, &scope, registered, input).await?;
        let Handler::SafeWrite(owner) = registered.definition.handler else {
            return Err(Error::WritesNotEnabled);
        };
        owner
            .preview(&crate::TransactionRead(tx, reservations), &scope, input)
            .await
            .map(|output| output.envelope(key))
    }

    /// The owner's preview, then its commit on `tx`, which must be this
    /// execution's (`Execution::begin`): the job's lease, caller and trace.
    /// The caller records its own result for the call in the same `tx` and
    /// commits it; dropping `tx` rolls the owner's write back.
    pub async fn commit(
        &self,
        tx: &mut MutationTransaction<'_>,
        execution: &Execution,
        key: &str,
        input: &Value,
    ) -> Result<Value> {
        let scope = self.scope(execution, key)?;
        let actor = match scope.principal() {
            Some(principal) => ("system", principal.to_owned()),
            None => ("user", scope.user().to_string()),
        };
        if tx.context().tenant != scope.tenant()
            || tx.context().actor.kind != actor.0
            || tx.context().actor.id != actor.1
        {
            return Err(bilibi_database::Error::Forbidden.into());
        }
        let registered = self.writable(key)?;
        self.check(tx.sql(), &scope, registered, input).await?;
        let Handler::SafeWrite(owner) = registered.definition.handler else {
            return Err(Error::WritesNotEnabled);
        };
        let reservations = crate::PreviewReservations::default();
        let preview = owner
            .preview(
                &crate::TransactionRead(tx.sql(), &reservations),
                &scope,
                input,
            )
            .await;
        if matches!(
            preview,
            Err(Error::Backend(
                bilibi_database::Error::UnrecoveredTransaction(_)
            ))
        ) {
            tx.poison();
        }
        preview?;
        // A job's door already holds its transaction, so nothing was staged
        // before it (`WriteHandler::prepare`).
        Ok(owner
            .commit_prepared(tx, &scope, input, None)
            .await?
            .envelope(key))
    }

    /// A read contract as the job's person: the same validation and logged
    /// capability decision as `Registry::call`.
    pub async fn read(
        &self,
        client: &mut (impl GenericClient + Sync),
        execution: &Execution,
        key: &str,
        input: &Value,
    ) -> Result<Value> {
        let scope = match execution.scope() {
            Some(scope) if scope.principal().is_none() => scope.clone().in_job(),
            _ => return Err(bilibi_database::Error::Forbidden.into()),
        };
        self.readable(key)?;
        self.registry.call(client, &scope, key, input).await
    }

    async fn check(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        registered: &RegisteredOperation,
        input: &Value,
    ) -> Result<()> {
        crate::schema::validate(&registered.input, input).map_err(Error::InvalidInput)?;
        crate::authorize_target(client, scope, registered.definition.capability, input).await?;
        Ok(())
    }
}
