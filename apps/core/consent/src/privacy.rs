//! Consent's personal-data store (ADR 05, `base/personal_data`): decisions,
//! current state, suppressions and the dispatch-time log of the person, and of
//! the addresses the operator confirmed are theirs.
//!
//! Erasure keeps the bare fact of each decision and check (company, purpose,
//! channel, outcome, times), since proof of a withdrawal may need to outlive
//! the request, and clears its address, evidence and correlation key. An
//! address's current state is deleted: with no address it answers nothing.
//! An active suppression is the answer that stops future sends, so it is kept
//! under its reason with its address replaced by a keyed hash, which
//! `answer` still matches; a lifted one keeps no address at all. The README's
//! "Privacy requests" section gives the reason for Domains to copy.
use crate::{CURRENT_RECORD, DECISION_RECORD, LOG_RECORD, SUPPRESSION_RECORD};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_personal_data::{
    Action, Candidate, Disclosure, Erased, ErasedRecord, Erasure, Exported, Handler, HandlerFuture,
    Holds, Identifier, Kept, KindCount, Located, PersonalData, RecordKind, Subject, ERASED,
    MAX_CANDIDATES, PERSONAL_DATA, RECORD_KINDS,
};
use bilibi_settings::Settings;
use bilibi_tenancy::{ReadClient, Scope};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Map, Value};
use sha2::Sha256;
use std::sync::{Arc, OnceLock};
use tokio_postgres::{types::ToSql, Client, GenericClient, Row};

const DECISION_FIELDS: [&str; 2] = ["address", "evidence"];
const CURRENT_FIELDS: [&str; 1] = ["address"];
const SUPPRESSION_FIELDS: [&str; 3] = ["address", "evidence", "lift_reason"];
const LOG_FIELDS: [&str; 2] = ["address", "key"];
/// The audit kind of the tenant's address key row (no personal field).
const KEY_KIND: &str = "core.consent.address_key";
/// `Settings::seal_owned` names the key by owner, tenant and this record.
const KEY_OWNER: &str = "core/consent.address_key";
const KEY_RECORD: i64 = 1;
const DIGEST_DOMAIN: &[u8] = b"bilibi-consent-address-v1|";

static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();
/// Bound once by the Consent service factory.
pub(crate) fn bind(settings: Arc<Settings>) -> bilibi_module_registry::Result<()> {
    SETTINGS
        .set(settings)
        .map_err(|_| RegistryError::Service("duplicate consent privacy binding".into()))
}
fn settings() -> Result<&'static Settings> {
    SETTINGS.get().map(|s| &**s).ok_or(Error::Internal)
}

/// HMAC-SHA-256 of a normalised address under the tenant's key.
fn digest(key: &[u8], address: &str) -> Result<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| Error::Internal)?;
    mac.update(DIGEST_DOMAIN);
    mac.update(address.as_bytes());
    Ok(mac.finalize().into_bytes().to_vec())
}
fn open(settings: &Settings, tenant: i64, sealed: &Value) -> Result<Vec<u8>> {
    let opened = settings.open_owned(KEY_OWNER, tenant, KEY_RECORD, sealed)?;
    let hex = opened.as_str().filter(|h| h.len() == 64 && h.is_ascii());
    hex.and_then(|hex| {
        (0..32)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
            .collect()
    })
    .ok_or(Error::Internal)
}
/// The keyed hash `answer` matches erased suppressions by, or `None` when the
/// tenant has no key yet (so no suppression was hashed). A key that cannot be
/// opened (no Settings key) refuses the check rather than miss a suppression.
pub(crate) async fn address_hash(
    client: &dyn ReadClient,
    settings: &Settings,
    tenant: i64,
    address: Option<&str>,
) -> Result<Option<Vec<u8>>> {
    let Some(address) = address else {
        return Ok(None);
    };
    let Some(row) = client
        .query_opt(
            "SELECT sealed FROM core_consent_keys WHERE tenant_id=$1",
            &[&tenant],
        )
        .await?
    else {
        return Ok(None);
    };
    digest(&open(settings, tenant, &row.get(0))?, address).map(Some)
}
/// The tenant's key, created and sealed on first use in the erasure's
/// transaction. Refuses without a Settings key, so nothing is erased unhashed.
async fn ensure_key(
    tx: &mut MutationTransaction<'_>,
    settings: &Settings,
    tenant: i64,
) -> Result<Vec<u8>> {
    const SELECT: &str = "SELECT sealed FROM core_consent_keys WHERE tenant_id=$1";
    if let Some(row) = tx.sql().query_opt(SELECT, &[&tenant]).await? {
        return open(settings, tenant, &row.get(0));
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|_| Error::Internal)?;
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    let sealed = settings.seal_owned(KEY_OWNER, tenant, KEY_RECORD, &json!(hex))?;
    tx.mutate(move |sql| {
        Box::pin(async move {
            let inserted = sql
                .execute(
                    "INSERT INTO core_consent_keys (tenant_id,sealed) VALUES ($1,$2) ON CONFLICT (tenant_id) DO NOTHING",
                    &[&tenant, &sealed],
                )
                .await?;
            // The change names the key row, never the key.
            let changes = (inserted == 1)
                .then(|| Change {
                    kind: KEY_KIND.into(),
                    id: tenant,
                    old: json!({}),
                    new: json!({"sealed": true}),
                })
                .into_iter()
                .collect();
            Ok(((), changes))
        })
    })
    .await?;
    // A concurrent erasure may have created it first.
    let row = tx.sql().query_one(SELECT, &[&tenant]).await?;
    open(settings, tenant, &row.get(0))
}

/// The requester's emails (as Consent stores them: trimmed, lower-case) and
/// phone digit patterns (`Subject::patterns`), for identifier-only matches.
fn identifiers(subject: &Subject) -> (Vec<String>, Vec<String>) {
    let emails = subject
        .identifiers
        .iter()
        .filter_map(|identifier| match identifier {
            Identifier::Email(value) => Some(value.trim().to_lowercase()),
            Identifier::Phone(_) => None,
        })
        .filter(|value| !value.is_empty())
        .collect();
    (emails, subject.patterns().1)
}
/// An address-only row matching `$2` (emails) or `$3` (phone digits).
const MATCH: &str = "person_id IS NULL AND address IS NOT NULL AND (address=ANY($2) OR regexp_replace(address,'[^0-9]','','g') ILIKE ANY($3))";

/// The addresses of the candidates the operator confirmed. A candidate is one
/// address, named by its first decision, suppression or check row.
async fn confirmed_addresses(
    client: &(impl GenericClient + Sync),
    tenant: i64,
    subject: &Subject,
) -> Result<Vec<String>> {
    let ids = [DECISION_RECORD, SUPPRESSION_RECORD, LOG_RECORD].map(|k| subject.confirmed_ids(k));
    if ids.iter().all(Vec::is_empty) {
        return Ok(Vec::new());
    }
    Ok(client
        .query(
            "SELECT address FROM core_consent_decisions WHERE tenant_id=$1 AND id=ANY($2) AND person_id IS NULL AND address IS NOT NULL \
             UNION SELECT address FROM core_consent_suppressions WHERE tenant_id=$1 AND id=ANY($3) AND person_id IS NULL AND address IS NOT NULL \
             UNION SELECT address FROM core_consent_log WHERE tenant_id=$1 AND id=ANY($4) AND person_id IS NULL AND address IS NOT NULL \
             ORDER BY 1",
            &[&tenant, &ids[0], &ids[1], &ids[2]],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

struct Store;
static STORE: Store = Store;
impl Handler for Store {
    fn locate<'a>(
        &'a self,
        client: &'a Client,
        scope: &'a Scope,
        subject: &'a Subject,
    ) -> HandlerFuture<'a, Located> {
        Box::pin(async move {
            let tenant = scope.tenant();
            let person = subject.person.get();
            let (emails, phones) = identifiers(subject);
            let counts = client
                .query_one(
                    &format!(
                        "SELECT \
                         (SELECT count(*) FROM core_consent_decisions WHERE tenant_id=$1 AND person_id=$4), \
                         (SELECT count(*) FROM core_consent_decisions WHERE tenant_id=$1 AND {MATCH}), \
                         (SELECT count(*) FROM core_consent_current WHERE tenant_id=$1 AND person_id=$4), \
                         (SELECT count(*) FROM core_consent_current WHERE tenant_id=$1 AND {MATCH}), \
                         (SELECT count(*) FROM core_consent_suppressions WHERE tenant_id=$1 AND person_id=$4), \
                         (SELECT count(*) FROM core_consent_suppressions WHERE tenant_id=$1 AND {MATCH}), \
                         (SELECT count(*) FROM core_consent_log WHERE tenant_id=$1 AND person_id=$4), \
                         (SELECT count(*) FROM core_consent_log WHERE tenant_id=$1 AND {MATCH})"
                    ),
                    &[&tenant, &emails, &phones, &person],
                )
                .await?;
            let kinds = [
                DECISION_RECORD,
                CURRENT_RECORD,
                SUPPRESSION_RECORD,
                LOG_RECORD,
            ]
            .iter()
            .enumerate()
            .map(|(i, kind)| KindCount {
                kind: (*kind).into(),
                by_person: counts.get(i * 2),
                by_identifier: counts.get(i * 2 + 1),
            })
            .collect();
            if emails.is_empty() && phones.is_empty() {
                return Ok(Located {
                    kinds,
                    ..Located::default()
                });
            }
            // One candidate per address, named by its first row; the summary
            // counts its rows without showing the address.
            let rows = client
                .query(
                    &format!(
                        "WITH m AS ( \
                         SELECT address,1 AS rank,id,recorded_at AS at FROM core_consent_decisions WHERE tenant_id=$1 AND {MATCH} \
                         UNION ALL SELECT address,2,id,suppressed_at FROM core_consent_suppressions WHERE tenant_id=$1 AND {MATCH} \
                         UNION ALL SELECT address,3,id,at FROM core_consent_log WHERE tenant_id=$1 AND {MATCH}) \
                         SELECT (array_agg(rank ORDER BY rank,id))[1],(array_agg(id ORDER BY rank,id))[1], \
                         count(*) FILTER (WHERE rank=1),count(*) FILTER (WHERE rank=2),count(*) FILTER (WHERE rank=3), \
                         to_char(min(at) AT TIME ZONE 'UTC','YYYY-MM-DD'),bool_or(strpos(address,'@')>0) \
                         FROM m GROUP BY address ORDER BY 1,2 LIMIT $4"
                    ),
                    &[&tenant, &emails, &phones, &(MAX_CANDIDATES as i64 + 1)],
                )
                .await?;
            let truncated = rows.len() > MAX_CANDIDATES;
            let candidates = rows
                .iter()
                .take(MAX_CANDIDATES)
                .map(|row| {
                    let kind = match row.get::<_, i32>(0) {
                        1 => DECISION_RECORD,
                        2 => SUPPRESSION_RECORD,
                        _ => LOG_RECORD,
                    };
                    let what = if row.get::<_, bool>(6) { "an email address" } else { "a phone number or handle" };
                    Candidate {
                        kind: kind.into(),
                        id: row.get::<_, i64>(1).to_string(),
                        summary: format!(
                            "Consent for {what}: {} decisions, {} suppressions, {} checks since {} UTC",
                            row.get::<_, i64>(2),
                            row.get::<_, i64>(3),
                            row.get::<_, i64>(4),
                            row.get::<_, String>(5)
                        ),
                    }
                })
                .collect();
            Ok(Located {
                kinds,
                candidates,
                truncated,
            })
        })
    }

    fn export<'a>(
        &'a self,
        client: &'a Client,
        scope: &'a Scope,
        subject: &'a Subject,
        grant: &'a Disclosure,
    ) -> HandlerFuture<'a, Exported> {
        Box::pin(async move {
            grant.check_subject(scope, subject)?;
            let tenant = scope.tenant();
            let addresses = confirmed_addresses(client, tenant, subject).await?;
            let params: [&(dyn ToSql + Sync); 3] = [&tenant, &subject.person.get(), &addresses];
            const OF: &str =
                "tenant_id=$1 AND (person_id=$2 OR (person_id IS NULL AND address=ANY($3)))";
            let all = |sql: String| async move {
                let row = client.query_one(&sql, &params).await?;
                Ok::<Value, Error>(row.get(0))
            };
            // Every field erasure clears is disclosed.
            let decisions = all(format!(
                "SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'company_id',company_id,'person_id',person_id,'address',address,\
                 'purpose',purpose,'channel',channel,'granted',granted,'source',source,'captured_on',captured_on,'evidence',evidence,\
                 'version',version,'recorded_at',recorded_at) ORDER BY id),'[]') FROM core_consent_decisions WHERE {OF}"
            ))
            .await?;
            let current = all(format!(
                "SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'company_id',company_id,'person_id',person_id,'address',address,\
                 'purpose',purpose,'channel',channel,'granted',granted,'version',version,'recorded_at',recorded_at) ORDER BY id),'[]') \
                 FROM core_consent_current WHERE {OF}"
            ))
            .await?;
            let suppressions = all(format!(
                "SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'company_id',company_id,'person_id',person_id,'address',address,\
                 'channel',channel,'reason',reason,'evidence',evidence,'suppressed_at',suppressed_at,'lifted_at',lifted_at,\
                 'lift_reason',lift_reason) ORDER BY id),'[]') FROM core_consent_suppressions WHERE {OF}"
            ))
            .await?;
            let checks = all(format!(
                "SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'company_id',company_id,'person_id',person_id,'address',address,\
                 'channel',channel,'purpose',purpose,'key',key,'at',at,'outcome',outcome,'caller',caller) ORDER BY id),'[]') \
                 FROM core_consent_log WHERE {OF}"
            ))
            .await?;
            Ok(Exported {
                records: json!({
                    "consent_decisions": decisions, "consent_current": current,
                    "consent_suppressions": suppressions, "consent_checks": checks,
                }),
                files: Vec::new(),
            })
        })
    }

    fn erase<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        subject: &'a Subject,
        grant: &'a Erasure,
        holds: &'a Holds,
    ) -> HandlerFuture<'a, Erased> {
        Box::pin(async move {
            grant.check_subject(scope, subject)?;
            let tenant = scope.tenant();
            let person = subject.person.get();
            let addresses = confirmed_addresses(tx.sql(), tenant, subject).await?;
            let mut erased = Erased::default();
            anonymise(
                tx, grant, holds, tenant, person, &addresses, &mut erased, DECISION_RECORD, &DECISION_FIELDS,
                "SELECT id,address,evidence FROM core_consent_decisions WHERE tenant_id=$1 AND (person_id=$2 OR (person_id IS NULL AND address=ANY($3))) ORDER BY id FOR UPDATE",
                "UPDATE core_consent_decisions SET address=NULL,evidence=NULL,erased_at=now() WHERE tenant_id=$1 AND id=ANY($2)",
            )
            .await?;
            anonymise(
                tx, grant, holds, tenant, person, &addresses, &mut erased, LOG_RECORD, &LOG_FIELDS,
                "SELECT id,address,key FROM core_consent_log WHERE tenant_id=$1 AND (person_id=$2 OR (person_id IS NULL AND address=ANY($3))) ORDER BY id FOR UPDATE",
                "UPDATE core_consent_log SET address=NULL,key='[erased]',erased_at=now() WHERE tenant_id=$1 AND id=ANY($2)",
            )
            .await?;
            current(tx, grant, holds, tenant, &addresses, &mut erased).await?;
            suppressions(tx, grant, holds, tenant, person, &addresses, &mut erased).await?;
            Ok(erased)
        })
    }
}

/// A field's value, `None` when it holds nothing personal any more.
fn live(row: &Row, index: usize) -> Option<String> {
    row.get::<_, Option<String>>(index).filter(|v| v != ERASED)
}
/// The change for `fields` (columns 1.. of `row`): `[erased]` where a value was.
fn change(kind: &str, id: i64, fields: &[&str], row: &Row) -> Change {
    let mut old = Map::new();
    let mut new = Map::new();
    for (index, field) in fields.iter().enumerate() {
        let value = live(row, index + 1);
        new.insert(
            (*field).into(),
            value.as_ref().map_or(Value::Null, |_| json!(ERASED)),
        );
        old.insert((*field).into(), json!(value));
    }
    Change {
        kind: kind.into(),
        id,
        old: Value::Object(old),
        new: Value::Object(new),
    }
}

/// Clears `fields` of the subject's rows of one kind that still hold a value,
/// keeping held rows. `select` reads `id` and the fields for `$1` tenant, `$2`
/// person and `$3` addresses; `update` clears them for `$1` tenant, `$2` ids.
#[allow(clippy::too_many_arguments)]
async fn anonymise(
    tx: &mut MutationTransaction<'_>,
    grant: &Erasure,
    holds: &Holds,
    tenant: i64,
    person: i64,
    addresses: &[String],
    erased: &mut Erased,
    kind: &'static str,
    fields: &'static [&'static str],
    select: &str,
    update: &'static str,
) -> Result<()> {
    let rows = tx
        .sql()
        .query(select, &[&tenant, &person, &addresses])
        .await?;
    let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
    let held = holds.records(grant, kind, &ids).await?;
    let mut due = Vec::new();
    let mut changes = Vec::new();
    for row in &rows {
        let id: i64 = row.get(0);
        if let Some(hold) = held.get(&id) {
            erased.kept.push(Kept::held(kind, id, *hold));
        } else if (1..=fields.len()).any(|i| live(row, i).is_some()) {
            due.push(id);
            changes.push(change(kind, id, fields, row));
        }
    }
    if due.is_empty() {
        return Ok(());
    }
    let count = due.len() as i64;
    erased.records.extend(due.iter().map(|&id| ErasedRecord {
        kind: kind.into(),
        id,
        person: None,
    }));
    tx.mutate(move |sql| {
        Box::pin(async move {
            sql.execute(update, &[&tenant, &due]).await?;
            Ok(((), changes))
        })
    })
    .await?;
    erased.count(kind, Action::Anonymised, count);
    Ok(())
}

/// Deletes the current state of the confirmed addresses: with its address
/// cleared it would answer nothing, and the decisions keep the bare fact. A
/// person's current state holds no personal field and stays.
async fn current(
    tx: &mut MutationTransaction<'_>,
    grant: &Erasure,
    holds: &Holds,
    tenant: i64,
    addresses: &[String],
    erased: &mut Erased,
) -> Result<()> {
    if addresses.is_empty() {
        return Ok(());
    }
    let rows = tx
        .sql()
        .query(
            "SELECT id,address FROM core_consent_current WHERE tenant_id=$1 AND person_id IS NULL AND address=ANY($2) ORDER BY id FOR UPDATE",
            &[&tenant, &addresses],
        )
        .await?;
    let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
    let held = holds.records(grant, CURRENT_RECORD, &ids).await?;
    let mut due = Vec::new();
    let mut changes = Vec::new();
    for row in &rows {
        let id: i64 = row.get(0);
        if let Some(hold) = held.get(&id) {
            erased.kept.push(Kept::held(CURRENT_RECORD, id, *hold));
            continue;
        }
        due.push(id);
        let mut gone = change(CURRENT_RECORD, id, &CURRENT_FIELDS, row);
        gone.new = Value::Null;
        changes.push(gone);
    }
    if due.is_empty() {
        return Ok(());
    }
    let count = due.len() as i64;
    erased.records.extend(due.iter().map(|&id| ErasedRecord {
        kind: CURRENT_RECORD.into(),
        id,
        person: None,
    }));
    tx.mutate(move |sql| {
        Box::pin(async move {
            sql.execute(
                "DELETE FROM core_consent_current WHERE tenant_id=$1 AND id=ANY($2)",
                &[&tenant, &due],
            )
            .await?;
            Ok(((), changes))
        })
    })
    .await?;
    erased.count(CURRENT_RECORD, Action::Deleted, count);
    Ok(())
}

/// Clears a suppression's evidence and lift reason and its address. An active
/// suppression of an address keeps the address's keyed hash, so the check
/// still refuses it; a lifted one keeps none.
async fn suppressions(
    tx: &mut MutationTransaction<'_>,
    grant: &Erasure,
    holds: &Holds,
    tenant: i64,
    person: i64,
    addresses: &[String],
    erased: &mut Erased,
) -> Result<()> {
    let rows = tx
        .sql()
        .query(
            "SELECT id,address,evidence,lift_reason,lifted_at IS NULL FROM core_consent_suppressions \
             WHERE tenant_id=$1 AND (person_id=$2 OR (person_id IS NULL AND address=ANY($3))) ORDER BY id FOR UPDATE",
            &[&tenant, &person, &addresses],
        )
        .await?;
    let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
    let held = holds.records(grant, SUPPRESSION_RECORD, &ids).await?;
    let mut due = Vec::new();
    for row in &rows {
        let id: i64 = row.get(0);
        if let Some(hold) = held.get(&id) {
            erased.kept.push(Kept::held(SUPPRESSION_RECORD, id, *hold));
        } else if (1..=SUPPRESSION_FIELDS.len()).any(|i| live(row, i).is_some()) {
            due.push(row);
        }
    }
    if due.is_empty() {
        return Ok(());
    }
    let key = if due
        .iter()
        .any(|row| row.get::<_, bool>(4) && live(row, 1).is_some())
    {
        Some(ensure_key(tx, settings()?, tenant).await?)
    } else {
        None
    };
    let mut writes = Vec::new();
    let mut changes = Vec::new();
    for row in due {
        let id: i64 = row.get(0);
        let hash = match (&key, live(row, 1), row.get::<_, bool>(4)) {
            (Some(key), Some(address), true) => Some(digest(key, &address)?),
            _ => None,
        };
        writes.push((id, hash));
        changes.push(change(SUPPRESSION_RECORD, id, &SUPPRESSION_FIELDS, row));
    }
    let count = writes.len() as i64;
    erased
        .records
        .extend(writes.iter().map(|(id, _)| ErasedRecord {
            kind: SUPPRESSION_RECORD.into(),
            id: *id,
            person: None,
        }));
    tx.mutate(move |sql| {
        Box::pin(async move {
            for (id, hash) in &writes {
                sql.execute(
                    "UPDATE core_consent_suppressions SET address_hash=COALESCE($3,address_hash),address=NULL,evidence=NULL,\
                     lift_reason=CASE WHEN lift_reason IS NULL THEN NULL ELSE '[erased]' END,erased_at=now() WHERE tenant_id=$1 AND id=$2",
                    &[&tenant, id, hash],
                )
                .await?;
            }
            Ok(((), changes))
        })
    })
    .await?;
    erased.count(SUPPRESSION_RECORD, Action::Anonymised, count);
    Ok(())
}

inventory::submit! { Contribution::new(PERSONAL_DATA, "core/consent.consent",
Provenance { module: crate::MODULE, source: file!(), line: line!() },
&PersonalData { label: "Communication consent", kinds: &[DECISION_RECORD, CURRENT_RECORD, SUPPRESSION_RECORD, LOG_RECORD], person_subjects: &[], handler: &STORE }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/consent.decision",
Provenance { module: crate::MODULE, source: file!(), line: line!() },
&RecordKind { kind: DECISION_RECORD, fields: &DECISION_FIELDS, subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/consent.current",
Provenance { module: crate::MODULE, source: file!(), line: line!() },
&RecordKind { kind: CURRENT_RECORD, fields: &CURRENT_FIELDS, subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/consent.suppression",
Provenance { module: crate::MODULE, source: file!(), line: line!() },
&RecordKind { kind: SUPPRESSION_RECORD, fields: &SUPPRESSION_FIELDS, subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/consent.log",
Provenance { module: crate::MODULE, source: file!(), line: line!() },
&RecordKind { kind: LOG_RECORD, fields: &LOG_FIELDS, subject_identifier: false, person: false }) }
