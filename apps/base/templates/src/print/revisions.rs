use super::{PrintContract, ResolvedPrint};
use crate::{catalogue, store::Selection, Origin, Templates};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{ReadClient, Scope};
use serde::Serialize;
use serde_json::{json, Value};
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
    pub effective: ResolvedPrint<'static>,
    pub inherited: ResolvedPrint<'static>,
}

impl PrintEdit {
    pub(crate) fn check(&self, expected: i64, restoring: bool) -> Result<()> {
        if self.revision != expected {
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
        select(contract, &languages, &heads, None)
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
        let head = heads
            .iter()
            .find(|head| head.language == languages[0] && head.company == company);
        let revision = head.map_or(0, |head| head.id);
        Ok(PrintEdit {
            revision,
            customised: head.is_some_and(|head| head.layout.is_some()),
            effective: select(contract, &languages, &heads, None)?,
            inherited: select(contract, &languages, &heads, Some(revision))?,
        })
    }

    pub(crate) async fn change_print(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        selection: &Selection<'_>,
        expected: i64,
        layout: Option<&str>,
    ) -> Result<i64> {
        let contract = self.print_contract(selection.key)?;
        let key = contract.key;
        let version = contract.version;
        let company = selection.company.map(OrganisationId::get);
        let language = catalogue::language(selection.language).ok_or(Error::Invalid)?;
        let tenant = scope.tenant();
        tx.sql().execute(
            "SELECT pg_advisory_xact_lock(hashtextextended('base-print:' || $1::bigint::text || ':' || COALESCE($2::bigint,0)::text || ':' || $3 || ':' || $4,0))",
            &[&tenant, &company, &key, &language],
        ).await?;
        let before = self.edit_print(tx.sql(), scope, selection, true).await?;
        before.check(expected, layout.is_none())?;
        let layout = layout.map(str::to_owned);
        if let Some(layout) = &layout {
            contract
                .compile(&language, layout)
                .and_then(|compiled| compiled.preview())
                .map_err(|problem| Error::refused(problem.to_string()))?;
        }
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
                old: json!({"revision":before.revision,"effective":before.effective}),
                new: json!({"key":key,"company_id":company,"language":language,"contract_version":version,"layout":layout,"version":id}),
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
        let names: BTreeMap<_, _> = companies
            .iter()
            .filter(|company| !company.archived)
            .map(|company| (company.id.get(), company.name.as_str()))
            .collect();
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
        let published: BTreeMap<_, _> = rows
            .iter()
            .map(|row| {
                (
                    (
                        row.get::<_, &str>("template_key"),
                        row.get::<_, &str>("language"),
                        row.get::<_, Option<i64>>("company_id"),
                    ),
                    (
                        row.get::<_, i64>("id"),
                        row.get::<_, &str>("contract_version"),
                    ),
                )
            })
            .collect();
        Ok(rows.iter().filter_map(|row| {
            let key: &str = row.get("template_key");
            let contract = self.print_contract(key).ok()?;
            let company: Option<i64> = row.get("company_id");
            let name = company.and_then(|id| names.get(&id).copied()).unwrap_or("Workspace");
            let language: &str = row.get("language");
            let revision: i64 = row.get("id");
            let version: &str = row.get("contract_version");
            let inherited = languages(contract, language, None).ok()?.into_iter().find_map(|locale| {
                let mut sources = [company, None].into_iter().filter_map(|source| {
                    let (id, version) = published.get(&(key, locale.as_str(), source))?;
                    (*id != revision && *version == contract.version).then_some(source)
                });
                if let Some(source) = sources.next() {
                    Some(if source.is_some() { "company layout" } else { "workspace layout" })
                } else {
                    contract.defaults.iter().any(|default| default.language == locale).then_some("packaged layout")
                }
            })?;

            Some(json!({
                "key": key, "company_id": company, "language": language, "revision": revision,
                "applicable": contract.version == version,
                "description": format!("{} · {name} · {language} · Revision {revision} · Inherits {inherited}", contract.label),
            }))
        }).collect())
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
    Ok(rows
        .into_iter()
        .map(|row| Head {
            id: row.get("id"),
            company: row.get("company_id"),
            language: row.get("language"),
            contract_version: row.get("contract_version"),
            layout: row.get("layout"),
        })
        .collect())
}

fn select(
    contract: &PrintContract,
    languages: &[String],
    heads: &[Head],
    exclude: Option<i64>,
) -> Result<ResolvedPrint<'static>> {
    for language in languages {
        if let Some(head) = heads
            .iter()
            .filter(|head| {
                head.language == *language
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
    pub(crate) async fn print_revision(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        revision: i64,
    ) -> Result<(
        &'static PrintContract,
        Option<OrganisationId>,
        String,
        String,
    )> {
        let row = client
            .query_opt(
                "SELECT company_id, template_key, language, contract_version, layout
               FROM base_print_revisions WHERE tenant_id=$1 AND id=$2 AND layout IS NOT NULL",
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
        Ok((contract, company, row.get("language"), row.get("layout")))
    }
}
