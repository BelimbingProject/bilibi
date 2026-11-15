//! Action disclosure and retained-evidence proof through real privacy requests.
use bilibi_audit::{record_action, Attribution, RequestInfo};
use bilibi_privacy::RequestView;
use serde_json::{json, Value};
use tokio_postgres::Client;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const EVENT: &str = "walkthrough.audit_privacy";
const ROWS: usize = 1004;

pub async fn prepare(client: &mut Client, scope: &bilibi_tenancy::Scope) -> Result<()> {
    let tx = client.transaction().await?;
    let mut who = Attribution {
        tenant: Some(1),
        actor_type: "user",
        actor_id: 1,
        impersonator_id: None,
        actor_role: None,
        system_principal: None,
        trace_id: None,
        agent_self_report: None,
        client_identity: None,
    };
    let request = RequestInfo {
        ip: Some("192.0.2.1".parse()?),
        url: Some("/privacy-fixture".into()),
        user_agent: Some("Privacy walkthrough".into()),
    };
    for i in 0..1001 {
        record_action(
            &tx,
            &who,
            Some(&request),
            EVENT,
            json!({"number": i}),
            i == 0,
        )
        .await?;
    }
    // Person-backed agent and the subject impersonating another account.
    who.actor_type = "agent";
    record_action(
        &tx,
        &who,
        Some(&request),
        EVENT,
        json!({"number": 1001}),
        false,
    )
    .await?;
    who.actor_type = "user";
    who.actor_id = 2;
    who.impersonator_id = Some(1);
    record_action(
        &tx,
        &who,
        Some(&request),
        EVENT,
        json!({"number": 1002}),
        false,
    )
    .await?;
    // An action matching both paths still appears exactly once.
    who.actor_id = 1;
    record_action(
        &tx,
        &who,
        Some(&request),
        EVENT,
        json!({"number": 1003}),
        false,
    )
    .await?;
    // Different person and different tenant must not leak through the export.
    who.actor_id = 2;
    who.impersonator_id = None;
    record_action(
        &tx,
        &who,
        Some(&request),
        EVENT,
        json!({"excluded": "unlinked account"}),
        false,
    )
    .await?;
    who.tenant = Some(2);
    who.actor_id = 1;
    record_action(
        &tx,
        &who,
        Some(&request),
        EVENT,
        json!({"excluded": "other tenant"}),
        false,
    )
    .await?;
    tx.commit().await?;
    if bilibi_audit::count_account_actions(client, scope, &[1]).await? != ROWS as i64
        || bilibi_audit::count_account_actions(client, scope, &[1, 2]).await? != ROWS as i64 + 1
    {
        return Err(
            "action discovery lost attribution, duplicated an action or crossed tenants".into(),
        );
    }
    Ok(())
}

pub fn disclosure(bundle: &Value) -> Result<()> {
    let actions = bundle["stores"]
        .as_array()
        .ok_or("no stores")?
        .iter()
        .find(|store| store["store"] == "core/user.accounts")
        .and_then(|store| store["records"]["audit_actions"].as_array())
        .ok_or("account actions missing from disclosure")?;
    let proof: Vec<_> = actions.iter().filter(|row| row["event"] == EVENT).collect();
    if proof.len() != ROWS
        || proof.iter().enumerate().any(|(i, row)| {
            row["payload"]["number"] != json!(i)
                || row["ip_address"] != "192.0.2.1"
                || row["url"] != "/privacy-fixture"
                || row["user_agent"] != "Privacy walkthrough"
        })
        || proof[0]["retained"] != true
        || proof[1001]["actor_type"] != "agent"
        || proof[1002]["impersonator_id"] != 1
    {
        return Err("action disclosure lost a page, attribution or request values, or leaked an unrelated action".into());
    }
    println!("audit action disclosure: all {ROWS} rows across pages, agent and impersonation, no unrelated person or tenant");
    Ok(())
}

pub async fn kept(client: &Client, request: &RequestView) -> Result<()> {
    let kept = request
        .stores
        .iter()
        .find(|store| store.store == "core/user.accounts")
        .and_then(|store| store.result.as_ref())
        .and_then(|result| result["kept"].as_array())
        .ok_or("account erasure result missing")?;
    if !kept.iter().any(|row| {
        row["kind"] == "audit.account_actions"
            && row["id"] == "1"
            && row["reason"] == "retained_until"
            && row["until"].is_null()
    }) {
        return Err(
            "erasure result omits retained account actions or promises a false deadline".into(),
        );
    }
    let remaining: i64 = client.query_one(
        "SELECT count(*) FROM base_audit_actions WHERE tenant_id=1 AND event=$1 AND erased_at IS NULL AND payload ? 'number'",
        &[&EVENT],
    ).await?.get(0);
    if remaining != ROWS as i64 {
        return Err("request erasure changed action evidence outside its retention policy".into());
    }
    println!("audit action erasure: result reports evidence kept; retention and bookmark policy unchanged");
    Ok(())
}
