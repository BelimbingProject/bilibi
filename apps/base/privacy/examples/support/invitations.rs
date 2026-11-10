//! Synthetic invitation regression fixture, exercised by the real request
//! runner. IDs are reserved only inside the opt-in disposable walkthrough DB.
use bilibi_database::{Change, MutationBoundary};
use bilibi_privacy::{HoldTarget, Privacy};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::Client;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub async fn prepare(
    app: &bilibi_web::App,
    client: &mut Client,
    scope: &Scope,
    boundary: &MutationBoundary,
    privacy: &Privacy,
) -> Result<()> {
    let mut tx = boundary
        .begin(
            client,
            scope.mutation_context("walkthrough.invitation_history"),
        )
        .await?;
    tx.mutate(|sql| Box::pin(async move {
        // Ada's pending and closed invitations, another Person's matching
        // email, and another tenant. A historical change deliberately contains
        // copied values so the guarded audit redaction is exercised as well.
        let rows = sql.query(
            "INSERT INTO customer_invitations(id,tenant_id,person_id,name,email,token_hash,invited_by,expires_at,revoked_at,accepted_at,user_id) OVERRIDING SYSTEM VALUE VALUES \
             (45101,1,1,'Invitation Ada','ada@example.test','\\x451001',4,now()+interval '7 days',NULL,NULL,NULL), \
             (45102,1,1,'Held Invitation Ada','held-ada@example.test','\\x451002',4,now()+interval '7 days',NULL,NULL,NULL), \
             (45103,1,2,'Other Person','ada@example.test','\\x451003',4,now()+interval '7 days',now(),NULL,NULL), \
             (45105,1,1,'Expired Ada','expired-ada@example.test','\\x451005',4,now()-interval '40 days',NULL,NULL,NULL), \
             (45106,1,1,'Revoked Ada','revoked-ada@example.test','\\x451006',4,now()+interval '7 days',now(),NULL,NULL), \
             (45107,1,1,'Accepted Ada','accepted-ada@example.test','\\x451007',4,now()+interval '7 days',NULL,now(),1) \
             RETURNING id,name,email",
            &[],
        ).await?;
        let changes = rows.iter().map(|row| Change { kind: "portal.invitation".into(),
            id: row.get(0), old: Value::Null,
            new: json!({"name": row.get::<_, String>(1), "email": row.get::<_, String>(2)}) }).collect();
        Ok(((), changes))
    })).await?;
    tx.commit().await?;
    client.batch_execute("INSERT INTO customer_invitations(id,tenant_id,person_id,name,email,token_hash,invited_by,expires_at) OVERRIDING SYSTEM VALUE VALUES \
        (45104,2,1,'Other Tenant','ada@example.test','\\x451004',5,now()+interval '7 days'); \
        INSERT INTO customer_registrations(id,tenant_id,name,email,token_hash,expires_at,verified_at,decision,decided_at,invitation_id) OVERRIDING SYSTEM VALUE VALUES \
        (45111,1,'Unlinked Request','ada@example.test','\\x451011',now()+interval '7 days',now(),NULL,NULL,NULL), \
        (45112,1,'Linked Request','ada@example.test','\\x451012',now()+interval '7 days',now(),'approved',now(),45101)").await?;
    privacy
        .place_hold(
            client,
            scope,
            HoldTarget::Record {
                kind: "portal.invitation".into(),
                id: 45102,
            },
            "Invitation evidence",
        )
        .await?;
    use bilibi_core::portal::{InvitationInput, PortalService, SiteInput, Status, SERVICE};
    let portal = app.services.get::<PortalService>(SERVICE)?;
    let mut tx = boundary
        .begin(client, scope.mutation_context("walkthrough.portal.site"))
        .await?;
    portal
        .set_site_in(
            &mut tx,
            scope,
            &SiteInput {
                address: "invitation-privacy".into(),
                title: "Synthetic invitation portal".into(),
                enabled: Some(true),
                ..Default::default()
            },
        )
        .await?
        .map_err(|_| "fixture portal site refused")?;
    tx.commit().await?;
    let input = InvitationInput {
        person: bilibi_party_ids::PersonId::new(2)?,
        affiliation: None,
        name: "Synthetic Invitee".into(),
        email: "lifecycle@example.test".into(),
    };
    let mut created = Vec::new();
    for _ in 0..2 {
        let mut tx = boundary
            .begin(client, scope.mutation_context("walkthrough.portal.invite"))
            .await?;
        let invitation = portal
            .invite_in(&mut tx, scope, &input)
            .await?
            .map_err(|_| "fixture invitation refused")?;
        created.push(invitation.id);
        tx.commit().await?;
    }
    if portal.invitation(client, scope, created[0]).await?.status != Status::Revoked {
        return Err("replacement did not revoke its predecessor".into());
    }
    let mut tx = boundary
        .begin(client, scope.mutation_context("walkthrough.portal.revoke"))
        .await?;
    portal
        .revoke_in(&mut tx, scope, created[1])
        .await?
        .map_err(|_| "revoke refused")?;
    tx.commit().await?;
    let copied: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM base_audit_mutations \
        WHERE tenant_id=$1 AND auditable_type='portal.invitation' AND auditable_id=ANY($2) \
        AND (new_values ? 'name' OR new_values ? 'email'))",
            &[
                &scope.tenant(),
                &created.iter().map(i64::to_string).collect::<Vec<_>>(),
            ],
        )
        .await?
        .get(0);
    if copied {
        return Err("new invitation audit evidence copied personal values".into());
    }
    println!("real invitation creation/replacement/revocation: lifecycle preserved; audit has no copied name/email");
    Ok(())
}

pub fn disclosure(bundle: &Value) -> Result<()> {
    let stores = bundle["stores"].as_array().ok_or("missing export stores")?;
    let invitations = stores
        .iter()
        .find(|store| store["store"] == "core/portal.invitations")
        .and_then(|store| store["records"]["customer_invitations"].as_array())
        .ok_or("invitation store absent from export")?;
    if invitations.len() != 5
        || invitations.iter().any(|row| row["person_id"] != 1)
        || invitations
            .iter()
            .any(|row| row.get("token_hash").is_some())
        || !invitations
            .iter()
            .any(|row| row["id"] == 45101 && row["email"] == "ada@example.test")
    {
        return Err(
            "invitation disclosure lost linked data or crossed an identity boundary".into(),
        );
    }
    for id in [45101, 45102, 45105, 45106, 45107] {
        if !invitations.iter().any(|row| row["id"] == id) {
            return Err("invitation disclosure omitted a lifecycle state".into());
        }
    }
    let registrations = stores
        .iter()
        .find(|store| store["store"] == "core/portal.registrations")
        .and_then(|store| store["records"]["customer_account_requests"].as_array())
        .ok_or("linked registration absent from export")?;
    if registrations.len() != 1 || registrations[0]["id"] != 45112 {
        return Err("registration disclosure bypassed confirmation or lost its Person link".into());
    }
    println!("invitation access: all lifecycle states and linked registration, no unconfirmed candidate, other Person or tenant, no digest");
    Ok(())
}

pub async fn erased(client: &Client) -> Result<()> {
    let correct: bool = client.query_one(
        "SELECT \
         (SELECT name='[erased]' AND email='[erased]' AND revoked_at IS NOT NULL FROM customer_invitations WHERE tenant_id=1 AND id=45101) \
         AND (SELECT name='Held Invitation Ada' AND revoked_at IS NULL FROM customer_invitations WHERE tenant_id=1 AND id=45102) \
         AND (SELECT count(*)=3 FROM customer_invitations WHERE tenant_id=1 AND id IN (45105,45106,45107) AND name='[erased]' AND email='[erased]') \
         AND (SELECT accepted_at IS NOT NULL AND revoked_at IS NULL AND user_id=1 FROM customer_invitations WHERE tenant_id=1 AND id=45107) \
         AND (SELECT name='Other Person' FROM customer_invitations WHERE tenant_id=1 AND id=45103) \
         AND (SELECT name='Other Tenant' FROM customer_invitations WHERE tenant_id=2 AND id=45104) \
         AND EXISTS(SELECT 1 FROM customer_registrations WHERE tenant_id=1 AND id=45111 AND name='Unlinked Request') \
         AND NOT EXISTS(SELECT 1 FROM customer_registrations WHERE tenant_id=1 AND id=45112) \
         AND NOT EXISTS(SELECT 1 FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type='portal.invitation' AND auditable_id='45101' \
             AND (new_values->>'email' IS DISTINCT FROM '[erased]' OR new_values->>'name' IS DISTINCT FROM '[erased]'))",
        &[],
    ).await?.get(0);
    if !correct {
        return Err(
            "invitation erasure, hold, isolation or historical audit redaction failed".into(),
        );
    }
    println!("invitation erasure: values and history erased, link revoked, held and unrelated rows intact");
    Ok(())
}
