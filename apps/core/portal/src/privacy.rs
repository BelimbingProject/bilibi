//! Portal registration personal data (ADR 05). A request becomes an email
//! candidate only after its confirmation link was used. The operator confirms
//! an unlinked candidate before access, portability or erasure acts on it;
//! approved requests are already linked to their Person through the invitation.
//! Legal holds keep either the request or its linked Person. The shared privacy
//! sweep purges expired unconfirmed requests and decided requests after the
//! primary company's configured period.
use bilibi_database::{Change, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_personal_data::{
    Action, Candidate, Disclosure, Erased, ErasedRecord, Erasure, Exported, Handler, HandlerFuture,
    Holds, Identifier, Kept, KindCount, Located, PersonalData, RecordKind, Subject, MAX_CANDIDATES,
    PERSONAL_DATA, RECORD_KINDS,
};
use bilibi_privacy::{Companies, Sweeper, Swept};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio_postgres::{Client, GenericClient};

pub(crate) const KIND: &str = "portal.registration";
const FIELDS: [&str; 3] = ["name", "email", "company_name"];

fn emails(subject: &Subject) -> Vec<String> {
    subject
        .identifiers
        .iter()
        .filter_map(|identifier| match identifier {
            Identifier::Email(value) => Some(value.trim().to_lowercase()),
            Identifier::Phone(_) => None,
        })
        .filter(|value| !value.is_empty())
        .collect()
}

async fn subject_ids(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    subject: &Subject,
) -> bilibi_database::Result<Vec<i64>> {
    let confirmed = subject.confirmed_ids(KIND);
    Ok(client
        .query(
            "SELECT DISTINCT r.id FROM customer_registrations r \
             LEFT JOIN customer_invitations i ON i.tenant_id=r.tenant_id AND i.id=r.invitation_id \
             WHERE r.tenant_id=$1 AND (i.person_id=$2 OR (r.verified_at IS NOT NULL AND r.id=ANY($3))) ORDER BY r.id",
            &[&scope.tenant(), &subject.person.get(), &confirmed],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

struct Registrations;
static REGISTRATIONS: Registrations = Registrations;
impl Handler for Registrations {
    fn locate<'a>(
        &'a self,
        client: &'a Client,
        scope: &'a Scope,
        subject: &'a Subject,
    ) -> HandlerFuture<'a, Located> {
        Box::pin(async move {
            let emails = emails(subject);
            let counts = client
                .query_one(
                    "SELECT count(*) FILTER (WHERE i.person_id=$2), \
                     count(*) FILTER (WHERE r.verified_at IS NOT NULL AND r.email=ANY($3) AND i.person_id IS DISTINCT FROM $2) \
                     FROM customer_registrations r LEFT JOIN customer_invitations i \
                     ON i.tenant_id=r.tenant_id AND i.id=r.invitation_id WHERE r.tenant_id=$1",
                    &[&scope.tenant(), &subject.person.get(), &emails],
                )
                .await?;
            let rows = if emails.is_empty() {
                Vec::new()
            } else {
                client
                    .query(
                        "SELECT r.id,CASE WHEN r.decision IS NOT NULL THEN r.decision ELSE 'waiting' END, \
                         to_char(r.created_at AT TIME ZONE 'UTC','YYYY-MM-DD HH24:MI') \
                         FROM customer_registrations r LEFT JOIN customer_invitations i \
                         ON i.tenant_id=r.tenant_id AND i.id=r.invitation_id \
                         WHERE r.tenant_id=$1 AND r.verified_at IS NOT NULL AND r.email=ANY($2) \
                         AND i.person_id IS NULL ORDER BY r.id LIMIT $3",
                        &[&scope.tenant(), &emails, &(MAX_CANDIDATES as i64 + 1)],
                    )
                    .await?
            };
            let truncated = rows.len() > MAX_CANDIDATES;
            let candidates = rows
                .iter()
                .take(MAX_CANDIDATES)
                .map(|row| Candidate {
                    kind: KIND.into(),
                    id: row.get::<_, i64>(0).to_string(),
                    summary: format!(
                        "Confirmed customer account request {}: {}, {} UTC",
                        row.get::<_, i64>(0),
                        row.get::<_, String>(1),
                        row.get::<_, String>(2)
                    ),
                })
                .collect();
            Ok(Located {
                kinds: vec![KindCount {
                    kind: KIND.into(),
                    by_person: counts.get(0),
                    by_identifier: counts.get(1),
                }],
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
            let ids = subject_ids(client, scope, subject).await?;
            if ids.is_empty() {
                return Ok(Exported::default());
            }
            let rows = client
                .query(
                    "SELECT id,name,email,company_name,created_at::text,verified_at::text,decision,decided_at::text,invitation_id \
                     FROM customer_registrations WHERE tenant_id=$1 AND id=ANY($2) ORDER BY id",
                    &[&scope.tenant(), &ids],
                )
                .await?;
            let registrations: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id": row.get::<_, i64>(0), "name": row.get::<_, String>(1),
                        "email": row.get::<_, String>(2), "company_name": row.get::<_, Option<String>>(3),
                        "created_at": row.get::<_, String>(4), "verified_at": row.get::<_, Option<String>>(5),
                        "decision": row.get::<_, Option<String>>(6), "decided_at": row.get::<_, Option<String>>(7),
                        "invitation_id": row.get::<_, Option<i64>>(8),
                    })
                })
                .collect();
            Ok(Exported {
                records: json!({"customer_account_requests": registrations}),
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
            let ids = subject_ids(tx.sql(), scope, subject).await?;
            let rows = tx
                .sql()
                .query(
                    "SELECT r.id,i.person_id FROM customer_registrations r LEFT JOIN customer_invitations i \
                     ON i.tenant_id=r.tenant_id AND i.id=r.invitation_id \
                     WHERE r.tenant_id=$1 AND r.id=ANY($2) ORDER BY r.id FOR UPDATE OF r",
                    &[&tenant, &ids],
                )
                .await?;
            erase_rows(tx, grant, holds, tenant, &rows).await
        })
    }
}

async fn erase_rows(
    tx: &mut MutationTransaction<'_>,
    grant: &Erasure,
    holds: &Holds,
    tenant: i64,
    rows: &[tokio_postgres::Row],
) -> bilibi_database::Result<Erased> {
    let keys: Vec<(i64, Option<i64>)> = rows.iter().map(|row| (row.get(0), row.get(1))).collect();
    let ids: Vec<i64> = keys.iter().map(|(id, _)| *id).collect();
    let people: Vec<i64> = keys.iter().filter_map(|(_, person)| *person).collect();
    let record_holds = holds.records(grant, KIND, &ids).await?;
    let person_holds = holds.persons(grant, &people).await?;
    let mut erased = Erased::default();
    let mut due = Vec::new();
    let mut persons = BTreeMap::new();
    for (id, person) in keys {
        let hold = record_holds
            .get(&id)
            .copied()
            .or_else(|| person.and_then(|person| person_holds.get(&person).copied()));
        if let Some(hold) = hold {
            erased.kept.push(Kept::held(KIND, id, hold));
        } else {
            due.push(id);
            persons.insert(id, person);
        }
    }
    if due.is_empty() {
        return Ok(erased);
    }
    let deleted: Vec<i64> = tx
        .mutate(move |sql| {
            Box::pin(async move {
                let rows = sql
                    .query(
                        "DELETE FROM customer_registrations WHERE tenant_id=$1 AND id=ANY($2) RETURNING id",
                        &[&tenant, &due],
                    )
                    .await?;
                let changes = rows
                    .iter()
                    .map(|row| Change {
                        kind: KIND.into(),
                        id: row.get(0),
                        old: json!({"status": "stored"}),
                        new: Value::Null,
                    })
                    .collect();
                Ok((rows.iter().map(|row| row.get(0)).collect(), changes))
            })
        })
        .await?;
    erased.count(KIND, Action::Deleted, deleted.len() as i64);
    erased
        .records
        .extend(deleted.into_iter().map(|id| ErasedRecord {
            kind: KIND.into(),
            id,
            person: persons.get(&id).copied().flatten(),
        }));
    Ok(erased)
}

struct RegistrationRetention;
static REGISTRATION_RETENTION: RegistrationRetention = RegistrationRetention;
impl Sweeper for RegistrationRetention {
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
            let (_, primary, _) = companies.parameters();
            if !primary {
                return Ok(Swept::default());
            }
            let tenant = scope.tenant();
            let rows = tx
                .sql()
                .query(
                    "SELECT r.id,i.person_id FROM customer_registrations r LEFT JOIN customer_invitations i \
                     ON i.tenant_id=r.tenant_id AND i.id=r.invitation_id \
                     WHERE r.tenant_id=$1 AND r.id>$2 AND ( \
                       (r.verified_at IS NULL AND r.decision IS NULL AND r.expires_at<$3::text::timestamptz) OR \
                       (r.decision IS NOT NULL AND r.decided_at<$3::text::timestamptz)) \
                     ORDER BY r.id LIMIT $4 FOR UPDATE OF r",
                    &[&tenant, &after, &cutoff, &limit],
                )
                .await?;
            let next = (rows.len() as i64 == limit)
                .then(|| rows.last().map(|row| row.get::<_, i64>(0)))
                .flatten();
            let erased = erase_rows(tx, grant, holds, tenant, &rows).await?;
            Ok(Swept {
                acted: erased.actions.iter().map(|action| action.count).sum(),
                kept: erased.kept.len() as i64,
                records: erased.records,
                next,
            })
        })
    }
}

inventory::submit! { Contribution::new(PERSONAL_DATA, "core/portal.registrations",
Provenance { module: "core/portal", source: file!(), line: line!() },
&PersonalData { label: "Customer account requests", kinds: &[KIND], person_subjects: &[], handler: &REGISTRATIONS }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/portal.registration",
Provenance { module: "core/portal", source: file!(), line: line!() },
&RecordKind { kind: KIND, fields: &FIELDS, subject_identifier: false, person: false }) }

bilibi_privacy::record_class! {
    module: "core/portal", key: "core/portal.registration_requests", label: "Customer account requests",
    clock_start: "when an unconfirmed request expired or staff decided it", action: Purge,
    business_value: false, floor_days: None, sweeper: &REGISTRATION_RETENTION,
}
