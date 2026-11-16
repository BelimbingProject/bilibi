//! Group roles: a group another module owns (a Core team) carries roles, and
//! its members hold them while they are members. Authz keeps the group's
//! label, company, roles and members in its own tables; the owner writes them
//! only through these methods, inside its own `MutationTransaction`, so Base
//! never reads the owner's tables and every grant check stays here.
//!
//! Giving a group a role, and adding a member to a group that carries roles,
//! are grant changes: role-update permission, the subset rule (the actor holds
//! every key the role gives, in the group's company) and step-up
//! (`base.authz.principal_role.set`). A group never carries a full-access or
//! an engagement role. A group with a company grants its roles in that
//! company only (ADR 20). Removing a member only narrows, so it is never
//! checked here. Decisions read the rows live; nothing is cached.
use super::Grants;
use crate::{authorize_key, roles::RoleSummary};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_tenancy::Scope;
use serde_json::json;
use std::collections::BTreeSet;
use tokio_postgres::GenericClient;

/// A group as its owner names it now.
#[derive(Clone, Copy, Debug)]
pub struct Group<'a> {
    /// The owner's noun for it, such as "team": lowercase letters and `_`.
    pub source: &'a str,
    /// The owner's stable id.
    pub id: i64,
    /// Its name now ("Line 2 crew"); role displays say "via team Line 2 crew".
    pub label: &'a str,
    /// Its roles apply in this company only; `None` is tenant-wide.
    pub company: Option<i64>,
}
impl Group<'_> {
    fn check(&self) -> Result<()> {
        let source = self.source.as_bytes();
        let label = self.label.chars().count();
        if self.id <= 0
            || source.is_empty()
            || source.len() > 40
            || !source[0].is_ascii_lowercase()
            || !source.iter().all(|b| b.is_ascii_lowercase() || *b == b'_')
            || !(1..=200).contains(&label)
            || self.company.is_some_and(|id| id <= 0)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
/// What Authz holds for a group: its company and the roles it carries.
struct Stored {
    company: Option<i64>,
    roles: Vec<i64>,
}
async fn stored(
    client: &(impl GenericClient + Sync),
    tenant: i64,
    group: &Group<'_>,
    lock: bool,
) -> Result<Option<Stored>> {
    let Some(row) = client
        .query_opt(
            &format!(
                "SELECT company_id FROM authz_groups WHERE tenant_id=$1 AND source=$2 AND group_id=$3{}",
                if lock { " FOR UPDATE" } else { "" }
            ),
            &[&tenant, &group.source, &group.id],
        )
        .await?
    else {
        return Ok(None);
    };
    let roles = client
        .query(
            "SELECT role_id FROM authz_group_roles WHERE tenant_id=$1 AND source=$2 AND group_id=$3 ORDER BY role_id",
            &[&tenant, &group.source, &group.id],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    Ok(Some(Stored {
        company: row.get(0),
        roles,
    }))
}
/// The roles a group carries, by name. A read for the owner's record page,
/// which has already shown the reader the group.
pub async fn group_roles(
    client: &impl GenericClient,
    scope: &Scope,
    source: &str,
    group: i64,
) -> Result<Vec<RoleSummary>> {
    let rows = client
        .query(
            &format!("SELECT {} FROM roles r JOIN authz_group_roles g ON g.tenant_id=r.tenant_id AND g.role_id=r.id WHERE g.tenant_id=$1 AND g.source=$2 AND g.group_id=$3 ORDER BY lower(r.name),r.id", crate::roles::SUMMARY),
            &[&scope.tenant(), &source, &group],
        )
        .await?;
    Ok(rows.iter().map(crate::roles::summary).collect())
}

impl Grants {
    /// The grant checks for giving `roles` to a group's members in `company`:
    /// role-update permission; no full-access or engagement role; with a
    /// company, only keys that support company scope; and the actor holds
    /// every key there (the subset rule `validate_assignment` applies).
    async fn check_group_grant(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        source: &str,
        company: Option<i64>,
        roles: &[i64],
    ) -> Result<()> {
        self.verify_user(client, scope, scope.user()).await?;
        authorize_key(client, scope, "admin.authz.role.update").await?;
        if let Some(company) = company {
            let company =
                bilibi_party_ids::OrganisationId::new(company).map_err(|_| Error::Invalid)?;
            self.parties
                .organisation_target(client, scope, company, "admin.authz.role.update")
                .await?;
        }
        let actor =
            crate::effective_capabilities_in_company(client, scope, scope.user(), company).await?;
        for role in roles {
            let row = client
                .query_opt(
                    "SELECT name,grant_all,requires_engagement FROM roles WHERE tenant_id=$1 AND id=$2",
                    &[&scope.tenant(), role],
                )
                .await?
                .ok_or(Error::NotFound)?;
            let name: String = row.get(0);
            if row.get::<_, bool>(1) {
                return Err(Error::Refused(format!(
                    "{name} gives full access, so a {source} cannot carry it. Give it to a person instead."
                )));
            }
            if row.get::<_, bool>(2) {
                return Err(Error::Refused(format!(
                    "{name} needs a company and a purpose given by hand, so a {source} cannot carry it."
                )));
            }
            for key in client
                .query(
                    "SELECT capability,reach FROM role_capabilities WHERE tenant_id=$1 AND role_id=$2",
                    &[&scope.tenant(), role],
                )
                .await?
            {
                let reach = crate::Reach::parse(key.get(1)).unwrap_or(crate::Reach::All);
                let key: &str = key.get(0);
                self.staff_key(key)?;
                if company.is_some()
                    && !self
                        .vocabulary
                        .capabilities
                        .get(key)
                        .is_some_and(|c| c.definition.company_scoped)
                {
                    return Err(Error::Refused(format!(
                        "{name} has permissions that apply across the organisation, so a {source} with a company cannot carry it."
                    )));
                }
                // A team role carries its reach (#683): the actor holds the
                // key at least that far.
                if !actor.reaches(key, reach) {
                    return Err(Error::Forbidden);
                }
            }
        }
        Ok(())
    }
    fn step_up_target(group: &Group<'_>, what: &str) -> String {
        format!("{}:{} {what}", group.source, group.id)
    }
    /// The roles this actor may give a group in `company` now: visible to
    /// the actor, neither full-access nor engagement, every key held there
    /// and, with a company, every key supporting company scope. For the
    /// owner's role picker; the write checks again.
    pub async fn group_role_choices(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        company: Option<i64>,
    ) -> Result<Vec<RoleSummary>> {
        // A read for a picker: nothing is logged; the write decides again.
        if scope.principal().is_some()
            || !crate::effective_capabilities(client, scope)
                .await?
                .contains("admin.authz.role.update")
        {
            return Ok(Vec::new());
        }
        let companies = self.visible_companies(client, scope).await?;
        let actor =
            crate::effective_capabilities_in_company(client, scope, scope.user(), company).await?;
        let rows = client
            .query(
                &format!("SELECT {}, COALESCE((SELECT array_agg(c.reach || ':' || c.capability) FROM role_capabilities c WHERE c.tenant_id=r.tenant_id AND c.role_id=r.id),'{{}}') FROM roles r WHERE r.tenant_id=$1 AND (r.company_id IS NULL OR r.company_id=ANY($2)) AND NOT r.grant_all AND NOT r.requires_engagement ORDER BY lower(r.name),r.id", crate::roles::SUMMARY),
                &[&scope.tenant(), &companies],
            )
            .await?;
        Ok(rows
            .iter()
            .filter(|row| {
                let grants: Vec<String> = row.get(9);
                !grants.is_empty()
                    && grants.iter().all(|grant| {
                        let (reach, key) = grant.split_once(':').unwrap_or(("all", grant));
                        self.vocabulary.capabilities.get(key).is_some_and(|c| {
                            c.definition.audience != crate::Audience::Portal
                                && (company.is_none() || c.definition.company_scoped)
                        }) && actor
                            .reaches(key, crate::Reach::parse(reach).unwrap_or(crate::Reach::All))
                    })
            })
            .map(crate::roles::summary)
            .collect())
    }
    /// Checks giving (`attached`) or taking away one role of a group, on the
    /// caller's read connection, for the owner's review before it writes.
    pub async fn validate_group_role(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        group: &Group<'_>,
        role: i64,
        attached: bool,
    ) -> Result<()> {
        group.check()?;
        if attached {
            self.check_group_grant(client, scope, group.source, group.company, &[role])
                .await
        } else {
            self.verify_user(client, scope, scope.user()).await?;
            authorize_key(client, scope, "admin.authz.role.update").await
        }
    }
    /// Gives or takes away one role of a group inside the owner's audited
    /// transaction: the same checks again, then step-up, then the write. The
    /// audit entry is `{source}_role` under the group's id.
    pub async fn set_group_role_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        group: &Group<'_>,
        role: i64,
        attached: bool,
    ) -> Result<()> {
        group.check()?;
        if tx.context().tenant != scope.tenant() {
            return Err(Error::Forbidden);
        }
        let tenant = scope.tenant();
        if let Some(held) = stored(tx.sql(), tenant, group, true).await? {
            if held.company != group.company {
                return Err(Error::Conflict);
            }
            if held.roles.contains(&role) == attached {
                return Err(Error::Conflict);
            }
        } else if !attached {
            return Err(Error::Conflict);
        }
        tx.sql()
            .query_opt(
                "SELECT 1 FROM roles WHERE tenant_id=$1 AND id=$2 FOR SHARE",
                &[&tenant, &role],
            )
            .await?
            .ok_or(Error::NotFound)?;
        self.validate_group_role(tx.sql(), scope, group, role, attached)
            .await?;
        self.step_up
            .require(
                scope,
                crate::step_up::PRINCIPAL_ROLE,
                &Self::step_up_target(group, &format!("role:{role} attached:{attached}")),
            )
            .await?;
        let (source, id, label, company) = (
            group.source.to_owned(),
            group.id,
            group.label.to_owned(),
            group.company,
        );
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute("INSERT INTO authz_groups(tenant_id,source,group_id,label,company_id) VALUES($1,$2,$3,$4,$5) ON CONFLICT(tenant_id,source,group_id) DO UPDATE SET label=EXCLUDED.label", &[&tenant,&source,&id,&label,&company]).await?;
            if attached {
                sql.execute("INSERT INTO authz_group_roles(tenant_id,source,group_id,role_id) VALUES($1,$2,$3,$4)", &[&tenant,&source,&id,&role]).await?;
            } else {
                sql.execute("DELETE FROM authz_group_roles WHERE tenant_id=$1 AND source=$2 AND group_id=$3 AND role_id=$4", &[&tenant,&source,&id,&role]).await?;
            }
            let name: String = sql.query_one("SELECT name FROM roles WHERE tenant_id=$1 AND id=$2", &[&tenant,&role]).await?.get(0);
            let side = |on: bool| if on { json!({"role_id":role,"role":name,"company_id":company}) } else { json!({}) };
            Ok(((), vec![Change { kind: format!("{source}_role"), id, old: side(!attached), new: side(attached) }]))
        })).await
    }
    /// Checks adding `added` to a group on the caller's read connection. A
    /// group that carries roles gives them to every new member, so this is
    /// the same grant check as giving the roles; a group without roles adds
    /// nothing here.
    pub async fn validate_group_members(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        group: &Group<'_>,
        added: &[i64],
    ) -> Result<()> {
        group.check()?;
        if added.is_empty() {
            return Ok(());
        }
        let Some(held) = stored(client, scope.tenant(), group, false).await? else {
            return Ok(());
        };
        if held.roles.is_empty() {
            return Ok(());
        }
        self.check_group_grant(client, scope, group.source, held.company, &held.roles)
            .await
    }
    /// Applies a membership change inside the owner's audited transaction.
    /// Additions to a group that carries roles pass the grant check and
    /// step-up first; removals only narrow and are never checked. The owner
    /// has already proved each added account is live staff; it is proved
    /// again here before it can hold a role.
    pub async fn set_group_members_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        group: &Group<'_>,
        added: &[i64],
        removed: &[i64],
    ) -> Result<()> {
        group.check()?;
        if tx.context().tenant != scope.tenant() {
            return Err(Error::Forbidden);
        }
        let added: Vec<i64> = added
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let removed: Vec<i64> = removed
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if added.is_empty() && removed.is_empty() {
            return Ok(());
        }
        if added.iter().chain(&removed).any(|user| *user <= 0) {
            return Err(Error::Invalid);
        }
        let tenant = scope.tenant();
        let held = stored(tx.sql(), tenant, group, true).await?;
        if held
            .as_ref()
            .is_some_and(|held| held.company != group.company)
        {
            return Err(Error::Conflict);
        }
        let carries = held.as_ref().is_some_and(|held| !held.roles.is_empty());
        if carries && !added.is_empty() {
            self.validate_group_members(tx.sql(), scope, group, &added)
                .await?;
            if self
                .authentication
                .live_staff(tx.sql(), scope, &added)
                .await?
                != added
            {
                return Err(Error::Invalid);
            }
            self.step_up
                .require(
                    scope,
                    crate::step_up::PRINCIPAL_ROLE,
                    &Self::step_up_target(group, &format!("members:+{added:?}")),
                )
                .await?;
        }
        let (source, id, label, company) = (
            group.source.to_owned(),
            group.id,
            group.label.to_owned(),
            group.company,
        );
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute("INSERT INTO authz_groups(tenant_id,source,group_id,label,company_id) VALUES($1,$2,$3,$4,$5) ON CONFLICT(tenant_id,source,group_id) DO UPDATE SET label=EXCLUDED.label", &[&tenant,&source,&id,&label,&company]).await?;
            sql.execute("INSERT INTO authz_group_members(tenant_id,source,group_id,user_id) SELECT $1,$2,$3,unnest($4::bigint[]) ON CONFLICT DO NOTHING", &[&tenant,&source,&id,&added]).await?;
            sql.execute("DELETE FROM authz_group_members WHERE tenant_id=$1 AND source=$2 AND group_id=$3 AND user_id=ANY($4)", &[&tenant,&source,&id,&removed]).await?;
            Ok(((), vec![Change { kind: "authz.group_members".into(), id, old: json!({"source":source,"removed":removed}), new: json!({"source":source,"added":added}) }]))
        })).await
    }
    /// Checks a group's new company on the caller's read connection: moving
    /// a group that carries roles gives them in the new company (or
    /// tenant-wide), so it is the same grant check as giving them.
    pub async fn validate_group_move(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        group: &Group<'_>,
    ) -> Result<()> {
        group.check()?;
        match stored(client, scope.tenant(), group, false).await? {
            Some(held) if held.company != group.company && !held.roles.is_empty() => {
                self.check_group_grant(client, scope, group.source, group.company, &held.roles)
                    .await
            }
            _ => Ok(()),
        }
    }
    /// Records the group's name and company after the owner changed them,
    /// inside the owner's audited transaction. A new company for a group that
    /// carries roles passes the grant check and step-up first; a new name
    /// needs nothing.
    pub async fn sync_group_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        group: &Group<'_>,
    ) -> Result<()> {
        group.check()?;
        if tx.context().tenant != scope.tenant() {
            return Err(Error::Forbidden);
        }
        let tenant = scope.tenant();
        let Some(held) = stored(tx.sql(), tenant, group, true).await? else {
            return Ok(());
        };
        let label: String = tx
            .sql()
            .query_one(
                "SELECT label FROM authz_groups WHERE tenant_id=$1 AND source=$2 AND group_id=$3",
                &[&tenant, &group.source, &group.id],
            )
            .await?
            .get(0);
        if label == group.label && held.company == group.company {
            return Ok(());
        }
        if held.company != group.company && !held.roles.is_empty() {
            self.validate_group_move(tx.sql(), scope, group).await?;
            self.step_up
                .require(
                    scope,
                    crate::step_up::PRINCIPAL_ROLE,
                    &Self::step_up_target(group, &format!("company:{:?}", group.company)),
                )
                .await?;
        }
        let (source, id, new_label, company, old_company) = (
            group.source.to_owned(),
            group.id,
            group.label.to_owned(),
            group.company,
            held.company,
        );
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute("UPDATE authz_groups SET label=$4,company_id=$5 WHERE tenant_id=$1 AND source=$2 AND group_id=$3", &[&tenant,&source,&id,&new_label,&company]).await?;
            Ok(((), vec![Change { kind: "authz.group".into(), id, old: json!({"source":source,"label":label,"company_id":old_company}), new: json!({"source":source,"label":new_label,"company_id":company}) }]))
        })).await
    }
}
