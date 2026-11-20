use crate::{catalogue, Origin, Resolved, Template, Text, CATALOGUE, MANAGE, VIEW};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::Catalog;
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{NamedOrganisation, PartyProofs, ReadClient, Scope};
use serde::Serialize;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

pub struct Templates {
    entries: BTreeMap<&'static str, &'static Template>,
    pub(crate) parties: Arc<PartyProofs>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Edit {
    pub revision: i64,
    pub customised: bool,
    pub effective: Resolved,
    pub inherited: Resolved,
}

struct Head {
    id: i64,
    company: Option<i64>,
    language: String,
    slots: Option<Text>,
}

pub(crate) struct Selection<'a> {
    pub key: &'a str,
    pub company: Option<OrganisationId>,
    pub language: &'a str,
}

pub(crate) struct Revision<'a> {
    pub selection: Selection<'a>,
    pub expected: i64,
    pub slots: Option<Text>,
}

impl Templates {
    pub(crate) fn new(
        catalogue: &Catalog,
        parties: Arc<PartyProofs>,
    ) -> bilibi_module_registry::Result<Self> {
        let entries = catalogue
            .entries::<Template>(CATALOGUE)?
            .iter()
            .map(|entry| Ok((entry.id, entry.get::<Template>()?)))
            .collect::<bilibi_module_registry::Result<_>>()?;
        Ok(Self { entries, parties })
    }

    pub fn list(&self) -> impl Iterator<Item = &'static Template> + '_ {
        self.entries.values().copied()
    }

    pub fn get(&self, key: &str) -> Result<&'static Template> {
        self.entries.get(key).copied().ok_or(Error::NotFound)
    }

    /// Resolve text for a business record. The consumer authorizes its record
    /// and supplies that record's company. The selected desk is irrelevant.
    pub async fn resolve(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        key: &str,
        company: Option<OrganisationId>,
        requested: &str,
        fallback: Option<&str>,
    ) -> Result<Resolved> {
        let entry = self.get(key)?;
        let languages = catalogue::languages(requested, fallback, entry.base_language)
            .map_err(|problem| Error::refused(problem.to_string()))?;
        let heads = heads(client, scope, key, company, &languages).await?;
        Ok(select(entry, &languages, &heads, None))
    }

    pub(crate) async fn companies(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
    ) -> Result<Vec<NamedOrganisation>> {
        self.parties.organisations_in_scope(client, scope).await
    }

    async fn authorize(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        company: Option<OrganisationId>,
        write: bool,
    ) -> Result<()> {
        let capability = if write { MANAGE } else { VIEW };
        match company {
            None => bilibi_authz::authorize_proof(client, scope, capability).await,
            Some(id) => {
                let company = self
                    .parties
                    .organisation_target(client, scope, id, capability)
                    .await?;
                if write && company.archived {
                    return Err(Error::refused("Archived companies are read-only."));
                }
                Ok(())
            }
        }
    }

    pub(crate) async fn edit(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        selection: &Selection<'_>,
        write: bool,
    ) -> Result<Edit> {
        self.authorize(client, scope, selection.company, write)
            .await?;
        let entry = self.get(selection.key)?;
        let languages = catalogue::languages(selection.language, None, entry.base_language)
            .map_err(|problem| Error::refused(problem.to_string()))?;
        let heads = heads(client, scope, entry.key, selection.company, &languages).await?;
        let company = selection.company.map(OrganisationId::get);
        let head = heads
            .iter()
            .find(|head| head.language == languages[0] && head.company == company);
        let revision = head.map_or(0, |head| head.id);
        Ok(Edit {
            revision,
            customised: head.is_some_and(|head| head.slots.is_some()),
            effective: select(entry, &languages, &heads, None),
            inherited: select(entry, &languages, &heads, Some(revision)),
        })
    }

    pub(crate) async fn overrides(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
    ) -> Result<Vec<serde_json::Value>> {
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
                SELECT DISTINCT ON (template_key,company_id,language)
                    id, template_key, company_id, language, slots
                FROM base_template_revisions
                WHERE tenant_id=$1 AND (company_id=ANY($2) OR company_id IS NULL)
                ORDER BY template_key,company_id,language,id DESC
             ) AS heads WHERE slots IS NOT NULL",
                &[&scope.tenant(), &ids],
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let key: &str = row.get("template_key");
                let entry = self.get(key).ok()?;
                let company: Option<i64> = row.get("company_id");
                let name = company
                    .and_then(|id| names.get(&id).copied())
                    .unwrap_or("Workspace");
                let language: &str = row.get("language");
                let description = format!("{} · {name} · {language}", entry.label);
                Some(json!({
                    "description": description, "key":key, "company_id":company,
                    "language":language,"revision":row.get::<_,i64>("id"),
                    "slots":row.get::<_,serde_json::Value>("slots")
                }))
            })
            .collect())
    }

    pub(crate) async fn change(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        change: Revision<'_>,
    ) -> Result<i64> {
        let key = self.get(change.selection.key)?.key;
        let company = change.selection.company.map(OrganisationId::get);
        let language = catalogue::language(change.selection.language).ok_or(Error::Invalid)?;
        let tenant = scope.tenant();
        tx.sql().execute(
            "SELECT pg_advisory_xact_lock(hashtextextended('base-templates:' || $1::bigint::text || ':' || COALESCE($2::bigint,0)::text || ':' || $3 || ':' || $4,0))",
            &[&tenant, &company, &key, &language],
        ).await?;
        let before = self.edit(tx.sql(), scope, &change.selection, true).await?;
        before.check(&change)?;
        let slots = change.slots.map(|slots| json!(slots));
        tx.mutate(|sql| Box::pin(async move {
            let row = sql.query_one(
                "INSERT INTO base_template_revisions (tenant_id,company_id,template_key,language,slots)
                 VALUES ($1,$2,$3,$4,$5) RETURNING id",
                &[&tenant, &company, &key, &language, &slots],
            ).await?;
            let id = row.get(0);
            Ok((id, vec![Change {
                kind: "base/templates.revision".into(),
                id,
                old: json!({"revision":before.revision,"effective":before.effective}),
                new: json!({"key":key,"company_id":company,"language":language,"slots":slots,"version":id}),
            }]))
        })).await
    }
}

impl Edit {
    pub(crate) fn check(&self, change: &Revision<'_>) -> Result<()> {
        if self.revision != change.expected {
            return Err(Error::Conflict);
        }
        if change.slots.is_none() && !self.customised {
            return Err(Error::refused(
                "This wording already follows the inherited template.",
            ));
        }
        Ok(())
    }
}

async fn heads(
    client: &dyn ReadClient,
    scope: &Scope,
    key: &str,
    company: Option<OrganisationId>,
    languages: &[String],
) -> Result<Vec<Head>> {
    let rows = client
        .query(
            "SELECT DISTINCT ON (language,company_id) id, company_id, language, slots
         FROM base_template_revisions
         WHERE tenant_id=$1 AND template_key=$2 AND language=ANY($3)
           AND (company_id=$4 OR company_id IS NULL)
         ORDER BY language,company_id,id DESC",
            &[
                &scope.tenant(),
                &key,
                &languages,
                &company.map(OrganisationId::get),
            ],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            let slots = row
                .get::<_, Option<serde_json::Value>>("slots")
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| Error::Internal)?;
            Ok(Head {
                id: row.get("id"),
                company: row.get("company_id"),
                language: row.get("language"),
                slots,
            })
        })
        .collect()
}

fn select(
    entry: &Template,
    languages: &[String],
    heads: &[Head],
    exclude: Option<i64>,
) -> Resolved {
    for language in languages {
        let head = heads
            .iter()
            .filter(|head| {
                head.language == *language && Some(head.id) != exclude && head.slots.is_some()
            })
            .max_by_key(|head| head.company.is_some());
        if let Some(head) = head {
            return Resolved {
                origin: match head.company {
                    Some(company_id) => Origin::Company {
                        company_id,
                        revision: head.id,
                    },
                    None => Origin::Tenant { revision: head.id },
                },
                language: language.clone(),
                slots: head.slots.clone().expect("selected a published revision"),
            };
        }
        if let Some(default) = entry
            .defaults
            .iter()
            .find(|default| default.language == language)
        {
            return default.resolve();
        }
    }
    unreachable!("the validated base language is always a candidate")
}
