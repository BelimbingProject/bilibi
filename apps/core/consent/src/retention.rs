//! Consent's record class (ADR 05, `base/privacy` retention): the dispatch
//! log. A check is the proof that a send was allowed or refused, so it is
//! kept; once its company's period has passed since the check, its address
//! and correlation key are cleared, as an erasure clears them, and the fact
//! stays (company, person, channel, purpose, outcome, caller, times). Nothing
//! is swept until a company turns the class on and sets a period, and a check
//! a legal hold names, by record or by its person, is kept.
use crate::LOG_RECORD;
use bilibi_database::{Change, MutationTransaction};
use bilibi_personal_data::{ErasedRecord, Erasure, HandlerFuture, Holds, ERASED};
use bilibi_privacy::{Companies, Sweeper, Swept};
use bilibi_tenancy::Scope;
use serde_json::json;

struct DispatchLog;
static DISPATCH_LOG: DispatchLog = DispatchLog;
impl Sweeper for DispatchLog {
    fn sweep<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        grant: &'a Erasure,
        companies: &'a Companies,
        cutoff: &'a str,
        holds: &'a Holds,
        after: i64,
        limit: i64,
    ) -> HandlerFuture<'a, Swept> {
        Box::pin(async move {
            grant.check(scope)?;
            let tenant = scope.tenant();
            let (company, primary, listed) = companies.parameters();
            let rows = tx
                .sql()
                .query(
                    &format!(
                        "SELECT id,person_id FROM core_consent_log WHERE tenant_id=$1 AND erased_at IS NULL AND at < $2::text::timestamptz \
                         AND id>$3 AND {} ORDER BY id LIMIT $4 FOR UPDATE",
                        Companies::predicate("company_id", 5)
                    ),
                    &[&tenant, &cutoff, &after, &limit, &company, &primary, &listed],
                )
                .await?;
            let keys: Vec<(i64, Option<i64>)> =
                rows.iter().map(|row| (row.get(0), row.get(1))).collect();
            let ids: Vec<i64> = keys.iter().map(|(id, _)| *id).collect();
            let persons: Vec<i64> = keys.iter().filter_map(|(_, person)| *person).collect();
            let held_records = holds.records(grant, LOG_RECORD, &ids).await?;
            let held_persons = holds.persons(grant, &persons).await?;
            let (kept, due): (Vec<_>, Vec<_>) = keys.iter().partition(|(id, person)| {
                held_records.contains_key(id)
                    || person.is_some_and(|p| held_persons.contains_key(&p))
            });
            let mut swept = Swept {
                kept: kept.len() as i64,
                next: (keys.len() as i64 == limit)
                    .then(|| keys.last().map(|(id, _)| *id))
                    .flatten(),
                ..Swept::default()
            };
            // A write must return changes: nothing to act on is not a write.
            if due.is_empty() {
                return Ok(swept);
            }
            let due: Vec<i64> = due.iter().map(|(id, _)| *id).collect();
            let records: Vec<(i64, Option<i64>)> = tx
                .mutate(move |sql| {
                    Box::pin(async move {
                        let rows = sql
                            .query(
                                "UPDATE core_consent_log n SET address=NULL,key=$3,erased_at=now() FROM core_consent_log o \
                                 WHERE n.tenant_id=$1 AND n.id=ANY($2) AND o.tenant_id=n.tenant_id AND o.id=n.id \
                                 RETURNING n.id,n.person_id,o.address,o.key",
                                &[&tenant, &due, &ERASED],
                            )
                            .await?;
                        let changes = rows
                            .iter()
                            .map(|row| Change {
                                kind: LOG_RECORD.into(),
                                id: row.get(0),
                                // As an erasure records it: `base/privacy`
                                // clears the old values from this evidence in
                                // the same transaction (`Swept::records`).
                                old: json!({"address": row.get::<_, Option<String>>(2), "key": row.get::<_, String>(3)}),
                                new: json!({"address": row.get::<_, Option<String>>(2).map(|_| ERASED), "key": ERASED}),
                            })
                            .collect();
                        Ok((rows.iter().map(|row| (row.get(0), row.get(1))).collect(), changes))
                    })
                })
                .await?;
            swept.acted = records.len() as i64;
            swept.records = records
                .into_iter()
                .map(|(id, person)| ErasedRecord {
                    kind: LOG_RECORD.into(),
                    id,
                    person,
                })
                .collect();
            Ok(swept)
        })
    }
}

bilibi_privacy::record_class! {
    module: "core/consent", key: "core/consent.dispatch_log", label: "Consent dispatch checks",
    clock_start: "when the check was made", action: Anonymise,
    business_value: true, floor_days: None, sweeper: &DISPATCH_LOG,
}
