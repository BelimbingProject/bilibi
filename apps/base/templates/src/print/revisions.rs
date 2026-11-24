use super::{Checked, PrintContract, ResolvedPrint};
use crate::catalogue::Problem;
use crate::{catalogue, store::Selection, Origin, Templates};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{ReadClient, Scope};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{borrow::Cow, collections::BTreeMap};

pub(crate) const AUDIT_KIND: &str = "base/templates.print_revision";

struct Head {
    id: i64,
    company: Option<i64>,
    language: String,
    contract_version: String,
    layout: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PrintEdit {
    pub revision: i64,
    pub customised: bool,
    pub review: String,
    pub effective: ResolvedPrint<'static>,
    pub inherited: ResolvedPrint<'static>,
}

impl PrintEdit {
    pub(crate) fn check(&self, expected: &str, restoring: bool) -> Result<()> {
        if self.review != expected {
            return Err(Error::Conflict);
        }
        if restoring && !self.customised {
            return Err(Error::refused(
                "This print layout already follows its inherited default.",
            ));
        }
        Ok(())
    }
}

impl Templates {
    /// Read the print layout for an authorized source. The owning module
    /// supplies the record's company even when the actor selected another desk.
    pub async fn resolve_print(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        key: &str,
        company: Option<OrganisationId>,
        requested: &str,
        fallback: Option<&str>,
    ) -> Result<ResolvedPrint<'static>> {
        let contract = self.print_contract(key)?;
        let languages = languages(contract, requested, fallback)?;
        let heads = heads(client, scope, contract, company, &languages).await?;
        select(
            contract,
            &languages,
            &heads,
            company.map(OrganisationId::get),
            None,
        )
    }

    pub(crate) async fn edit_print(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        selection: &Selection<'_>,
        write: bool,
    ) -> Result<PrintEdit> {
        self.authorize(client, scope, selection.company, write)
            .await?;
        let contract = self.print_contract(selection.key)?;
        let languages = languages(contract, selection.language, None)?;
        let heads = heads(client, scope, contract, selection.company, &languages).await?;
        let company = selection.company.map(OrganisationId::get);
        edit(contract, &languages, &heads, company)
    }

    pub(crate) async fn change_print(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        selection: &Selection<'_>,
        expected: &str,
        layout: Option<&str>,
    ) -> Result<i64> {
        let contract = self.print_contract(selection.key)?;
        let key = contract.key;
        let version = contract.version;
        let company = selection.company.map(OrganisationId::get);
        let language = catalogue::language(selection.language).ok_or(Error::Invalid)?;
        let tenant = scope.tenant();
        if let Some(layout) = layout {
            self.validate_print(selection, layout)
                .map_err(|problem| Error::refused(problem.to_string()))?;
        }
        tx.sql().execute(
            "SELECT pg_advisory_xact_lock(hashtextextended('base-print:' || $1::bigint::text || ':' || $2,0))",
            &[&tenant, &key],
        ).await?;
        let before = self.edit_print(tx.sql(), scope, selection, true).await?;
        before.check(expected, layout.is_none())?;
        let layout = layout.map(str::to_owned);
        let old = json!({
            "key":key,"company_id":company,"language":language,"contract_version":version,
            "revision":before.revision,"effective":identity(&before.effective),
        });
        let content = layout.as_deref().map_or_else(
            || identity(&before.inherited),
            |layout| json!({"language":language,"sha256":digest(layout.as_bytes())}),
        );
        tx.mutate(|sql| Box::pin(async move {
            let row = sql.query_one(
                "INSERT INTO base_print_revisions (tenant_id, company_id, template_key, language, contract_version, layout)
                 VALUES ($1,$2,$3,$4,$5,$6) RETURNING id",
                &[&tenant, &company, &key, &language, &version, &layout],
            ).await?;
            let id = row.get(0);
            Ok((id, vec![Change {
                kind: AUDIT_KIND.into(),
                id,
                old,
                new: json!({"key":key,"company_id":company,"language":language,"contract_version":version,"content":content,"version":id}),
            }]))
        })).await
    }

    pub(crate) async fn print_overrides(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
    ) -> Result<Vec<Value>> {
        self.authorize(client, scope, None, false).await?;
        let companies = self.companies(client, scope).await?;
        let mut names = BTreeMap::new();
        for company in companies.into_iter().filter(|company| !company.archived) {
            match self.authorize(client, scope, Some(company.id), false).await {
                Ok(()) => {
                    names.insert(company.id.get(), company.name);
                }
                Err(Error::Forbidden | Error::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        let ids: Vec<_> = names.keys().copied().collect();
        let rows = client
            .query(
                "SELECT * FROM (
                SELECT DISTINCT ON (template_key, company_id, language)
                    id, template_key, company_id, language, contract_version, layout
                FROM base_print_revisions
                WHERE tenant_id=$1 AND (company_id=ANY($2) OR company_id IS NULL)
                ORDER BY template_key, company_id, language, id DESC
            ) AS heads WHERE layout IS NOT NULL",
                &[&scope.tenant(), &ids],
            )
            .await?;
        let mut published: BTreeMap<&str, Vec<Head>> = BTreeMap::new();
        for row in &rows {
            published
                .entry(row.get("template_key"))
                .or_default()
                .push(Head::from(row));
        }
        let mut overrides = Vec::new();
        for (key, heads) in published {
            let Ok(contract) = self.print_contract(key) else {
                continue;
            };
            for head in &heads {
                let company = head.company;
                let name = company
                    .and_then(|id| names.get(&id))
                    .map_or("Workspace", String::as_str);
                let language = &head.language;
                let revision = head.id;
                let languages = languages(contract, language, None)?;
                let edit = edit(contract, &languages, &heads, company)?;
                let inherited = match edit.inherited.origin {
                    Origin::Company { .. } => "company layout",
                    Origin::Tenant { .. } => "workspace layout",
                    Origin::Packaged { .. } => "packaged layout",
                };
                overrides.push(json!({
                    "key": key, "company_id": company, "language": language, "revision": revision,
                    "review": edit.review,
                    "applicable": contract.version == head.contract_version,
                    "description": format!("{} · {name} · {language} · Revision {revision} · Inherits {inherited}", contract.label),
                }));
            }
        }
        Ok(overrides)
    }
}

fn languages(
    contract: &PrintContract,
    requested: &str,
    fallback: Option<&str>,
) -> Result<Vec<String>> {
    catalogue::languages(requested, fallback, contract.base_language)
        .map_err(|problem| Error::refused(problem.to_string()))
}

async fn heads(
    client: &dyn ReadClient,
    scope: &Scope,
    contract: &PrintContract,
    company: Option<OrganisationId>,
    languages: &[String],
) -> Result<Vec<Head>> {
    let rows = client.query(
        "SELECT DISTINCT ON (language, company_id) id, company_id, language, contract_version, layout
         FROM base_print_revisions
         WHERE tenant_id=$1 AND template_key=$2 AND language=ANY($3)
           AND (company_id=$4 OR company_id IS NULL)
         ORDER BY language, company_id, id DESC",
        &[&scope.tenant(), &contract.key, &languages, &company.map(OrganisationId::get)],
    ).await?;
    Ok(rows.iter().map(Head::from).collect())
}

impl From<&tokio_postgres::Row> for Head {
    fn from(row: &tokio_postgres::Row) -> Self {
        Self {
            id: row.get("id"),
            company: row.get("company_id"),
            language: row.get("language"),
            contract_version: row.get("contract_version"),
            layout: row.get("layout"),
        }
    }
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn identity(resolved: &ResolvedPrint<'_>) -> Value {
    json!({
        "origin":resolved.origin,"language":resolved.language,
        "sha256":digest(resolved.layout.as_bytes()),
    })
}

fn edit(
    contract: &PrintContract,
    languages: &[String],
    heads: &[Head],
    company: Option<i64>,
) -> Result<PrintEdit> {
    let head = heads
        .iter()
        .find(|head| head.language == languages[0] && head.company == company);
    let revision = head.map_or(0, |head| head.id);
    let effective = select(contract, languages, heads, company, None)?;
    let inherited = select(contract, languages, heads, company, Some(revision))?;
    let review = digest(
        &serde_json::to_vec(&json!([
            contract.key,
            contract.version,
            company,
            languages[0],
            revision,
            identity(&effective),
            identity(&inherited),
        ]))
        .map_err(|_| Error::Internal)?,
    );
    Ok(PrintEdit {
        revision,
        customised: head.is_some_and(|head| head.layout.is_some()),
        review,
        effective,
        inherited,
    })
}

fn select(
    contract: &PrintContract,
    languages: &[String],
    heads: &[Head],
    company: Option<i64>,
    exclude: Option<i64>,
) -> Result<ResolvedPrint<'static>> {
    for language in languages {
        if let Some(head) = heads
            .iter()
            .filter(|head| {
                (head.company.is_none() || head.company == company)
                    && head.language == *language
                    && Some(head.id) != exclude
                    && head.contract_version == contract.version
                    && head.layout.is_some()
            })
            .max_by_key(|head| head.company.is_some())
        {
            let origin = match head.company {
                Some(company_id) => Origin::Company {
                    company_id,
                    revision: head.id,
                },
                None => Origin::Tenant { revision: head.id },
            };
            return Ok(ResolvedPrint {
                language: language.clone(),
                origin,
                layout: Cow::Owned(head.layout.clone().expect("selected a published layout")),
            });
        }
        if let Some(default) = contract
            .defaults
            .iter()
            .find(|default| default.language == language)
        {
            return Ok(default.resolve());
        }
    }
    Err(Error::Internal)
}

impl Templates {
    pub(crate) fn validate_print(&self, selection: &Selection<'_>, layout: &str) -> Checked<()> {
        self.print_contract(selection.key)
            .map_err(|error| Problem::new("key", error.to_string()))?
            .compile(selection.language, layout)?
            .preview()
            .map(|_| ())
    }

    pub(crate) async fn print_revision(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        revision: i64,
    ) -> Result<(&'static PrintContract, String, String)> {
        let row = client
            .query_opt(
                "SELECT company_id, template_key, language, contract_version, layout
               FROM base_print_revisions AS current
               WHERE current.tenant_id=$1 AND current.id=$2 AND current.layout IS NOT NULL
                 AND NOT EXISTS (
                     SELECT 1 FROM base_print_revisions AS newer
                     WHERE newer.tenant_id=$1 AND newer.template_key=current.template_key
                       AND newer.company_id IS NOT DISTINCT FROM current.company_id
                       AND newer.language=current.language AND newer.id > current.id
                 )",
                &[&scope.tenant(), &revision],
            )
            .await?
            .ok_or(Error::NotFound)?;
        let company = row
            .get::<_, Option<i64>>("company_id")
            .map(OrganisationId::try_from)
            .transpose()
            .map_err(|_| Error::Internal)?;
        self.authorize(client, scope, company, false).await?;
        let contract = self.print_contract(row.get("template_key"))?;
        if row.get::<_, &str>("contract_version") != contract.version {
            return Err(Error::refused(
                "This revision needs a layout for the current print contract.",
            ));
        }
        Ok((contract, row.get("language"), row.get("layout")))
    }
}
