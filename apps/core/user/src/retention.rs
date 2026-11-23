//! User's record classes (ADR 05, `base/privacy` retention). Each account
//! follows its company's settings (the primary company's for an account of
//! no company), and a notification its recipient's. Closed accounts are
//! anonymised once the period has passed since they were closed: the row and
//! its ID stay, while its tenant-owned name and sign-in links do not. The
//! installation-owned sign-in email is outside tenant retention. Notifications, which
//! have no business value, are purged once the period has passed since they
//! were sent. An invitation never accepted is closed once the period has passed
//! since it expired or was revoked, and its address and the sign-in-less
//! account prepared for it are anonymised. Neither is swept until a company turns it on and sets a period,
//! and both skip what legal holds name: the record, or the account's person.
//! A held accepted invitation keeps its account, and a held prepared account
//! its invitation, so the run after release finishes both.
use bilibi_database::{Change, MutationTransaction};
use bilibi_personal_data::{ErasedRecord, Erasure, HandlerFuture, Holds, ERASED};
use bilibi_privacy::{Companies, Sweeper, Swept};
use bilibi_tenancy::Scope;
use serde_json::json;
use std::collections::BTreeMap;

const USER_KIND: &str = "user";
const NOTIFICATION_KIND: &str = "notification";

/// Which of `rows` (record ID, its person) a hold keeps.
async fn held(
    client: &dyn bilibi_tenancy::ReadClient,
    holds: &Holds,
    grant: &Erasure,
    kind: &str,
    rows: &[(i64, Option<i64>)],
) -> bilibi_database::Result<Vec<i64>> {
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    let persons: Vec<i64> = rows.iter().filter_map(|(_, person)| *person).collect();
    let records = holds.records(client, grant, kind, &ids).await?;
    let people: BTreeMap<i64, i64> = holds.persons(client, grant, &persons).await?;
    Ok(rows
        .iter()
        .filter(|(id, person)| {
            records.contains_key(id) || person.is_some_and(|p| people.contains_key(&p))
        })
        .map(|(id, _)| *id)
        .collect())
}
fn next(rows: &[(i64, Option<i64>)], limit: i64) -> Option<i64> {
    (rows.len() as i64 == limit)
        .then(|| rows.last().map(|(id, _)| *id))
        .flatten()
}

struct ClosedAccounts;
static CLOSED_ACCOUNTS: ClosedAccounts = ClosedAccounts;
impl Sweeper for ClosedAccounts {
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
            let tenant = scope.tenant();
            let (company, primary, listed) = companies.parameters();
            let rows = tx
                .sql()
                .query(
                    &format!(
                        "SELECT u.id,u.person_id,u.name FROM users u WHERE u.tenant_id=$1 AND u.status='deleted' AND (u.anonymised_at IS NULL OR u.name<>$5) \
                         AND u.deleted_at < $2::text::timestamptz AND u.id>$3 AND {} ORDER BY u.id LIMIT $4 FOR UPDATE OF u",
                        Companies::predicate("organisation_id", 6)
                    ),
                    &[&tenant, &cutoff, &after, &limit, &ERASED, &company, &primary, &listed],
                )
                .await?;
            let keys: Vec<(i64, Option<i64>)> =
                rows.iter().map(|row| (row.get(0), row.get(1))).collect();
            let mut kept = held(tx.sql(), holds, grant, USER_KIND, &keys).await?;
            // A held accepted invitation keeps its account.
            let open: Vec<i64> = keys
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| !kept.contains(id))
                .collect();
            for (_, user, _) in crate::privacy::invitations::held_accepted_invitations(
                tx.sql(),
                holds,
                grant,
                tenant,
                &open,
            )
            .await?
            {
                if !kept.contains(&user) {
                    kept.push(user);
                }
            }
            let due: Vec<(i64, String)> = rows
                .iter()
                .filter(|row| !kept.contains(&row.get::<_, i64>(0)))
                .map(|row| (row.get(0), row.get(2)))
                .collect();
            let persons: std::collections::BTreeMap<i64, Option<i64>> =
                keys.iter().copied().collect();
            let mut swept = Swept {
                kept: kept.len() as i64,
                next: next(&keys, limit),
                ..Swept::default()
            };
            // A write must return changes: nothing to act on is not a write.
            if due.is_empty() {
                return Ok(swept);
            }
            // A sign-in link a legal hold names stays with its account.
            let due_users: Vec<i64> = due.iter().map(|(user, _)| *user).collect();
            let identities: Vec<i64> = tx
                .sql()
                .query(
                    "SELECT id FROM user_external_identities WHERE tenant_id=$1 AND user_id=ANY($2) FOR UPDATE",
                    &[&tenant, &due_users],
                )
                .await?
                .iter()
                .map(|row| row.get(0))
                .collect();
            let keep_identities: Vec<i64> = holds
                .records(
                    tx.sql(),
                    grant,
                    crate::privacy::EXTERNAL_IDENTITY_KIND,
                    &identities,
                )
                .await?
                .into_keys()
                .collect();
            let (ids, extra): (Vec<i64>, Vec<(&'static str, i64)>) = tx
                .mutate(move |sql| {
                    Box::pin(async move {
                        let mut ids = Vec::with_capacity(due.len());
                        let mut changes = Vec::with_capacity(due.len());
                        let mut extra = Vec::new();
                        for (user, name) in due {
                            sql.execute(
                                "UPDATE users SET name=$3,anonymised_at=now(),version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                                &[&tenant, &user, &ERASED],
                            )
                            .await?;
                            let links =
                                crate::privacy::remove_links(sql, tenant, user, &keep_identities)
                                    .await?;
                            changes.push(Change {
                                kind: USER_KIND.into(),
                                id: user,
                                old: json!({"name": name}),
                                new: json!({"name": ERASED, "links_removed": links.len()}),
                            });
                            changes.extend(links);
                            let (invitation_changes, records) =
                                crate::privacy::invitations::anonymise_accepted_invitations(sql, tenant, user)
                                    .await?;
                            changes.extend(invitation_changes);
                            extra.extend(records);
                            ids.push(user);
                        }
                        Ok(((ids, extra), changes))
                    })
                })
                .await?;
            swept.acted = ids.len() as i64;
            swept.records = ids
                .into_iter()
                .map(|user| {
                    let person = persons.get(&user).copied().flatten();
                    ErasedRecord {
                        kind: USER_KIND.into(),
                        id: user,
                        person,
                    }
                })
                .chain(extra.into_iter().map(|(kind, id)| ErasedRecord {
                    kind: kind.into(),
                    id,
                    person: None,
                }))
                .collect();
            Ok(swept)
        })
    }
}

struct LapsedInvitations;
static LAPSED_INVITATIONS: LapsedInvitations = LapsedInvitations;
impl Sweeper for LapsedInvitations {
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
            let tenant = scope.tenant();
            let (company, primary, listed) = companies.parameters();
            let rows = tx
                .sql()
                .query(
                    &format!(
                        "SELECT v.id FROM user_invitations v JOIN users u ON u.tenant_id=v.tenant_id AND u.id=v.user_id \
                         WHERE v.tenant_id=$1 AND v.accepted_at IS NULL AND v.email NOT LIKE 'erased-inv-%@erased.invalid' \
                         AND least(v.expires_at, coalesce(v.revoked_at, v.expires_at)) < $2::text::timestamptz AND v.id>$3 AND {} ORDER BY v.id LIMIT $4 FOR UPDATE OF v",
                        Companies::predicate("u.organisation_id", 5)
                    ),
                    &[&tenant, &cutoff, &after, &limit, &company, &primary, &listed],
                )
                .await?;
            let keys: Vec<(i64, Option<i64>)> = rows.iter().map(|row| (row.get(0), None)).collect();
            let mut kept = held(
                tx.sql(),
                holds,
                grant,
                crate::privacy::INVITATION_KIND,
                &keys,
            )
            .await?;
            // A held prepared account keeps its invitation.
            let open: Vec<i64> = keys
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| !kept.contains(id))
                .collect();
            for (invitation, _, _) in crate::privacy::invitations::held_prepared_accounts(
                tx.sql(),
                holds,
                grant,
                tenant,
                &open,
            )
            .await?
            {
                kept.push(invitation);
            }
            let due: Vec<i64> = open.into_iter().filter(|id| !kept.contains(id)).collect();
            let mut swept = Swept {
                kept: kept.len() as i64,
                next: next(&keys, limit),
                ..Swept::default()
            };
            // A write must return changes: nothing to act on is not a write.
            if due.is_empty() {
                return Ok(swept);
            }
            let acted = due.len() as i64;
            let records = tx
                .mutate(move |sql| {
                    Box::pin(async move {
                        let (changes, records) =
                            crate::privacy::invitations::anonymise_invitations(sql, tenant, &due)
                                .await?;
                        Ok((records, changes))
                    })
                })
                .await?;
            swept.acted = acted;
            swept.records = records
                .into_iter()
                .map(|(kind, id)| ErasedRecord {
                    kind: kind.into(),
                    id,
                    person: None,
                })
                .collect();
            Ok(swept)
        })
    }
}

struct Notifications;
static NOTIFICATIONS: Notifications = Notifications;
impl Sweeper for Notifications {
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
            let tenant = scope.tenant();
            // A notification follows its recipient's company.
            let (company, primary, listed) = companies.parameters();
            let rows = tx
                .sql()
                .query(
                    &format!(
                        "SELECT n.id,u.person_id FROM notifications n JOIN users u ON u.tenant_id=n.tenant_id AND u.id=n.user_id \
                         WHERE n.tenant_id=$1 AND n.created_at < $2::text::timestamptz AND n.id>$3 AND {} ORDER BY n.id LIMIT $4 FOR UPDATE OF n",
                        Companies::predicate("u.organisation_id", 5)
                    ),
                    &[&tenant, &cutoff, &after, &limit, &company, &primary, &listed],
                )
                .await?;
            let keys: Vec<(i64, Option<i64>)> =
                rows.iter().map(|row| (row.get(0), row.get(1))).collect();
            let kept = held(tx.sql(), holds, grant, NOTIFICATION_KIND, &keys).await?;
            let due: Vec<i64> = keys
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| !kept.contains(id))
                .collect();
            let mut swept = Swept {
                kept: kept.len() as i64,
                next: next(&keys, limit),
                ..Swept::default()
            };
            // A write must return changes: nothing to act on is not a write.
            if due.is_empty() {
                return Ok(swept);
            }
            let ids: Vec<i64> = tx
                .mutate(move |sql| {
                    Box::pin(async move {
                        let rows = sql
                            .query(
                                "DELETE FROM notifications WHERE tenant_id=$1 AND id = ANY($2) RETURNING id,user_id",
                                &[&tenant, &due],
                            )
                            .await?;
                        let changes = rows
                            .iter()
                            .map(|row| Change {
                                kind: NOTIFICATION_KIND.into(),
                                id: row.get(0),
                                old: json!({"recipient": row.get::<_, i64>(1)}),
                                new: json!({}),
                            })
                            .collect();
                        Ok((rows.iter().map(|row| row.get(0)).collect(), changes))
                    })
                })
                .await?;
            swept.acted = ids.len() as i64;
            swept.records = ids
                .into_iter()
                .map(|id| ErasedRecord {
                    kind: NOTIFICATION_KIND.into(),
                    id,
                    person: None,
                })
                .collect();
            Ok(swept)
        })
    }
}

bilibi_privacy::record_class! {
    module: "core/user", key: "core/user.closed_accounts", label: "Closed user accounts",
    clock_start: "when the account was closed", action: Anonymise,
    business_value: true, floor_days: None, sweeper: &CLOSED_ACCOUNTS,
}
bilibi_privacy::record_class! {
    module: "core/user", key: "core/user.notifications", label: "Notifications",
    clock_start: "when the notification was sent", action: Purge,
    business_value: false, floor_days: None, sweeper: &NOTIFICATIONS,
}
bilibi_privacy::record_class! {
    module: "core/user", key: "core/user.lapsed_invitations", label: "Lapsed membership invitations",
    clock_start: "when the invitation expired or was revoked", action: Anonymise,
    business_value: false, floor_days: None, sweeper: &LAPSED_INVITATIONS,
}
