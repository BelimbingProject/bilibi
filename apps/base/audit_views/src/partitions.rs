//! Operator policy and operation contract for Audit's guarded partition path.
use bilibi_agent_api::{
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, ReadHandler,
    WriteHandler, DEFINITIONS,
};
use bilibi_audit::{AuditLedger, PartitionRemoval};
use bilibi_database::{Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, Settings, ValueType};
use bilibi_tenancy::{ReadClient, Scope};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

pub const READ: &str = "admin.audit.partition.view";
pub const REMOVE: &str = "admin.audit.partition.delete";
pub const RETENTION_DAYS: &str = "audit.partition_retention_days";
pub const SERVICE: &str = "base/audit_views.partitions";
const MODULE: &str = "base/audit_views";
static BOUND: OnceLock<Arc<Partitions>> = OnceLock::new();

pub struct Partitions {
    settings: Arc<Settings>,
}
impl Partitions {
    pub(crate) fn install(settings: Arc<Settings>) -> bilibi_module_registry::Result<Arc<Self>> {
        let service = Arc::new(Self { settings });
        BOUND.set(service.clone()).map_err(|_| {
            bilibi_module_registry::RegistryError::Service(
                "duplicate audit partition binding".into(),
            )
        })?;
        Ok(service)
    }
    async fn days(&self, client: &dyn ReadClient) -> Result<i32> {
        self.settings
            .get_global(client, RETENTION_DAYS)
            .await?
            .as_i64()
            .and_then(|days| i32::try_from(days).ok())
            .filter(|days| (1..=365_000).contains(days))
            .ok_or(Error::Invalid)
    }
    pub async fn preview(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        ledger: AuditLedger,
        month: &str,
    ) -> Result<PartitionRemoval> {
        bilibi_authz::authorize_proof(client, scope, REMOVE).await?;
        bilibi_audit::partition_preview(client, ledger, month, self.days(client).await?).await
    }
    pub async fn remove(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        ledger: AuditLedger,
        month: &str,
        reason: &str,
    ) -> Result<PartitionRemoval> {
        bilibi_authz::authorize_operator(tx.sql(), scope, REMOVE).await?;
        let days = self.days(tx.sql()).await?;
        bilibi_audit::remove_partition(tx, scope, ledger, month, days, reason).await
    }
}
fn bound() -> Result<&'static Partitions> {
    BOUND.get().map(Arc::as_ref).ok_or(Error::Internal)
}
fn ledger(input: &Value) -> Result<AuditLedger> {
    match input["ledger"].as_str() {
        Some("actions") => Ok(AuditLedger::Actions),
        Some("mutations") => Ok(AuditLedger::Mutations),
        _ => Err(Error::Invalid),
    }
}
struct Remove;
impl WriteHandler for Remove {
    fn owner(&self) -> &'static str {
        MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let reason = bilibi_agent_api::text_field(input, "reason");
            if reason.is_empty() {
                return Err(bilibi_agent_api::invalid(
                    "reason",
                    "Explain why this month can be removed.",
                ));
            }
            let result = bound()?
                .preview(
                    client,
                    scope,
                    ledger(input)?,
                    input["month"].as_str().ok_or(Error::Invalid)?,
                )
                .await?;
            Ok(json!({"summary":format!("Permanently remove {} {} rows recorded in {} across all workspaces; keep {} retained rows. This cannot be undone.",result.removed,result.ledger.as_str(),result.month,result.retained),"partition":result,"reason":reason}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let result = bound()?
                .remove(
                    tx,
                    scope,
                    ledger(input)?,
                    input["month"].as_str().ok_or(Error::Invalid)?,
                    &bilibi_agent_api::text_field(input, "reason"),
                )
                .await?;
            Ok(json!(result).into())
        })
    }
}
struct List;
impl ReadHandler for List {
    fn owner(&self) -> &'static str {
        MODULE
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        _input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_proof(client, scope, READ).await?;
            let days = bound()?.settings.get_global(client, RETENTION_DAYS).await?;
            // The catalogue is small and independent of the number of rows.
            let rows = client.query("SELECT CASE p.relname WHEN 'base_audit_actions' THEN 'actions' ELSE 'mutations' END, \
                to_char(to_date(right(c.relname,6),'YYYYMM'),'YYYY-MM-DD') \
                FROM pg_inherits i JOIN pg_class p ON p.oid=i.inhparent JOIN pg_class c ON c.oid=i.inhrelid \
                WHERE p.oid IN ('base_audit_actions'::regclass,'base_audit_mutations'::regclass) AND c.relname ~ '_[0-9]{6}$' \
                ORDER BY 2,1",&[]).await?;
            Ok(json!({"retention_days":days,"partitions":rows.iter().map(|r|json!({"ledger":r.get::<_,String>(0),"month":r.get::<_,String>(1)})).collect::<Vec<_>>()}).into())
        })
    }
}
fn empty() -> Value {
    json!({"type":"object","properties":{}})
}
fn input() -> Value {
    json!({"type":"object","required":["ledger","month","reason"],"properties":{
        "ledger":{"type":"string","enum":["actions","mutations"]},
        "month":{"type":"string","format":"date","description":"First day of the recording month in UTC, from base.audit_views.partitions."},
        "reason":{"type":"string","maxLength":2000}}})
}
fn output() -> Value {
    json!({"type":"object"})
}
inventory::submit! { Contribution::new(bilibi_settings::DEFINITIONS, RETENTION_DAYS, Provenance { module: MODULE, source: file!(), line: line!() }, &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global], default: || json!(null), nullable: true, encrypted: false,
    minimum: Some(1.0), maximum: Some(365_000.0), validator: None,
    editor: Some(Editor { group: "Audit", label: "Keep audit rows for at least this many days (server owner only)",
        help: "Applies to every workspace; leave empty to prevent removal.", capability: Some(REMOVE), reveal_duration_ms: 5_000 }),
})) }
inventory::submit! { Contribution::new(DEFINITIONS, "base/audit_views.partitions", Provenance { module: MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key:"base.audit_views.partitions", title:"List audit months", summary:"Recording months in both ledgers and the minimum retention period. A blank period prevents removal.", keywords:&["audit","partition","retention"], capability:READ, input:empty, output, handler:Handler::Read(&List), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.audit_views.partition.remove", title:"Remove an expired audit month", summary:"Permanently remove one expired recording month, preserve retained rows and record chain checkpoints. Requires a configured minimum retention period and no running verification.", keywords:&["audit","partition","retention","remove"], capability:REMOVE, input, output, handler:Handler::SafeWrite(&Remove), channels:Channels::Every, step_up:&[] },
    ], guides:&[],
}) }
