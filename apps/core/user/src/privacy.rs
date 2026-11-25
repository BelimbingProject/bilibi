//! User's personal-data store (ADR 05, `base/personal_data`): the accounts linked to
//! the person, their notifications and action evidence. Erasure closes accounts as
//! `remove_membership` does (membership sessions ended) and anonymises it: the
//! row and its ID stay, while its tenant-owned name and links do not. The
//! installation-owned sign-in email, password, factors, pending sign-ins and
//! failed-attempt counters are outside a customer's erasure. Single sign-on links are deleted, as are
//! notifications, each unless a legal hold names it. The account's SCIM
//! document keeps only its resource ID, and a first administrator's receipt
//! its placeholder email. Accounts that only share an email are counted, never
//! touched. A membership invitation is found by its address: erasure closes it
//! and anonymises the address and the sign-in-less account prepared for it.
//! Session's sign-in history (time, address, device) is security evidence:
//! the export lists it, erasure leaves it and reports it kept, and Session's
//! expiry sweep deletes each record 90 days after the sign-in.
use bilibi_database::{Change, Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_personal_data::{
    Action, Disclosure, Erased, ErasedRecord, Erasure, Exported, Handler, HandlerFuture, Holds,
    Identifier, Kept, KeptReason, KindCount, Located, PersonalData, RecordKind, Subject, ERASED,
    PERSONAL_DATA, RECORD_KINDS,
};
use bilibi_security::Sessions;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use tokio_postgres::Client;

pub(crate) mod invitations;
use invitations::{
    anonymise_accepted_invitations, anonymise_invitations, held_accepted_invitations,
    held_prepared_accounts,
};

const USER_KIND: &str = "user";
pub(crate) const IDENTITY_KIND: &str = "user_identity";
const NOTIFICATION_KIND: &str = "notification";
/// The certificate's name for an account's retained sign-in history; not an
/// audit record kind, so no store erases it.
const SIGN_IN_HISTORY_KIND: &str = "session.sign_in_history";
const AUDIT_ACTIONS_KIND: &str = "audit.account_actions";
/// A membership invitation: the invitee's address, and the name on the
/// account prepared for it, which has no sign-in yet.
pub(crate) const INVITATION_KIND: &str = "user.invitation";
/// The account's SCIM document, as SCIM provisioning records it.
const SCIM_KIND: &str = "user.scim_user";
/// A single sign-on link, as SSO records it.
pub(crate) const EXTERNAL_IDENTITY_KIND: &str = "user.external_identity";
/// The first administrator's receipt, as the bootstrap records it.
const BOOTSTRAP_KIND: &str = "user.bootstrap";
/// An agent's two-factor reset, which once recorded the account's name and
/// email.
const TWO_FACTOR_RESET_KIND: &str = "user_two_factor_reset";
const TWO_FACTOR_RESET_REQUEST_KIND: &str = "user_two_factor_reset_request";
/// Every top-level attribute of a SCIM document: an identity provider may
/// send any attribute or vendor extension, all of them about the person.
const SCIM_FIELDS: [&str; 1] = ["*"];

struct Bound {
    sessions: Arc<Sessions>,
}
static BOUND: OnceLock<Bound> = OnceLock::new();
/// Bound once by the User service factory.
pub(crate) fn bind(sessions: Arc<Sessions>) -> bilibi_module_registry::Result<()> {
    BOUND.set(Bound { sessions }).map_err(|_| {
        bilibi_module_registry::RegistryError::Service("duplicate user privacy binding".into())
    })
}

fn emails(subject: &Subject) -> Vec<String> {
    subject
        .identifiers
        .iter()
        .filter_map(|identifier| match identifier {
            Identifier::Email(value) => Some(value.to_lowercase()),
            Identifier::Phone(_) => None,
        })
        .collect()
}

struct Accounts;
static ACCOUNTS: Accounts = Accounts;
impl Handler for Accounts {
    fn locate<'a>(
        &'a self,
        client: &'a tokio_postgres::Transaction<'_>,
        scope: &'a Scope,
        subject: &'a Subject,
    ) -> HandlerFuture<'a, Located> {
        Box::pin(async move {
            let client = client.client();
            let row = client
                .query_one(
                    "SELECT count(*) FILTER (WHERE u.person_id=$2), count(*) FILTER (WHERE u.person_id IS DISTINCT FROM $2 AND lower(i.email)=ANY($3)), \
                     (SELECT count(*) FROM notifications n JOIN users u ON u.tenant_id=n.tenant_id AND u.id=n.user_id WHERE n.tenant_id=$1 AND u.person_id=$2), \
                     (SELECT count(*) FROM user_two_factor_reset_requests r JOIN users u ON u.tenant_id=r.tenant_id AND u.id=r.user_id WHERE r.tenant_id=$1 AND u.person_id=$2) \
                     FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1",
                    &[&scope.tenant(), &subject.person.get(), &emails(subject)],
                )
                .await?;
            let invitations: i64 = client
                .query_one(
                    "SELECT count(*) FROM user_invitations WHERE tenant_id=$1 AND email=ANY($2)",
                    &[&scope.tenant(), &emails(subject)],
                )
                .await?
                .get(0);
            let users: Vec<i64> = client
                .query(
                    "SELECT id FROM users WHERE tenant_id=$1 AND person_id=$2",
                    &[&scope.tenant(), &subject.person.get()],
                )
                .await?
                .iter()
                .map(|row| row.get(0))
                .collect();
            let actions = bilibi_audit::count_account_actions(client, scope, &users).await?;
            Ok(Located {
                kinds: vec![
                    KindCount {
                        kind: AUDIT_ACTIONS_KIND.into(),
                        by_person: actions,
                        by_identifier: 0,
                    },
                    KindCount {
                        kind: INVITATION_KIND.into(),
                        by_person: 0,
                        by_identifier: invitations,
                    },
                    KindCount {
                        kind: USER_KIND.into(),
                        by_person: row.get(0),
                        by_identifier: row.get(1),
                    },
                    KindCount {
                        kind: NOTIFICATION_KIND.into(),
                        by_person: row.get(2),
                        by_identifier: 0,
                    },
                    KindCount {
                        kind: TWO_FACTOR_RESET_REQUEST_KIND.into(),
                        by_person: row.get(3),
                        by_identifier: 0,
                    },
                ],
                ..Located::default()
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
            let accounts = client
                .query(
                    "SELECT u.id,u.name,i.email,u.status,i.email_verified_at::text,u.deleted_at::text,\
                     coalesce((SELECT jsonb_agg(jsonb_build_object('type',n.type,'title',n.title,'body',n.body,'url',n.url,'read_at',n.read_at::text,'created_at',n.created_at::text) ORDER BY n.id) \
                       FROM notifications n WHERE n.tenant_id=u.tenant_id AND n.user_id=u.id),'[]'::jsonb), \
                     coalesce((SELECT jsonb_agg(s.document ORDER BY s.id) FROM user_scim_users s \
                       WHERE s.tenant_id=u.tenant_id AND s.user_id=u.id),'[]'::jsonb), \
                     coalesce((SELECT jsonb_agg(jsonb_build_object('company',r.company_name,'request_ip',host(r.request_ip),'recipient',r.recipient_kind,'requested_at',r.requested_at::text,'closed_at',r.closed_at::text) ORDER BY r.id) FROM user_two_factor_reset_requests r WHERE r.tenant_id=u.tenant_id AND r.user_id=u.id),'[]'::jsonb) \
                     FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1 AND u.person_id=$2 ORDER BY u.id",
                    &[&scope.tenant(), &subject.person.get()],
                )
                .await?;
            let bound = BOUND.get().ok_or(Error::Internal)?;
            let user_ids: Vec<i64> = accounts.iter().map(|row| row.get(0)).collect();
            let actions =
                bilibi_audit::export_account_actions(client, scope, grant, &user_ids).await?;
            let sign_ins = bound
                .sessions
                .sign_in_history(client, scope, grant, &user_ids)
                .await?;
            let records: Vec<Value> = accounts
                .iter()
                .map(|row| {
                    let user = row.get::<_, i64>(0);
                    let history: Vec<Value> = sign_ins
                        .sign_ins
                        .iter()
                        .filter(|sign_in| sign_in.user_id == user)
                        .map(|sign_in| {
                            json!({
                                "signed_in_at": sign_in.signed_in_at, "device": sign_in.device,
                                "ip_address": sign_in.ip_address, "place": sign_in.place,
                                "user_agent": sign_in.user_agent,
                                "retained_until": sign_in.retained_until,
                            })
                        })
                        .collect();
                    let alerts: Vec<Value> = sign_ins
                        .alerts
                        .iter()
                        .filter(|alert| alert.user_id == user)
                        .map(|alert| json!({"device": alert.device, "alerted_on": alert.alerted_on}))
                        .collect();
                    json!({
                        "sign_ins": history, "new_device_email_claims": alerts,
                        "id": row.get::<_, i64>(0), "name": row.get::<_, String>(1), "email": row.get::<_, String>(2),
                        "status": row.get::<_, String>(3), "email_verified_at": row.get::<_, Option<String>>(4),
                        "deleted_at": row.get::<_, Option<String>>(5), "notifications": row.get::<_, Value>(6),
                        "scim_documents": row.get::<_, Value>(7),
                        "two_factor_reset_requests": row.get::<_, Value>(8),
                    })
                })
                .collect();
            let invitations: Vec<Value> = client
                .query(
                    "SELECT v.id,u.name,v.email,u.status,v.created_at::text,v.expires_at::text,v.accepted_at::text,v.revoked_at::text \
                     FROM user_invitations v JOIN users u ON u.tenant_id=v.tenant_id AND u.id=v.user_id WHERE v.tenant_id=$1 AND v.email=ANY($2) ORDER BY v.id",
                    &[&scope.tenant(), &emails(subject)],
                )
                .await?
                .iter()
                .map(|row| {
                    json!({
                        "id": row.get::<_, i64>(0), "name": row.get::<_, String>(1), "email": row.get::<_, String>(2),
                        "status": row.get::<_, String>(3), "created_at": row.get::<_, String>(4),
                        "expires_at": row.get::<_, String>(5), "accepted_at": row.get::<_, Option<String>>(6),
                        "revoked_at": row.get::<_, Option<String>>(7),
                    })
                })
                .collect();
            Ok(Exported {
                records: json!({"accounts": records, "invitations": invitations, "audit_actions": actions}),
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
            let bound = BOUND.get().ok_or(Error::Internal)?;
            let tenant = scope.tenant();
            let users = tx
                .sql()
                .query(
                    "SELECT u.id,u.name,i.email,u.status,(u.name=$3 AND u.status='deleted'),i.id FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=$1 AND u.person_id=$2 ORDER BY u.id FOR UPDATE OF u",
                    &[&tenant, &subject.person.get(), &ERASED],
                )
                .await?;
            let ids: Vec<i64> = users.iter().map(|row| row.get(0)).collect();
            let held_users = holds.records(tx.sql(), grant, USER_KIND, &ids).await?;
            let mut erased = Erased::default();
            for row in users {
                let (user, name, email, status, done, identity): (
                    i64,
                    String,
                    String,
                    String,
                    bool,
                    i64,
                ) = (
                    row.get(0),
                    row.get(1),
                    row.get(2),
                    row.get(3),
                    row.get(4),
                    row.get(5),
                );
                // The address and credentials belong to the installation,
                // not this customer privacy store. Name the retained shared
                // identity on the certificate instead of claiming its email
                // was erased with this membership.
                erased.kept.push(Kept {
                    kind: IDENTITY_KIND.into(),
                    id: identity.to_string(),
                    reason: KeptReason::Shared,
                    until: None,
                    hold: None,
                });
                if bilibi_audit::count_account_actions(tx.sql(), scope, &[user]).await? > 0 {
                    erased.kept.push(Kept {
                        kind: AUDIT_ACTIONS_KIND.into(),
                        id: user.to_string(),
                        reason: KeptReason::Evidence,
                        until: None,
                        hold: None,
                    });
                }
                if let Some(hold) = held_users.get(&user) {
                    erased.kept.push(Kept::held(USER_KIND, user, *hold));
                    continue;
                }
                let held_accepted =
                    held_accepted_invitations(tx.sql(), holds, grant, tenant, &[user]).await?;
                if let Some((_, _, hold)) = held_accepted.first() {
                    for (invitation, _, hold) in &held_accepted {
                        erased
                            .kept
                            .push(Kept::held(INVITATION_KIND, invitation, *hold));
                    }
                    erased.kept.push(Kept::held(USER_KIND, user, *hold));
                    continue;
                }
                if user == scope.user() {
                    // The approver's own account cannot be the one erased.
                    return Err(Error::Forbidden);
                }
                // The sign-in history (time, address, device) is security
                // evidence: erasure leaves it, and Session's expiry sweep
                // deletes each record 90 days after the sign-in.
                if let Some(until) = bound
                    .sessions
                    .sign_in_retained_until(tx.sql(), scope, grant, user)
                    .await?
                {
                    erased.kept.push(Kept {
                        kind: SIGN_IN_HISTORY_KIND.into(),
                        id: user.to_string(),
                        reason: KeptReason::RetainedUntil,
                        until: Some(until),
                        hold: None,
                    });
                }
                // Each notification on its own: a held one stays.
                let notices: Vec<i64> = tx
                    .sql()
                    .query(
                        "SELECT id FROM notifications WHERE tenant_id=$1 AND user_id=$2 ORDER BY id FOR UPDATE",
                        &[&tenant, &user],
                    )
                    .await?
                    .iter()
                    .map(|row| row.get(0))
                    .collect();
                let held_notices = holds
                    .records(tx.sql(), grant, NOTIFICATION_KIND, &notices)
                    .await?;
                for (notice, hold) in &held_notices {
                    erased
                        .kept
                        .push(Kept::held(NOTIFICATION_KIND, notice, *hold));
                }
                let gone: Vec<i64> = notices
                    .into_iter()
                    .filter(|id| !held_notices.contains_key(id))
                    .collect();
                let requests: Vec<(i64, Option<String>)> = tx
                    .sql()
                    .query(
                        "SELECT id,host(request_ip) FROM user_two_factor_reset_requests WHERE tenant_id=$1 AND user_id=$2 ORDER BY id FOR UPDATE",
                        &[&tenant, &user],
                    )
                    .await?
                    .iter()
                    .map(|row| (row.get(0), row.get(1)))
                    .collect();
                let held_requests = holds
                    .records(
                        tx.sql(),
                        grant,
                        TWO_FACTOR_RESET_REQUEST_KIND,
                        &requests.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                    )
                    .await?;
                for (request, hold) in &held_requests {
                    erased
                        .kept
                        .push(Kept::held(TWO_FACTOR_RESET_REQUEST_KIND, request, *hold));
                }
                let requests: Vec<(i64, String)> = requests
                    .into_iter()
                    .filter_map(|(id, ip)| (!held_requests.contains_key(&id)).then_some((id, ip?)))
                    .collect();
                let request_ids: Vec<i64> = if requests.is_empty() {
                    Vec::new()
                } else {
                    tx.mutate(move |sql| {
                        Box::pin(async move {
                            let ids: Vec<i64> = requests.iter().map(|(id, _)| *id).collect();
                            sql.execute(
                                "UPDATE user_two_factor_reset_requests SET request_ip=NULL WHERE tenant_id=$1 AND id=ANY($2)",
                                &[&tenant, &ids],
                            )
                            .await?;
                            let changes = requests
                                .into_iter()
                                .map(|(id, ip)| Change {
                                    kind: TWO_FACTOR_RESET_REQUEST_KIND.into(),
                                    id,
                                    old: json!({"request_ip": ip}),
                                    new: json!({"request_ip": null}),
                                })
                                .collect();
                            Ok((ids, changes))
                        })
                    })
                    .await?
                };
                let placeholder = format!("erased-{user}@erased.invalid");
                // The account's other records, each kept while a hold names
                // it, and written only while it, or its history, still holds
                // a value: a repeat erasure of a clean account writes nothing.
                let scim_rows: Vec<(i64, bool)> = tx
                    .sql()
                    .query(
                        "SELECT id,document->>'userName'=$3 FROM user_scim_users WHERE tenant_id=$1 AND user_id=$2 ORDER BY id FOR UPDATE",
                        &[&tenant, &user, &placeholder],
                    )
                    .await?
                    .iter()
                    .map(|row| (row.get(0), row.get(1)))
                    .collect();
                let held_scim = holds
                    .records(
                        tx.sql(),
                        grant,
                        SCIM_KIND,
                        &scim_rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                    )
                    .await?;
                let mut scim = Vec::new();
                for (id, clean) in scim_rows {
                    if let Some(hold) = held_scim.get(&id) {
                        erased.kept.push(Kept::held(SCIM_KIND, id, *hold));
                    } else if !clean || bilibi_audit::holds_values(tx, SCIM_KIND, id).await? {
                        scim.push(id);
                    }
                }
                let identities: Vec<i64> = tx
                    .sql()
                    .query(
                        "SELECT id FROM user_external_identities WHERE tenant_id=$1 AND user_id=$2 ORDER BY id FOR UPDATE",
                        &[&tenant, &user],
                    )
                    .await?
                    .iter()
                    .map(|row| row.get(0))
                    .collect();
                let held_identities = holds
                    .records(tx.sql(), grant, EXTERNAL_IDENTITY_KIND, &identities)
                    .await?;
                // Ending an account removes every sign-in link it has (the
                // sign-in factor's lifecycle), so a hold on one keeps the
                // whole account, as a hold on the account does.
                if let Some(hold) = held_identities.values().next() {
                    for (identity, hold) in &held_identities {
                        erased
                            .kept
                            .push(Kept::held(EXTERNAL_IDENTITY_KIND, identity, *hold));
                    }
                    erased.kept.push(Kept::held(USER_KIND, user, *hold));
                    continue;
                }
                let receipt = tx
                    .sql()
                    .query_opt(
                        "SELECT admin_email=$3 FROM user_bootstrap WHERE tenant_id=$1 AND user_id=$2 FOR UPDATE",
                        &[&tenant, &user, &placeholder],
                    )
                    .await?
                    .map(|row| row.get::<_, bool>(0));
                let bootstrap = match receipt {
                    None => false,
                    Some(clean) => {
                        match holds
                            .records(tx.sql(), grant, BOOTSTRAP_KIND, &[1])
                            .await?
                            .get(&1)
                        {
                            Some(hold) => {
                                erased.kept.push(Kept::held(BOOTSTRAP_KIND, 1, *hold));
                                false
                            }
                            None => {
                                !clean || bilibi_audit::holds_values(tx, BOOTSTRAP_KIND, 1).await?
                            }
                        }
                    }
                };
                // Earlier resets recorded the name and email under their own
                // kind: an erasure change of that kind lets the database clear
                // that history too.
                let reset = match holds
                    .records(tx.sql(), grant, TWO_FACTOR_RESET_KIND, &[user])
                    .await?
                    .get(&user)
                {
                    Some(hold) => {
                        erased
                            .kept
                            .push(Kept::held(TWO_FACTOR_RESET_KIND, user, *hold));
                        false
                    }
                    None => bilibi_audit::holds_values(tx, TWO_FACTOR_RESET_KIND, user).await?,
                };
                let user_history = done && bilibi_audit::holds_values(tx, USER_KIND, user).await?;
                let reset_before = json!({"name": name, "email": email});
                let before = json!({"name": name, "status": status});
                let writes = !done
                    || user_history
                    || !gone.is_empty()
                    || !scim.is_empty()
                    || bootstrap
                    || reset;
                let (removed, other) = if !writes {
                    (Vec::new(), Vec::new())
                } else {
                    tx.mutate(move |sql| {
                        Box::pin(async move {
                            let mut changes = Vec::new();
                            // Every other record holding the account's values,
                            // reported so their history is cleared too.
                            let mut other: Vec<(&'static str, i64)> = Vec::new();
                            if !done {
                                sql.execute(
                                    "UPDATE users SET name=$3,status='deleted',deleted_at=coalesce(deleted_at,now()),anonymised_at=now(),version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                                    &[&tenant, &user, &ERASED],
                                )
                                .await?;
                                let links = remove_links(sql, tenant, user, &[]).await?;
                                other.extend(
                                    links
                                        .iter()
                                        .filter(|link| link.kind == EXTERNAL_IDENTITY_KIND)
                                        .map(|link| (EXTERNAL_IDENTITY_KIND, link.id)),
                                );
                                changes.push(Change {
                                    kind: USER_KIND.into(),
                                    id: user,
                                    old: before,
                                    new: json!({"name": ERASED, "status": "deleted", "links_removed": links.len()}),
                                });
                                if let Some(change) =
                                    crate::identity_lifecycle::mark_orphaned(sql, identity).await?
                                {
                                    changes.push(change);
                                }
                                changes.extend(links);
                                let (invitation_changes, records) =
                                    anonymise_accepted_invitations(sql, tenant, user).await?;
                                changes.extend(invitation_changes);
                                other.extend(records);
                            } else if user_history {
                                // Already anonymised, but earlier history still
                                // holds values: this change lets it be cleared.
                                sql.execute(
                                    "UPDATE users SET version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                                    &[&tenant, &user],
                                )
                                .await?;
                                changes.push(Change {
                                    kind: USER_KIND.into(),
                                    id: user,
                                    old: before,
                                    new: json!({"name": ERASED, "status": "deleted"}),
                                });
                                other.push((USER_KIND, user));
                            }
                            // The identity provider keeps the resource ID; its
                            // document keeps nothing about the person.
                            for row in sql
                                .query(
                                    "UPDATE user_scim_users s SET active=false,document=jsonb_build_object('schemas',o.document->'schemas','id',s.resource_id,'userName',$3::text,'active',false) \
                                     FROM (SELECT id,document FROM user_scim_users WHERE tenant_id=$1 AND id=ANY($2)) o WHERE s.id=o.id RETURNING s.id,o.document,s.document",
                                    &[&tenant, &scim, &placeholder],
                                )
                                .await?
                            {
                                let id: i64 = row.get(0);
                                changes.push(Change { kind: SCIM_KIND.into(), id, old: row.get(1), new: row.get(2) });
                                other.push((SCIM_KIND, id));
                            }
                            if bootstrap {
                                let row = sql
                                    .query_one(
                                        "UPDATE user_bootstrap b SET admin_email=$3 FROM (SELECT admin_email FROM user_bootstrap WHERE tenant_id=$1 AND user_id=$2) o \
                                         WHERE b.tenant_id=$1 AND b.user_id=$2 RETURNING o.admin_email",
                                        &[&tenant, &user, &placeholder],
                                    )
                                    .await?;
                                changes.push(Change {
                                    kind: BOOTSTRAP_KIND.into(),
                                    id: 1,
                                    old: json!({"admin_email": row.get::<_, String>(0)}),
                                    new: json!({"admin_email": ERASED}),
                                });
                                other.push((BOOTSTRAP_KIND, 1));
                            }
                            if reset {
                                changes.push(Change {
                                    kind: TWO_FACTOR_RESET_KIND.into(),
                                    id: user,
                                    old: reset_before,
                                    new: json!({"name": ERASED, "email": ERASED}),
                                });
                                other.push((TWO_FACTOR_RESET_KIND, user));
                            }
                            let rows = sql
                                .query("DELETE FROM notifications WHERE tenant_id=$1 AND id=ANY($2) RETURNING id", &[&tenant, &gone])
                                .await?;
                            let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
                            changes.extend(ids.iter().map(|id| Change {
                                kind: NOTIFICATION_KIND.into(),
                                id: *id,
                                old: json!({"recipient": user}),
                                new: json!({}),
                            }));
                            Ok(((ids, other), changes))
                        })
                    })
                    .await?
                };
                if !done {
                    let ended = bound.sessions.revoke(tx, user, None).await?;
                    let sessions = bound.sessions.clone();
                    tx.after_commit(move || sessions.announce(ended));
                }
                erased.count(USER_KIND, Action::Anonymised, 1);
                erased.count(NOTIFICATION_KIND, Action::Deleted, removed.len() as i64);
                erased.count(
                    TWO_FACTOR_RESET_REQUEST_KIND,
                    Action::Anonymised,
                    request_ids.len() as i64,
                );
                // Only records this transaction changed: the database clears
                // evidence of nothing else.
                if !done {
                    erased.records.push(ErasedRecord {
                        kind: USER_KIND.into(),
                        id: user,
                        person: None,
                    });
                }
                erased
                    .records
                    .extend(removed.into_iter().map(|id| ErasedRecord {
                        kind: NOTIFICATION_KIND.into(),
                        id,
                        person: None,
                    }));
                erased
                    .records
                    .extend(other.into_iter().map(|(kind, id)| ErasedRecord {
                        kind: kind.into(),
                        id,
                        person: None,
                    }));
                erased
                    .records
                    .extend(request_ids.into_iter().map(|id| ErasedRecord {
                        kind: TWO_FACTOR_RESET_REQUEST_KIND.into(),
                        id,
                        person: None,
                    }));
            }
            let invitations: Vec<i64> = tx
                .sql()
                .query(
                    "SELECT id FROM user_invitations WHERE tenant_id=$1 AND email=ANY($2) ORDER BY id FOR UPDATE",
                    &[&tenant, &emails(subject)],
                )
                .await?
                .iter()
                .map(|row| row.get(0))
                .collect();
            let held_invitations = holds
                .records(tx.sql(), grant, INVITATION_KIND, &invitations)
                .await?;
            for (id, hold) in &held_invitations {
                erased.kept.push(Kept::held(INVITATION_KIND, id, *hold));
            }
            let open: Vec<i64> = invitations
                .into_iter()
                .filter(|id| !held_invitations.contains_key(id))
                .collect();
            let held_accounts =
                held_prepared_accounts(tx.sql(), holds, grant, tenant, &open).await?;
            for (invitation, account, hold) in &held_accounts {
                erased.kept.push(Kept::held(USER_KIND, account, *hold));
                erased
                    .kept
                    .push(Kept::held(INVITATION_KIND, invitation, *hold));
            }
            let due: Vec<i64> = open
                .into_iter()
                .filter(|id| !held_accounts.iter().any(|(held, _, _)| held == id))
                .collect();
            if !due.is_empty() {
                let records = tx
                    .mutate(move |sql| {
                        Box::pin(async move {
                            let (changes, records) =
                                anonymise_invitations(sql, tenant, &due).await?;
                            Ok((records, changes))
                        })
                    })
                    .await?;
                erased.count(
                    INVITATION_KIND,
                    Action::Anonymised,
                    records
                        .iter()
                        .filter(|(kind, _)| *kind == INVITATION_KIND)
                        .count() as i64,
                );
                erased
                    .records
                    .extend(records.into_iter().map(|(kind, id)| ErasedRecord {
                        kind: kind.into(),
                        id,
                        person: None,
                    }));
            }
            Ok(erased)
        })
    }
}

inventory::submit! { Contribution::new(PERSONAL_DATA, "core/user.accounts",
Provenance { module: "core/user", source: file!(), line: line!() },
&PersonalData { label: "User accounts and notifications", kinds: &[USER_KIND, IDENTITY_KIND, NOTIFICATION_KIND, INVITATION_KIND, SCIM_KIND, EXTERNAL_IDENTITY_KIND, BOOTSTRAP_KIND, TWO_FACTOR_RESET_KIND, TWO_FACTOR_RESET_REQUEST_KIND], person_subjects: &[], handler: &ACCOUNTS }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/user.user",
Provenance { module: "core/user", source: file!(), line: line!() },
&RecordKind { kind: USER_KIND, fields: &["name", "email"], subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/user.notification",
Provenance { module: "core/user", source: file!(), line: line!() },
&RecordKind { kind: NOTIFICATION_KIND, fields: &["title", "body"], subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/user.invitation",
Provenance { module: "core/user", source: file!(), line: line!() },
&RecordKind { kind: INVITATION_KIND, fields: &["email"], subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/user.scim_user",
Provenance { module: "core/user", source: file!(), line: line!() },
&RecordKind { kind: SCIM_KIND, fields: &SCIM_FIELDS, subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/user.identity",
Provenance { module: "core/user", source: file!(), line: line!() },
&RecordKind { kind: IDENTITY_KIND, fields: &["email"], subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/user.external_identity",
Provenance { module: "core/user", source: file!(), line: line!() },
&RecordKind { kind: EXTERNAL_IDENTITY_KIND, fields: &["subject"], subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/user.bootstrap",
Provenance { module: "core/user", source: file!(), line: line!() },
&RecordKind { kind: BOOTSTRAP_KIND, fields: &["admin_email"], subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/user.two_factor_reset",
Provenance { module: "core/user", source: file!(), line: line!() },
&RecordKind { kind: TWO_FACTOR_RESET_KIND, fields: &["name", "email"], subject_identifier: false, person: false }) }
inventory::submit! { Contribution::new(RECORD_KINDS, "core/user.two_factor_reset_request",
Provenance { module: "core/user", source: file!(), line: line!() },
&RecordKind { kind: TWO_FACTOR_RESET_REQUEST_KIND, fields: &["company", "request_ip"], subject_identifier: false, person: false }) }

/// Deletes a tenant membership's provider links and external sessions, one
/// `Change` per row. The installation-owned identity and credentials stay.
pub(crate) async fn remove_links(
    sql: &tokio_postgres::Transaction<'_>,
    tenant: i64,
    user: i64,
    keep_identities: &[i64],
) -> bilibi_database::Result<Vec<Change>> {
    let mut changes = Vec::new();
    let removed = |kind: &str, id: i64, old: Value| Change {
        kind: kind.into(),
        id,
        old,
        new: json!({}),
    };
    for row in sql
        .query(
            "DELETE FROM user_external_sessions WHERE tenant_id=$1 AND user_id=$2 RETURNING id",
            &[&tenant, &user],
        )
        .await?
    {
        changes.push(removed(
            "user.external_session",
            row.get(0),
            json!({"user": user}),
        ));
    }
    for row in sql
        .query("DELETE FROM user_external_identities WHERE tenant_id=$1 AND user_id=$2 AND NOT id=ANY($3) RETURNING id,provider_id", &[&tenant, &user, &keep_identities])
        .await?
    {
        changes.push(removed(
            "user.external_identity",
            row.get(0),
            json!({"user": user, "provider": row.get::<_, i64>(1)}),
        ));
    }
    Ok(changes)
}
