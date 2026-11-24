use crate::{
    catalogue, Origin, PrintContract, Resolved, Template, Text, Value, CATALOGUE, MANAGE,
    PLATFORM_CATALOGUE, PRINT_CATALOGUE, VIEW,
};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_module_registry::Catalog;
use bilibi_party_ids::OrganisationId;
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{NamedOrganisation, PartyProofs, ReadClient, Scope};
use serde::Serialize;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

pub struct Templates {
    entries: BTreeMap<&'static str, &'static Template>,
    platform: BTreeMap<&'static str, &'static Template>,
    settings: Arc<Settings>,
    print: BTreeMap<&'static str, &'static PrintContract>,
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
        settings: Arc<Settings>,
    ) -> bilibi_module_registry::Result<Self> {
        let collect = |id| {
            catalogue
                .entries::<Template>(id)?
                .iter()
                .map(|entry| Ok((entry.id, entry.get::<Template>()?)))
                .collect::<bilibi_module_registry::Result<BTreeMap<_, _>>>()
        };
        let entries = collect(CATALOGUE)?;
        let platform = collect(PLATFORM_CATALOGUE)?;
        if let Some(key) = platform.keys().find(|key| entries.contains_key(*key)) {
            return Err(bilibi_module_registry::RegistryError::Contribution(
                format!("Template {key} is declared in both catalogues."),
            ));
        }
        let print = catalogue
            .entries::<PrintContract>(PRINT_CATALOGUE)?
            .iter()
            .map(|entry| Ok((entry.id, entry.get::<PrintContract>()?)))
            .collect::<bilibi_module_registry::Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            entries,
            print,
            platform,
            parties,
            settings,
        })
    }

    pub fn list(&self) -> impl Iterator<Item = &'static Template> + '_ {
        self.entries.values().chain(self.platform.values()).copied()
    }

    pub fn get(&self, key: &str) -> Result<&'static Template> {
        self.entries
            .get(key)
            .or_else(|| self.platform.get(key))
            .copied()
            .ok_or(Error::NotFound)
    }

    pub(crate) fn print_contracts(&self) -> impl Iterator<Item = &'static PrintContract> + '_ {
        self.print.values().copied()
    }

    pub fn print_contract(&self, key: &str) -> Result<&'static PrintContract> {
        self.print.get(key).copied().ok_or(Error::NotFound)
    }

    pub(crate) fn editable(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Resolve an explicitly selected language for an authorized record.
    /// The consumer supplies the record's company, independent of the desk.
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
        self.select_for(client, scope.tenant(), entry, company, &languages)
            .await
    }

    async fn select_for(
        &self,
        client: &dyn ReadClient,
        tenant: i64,
        entry: &Template,
        company: Option<OrganisationId>,
        languages: &[String],
    ) -> Result<Resolved> {
        let heads = if self.editable(entry.key) {
            heads(client, tenant, entry.key, company, languages).await?
        } else {
            Vec::new()
        };
        Ok(select(entry, languages, &heads, None))
    }

    /// Render a platform-controlled template in its base language without a
    /// database read. Every text fact is flattened to one line.
    pub fn platform_wording(&self, key: &str, values: &[(&str, Value<'_>)]) -> Result<Text> {
        let entry = self.platform.get(key).ok_or(Error::NotFound)?;
        let languages = [entry.base_language.to_owned()];
        let resolved = select(entry, &languages, &[], None);
        // A name someone chose (a workspace, a portal) is one line of text
        // here: a line break in it could set a second link apart from the
        // message's own.
        let flat: Vec<(&str, Option<String>)> = values
            .iter()
            .map(|(id, value)| match value {
                Value::Text(text) => (
                    *id,
                    Some(
                        text.chars()
                            .map(|c| if c.is_control() { ' ' } else { c })
                            .collect(),
                    ),
                ),
                Value::Date(_) => (*id, None),
            })
            .collect();
        let values: Vec<(&str, Value<'_>)> = values
            .iter()
            .zip(&flat)
            .map(|((id, value), (_, text))| match text {
                Some(text) => (*id, Value::Text(text)),
                None => (*id, *value),
            })
            .collect();
        render_wording(entry, &languages, resolved, &values, &[])
    }

    /// Wording for unattended mail with no company. The caller proves the
    /// tenant from its own record before asking for the workspace language.
    pub async fn tenant_wording(
        &self,
        client: &dyn ReadClient,
        tenant: i64,
        key: &str,
        values: &[(&str, Value<'_>)],
    ) -> Result<Text> {
        let entry = self.get(key)?;
        let language = self
            .settings
            .get_tenant(client, tenant, crate::LANGUAGE)
            .await?;
        let languages = catalogue::language_chain(language.as_str(), entry.base_language);
        let resolved = self
            .select_for(client, tenant, entry, None, &languages)
            .await?;
        render_wording(entry, &languages, resolved, values, &[])
    }

    /// Render wording for an authorized record, using company and tenant
    /// language preferences before the consumer's locale and packaged base.
    pub async fn wording(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        key: &str,
        company: Option<OrganisationId>,
        language: &str,
        (values, omitted): (&[(&str, Value<'_>)], &[&str]),
    ) -> Result<Text> {
        let entry = self.get(key)?;
        let languages = self
            .preferred_languages(client, scope, entry, company, language)
            .await?;
        let resolved = self
            .select_for(client, scope.tenant(), entry, company, &languages)
            .await?;
        render_wording(entry, &languages, resolved, values, omitted)
    }

    async fn preferred_languages(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        entry: &Template,
        company: Option<OrganisationId>,
        requested: &str,
    ) -> Result<Vec<String>> {
        let fallback_chain = catalogue::languages(requested, None, entry.base_language)
            .map_err(|problem| Error::refused(problem.to_string()))?;
        if !self.editable(entry.key) {
            return Ok(fallback_chain);
        }

        let tenant_language = self
            .settings
            .get(client, scope, Target::Tenant, crate::LANGUAGE)
            .await?;
        let company_language = match company {
            Some(id) => {
                self.settings
                    .get(client, scope, Target::Company(id), crate::LANGUAGE)
                    .await?
                    .value
            }
            None => serde_json::Value::Null,
        };
        Ok(catalogue::language_chain(
            company_language
                .as_str()
                .into_iter()
                .chain(tenant_language.value.as_str())
                .chain(std::iter::once(requested)),
            entry.base_language,
        ))
    }

    pub(crate) async fn companies(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
    ) -> Result<Vec<NamedOrganisation>> {
        self.parties.organisations_in_scope(client, scope).await
    }

    pub(crate) async fn authorize(
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
        let heads = if self.editable(entry.key) {
            heads(
                client,
                scope.tenant(),
                entry.key,
                selection.company,
                &languages,
            )
            .await?
        } else {
            Vec::new()
        };
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
                let entry = self.entries.get(key)?;
                let company: Option<i64> = row.get("company_id");
                let name = company
                    .and_then(|id| names.get(&id).copied())
                    .unwrap_or("Workspace");
                let language: &str = row.get("language");
                let description = format!("{} · {name} · {language}", entry.label);
                let slots: crate::Text = serde_json::from_value(row.get("slots")).ok()?;
                let preview = entry.preview(&slots).ok();
                Some(json!({
                    "description": description, "key":key, "company_id":company,
                    "language":language,"revision":row.get::<_,i64>("id"),
                    "slots":slots, "preview":preview
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

fn render_wording(
    entry: &Template,
    languages: &[String],
    resolved: Resolved,
    values: &[(&str, Value<'_>)],
    omitted: &[&str],
) -> Result<Text> {
    let problem = match entry.render_delivery(&resolved.slots, values, omitted) {
        Ok(text) => return Ok(text),
        Err(problem) => problem,
    };
    if let Origin::Company { revision, .. } | Origin::Tenant { revision } = resolved.origin {
        let packaged = select(entry, languages, &[], None);
        if let Ok(text) = entry.render_delivery(&packaged.slots, values, omitted) {
            eprintln!("templates: {} revision {revision} could not be filled in ({problem}); the packaged wording was used", entry.key);
            return Ok(text);
        }
    }
    Err(Error::refused(problem.to_string()))
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
    tenant: i64,
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
            &[&tenant, &key, &languages, &company.map(OrganisationId::get)],
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
