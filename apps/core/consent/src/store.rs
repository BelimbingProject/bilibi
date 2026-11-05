//! `core_consent_*` SQL. Every statement binds the tenant; every write runs in
//! the caller's `MutationTransaction` and returns its `Change`s. Checks and
//! locks run before a write, so a refusal never poisons the caller's
//! transaction.
use crate::model::{Answer, Capture, Channel, Contact, NewAddress, Reason, Refusal, Source};
use crate::{
    Current, Decision, DecisionInput, Question, State, Suppression, SuppressionFilter,
    SuppressionInput, SuppressionPage, CURRENT_RECORD, DECISION_RECORD, LOG_RECORD,
    SUPPRESSION_RECORD,
};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_party_ids::PersonId;
use bilibi_tenancy::{ReadClient, Scope};
use jiff::Timestamp;
use serde_json::{json, Value};
use std::time::SystemTime;
use tokio_postgres::{types::ToSql, Row, Transaction};

fn time(value: SystemTime) -> Result<Timestamp> {
    Timestamp::try_from(value).map_err(|_| Error::Internal)
}
fn person(value: Option<i64>) -> Result<Option<PersonId>> {
    value
        .map(|id| PersonId::new(id).map_err(|_| Error::Internal))
        .transpose()
}
fn code<T>(row: &Row, column: &str, parse: fn(&str) -> Option<T>) -> Result<T> {
    parse(row.get::<_, &str>(column)).ok_or(Error::Internal)
}
fn contact(row: &Row) -> Result<Contact> {
    Ok(Contact {
        person: person(row.get("person_id"))?,
        address: row.get("address"),
    })
}
/// Who acts: their account, or the system principal's name.
fn actor(scope: &Scope) -> (Option<i64>, Option<String>) {
    match scope.principal() {
        Some(name) => (None, Some(name.to_owned())),
        None => (Some(scope.user()), None),
    }
}

/// Holds a transaction-scoped lock on one key of this module, so concurrent
/// writes to the same decision key or suppression take turns.
async fn lock(sql: &Transaction<'_>, tenant: i64, key: &str) -> Result<()> {
    sql.execute(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, $2))",
        &[&format!("core/consent:{key}"), &tenant],
    )
    .await?;
    Ok(())
}
fn subject_key(contact: &Contact) -> String {
    match (&contact.person, &contact.address) {
        (Some(person), _) => format!("person:{}", person.get()),
        (None, Some(address)) => format!("address:{address}"),
        (None, None) => String::new(),
    }
}

const DECISION_COLUMNS: &str = "id,company_id,person_id,address,purpose,channel,granted,source,captured_on,evidence,version,recorded_at,recorded_by,recorded_principal";
fn decision(row: &Row) -> Result<Decision> {
    Ok(Decision {
        id: row.get("id"),
        company_id: row.get("company_id"),
        contact: contact(row)?,
        purpose: row.get("purpose"),
        channel: code(row, "channel", Channel::parse)?,
        granted: row.get("granted"),
        source: code(row, "source", Source::parse)?,
        captured_on: code(row, "captured_on", Capture::parse)?,
        evidence: row.get("evidence"),
        version: row.get("version"),
        recorded_at: time(row.get("recorded_at"))?,
        recorded_by: row.get("recorded_by"),
        recorded_principal: row.get("recorded_principal"),
    })
}
fn decision_values(d: &Decision) -> Value {
    json!({
        "company_id": d.company_id,
        "person_id": d.contact.person.map(|p| p.get()),
        "address": d.contact.address,
        "purpose": d.purpose,
        "channel": d.channel.code(),
        "granted": d.granted,
        "source": d.source.code(),
        "captured_on": d.captured_on.code(),
        "evidence": d.evidence,
        "version": d.version,
    })
}

const CURRENT_COLUMNS: &str =
    "id,company_id,person_id,address,purpose,channel,granted,source,captured_on,version,recorded_at";
fn current(row: &Row) -> Result<Current> {
    Ok(Current {
        id: row.get("id"),
        company_id: row.get("company_id"),
        contact: contact(row)?,
        purpose: row.get("purpose"),
        channel: code(row, "channel", Channel::parse)?,
        granted: row.get("granted"),
        source: code(row, "source", Source::parse)?,
        captured_on: code(row, "captured_on", Capture::parse)?,
        version: row.get("version"),
        recorded_at: time(row.get("recorded_at"))?,
    })
}
fn current_values(c: &Current) -> Value {
    json!({
        "company_id": c.company_id,
        "person_id": c.contact.person.map(|p| p.get()),
        "address": c.contact.address,
        "purpose": c.purpose,
        "channel": c.channel.code(),
        "granted": c.granted,
        "source": c.source.code(),
        "captured_on": c.captured_on.code(),
        "version": c.version,
    })
}

const SUPPRESSION_COLUMNS: &str = "id,company_id,person_id,address,channel,reason,evidence,suppressed_at,lifted_at,lifted_by,lifted_principal,lift_reason";
fn suppression(row: &Row) -> Result<Suppression> {
    Ok(Suppression {
        id: row.get("id"),
        company_id: row.get("company_id"),
        contact: contact(row)?,
        channel: code(row, "channel", Channel::parse)?,
        reason: code(row, "reason", Reason::parse)?,
        evidence: row.get("evidence"),
        suppressed_at: time(row.get("suppressed_at"))?,
        lifted_at: row
            .get::<_, Option<SystemTime>>("lifted_at")
            .map(time)
            .transpose()?,
        lifted_by: row.get("lifted_by"),
        lifted_principal: row.get("lifted_principal"),
        lift_reason: row.get("lift_reason"),
    })
}
fn suppression_values(s: &Suppression) -> Value {
    json!({
        "company_id": s.company_id,
        "person_id": s.contact.person.map(|p| p.get()),
        "address": s.contact.address,
        "channel": s.channel.code(),
        "reason": s.reason.code(),
        "evidence": s.evidence,
        "lifted": s.lifted_at.is_some(),
        "lift_reason": s.lift_reason,
    })
}

/// The answer `check` gives, from the suppressions and the current state.
/// Steps 1, 2 and 4 of the README's order; the first refusal wins.
pub(crate) async fn answer(
    client: &dyn ReadClient,
    scope: &Scope,
    question: &Question,
    default: NewAddress,
) -> Result<Answer> {
    let tenant = scope.tenant();
    let person = question.contact.person.map(|p| p.get());
    let address = question.contact.address.as_deref();
    let channel = question.channel.code();
    let reasons = client
        .query(
            "SELECT DISTINCT reason FROM core_consent_suppressions WHERE tenant_id=$1 AND company_id=$2 AND channel=$3 AND lifted_at IS NULL AND (person_id=$4 OR address=$5)",
            &[&tenant, &question.company_id, &channel, &person, &address],
        )
        .await?;
    let reasons: Vec<Reason> = reasons
        .iter()
        .filter_map(|row| Reason::parse(row.get(0)))
        .collect();
    if !reasons.is_empty() {
        return Ok(Answer::Refused(Refusal::Suppressed));
    }
    // The decision for the channel beats one for any channel; between the
    // person's and the address's, the latest recorded wins, and a tie
    // refuses.
    let granted = client
        .query_opt(
            "SELECT granted FROM core_consent_current WHERE tenant_id=$1 AND company_id=$2 AND purpose=$3 AND channel IN ($4,'any') AND (person_id=$5 OR address=$6) ORDER BY channel='any', recorded_at DESC, granted LIMIT 1",
            &[&tenant, &question.company_id, &question.purpose, &channel, &person, &address],
        )
        .await?
        .map(|row| row.get::<_, bool>(0));
    Ok(match (granted, default) {
        (Some(true), _) | (None, NewAddress::OptedIn) => Answer::Allowed,
        (Some(false), _) => Answer::Refused(Refusal::Withdrawn),
        (None, NewAddress::OptedOut) => Answer::Refused(Refusal::NoConsent),
    })
}

/// Answers `question` at call time and logs the fresh answer under `key`.
pub(crate) async fn log(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    question: &Question,
    key: &str,
    caller: &'static str,
    default: NewAddress,
) -> Result<Answer> {
    let tenant = scope.tenant();
    let company_id = question.company_id;
    let answer = answer(tx.sql(), scope, question, default).await?;
    let person = question.contact.person.map(|p| p.get());
    let address = question.contact.address.clone();
    let channel = question.channel.code();
    let purpose = question.purpose.clone();
    let at = SystemTime::now();
    let key = key.to_owned();
    tx.mutate(move |sql| {
        Box::pin(async move {
            let row = sql
                .query_one(
                    "INSERT INTO core_consent_log (tenant_id,company_id,person_id,address,channel,purpose,key,at,outcome,caller) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id",
                    &[&tenant, &company_id, &person, &address, &channel, &purpose, &key, &at, &answer.code(), &caller],
                )
                .await?;
            let change = Change {
                kind: LOG_RECORD.into(),
                id: row.get(0),
                old: json!({}),
                new: json!({
                    "company_id": company_id, "person_id": person, "address": address, "channel": channel,
                    "purpose": purpose, "key": key, "outcome": answer.code(), "caller": caller,
                }),
            };
            Ok((answer, vec![change]))
        })
    })
    .await
}

/// The latest decision per purpose and channel for a person.
pub(crate) async fn current_for_person(
    client: &dyn ReadClient,
    scope: &Scope,
    company_id: i64,
    person: PersonId,
) -> Result<Vec<Current>> {
    client
        .query(
            &format!("SELECT {CURRENT_COLUMNS} FROM core_consent_current WHERE tenant_id=$1 AND company_id=$2 AND person_id=$3 ORDER BY purpose, channel"),
            &[&scope.tenant(), &company_id, &person.get()],
        )
        .await?
        .iter()
        .map(current)
        .collect()
}

/// Every version of a person's decisions, oldest first.
pub(crate) async fn history(
    client: &dyn ReadClient,
    scope: &Scope,
    company_id: i64,
    person: PersonId,
    purpose: Option<&str>,
    channel: Option<Channel>,
) -> Result<Vec<Decision>> {
    let channel = channel.map(Channel::code);
    client
        .query(
            &format!("SELECT {DECISION_COLUMNS} FROM core_consent_decisions WHERE tenant_id=$1 AND company_id=$2 AND person_id=$3 AND ($4::text IS NULL OR purpose=$4) AND ($5::text IS NULL OR channel=$5) ORDER BY recorded_at, id"),
            &[&scope.tenant(), &company_id, &person.get(), &purpose, &channel],
        )
        .await?
        .iter()
        .map(decision)
        .collect()
}

/// Writes the next version of the decision for its key and moves the
/// current state to it.
pub(crate) async fn record(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    input: &DecisionInput,
) -> Result<Decision> {
    let tenant = scope.tenant();
    let company_id = input.company_id;
    let subject = subject_key(&input.contact);
    lock(
        tx.sql(),
        tenant,
        &format!(
            "decision:{company_id}:{subject}:{}:{}",
            input.purpose,
            input.channel.code()
        ),
    )
    .await?;
    let person = input.contact.person.map(|p| p.get());
    let address = input.contact.address.clone();
    let before = tx
        .sql()
        .query_opt(
            &format!("SELECT {CURRENT_COLUMNS} FROM core_consent_current WHERE tenant_id=$1 AND company_id=$2 AND purpose=$3 AND channel=$4 AND (person_id=$5 OR address=$6) FOR UPDATE"),
            &[&tenant, &company_id, &input.purpose, &input.channel.code(), &person, &address],
        )
        .await?
        .as_ref()
        .map(current)
        .transpose()?;
    let version = before.as_ref().map_or(1, |c| c.version + 1);
    let (recorded_by, recorded_principal) = actor(scope);
    let input = input.clone();
    // `recorded_at` is when this row is written, not when the caller's
    // transaction began (`now()`). `check` lets the later of a person's and
    // an address's decisions win, so a decision written after another must
    // sort after it even if its transaction began first; written after the
    // key's lock, one key's versions also sort in order.
    tx.mutate(move |sql| {
        Box::pin(async move {
            let row = sql
                .query_one(
                    &format!("INSERT INTO core_consent_decisions (tenant_id,company_id,person_id,address,purpose,channel,granted,source,captured_on,evidence,version,recorded_by,recorded_principal,recorded_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,clock_timestamp()) RETURNING {DECISION_COLUMNS}"),
                    &[&tenant, &company_id, &person, &address, &input.purpose, &input.channel.code(), &input.granted, &input.source.code(), &input.captured_on.code(), &input.evidence, &version, &recorded_by, &recorded_principal],
                )
                .await?;
            let decision = decision(&row)?;
            let at = SystemTime::from(decision.recorded_at);
            let row = match &before {
                Some(old) => sql
                    .query_one(
                        &format!("UPDATE core_consent_current SET granted=$3,source=$4,captured_on=$5,version=$6,recorded_at=$7 WHERE tenant_id=$1 AND id=$2 RETURNING {CURRENT_COLUMNS}"),
                        &[&tenant, &old.id, &input.granted, &input.source.code(), &input.captured_on.code(), &version, &at],
                    )
                    .await?,
                None => sql
                    .query_one(
                        &format!("INSERT INTO core_consent_current (tenant_id,company_id,person_id,address,purpose,channel,granted,source,captured_on,version,recorded_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING {CURRENT_COLUMNS}"),
                        &[&tenant, &company_id, &person, &address, &input.purpose, &input.channel.code(), &input.granted, &input.source.code(), &input.captured_on.code(), &version, &at],
                    )
                    .await?,
            };
            let after = current(&row)?;
            let changes = vec![
                Change {
                    kind: DECISION_RECORD.into(),
                    id: decision.id,
                    old: json!({}),
                    new: decision_values(&decision),
                },
                Change {
                    kind: CURRENT_RECORD.into(),
                    id: after.id,
                    old: before.as_ref().map_or_else(|| json!({}), current_values),
                    new: current_values(&after),
                },
            ];
            Ok((decision, changes))
        })
    })
    .await
}

/// The active suppression with the same channel, contact and reason, or a
/// new one. The second item says whether this call created it.
pub(crate) async fn suppress(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    input: &SuppressionInput,
) -> Result<(Suppression, bool)> {
    let tenant = scope.tenant();
    let company_id = input.company_id;
    let subject = subject_key(&input.contact);
    lock(
        tx.sql(),
        tenant,
        &format!(
            "suppression:{company_id}:{subject}:{}:{}",
            input.channel.code(),
            input.reason.code()
        ),
    )
    .await?;
    let person = input.contact.person.map(|p| p.get());
    let address = input.contact.address.clone();
    let channel = input.channel.code();
    let reason = input.reason.code();
    if let Some(row) = tx
        .sql()
        .query_opt(
            &format!("SELECT {SUPPRESSION_COLUMNS} FROM core_consent_suppressions WHERE tenant_id=$1 AND company_id=$2 AND channel=$3 AND reason=$4 AND lifted_at IS NULL AND (person_id=$5 OR address=$6)"),
            &[&tenant, &company_id, &channel, &reason, &person, &address],
        )
        .await?
    {
        return Ok((suppression(&row)?, false));
    }
    let evidence = input.evidence.clone();
    let created = tx
        .mutate(move |sql| {
            Box::pin(async move {
                let row = sql
                    .query_one(
                        &format!("INSERT INTO core_consent_suppressions (tenant_id,company_id,person_id,address,channel,reason,evidence) VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING {SUPPRESSION_COLUMNS}"),
                        &[&tenant, &company_id, &person, &address, &channel, &reason, &evidence],
                    )
                    .await?;
                let created = suppression(&row)?;
                let change = Change {
                    kind: SUPPRESSION_RECORD.into(),
                    id: created.id,
                    old: json!({}),
                    new: suppression_values(&created),
                };
                Ok((created, vec![change]))
            })
        })
        .await?;
    Ok((created, true))
}

pub(crate) async fn suppression_by_id(
    client: &dyn ReadClient,
    scope: &Scope,
    id: i64,
    lock: bool,
) -> Result<Suppression> {
    let lock = if lock { " FOR UPDATE" } else { "" };
    let row = client
        .query_opt(
            &format!("SELECT {SUPPRESSION_COLUMNS} FROM core_consent_suppressions WHERE tenant_id=$1 AND id=$2{lock}"),
            &[&scope.tenant(), &id],
        )
        .await?
        .ok_or(Error::NotFound)?;
    suppression(&row)
}

/// Lifts an active suppression the caller has locked and read.
pub(crate) async fn lift(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    before: Suppression,
    reason: &str,
) -> Result<Suppression> {
    let tenant = scope.tenant();
    let (lifted_by, lifted_principal) = actor(scope);
    let reason = reason.to_owned();
    tx.mutate(move |sql| {
        Box::pin(async move {
            let row = sql
                .query_opt(
                    &format!("UPDATE core_consent_suppressions SET lifted_at=now(),lifted_by=$3,lifted_principal=$4,lift_reason=$5 WHERE tenant_id=$1 AND id=$2 AND lifted_at IS NULL RETURNING {SUPPRESSION_COLUMNS}"),
                    &[&tenant, &before.id, &lifted_by, &lifted_principal, &reason],
                )
                .await?
                .ok_or(Error::Conflict)?;
            let after = suppression(&row)?;
            let change = Change {
                kind: SUPPRESSION_RECORD.into(),
                id: after.id,
                old: suppression_values(&before),
                new: suppression_values(&after),
            };
            Ok((after, vec![change]))
        })
    })
    .await
}

/// A person's active person-level suppressions, newest first.
pub(crate) async fn active_for_person(
    client: &dyn ReadClient,
    scope: &Scope,
    company_id: i64,
    person: PersonId,
) -> Result<Vec<Suppression>> {
    client
        .query(
            &format!("SELECT {SUPPRESSION_COLUMNS} FROM core_consent_suppressions WHERE tenant_id=$1 AND company_id=$2 AND person_id=$3 AND lifted_at IS NULL ORDER BY suppressed_at DESC, id DESC"),
            &[&scope.tenant(), &company_id, &person.get()],
        )
        .await?
        .iter()
        .map(suppression)
        .collect()
}

/// A page of suppressions, newest first.
pub(crate) async fn suppressions(
    client: &dyn ReadClient,
    scope: &Scope,
    company_id: i64,
    filter: &SuppressionFilter,
) -> Result<SuppressionPage> {
    let search = filter.search.trim().to_lowercase();
    let pattern = format!(
        "%{}%",
        search
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let pattern = (!search.is_empty()).then_some(pattern);
    let channel = filter.channel.map(Channel::code);
    let reason = filter.reason.map(Reason::code);
    let state = match filter.state {
        State::Active => "active",
        State::Lifted => "lifted",
        State::All => "all",
    };
    let page_size = filter.page_size.clamp(1, 100);
    let offset = (filter.page.max(1) - 1) * page_size;
    const WHERE: &str = "tenant_id=$1 AND company_id=$2 AND ($3::text IS NULL OR lower(address) LIKE $3 OR person_id::text=$7) AND ($4::text IS NULL OR channel=$4) AND ($5::text IS NULL OR reason=$5) AND ($6='all' OR ($6='active')=(lifted_at IS NULL))";
    let search = search.as_str();
    let params: [&(dyn ToSql + Sync); 9] = [
        &scope.tenant(),
        &company_id,
        &pattern,
        &channel,
        &reason,
        &state,
        &search,
        &page_size,
        &offset,
    ];
    let total: i64 = client
        .query_opt(
            &format!("SELECT count(*) FROM core_consent_suppressions WHERE {WHERE}"),
            &params[..7],
        )
        .await?
        .map_or(0, |row| row.get(0));
    let rows = client
        .query(
            &format!("SELECT {SUPPRESSION_COLUMNS} FROM core_consent_suppressions WHERE {WHERE} ORDER BY suppressed_at DESC, id DESC LIMIT $8 OFFSET $9"),
            &params,
        )
        .await?;
    Ok(SuppressionPage {
        entries: rows.iter().map(suppression).collect::<Result<_>>()?,
        total,
    })
}
