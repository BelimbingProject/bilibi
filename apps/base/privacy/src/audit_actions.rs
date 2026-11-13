//! Retention of the action ledger (`base_audit_actions`): the record class
//! `base/privacy.audit_actions`. Like every class it is off until a company
//! turns it on and sets a period. Action rows name no company, so they follow
//! the tenant's primary company. Once the period has passed, a row loses its
//! address, URL, browser and payload values; who acted, when and the event
//! stay, so the row still answers "what happened" and the hash chain holds
//! (`bilibi_audit::erase_actions`). A retained action (its bookmark on the
//! Actions screen, or the bit recorded with it) is the exemption: it is
//! passed over and counted as kept. Legal holds name persons and records, and
//! an action row is neither, so none applies here.
use crate::{Companies, Sweeper, Swept};
use bilibi_database::MutationTransaction;
use bilibi_personal_data::{Erasure, HandlerFuture, Holds};
use bilibi_tenancy::Scope;

struct AuditActions;
static AUDIT_ACTIONS: AuditActions = AuditActions;
impl Sweeper for AuditActions {
    fn sweep<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        grant: &'a Erasure,
        companies: &'a Companies,
        cutoff: &'a str,
        _holds: &'a Holds,
        after: i64,
        limit: i64,
    ) -> HandlerFuture<'a, Swept> {
        Box::pin(async move {
            grant.check(scope)?;
            let batch = bilibi_audit::erase_actions(
                tx,
                grant,
                cutoff,
                companies.parameters(),
                after,
                limit,
            )
            .await?;
            Ok(Swept {
                acted: batch.erased,
                kept: batch.kept,
                records: Vec::new(),
                next: batch.next,
            })
        })
    }
}

crate::record_class! {
    module: "base/privacy", key: "base/privacy.audit_actions", label: "Audit actions",
    clock_start: "when the action happened", action: Anonymise,
    business_value: true, floor_days: None, sweeper: &AUDIT_ACTIONS,
}
