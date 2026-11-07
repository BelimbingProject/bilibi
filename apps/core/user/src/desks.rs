//! Staff desks: an extension to Core User's account panels and
//! Core Company's directory. One operation serves company, person and agents.
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
use serde_json::{json, Value};
use tokio_postgres::GenericClient;

impl bilibi_company::WorkAccess for crate::UserService {
    fn companies<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
    ) -> ProofFuture<'a, Vec<OrganisationId>> {
        Box::pin(async move {
            if scope.actor().kind() != bilibi_tenancy::AccountKind::Staff {
                return Ok(Vec::new());
            }
            let Some(person) = scope.actor().person() else {
                return Ok(Vec::new());
            };
            let row=client.query_opt("SELECT ARRAY(SELECT company_id FROM user_desks WHERE tenant_id=$1 AND person_id=$2)", &[&scope.tenant(), &person.get()]).await?.ok_or(Error::Internal)?;
            row.get::<_, Vec<i64>>(0)
                .into_iter()
                .map(|id| OrganisationId::new(id).map_err(|_| Error::Internal))
                .collect()
        })
    }
    fn reaches<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        company: OrganisationId,
    ) -> ProofFuture<'a, bool> {
        Box::pin(async move {
            if scope.actor().kind() != bilibi_tenancy::AccountKind::Staff {
                return Ok(false);
            }
            let Some(person) = scope.actor().person() else {
                return Ok(false);
            };
            Ok(client.query_opt("SELECT id FROM user_desks WHERE tenant_id=$1 AND person_id=$2 AND company_id=$3 FOR KEY SHARE", &[&scope.tenant(), &person.get(), &company.get()]).await?.is_some())
        })
    }
}
/// Owner checks shared by preview and commit; the latter locks the company
/// and person so retirement or archival cannot race the assignment. An
/// archived company keeps its desks (they let their people read its records)
/// and takes no new one, but a desk there can still be removed, so access to
/// an archived company only ever narrows.
pub(crate) async fn check(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    company: i64,
    person: i64,
    lock: bool,
    assigned: bool,
) -> Result<()> {
    bilibi_company::authorize_target(client, scope, company, bilibi_company::UPDATE).await?;
    if assigned {
        writable(client, scope.tenant(), company, lock).await?;
    } else {
        let suffix = if lock { " FOR NO KEY UPDATE" } else { "" };
        client
            .query_opt(
                &format!("SELECT id FROM companies WHERE tenant_id=$1 AND id=$2 AND deleted_at IS NULL{suffix}"),
                &[&scope.tenant(), &company],
            )
            .await?
            .ok_or(Error::Forbidden)?;
    }
    let suffix = if lock { " FOR UPDATE OF p" } else { "" };
    client.query_opt(&format!("SELECT p.id FROM persons p WHERE p.tenant_id=$1 AND p.id=$2 AND p.retired_at IS NULL AND EXISTS(SELECT 1 FROM users u WHERE u.tenant_id=p.tenant_id AND u.person_id=p.id AND u.kind='staff' AND u.deleted_at IS NULL){suffix}"), &[&scope.tenant(), &person]).await?.ok_or(Error::NotFound)?;
    Ok(())
}
pub(crate) async fn set(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    company: i64,
    person: i64,
) -> Result<Value> {
    check(tx.sql(), scope, company, person, true, true).await?;
    write_in(tx, scope, company, person).await
}
pub(crate) async fn write_in(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    company: i64,
    person: i64,
) -> Result<Value> {
    write_record(tx, scope.tenant(), company, person).await
}
/// System provisioning is already authorized by the locked SSO/SCIM connection.
/// It creates the first desk in the same transaction as that new account.
pub(crate) async fn provision_in(
    tx: &mut MutationTransaction<'_>,
    company: i64,
    person: i64,
) -> Result<Value> {
    if tx.context().actor.kind != "system" {
        return Err(Error::Forbidden);
    }
    write_record(tx, tx.context().tenant, company, person).await
}
/// The one archived guard every new desk passes, whichever path creates it
/// (assignment, a new account, provider provisioning): a desk is read access
/// to the company's records, so an archived company never takes a new one.
/// The company row is locked against a concurrent archive.
async fn writable(
    client: &(impl GenericClient + Sync),
    tenant: i64,
    company: i64,
    lock: bool,
) -> Result<()> {
    match bilibi_company::require_writable_in(client, tenant, company, lock).await {
        Ok(_) => Ok(()),
        Err(bilibi_company::WriteError::Access(error)) => Err(error),
        Err(_) => Err(Error::Forbidden),
    }
}
async fn write_record(
    tx: &mut MutationTransaction<'_>,
    tenant: i64,
    company: i64,
    person: i64,
) -> Result<Value> {
    writable(tx.sql(), tenant, company, true).await?;
    let value = json!({"person_id":person,"company_id":company,"assigned":true});
    let result = value.clone();
    tx.mutate(move |sql| Box::pin(async move {
        let row = sql.query_opt("INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES($1,$2,$3) ON CONFLICT(tenant_id,person_id,company_id) DO NOTHING RETURNING id", &[&tenant,&person,&company]).await?;
        let changes = row.into_iter().map(|row| Change {kind:"user.desk".into(), id:row.get(0), old:Value::Null, new:value.clone()}).collect();
        Ok((result,changes))
    })).await
}

/// Removes one person's desk at `company`: that company's provider-sourced
/// roles for the person's accounts are revoked and their sessions end in the
/// same transaction. `false` when the person had no desk there.
async fn remove_desk_in(
    tx: &mut MutationTransaction<'_>,
    service: &crate::UserService,
    scope: &Scope,
    company: OrganisationId,
    person: i64,
) -> Result<bool> {
    let tenant = scope.tenant();
    let company_id = company.get();
    let rows = tx
        .sql()
        .query(
            "SELECT id FROM user_desks WHERE tenant_id=$1 AND company_id=$2 AND person_id=$3 ORDER BY id FOR UPDATE",
            &[&tenant, &company_id, &person],
        )
        .await?;
    if rows.is_empty() {
        return Ok(false);
    }
    let removed_desks: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
    tx.mutate(move |sql| Box::pin(async move {
        sql.execute("DELETE FROM user_desks WHERE tenant_id=$1 AND company_id=$2 AND person_id=$3", &[&tenant,&company_id,&person]).await?;
        sql.execute("UPDATE users SET last_desk_id=NULL WHERE tenant_id=$1 AND person_id=$2 AND last_desk_id=$3", &[&tenant,&person,&company_id]).await?;
        Ok(((),removed_desks.into_iter().map(|id| Change {kind:"user.desk".into(),id,old:json!({"person_id":person,"company_id":company_id}),new:Value::Null}).collect()))
    })).await?;
    let users: Vec<i64> = tx
        .sql()
        .query(
            "SELECT id FROM users WHERE tenant_id=$1 AND person_id=$2 AND kind='staff' AND deleted_at IS NULL ORDER BY id FOR UPDATE",
            &[&tenant, &person],
        )
        .await?
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    service
        .sso_grants
        .revoke_company_identity_roles_in(tx, scope, company, &users)
        .await?;
    let mut ended = Vec::new();
    for user in users {
        ended.extend(service.sessions.revoke(tx, user, None).await?);
    }
    let sessions = service.sessions.clone();
    tx.after_commit(move || sessions.announce(ended));
    Ok(true)
}

impl crate::UserService {
    /// Shared desk write used by the reviewed operation on its audited transaction.
    pub async fn set_desk_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        company: i64,
        person: i64,
        assigned: bool,
    ) -> Result<Value> {
        let target = OrganisationId::new(company).map_err(|_| Error::Invalid)?;
        bilibi_company::lock_work_context_and_target(tx.sql(), scope, target).await?;
        let live = self.authentication.reprove(tx.sql(), scope).await?;
        check(tx.sql(), &live, company, person, true, assigned).await?;
        let result = if assigned {
            write_in(tx, &live, company, person).await?
        } else {
            let removed = remove_desk_in(tx, self, &live, target, person).await?;
            if !removed {
                return Err(Error::Invalid);
            }
            json!({"person_id":person,"company_id":company,"assigned":false})
        };
        Ok(result)
    }
}

impl crate::UserService {
    pub async fn set_desk(
        &self,
        scope: &Scope,
        company: i64,
        person: i64,
        assigned: bool,
    ) -> Result<Value> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .mutations
            .begin(&mut client, scope.mutation_context("user.desk.set"))
            .await?;
        let result = self
            .set_desk_in(&mut tx, scope, company, person, assigned)
            .await?;
        tx.commit().await?;
        Ok(result)
    }
}
