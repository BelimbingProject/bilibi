//! Invitations always belong to a Person. Email equality is not authority to
//! disclose an invitation linked to somebody else. Keep the lifecycle row and
//! registration link; erase its copied values and close any live bearer link.
use bilibi_database::{Change, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_personal_data::{
    Action, Disclosure, Erased, ErasedRecord, Erasure, Exported, Handler, HandlerFuture, Holds,
    Kept, KindCount, Located, PersonalData, RecordKind, Subject, ERASED, PERSONAL_DATA,
    RECORD_KINDS,
};
use bilibi_privacy::{Companies, Sweeper, Swept};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::{Client, Row};

const KIND: &str = "portal.invitation";
const FIELDS: [&str; 2] = ["name", "email"];
struct Invitations;
static INVITATIONS: Invitations = Invitations;

impl Handler for Invitations {
    fn locate<'a>(
        &'a self,
        client: &'a Client,
        scope: &'a Scope,
        subject: &'a Subject,
        _after: Option<&'a str>,
    ) -> HandlerFuture<'a, Located> {
        Box::pin(async move {
            let emails = crate::privacy::emails(subject);
            let counts = client
                .query_one(
                    "SELECT count(*) FILTER (WHERE person_id=$2), \
                     count(*) FILTER (WHERE person_id<>$2 AND email=ANY($3)) \
                     FROM customer_invitations WHERE tenant_id=$1",
                    &[&scope.tenant(), &subject.person.get(), &emails],
                )
                .await?;
            Ok(Located {
                kinds: vec![KindCount {
                    kind: KIND.into(),
                    by_person: counts.get(0),
                    by_identifier: counts.get(1),
                }],
                ..Located::default()
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
            let rows = client.query(
                "SELECT id,name,email,person_id,affiliation_id,organisation_id,created_at::text,expires_at::text,accepted_at::text,revoked_at::text,user_id,delivery,delivery_at::text \
                 FROM customer_invitations WHERE tenant_id=$1 AND person_id=$2 ORDER BY id",
                &[&scope.tenant(), &subject.person.get()],
            ).await?;
            let invitations: Vec<Value> = rows.iter().map(|row| json!({
                "id": row.get::<_, i64>(0), "name": row.get::<_, String>(1),
                "email": row.get::<_, String>(2), "person_id": row.get::<_, i64>(3),
                "affiliation_id": row.get::<_, Option<i64>>(4), "company_id": row.get::<_, Option<i64>>(5),
                "created_at": row.get::<_, String>(6), "expires_at": row.get::<_, String>(7),
                "accepted_at": row.get::<_, Option<String>>(8), "revoked_at": row.get::<_, Option<String>>(9),
                "user_id": row.get::<_, Option<i64>>(10), "delivery": row.get::<_, Option<String>>(11),
                "delivery_at": row.get::<_, Option<String>>(12),
            })).collect();
            Ok(Exported {
                records: json!({"customer_invitations": invitations}),
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
            let rows = tx.sql().query(
                "SELECT id,person_id FROM customer_invitations WHERE tenant_id=$1 AND person_id=$2 \
                 AND (name<>$3 OR email<>$3) ORDER BY id FOR UPDATE",
                &[&scope.tenant(), &subject.person.get(), &ERASED],
            ).await?;
            erase_rows(tx, scope, grant, holds, &rows).await
        })
    }
}

async fn erase_rows(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    grant: &Erasure,
    holds: &Holds,
    rows: &[Row],
) -> bilibi_database::Result<Erased> {
    let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
    let people: Vec<i64> = rows.iter().map(|row| row.get(1)).collect();
    let record_holds = holds.records(grant, KIND, &ids).await?;
    let person_holds = holds.persons(grant, &people).await?;
    let mut erased = Erased::default();
    let mut due = Vec::new();
    for row in rows {
        let (id, person): (i64, i64) = (row.get(0), row.get(1));
        if let Some(hold) = record_holds.get(&id).or_else(|| person_holds.get(&person)) {
            erased.kept.push(Kept::held(KIND, id, *hold));
        } else {
            due.push(id);
        }
    }
    if due.is_empty() {
        return Ok(erased);
    }
    let tenant = scope.tenant();
    let changed: Vec<(i64, i64)> = tx.mutate(move |sql| Box::pin(async move {
        let rows = sql.query(
            "UPDATE customer_invitations SET name=$3,email=$3, \
             revoked_at=CASE WHEN accepted_at IS NULL THEN coalesce(revoked_at,now()) ELSE revoked_at END \
             WHERE tenant_id=$1 AND id=ANY($2) RETURNING id,person_id",
            &[&tenant, &due, &ERASED],
        ).await?;
        let changes = rows.iter().map(|row| Change {
            kind: KIND.into(), id: row.get(0), old: json!({"personal_data": "stored"}),
            new: json!({"name": ERASED, "email": ERASED}),
        }).collect();
        Ok((rows.iter().map(|row| (row.get(0),row.get(1))).collect(), changes))
    })).await?;
    erased.count(KIND, Action::Anonymised, changed.len() as i64);
    erased
        .records
        .extend(changed.into_iter().map(|(id, person)| ErasedRecord {
            kind: KIND.into(),
            id,
            person: Some(person),
        }));
    Ok(erased)
}

impl Sweeper for Invitations {
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
            let (company, primary, listed) = companies.parameters();
            let rows = tx.sql().query(
                &format!("SELECT id,person_id FROM customer_invitations WHERE tenant_id=$1 AND id>$2 \
                    AND (name<>$5 OR email<>$5) AND least(expires_at,coalesce(accepted_at,revoked_at,expires_at))<$3::text::timestamptz \
                    AND {} ORDER BY id LIMIT $4 FOR UPDATE", Companies::predicate("organisation_id", 6)),
                &[&scope.tenant(), &after, &cutoff, &limit, &ERASED, &company, &primary, &listed],
            ).await?;
            let next = (rows.len() as i64 == limit)
                .then(|| rows.last().map(|row| row.get(0)))
                .flatten();
            let erased = erase_rows(tx, scope, grant, holds, &rows).await?;
            Ok(Swept {
                acted: erased.actions.iter().map(|action| action.count).sum(),
                kept: erased.kept.len() as i64,
                records: erased.records,
                next,
            })
        })
    }
}

inventory::submit! { Contribution::new(PERSONAL_DATA, "core/portal.invitations",
Provenance { module: "core/portal", source: file!(), line: line!() },
&PersonalData { label: "Customer invitations", kinds: &[KIND], person_subjects: &[], handler: &INVITATIONS }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/portal.invitation",
Provenance { module: "core/portal", source: file!(), line: line!() },
&RecordKind { kind: KIND, fields: &FIELDS, subject_identifier: false, person: false }) }

bilibi_privacy::record_class! {
    module: "core/portal", key: "core/portal.invitations", label: "Customer invitations",
    clock_start: "when the invitation first became accepted, revoked or expired", action: Anonymise,
    business_value: true, floor_days: None, sweeper: &INVITATIONS,
}
