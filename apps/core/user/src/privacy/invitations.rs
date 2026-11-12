//! Membership invitations under erasure and retention: an invitation and the
//! account prepared for it are anonymised together, so a legal hold on
//! either one keeps both.
use super::{INVITATION_KIND, USER_KIND};
use bilibi_database::Change;
use bilibi_personal_data::{Erasure, Holds, ERASED};
use serde_json::json;

/// The accounts among `users` whose accepted invitation a legal hold names,
/// as (invitation, account, hold). Anonymising an account anonymises its
/// accepted invitations, so a held one keeps its whole account, and the next
/// run after release finishes both.
pub(crate) async fn held_accepted_invitations(
    sql: &tokio_postgres::Transaction<'_>,
    holds: &Holds,
    grant: &Erasure,
    tenant: i64,
    users: &[i64],
) -> bilibi_database::Result<Vec<(i64, i64, i64)>> {
    let rows: Vec<(i64, i64)> = sql
        .query(
            "SELECT id,user_id FROM user_invitations WHERE tenant_id=$1 AND user_id=ANY($2) AND accepted_at IS NOT NULL AND email NOT LIKE 'erased-inv-%@erased.invalid' ORDER BY id FOR UPDATE",
            &[&tenant, &users],
        )
        .await?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    let held = holds.records(grant, INVITATION_KIND, &ids).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, user)| held.get(&id).map(|hold| (id, user, *hold)))
        .collect())
}

/// The invitations among `invitations` whose prepared (sign-in-less) account
/// a legal hold names, on the account or its person, as (invitation,
/// account, hold). Anonymising an invitation anonymises that account, so a
/// held account keeps its invitation, and the next run after release
/// finishes both.
pub(crate) async fn held_prepared_accounts(
    sql: &tokio_postgres::Transaction<'_>,
    holds: &Holds,
    grant: &Erasure,
    tenant: i64,
    invitations: &[i64],
) -> bilibi_database::Result<Vec<(i64, i64, i64)>> {
    let rows: Vec<(i64, i64, Option<i64>)> = sql
        .query(
            "SELECT v.id,u.id,u.person_id FROM user_invitations v JOIN users u ON u.tenant_id=v.tenant_id AND u.id=v.user_id \
             WHERE v.tenant_id=$1 AND v.id=ANY($2) AND u.identity_id IS NULL ORDER BY v.id FOR UPDATE OF u",
            &[&tenant, &invitations],
        )
        .await?
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    let users: Vec<i64> = rows.iter().map(|(_, user, _)| *user).collect();
    let persons: Vec<i64> = rows.iter().filter_map(|(_, _, person)| *person).collect();
    let held = holds.records(grant, USER_KIND, &users).await?;
    let people = holds.persons(grant, &persons).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, user, person)| {
            held.get(&user)
                .or_else(|| person.and_then(|person| people.get(&person)))
                .map(|hold| (id, user, *hold))
        })
        .collect())
}

/// Anonymises the accepted invitation(s) of an account being closed.
pub(crate) async fn anonymise_accepted_invitations(
    sql: &tokio_postgres::Transaction<'_>,
    tenant: i64,
    user: i64,
) -> bilibi_database::Result<(Vec<Change>, Vec<(&'static str, i64)>)> {
    let ids: Vec<i64> = sql
        .query(
            "SELECT id FROM user_invitations WHERE tenant_id=$1 AND user_id=$2 AND accepted_at IS NOT NULL AND email NOT LIKE 'erased-inv-%@erased.invalid' ORDER BY id FOR UPDATE",
            &[&tenant, &user],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    anonymise_invitations(sql, tenant, &ids).await
}

/// Anonymises invitations (the caller holds their rows locked): the address
/// goes, an unaccepted one is closed, and the account prepared for it, which
/// has no sign-in, is closed and loses its name. Returns the changes and the
/// records they touched.
pub(crate) async fn anonymise_invitations(
    sql: &tokio_postgres::Transaction<'_>,
    tenant: i64,
    ids: &[i64],
) -> bilibi_database::Result<(Vec<Change>, Vec<(&'static str, i64)>)> {
    let mut changes = Vec::new();
    let mut records = Vec::new();
    for row in sql
        .query(
            "UPDATE user_invitations v SET email='erased-inv-'||v.id||'@erased.invalid',revoked_at=CASE WHEN v.accepted_at IS NULL THEN coalesce(v.revoked_at,now()) END \
             FROM (SELECT id,email,user_id FROM user_invitations WHERE tenant_id=$1 AND id=ANY($2)) o WHERE v.id=o.id RETURNING v.id,o.email,o.user_id",
            &[&tenant, &ids],
        )
        .await?
    {
        let (id, email, user): (i64, String, i64) = (row.get(0), row.get(1), row.get(2));
        changes.push(Change {
            kind: INVITATION_KIND.into(),
            id,
            old: json!({"email": email}),
            new: json!({"email": ERASED}),
        });
        records.push((INVITATION_KIND, id));
        if let Some(row) = sql
            .query_opt(
                "UPDATE users u SET name=$3,status='deleted',deleted_at=coalesce(u.deleted_at,now()),anonymised_at=now(),version=u.version+1,updated_at=now() \
                 FROM (SELECT id,name FROM users WHERE tenant_id=$1 AND id=$2) o WHERE u.tenant_id=$1 AND u.id=o.id AND u.identity_id IS NULL RETURNING o.name",
                &[&tenant, &user, &ERASED],
            )
            .await?
        {
            changes.push(Change {
                kind: USER_KIND.into(),
                id: user,
                old: json!({"name": row.get::<_, String>(0)}),
                new: json!({"name": ERASED, "status": "deleted"}),
            });
            records.push((USER_KIND, user));
        }
    }
    Ok((changes, records))
}
