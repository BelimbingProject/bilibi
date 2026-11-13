//! Company's personal-data store (ADR 05, `base/personal_data`, #595). A
//! company can be a natural person's business (a sole proprietorship), and
//! its contact fields, free JSON and tenant-defined custom fields take a
//! person's details. Companies have no link to a person, so they are found
//! only by the request's email or phone, as candidates the operator confirms.
//!
//! - **Locate** lists each company whose email holds a request email, or
//!   whose phone digits hold a request phone's, as a candidate.
//! - **Export** lists the confirmed companies with every stored value.
//! - **Erase** clears a confirmed company's email, phone, tax ID, legal name,
//!   registration number, website, metadata and custom fields, and, for a
//!   sole proprietorship, its name and code. Its structure, status, revenue,
//!   jurisdiction, activities and relationships stay, so the records that
//!   name it stay whole. A company under a legal hold is kept. The company's
//!   audit history keeps which fields changed and when, not their values
//!   (`*`: custom fields are audited under tenant-defined `cf_<key>` keys).
use bilibi_database::{Change, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_personal_data::{
    Action, Candidate, Disclosure, Erased, ErasedRecord, Erasure, Exported, Handler, HandlerFuture,
    Holds, Kept, KindCount, Located, PersonalData, RecordKind, Subject, ERASED, MAX_CANDIDATES,
    PERSONAL_DATA, RECORD_KINDS,
};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::Client;

const KIND: &str = "company";
const FIELDS: [&str; 1] = ["*"];
/// A tenant's companies matching an email pattern (`$2`) or phone digits
/// pattern (`$3`) of the request (`Subject::patterns`).
const MATCHES: &str = "tenant_id=$1 AND (email ILIKE ANY($2) OR phone_digits LIKE ANY($3))";
/// The values erasure clears, as stored.
const ERASABLE: &str =
    "email,phone,tax_id,legal_name,registration_number,website,metadata,custom_fields,name,code";

struct Companies;
static COMPANIES: Companies = Companies;
impl Handler for Companies {
    fn locate<'a>(
        &'a self,
        client: &'a Client,
        scope: &'a Scope,
        subject: &'a Subject,
    ) -> HandlerFuture<'a, Located> {
        Box::pin(async move {
            let (emails, phones) = subject.patterns();
            let tenant = scope.tenant();
            let count: i64 = client
                .query_one(
                    &format!("SELECT count(*) FROM companies WHERE {MATCHES}"),
                    &[&tenant, &emails, &phones],
                )
                .await?
                .get(0);
            let rows = client
                .query(
                    &format!(
                        "SELECT id,status,deleted_at IS NOT NULL,to_char(created_at AT TIME ZONE 'UTC','YYYY-MM-DD') \
                         FROM companies WHERE {MATCHES} ORDER BY id LIMIT $4"
                    ),
                    &[&tenant, &emails, &phones, &(MAX_CANDIDATES as i64 + 1)],
                )
                .await?;
            let candidates = rows
                .iter()
                .take(MAX_CANDIDATES)
                .map(|row| {
                    let id: i64 = row.get(0);
                    let status: String = row.get(1);
                    let deleted = if row.get(2) { ", deleted" } else { "" };
                    Candidate {
                        kind: KIND.into(),
                        id: id.to_string(),
                        summary: format!(
                            "Company {id} ({status}{deleted}), created {}",
                            row.get::<_, String>(3)
                        ),
                    }
                })
                .collect();
            Ok(Located {
                kinds: vec![KindCount {
                    kind: KIND.into(),
                    by_person: 0,
                    by_identifier: count,
                }],
                candidates,
                truncated: rows.len() > MAX_CANDIDATES,
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
            let ids = subject.confirmed_ids(KIND);
            if ids.is_empty() {
                return Ok(Exported::default());
            }
            let records: Value = client
                .query_one(
                    "SELECT coalesce(json_agg(json_build_object(
                        'id',c.id,'name',c.name,'code',c.code,'status',c.status,'legal_name',c.legal_name,
                        'legal_entity_type',t.code,'registration_number',c.registration_number,'tax_id',c.tax_id,
                        'jurisdiction',c.jurisdiction,'email',c.email,'phone',c.phone,'website',c.website,
                        'scope_activities',c.scope_activities,'metadata',c.metadata,'custom_fields',c.custom_fields,
                        'created_at',c.created_at,'updated_at',c.updated_at,'deleted_at',c.deleted_at) ORDER BY c.id),'[]'::json)::jsonb
                     FROM companies c LEFT JOIN company_legal_entity_types t ON t.id=c.legal_entity_type_id
                     WHERE c.tenant_id=$1 AND c.id=ANY($2)",
                    &[&scope.tenant(), &ids],
                )
                .await?
                .get(0);
            Ok(Exported {
                records: json!({ "companies": records }),
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
            let ids = subject.confirmed_ids(KIND);
            let mut erased = Erased::default();
            if ids.is_empty() {
                return Ok(erased);
            }
            let rows = tx
                .sql()
                .query(
                    &format!(
                        "SELECT c.id,coalesce(t.code='sole_proprietorship',false),c.version,to_jsonb(c)-'id'-'legal_entity_type_id'-'version' \
                         FROM (SELECT id,legal_entity_type_id,version,{ERASABLE} FROM companies WHERE tenant_id=$1 AND id=ANY($2) ORDER BY id FOR UPDATE) c \
                         LEFT JOIN company_legal_entity_types t ON t.id=c.legal_entity_type_id ORDER BY c.id"
                    ),
                    &[&tenant, &ids],
                )
                .await?;
            let found: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
            let held = holds.records(grant, KIND, &found).await?;
            let person_hold = holds.person_hold(grant).await?;
            let mut count = 0;
            for row in rows {
                let (id, sole, version): (i64, bool, i64) = (row.get(0), row.get(1), row.get(2));
                if let Some(hold) = person_hold.or_else(|| held.get(&id).copied()) {
                    erased.kept.push(Kept::held(KIND, id, hold));
                    continue;
                }
                let stored: Value = row.get(3);
                let erased_code = format!("erased_{id}");
                // The values still held: a cleared or already erased one is not.
                let before: serde_json::Map<String, Value> = stored
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(field, value)| {
                        (sole || !matches!(field.as_str(), "name" | "code"))
                            && !value.is_null()
                            && ![json!(""), json!({}), json!(ERASED), json!(erased_code)]
                                .contains(value)
                    })
                    .map(|(field, value)| (field.clone(), value.clone()))
                    .collect();
                // A company with nothing left to clear is still written while
                // its history holds values, since the database clears evidence
                // only of a record this transaction changed.
                if before.is_empty() && !bilibi_audit::holds_values(tx, KIND, id).await? {
                    count += 1;
                    continue;
                }
                tx.mutate(move |sql| {
                    Box::pin(async move {
                        let updated: i64 = sql
                            .query_one(
                                "UPDATE companies SET email='',phone='',tax_id='',legal_name=NULL,registration_number=NULL,website=NULL,metadata=NULL,custom_fields='{}'::jsonb, \
                                 name=CASE WHEN $3 THEN $4 ELSE name END,code=CASE WHEN $3 THEN 'erased_'||id ELSE code END, \
                                 version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 RETURNING version",
                                &[&tenant, &id, &sole, &ERASED],
                            )
                            .await?
                            .get(0);
                        // `[erased]` where a value was, so the trail tells erasure from absence.
                        let mut old = before.clone();
                        let mut new: serde_json::Map<String, Value> =
                            before.keys().map(|field| (field.clone(), json!(ERASED))).collect();
                        old.insert("version".into(), json!(version));
                        new.insert("version".into(), json!(updated));
                        Ok(((), vec![Change { kind: KIND.into(), id, old: Value::Object(old), new: Value::Object(new) }]))
                    })
                })
                .await?;
                erased.records.push(ErasedRecord {
                    kind: KIND.into(),
                    id,
                    person: None,
                });
                count += 1;
            }
            erased.count(KIND, Action::Anonymised, count);
            Ok(erased)
        })
    }
}

inventory::submit! { Contribution::new(PERSONAL_DATA, "core/company.companies",
Provenance { module: "core/company", source: file!(), line: line!() },
&PersonalData { label: "Companies (contact and identifiers)", kinds: &[KIND], person_subjects: &[], handler: &COMPANIES }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/company.company",
Provenance { module: "core/company", source: file!(), line: line!() },
&RecordKind { kind: KIND, fields: &FIELDS, subject_identifier: false, person: false }) }
