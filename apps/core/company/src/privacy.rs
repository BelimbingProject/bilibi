//! Company's personal-data store (ADR 05, `base/personal_data`, #595). A
//! company can be a natural person's business (a sole proprietorship), and
//! a business can hold one person's email or phone. Companies have no link
//! to a person, so they are found only by the request's email or phone, as
//! candidates the operator confirms; each candidate names its legal form and
//! which identifier matched, so the operator tells the two apart.
//!
//! - **Locate** lists each company whose email holds a request email, or
//!   whose phone digits hold a request phone's, as a candidate.
//! - **Export** lists a confirmed sole proprietorship with every stored
//!   value; any other confirmed company with its name and legal form and only
//!   the email or phone that matches the request.
//! - **Erase** clears a confirmed sole proprietorship's email, phone, tax ID,
//!   legal name, registration number, website, metadata, custom fields, name
//!   and code. Any other confirmed company belongs to the business: only its
//!   email or phone that matches the request is cleared, and its identifiers,
//!   metadata and custom fields stay. Structure, status, revenue,
//!   jurisdiction, activities and relationships always stay, so the records
//!   that name it stay whole. A company under a legal hold is kept. The
//!   company's audit history keeps which fields changed and when, not the
//!   values of the declared `FIELDS` (custom values are audited under one
//!   nested `custom_fields` key); amounts and Extension fields keep theirs.
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
/// The company's audited fields that can hold a person's data: the values
/// erasure clears from a sole proprietorship, as stored (custom values are
/// audited under `custom_fields`, `field_contributions::custom_merge`).
const FIELDS: [&str; 10] = [
    "email",
    "phone",
    "tax_id",
    "legal_name",
    "registration_number",
    "website",
    "metadata",
    "custom_fields",
    "name",
    "code",
];
/// Whether a company's email matches an email pattern (`$2`), or its phone
/// digits a phone digits pattern (`$3`), of the request (`Subject::patterns`).
const EMAIL_MATCH: &str = "coalesce(c.email ILIKE ANY($2),false)";
const PHONE_MATCH: &str = "coalesce(c.phone_digits LIKE ANY($3),false)";

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
            if emails.is_empty() && phones.is_empty() {
                return Ok(Located::default());
            }
            let tenant = scope.tenant();
            let count: i64 = client
                .query_one(
                    &format!("SELECT count(*) FROM companies c WHERE c.tenant_id=$1 AND ({EMAIL_MATCH} OR {PHONE_MATCH})"),
                    &[&tenant, &emails, &phones],
                )
                .await?
                .get(0);
            let rows = client
                .query(
                    &format!(
                        "SELECT c.id,c.status,c.deleted_at IS NOT NULL,to_char(c.created_at AT TIME ZONE 'UTC','YYYY-MM-DD'), \
                         coalesce(t.name,'no legal form'),{EMAIL_MATCH},{PHONE_MATCH}, \
                         EXISTS(SELECT 1 FROM tenant_primary_companies p WHERE p.tenant_id=c.tenant_id AND p.company_id=c.id) \
                         FROM companies c LEFT JOIN company_legal_entity_types t ON t.id=c.legal_entity_type_id \
                         WHERE c.tenant_id=$1 AND ({EMAIL_MATCH} OR {PHONE_MATCH}) ORDER BY c.id LIMIT $4"
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
                    let matched = match (row.get::<_, bool>(5), row.get::<_, bool>(6)) {
                        (true, true) => "email and phone",
                        (true, false) => "email",
                        _ => "phone",
                    };
                    // Erasing it clears the workspace's own contact detail.
                    let own = if row.get(7) { "; this workspace's own company" } else { "" };
                    Candidate {
                        kind: KIND.into(),
                        id: id.to_string(),
                        summary: format!(
                            "Company {id} ({status}{deleted}), {}, created {}, matched {matched}{own}",
                            row.get::<_, String>(4),
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
            let (emails, phones) = subject.patterns();
            // A sole proprietorship is wholly the person's; a business hands
            // over only the contact value that is theirs.
            let records: Value = client
                .query_one(
                    &format!("SELECT coalesce(json_agg(CASE WHEN t.code='sole_proprietorship' THEN json_build_object(
                        'id',c.id,'name',c.name,'code',c.code,'status',c.status,'legal_name',c.legal_name,
                        'legal_entity_type',t.code,'registration_number',c.registration_number,'tax_id',c.tax_id,
                        'jurisdiction',c.jurisdiction,'email',c.email,'phone',c.phone,'website',c.website,
                        'scope_activities',c.scope_activities,'metadata',c.metadata,'custom_fields',c.custom_fields,
                        'created_at',c.created_at,'updated_at',c.updated_at,'deleted_at',c.deleted_at)
                      ELSE json_build_object('id',c.id,'name',c.name,'legal_entity_type',t.code,
                        'email',CASE WHEN {EMAIL_MATCH} THEN c.email END,'phone',CASE WHEN {PHONE_MATCH} THEN c.phone END) END
                      ORDER BY c.id),'[]'::json)::jsonb
                     FROM companies c LEFT JOIN company_legal_entity_types t ON t.id=c.legal_entity_type_id
                     WHERE c.tenant_id=$1 AND c.id=ANY($4)"),
                    &[&scope.tenant(), &emails, &phones, &ids],
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
            let (emails, phones) = subject.patterns();
            let rows = tx
                .sql()
                .query(
                    &format!(
                        "SELECT c.id,coalesce(t.code='sole_proprietorship',false),c.version,to_jsonb(c)-'id'-'legal_entity_type_id'-'version'-'phone_digits', \
                         {EMAIL_MATCH},{PHONE_MATCH} \
                         FROM (SELECT id,legal_entity_type_id,version,phone_digits,{} FROM companies WHERE tenant_id=$1 AND id=ANY($4) ORDER BY id FOR UPDATE) c \
                         LEFT JOIN company_legal_entity_types t ON t.id=c.legal_entity_type_id ORDER BY c.id",
                        FIELDS.join(",")
                    ),
                    &[&tenant, &emails, &phones, &ids],
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
                let (email, phone): (bool, bool) = (row.get(4), row.get(5));
                let erased_code = format!("erased_{id}");
                // A business keeps all but the matching contact value.
                let clears = |field: &str| match field {
                    "email" => sole || email,
                    "phone" => sole || phone,
                    _ => sole,
                };
                // The values still held: a cleared or already erased one is not.
                let before: serde_json::Map<String, Value> = stored
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(field, value)| {
                        clears(field)
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
                let (email, phone) = (clears("email"), clears("phone"));
                tx.mutate(move |sql| {
                    Box::pin(async move {
                        let updated: i64 = sql
                            .query_one(
                                "UPDATE companies SET email=CASE WHEN $5 THEN '' ELSE email END,phone=CASE WHEN $6 THEN '' ELSE phone END, \
                                 tax_id=CASE WHEN $3 THEN '' ELSE tax_id END,legal_name=CASE WHEN $3 THEN NULL ELSE legal_name END, \
                                 registration_number=CASE WHEN $3 THEN NULL ELSE registration_number END,website=CASE WHEN $3 THEN NULL ELSE website END, \
                                 metadata=CASE WHEN $3 THEN NULL ELSE metadata END,custom_fields=CASE WHEN $3 THEN '{}'::jsonb ELSE custom_fields END, \
                                 name=CASE WHEN $3 THEN $4 ELSE name END,code=CASE WHEN $3 THEN 'erased_'||id ELSE code END, \
                                 version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2 RETURNING version",
                                &[&tenant, &id, &sole, &ERASED, &email, &phone],
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
