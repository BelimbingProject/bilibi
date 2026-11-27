//! Run with the watcher_cases fixture mounted and --scripts on a disposable database.
use super::{check, Proof, DELIVER, DELIVERY_PRINCIPAL, OPENED, WATCHER_CAPABILITIES};
use bilibi_database::{Error, Result};
use serde_json::{json, Value};
use std::time::Duration;

// A retry can be scheduled beyond the queue's short readiness window.
async fn wait_for_terminal_run(proof: &Proof, case: i64, watcher: i64) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if proof.receipts(case).await?.iter().any(|receipt| {
                receipt.0 == watcher && !matches!(receipt.1.as_str(), "queued" | "matched")
            }) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| {
        println!("Timed out waiting for watcher {watcher} on case {case}");
        Error::Internal
    })?
}

pub(super) async fn walkthrough(proof: &Proof) -> Result<()> {
    let one = &proof.one;
    // This proof exercises script outcomes under CI load, including database
    // operations within the script budget. The infinite loop still times out.
    proof
        .setting(one, "watchers.script_milliseconds", json!(1000))
        .await?;
    proof
        .grant(one, DELIVERY_PRINCIPAL, DELIVER, Some(true))
        .await?;
    for key in WATCHER_CAPABILITIES
        .into_iter()
        .chain(["watchers.script.execute"])
    {
        proof.grant(one, "watchers", key, Some(true)).await?;
    }
    proof
        .setting(one, "watchers.script_heap_mib", json!(1))
        .await?;
    let foreign = proof
        .open(&proof.two, 100, "Separate company's case", "normal", "10")
        .await?;
    let foreign_target = format!("return [{{operation:'watcher_cases.cases.route',input:{{queue:'priority',case_id:{foreign}}}}}];");
    let call = json!({"operation":"watcher_cases.cases.route","input":{"queue":"priority"}});
    let foreign_call = json!({"operation":"watcher_cases.cases.route","input":{"queue":"priority","case_id":foreign}});
    let review = proof
        .review(
            one,
            "base.watchers.draft",
            json!({
                "name":"Declare a foreign subject", "watch":OPENED, "do":[{"script":{
                    "name":"Foreign subject", "source":foreign_target, "fields":[],
                    "calls":[foreign_call], "examples":[{"facts":{},"expect":[foreign_call]}]
                }}]
            }),
        )
        .await;
    check(
        review["valid"] == false
            && review["error"]["code"] == "invalid_input"
            && review["error"]["fields"]["do.0.input"][0]
                == "case_id is the trigger's record; leave it out",
        "the draft contract refuses a foreign subject even when declared",
    );
    let configuration = proof.open(one, 1, "priority", "normal", "10").await?;
    let lookup =
        json!({"operation":"watcher_cases.cases.lookup","input":{"case_id":configuration}});
    let lookup_reply = json!({"operation":"watcher_cases.cases.lookup","input":{"case_id":configuration},"reply":{"title":"priority","queue":null}});
    let (lookup_id, lookup_version) = proof
        .draft(
            one,
            json!({"name":"Route from a lookup","watch":OPENED,"do":[{"script":{
                "name":"Lookup route","fields":["priority"],"lookups":[lookup],"calls":[call],
                "source":format!("if (facts.priority !== 'urgent') return []; const target=lookup('watcher_cases.cases.lookup',{{case_id:{configuration}}}); return [{{operation:'watcher_cases.cases.route',input:{{queue:target.title}}}}];"),
                "examples":[
                    {"facts":{"priority":"normal"},"lookup_fixtures":[lookup_reply],"expect":[]},
                    {"facts":{"priority":"urgent"},"lookup_fixtures":[lookup_reply],"expect":[call]}
                ]
            }}]}),
        )
        .await?;
    proof
        .approve(one, lookup_id, lookup_version)
        .await
        .map_err(|_| Error::Invalid)?;
    let looked_up = proof
        .open(one, 1, "Lookup-driven case", "urgent", "10")
        .await?;
    wait_for_terminal_run(proof, looked_up, lookup_id).await?;
    let looked_up_state = proof.case(looked_up).await?;
    if looked_up_state.0.as_deref() != Some("priority") {
        proof.show(looked_up).await?;
        println!("     lookup-driven case state: {looked_up_state:?}");
    }
    check(
        looked_up_state.0.as_deref() == Some("priority"),
        "the script uses an approved read-contract reply",
    );
    let (same_lookup_id, replacement_version) = proof
        .draft(
            one,
            json!({"watcher_id":lookup_id,"name":"Route without a lookup","watch":OPENED,"do":[{"script":{
                "name":"Direct route","fields":["priority"],"calls":[call],
                "source":"if (facts.priority !== 'urgent') return []; return [{operation:'watcher_cases.cases.route',input:{queue:'priority'}}];",
                "examples":[{"facts":{"priority":"normal"},"expect":[]},{"facts":{"priority":"urgent"},"expect":[call]}]
            }}]}),
        )
        .await?;
    proof
        .approve(one, same_lookup_id, replacement_version)
        .await
        .map_err(|_| Error::Invalid)?;
    proof
        .grant(one, "watchers", "watcher-cases.case.view", Some(false))
        .await?;
    let refused_restore = proof.restore(one, lookup_id, lookup_version).await;
    let client = proof.db.get().await?;
    let active_number: i32 = client
        .query_one(
            "SELECT v.number FROM base_watchers w JOIN base_watcher_versions v ON v.tenant_id=w.tenant_id AND v.id=w.active_version_id WHERE w.tenant_id=$1 AND w.id=$2",
            &[&one.tenant(), &lookup_id],
        )
        .await?
        .get(0);
    drop(client);
    check(
        refused_restore.is_err() && active_number == replacement_version as i32,
        "restore refuses a missing live lookup grant and leaves the active version unchanged",
    );
    proof
        .grant(one, "watchers", "watcher-cases.case.view", Some(true))
        .await?;
    proof
        .restore(one, lookup_id, lookup_version)
        .await
        .map_err(|_| Error::Invalid)?;
    proof
        .switch(one, lookup_id, false)
        .await
        .map_err(|_| Error::Invalid)?;
    let page_target = proof
        .open(one, 1, "Person-run script", "urgent", "10")
        .await?;
    proof
        .write(
            one,
            "base.watchers.script.run",
            json!({"watcher_id":lookup_id,"version":lookup_version,"script":1,"subject":page_target.to_string()}),
        )
        .await
        .map_err(|why| {
            println!("     person-run script refused: {why}");
            Error::Invalid
        })?;
    check(
        proof.case(page_target).await?.0.as_deref() == Some("priority"),
        "the page, CLI and delegated-agent operation runs the exact approved script as its person",
    );
    let refused_target = proof
        .open(one, 1, "Person permission revoked", "urgent", "10")
        .await?;
    let client = proof.db.get().await?;
    client
        .execute(
            "DELETE FROM base_watcher_script_permissions WHERE tenant_id=$1 AND watcher_id=$2 AND principal='person'",
            &[&one.tenant(), &lookup_id],
        )
        .await?;
    drop(client);
    let refused_person = proof
        .write(
            one,
            "base.watchers.script.run",
            json!({"watcher_id":lookup_id,"version":lookup_version,"script":1,"subject":refused_target.to_string()}),
        )
        .await;
    check(
        refused_person
            .as_ref()
            .is_err_and(|why| why.contains("script_permission_refused"))
            && proof.case(refused_target).await?.1 == 0,
        "revoking this script's person permit refuses every person-initiated door before effects",
    );
    let approved = format!("return [{call}];");
    for (name, body, expected) in [
        ("Route an urgent case", approved.as_str(), None),
        (
            "File access",
            "if (typeof require === 'function') return []; return require('fs').readFileSync('example.txt');",
            Some("script_refused"),
        ),
        (
            "Network access",
            "if (typeof fetch === 'function') return []; return fetch('https://example.invalid/');",
            Some("script_refused"),
        ),
        (
            "Another company's record",
            foreign_target.as_str(),
            Some("script_call_refused"),
        ),
        (
            "Undeclared read contract",
            "return lookup('base.watchers.list', {});",
            Some("script_lookup_refused"),
        ),
        (
            "Undeclared operation",
            "return [{operation:'watcher_cases.cases.flag',input:{note:'outside declaration'}}];",
            Some("script_call_refused"),
        ),
        (
            "Heap limit",
            "const x=[]; for (let i = 0; i < 64; ++i) x.push(new Array(16384).fill(i)); return [];",
            Some("script_memory"),
        ),
        ("Time limit", "for (;;) {}", Some("script_timeout")),
    ] {
        let mut script = json!({"name":name,"fields":["priority"],"calls":[call],
            "source":format!("if (facts.priority !== 'urgent') return [];\n{body}"),
            "examples":[{"facts":{"priority":"normal"},"expect":[]}]});
        if expected.is_none() {
            script["examples"] = json!([
                {"facts":{"priority":"normal"},"expect":[]},
                {"facts":{"priority":"urgent"},"expect":[call]}
            ]);
        }
        let (id, version) = proof
            .draft(
                one,
                json!({"name":name,"watch":OPENED,"do":[{"script":script}]}),
            )
            .await?;
        proof.approve(one, id, version).await.map_err(|why| {
            println!("Approval refused: {why}");
            Error::Invalid
        })?;
        let record = proof.open(one, 1, name, "urgent", "10").await?;
        wait_for_terminal_run(proof, record, id).await?;
        let receipts = proof.receipts(record).await?;
        let outcome = receipts
            .iter()
            .find(|receipt| receipt.0 == id)
            .ok_or(Error::NotFound)?;
        let (queue, routed, _) = proof.case(record).await?;
        let effect_ok = match expected {
            None => outcome.1 == "done" && outcome.2.is_none()
                && outcome.3.len() == 1 && routed == 1 && queue.as_deref() == Some("priority"),
            Some(reason) => outcome.1 == "refused" && outcome.2.as_deref() == Some(reason)
                && outcome.3.is_empty() && routed == 0,
        };
        check(
            effect_ok,
            &format!("{name}: {} {:?}", outcome.1, outcome.2),
        );
        if expected.is_none() {
            let normal = proof.open(one, 1, "Normal case", "normal", "10").await?;
            wait_for_terminal_run(proof, normal, id).await?;
            let receipts = proof.receipts(normal).await?;
            check(
                proof.case(normal).await?.1 == 0
                    && receipts.iter().any(|receipt| receipt.0 == id && receipt.1 == "done" && receipt.3.is_empty()),
                "the normal case requests no operation",
            );
        }
        proof
            .switch(one, id, false)
            .await
            .map_err(|_| Error::Internal)?;
    }
    check(
        proof.case(foreign).await?.1 == 0,
        "the other company's record remains untouched",
    );
    let (permission_id, permission_version) = proof
        .draft(
            one,
            json!({"name":"Revoked script permission","watch":OPENED,"do":[{"script":{
                "name":"Revoked permission","fields":["priority"],"calls":[call],"source":approved,
                "examples":[{"facts":{"priority":"urgent"},"expect":[call]}]
            }}]}),
        )
        .await?;
    proof
        .approve(one, permission_id, permission_version)
        .await
        .map_err(|_| Error::Invalid)?;
    let client = proof.db.get().await?;
    client
        .execute(
            "DELETE FROM base_watcher_script_permissions WHERE tenant_id=$1 AND watcher_id=$2",
            &[&one.tenant(), &permission_id],
        )
        .await?;
    drop(client);
    let refused = proof
        .open(one, 1, "Permission revoked", "urgent", "10")
        .await?;
    wait_for_terminal_run(proof, refused, permission_id).await?;
    let receipts = proof.receipts(refused).await?;
    check(
        receipts.iter().any(|receipt| {
            receipt.0 == permission_id
                && receipt.1 == "refused"
                && receipt.2.as_deref() == Some("script_permission_refused")
        }) && proof.case(refused).await?.1 == 0,
        "revoking one script's permission refuses that script before effects",
    );
    outbound(proof).await?;
    println!("the script checks passed");
    Ok(())
}

/// Outbound terms: approval names the one destination and the data an
/// outbound action may send; every run checks the owner's resolved send.
async fn outbound(proof: &Proof) -> Result<()> {
    let one = &proof.one;
    proof
        .grant(one, "watchers", "watcher-cases.case.notify", Some(true))
        .await?;
    let notify = |include: Value| json!({"operation":"watcher_cases.cases.notify","input":{"include":include}});
    let narrow = notify(json!(["title", "priority"]));
    let broad = notify(json!(["title", "margin"]));
    let terms = json!([{"operation":"watcher_cases.cases.notify","destination":"desk@partner.example","data":["title","priority"]}]);
    let source = "if (facts.priority === 'urgent') return [{operation:'watcher_cases.cases.notify',input:{include:['title','priority']}}]; if (facts.priority === 'low') return [{operation:'watcher_cases.cases.notify',input:{include:['title','margin']}}]; return [];";
    let script = |outbound: Value| {
        json!({"name":"Share with the partner desk","fields":["priority"],"calls":[narrow, broad],"outbound":outbound,"source":source,
        "examples":[
            {"facts":{"priority":"normal"},"expect":[]},
            {"facts":{"priority":"urgent"},"expect":[narrow]},
            {"facts":{"priority":"low"},"expect":[broad]}
        ]})
    };
    let plain = proof
        .review(
            one,
            "base.watchers.draft",
            json!({"name":"Plain send","watch":OPENED,"do":[narrow]}),
        )
        .await;
    check(
        plain["valid"] == false && plain.to_string().contains("sends outside Bilibi"),
        "an outbound action is refused as a plain step: only a script with approved terms may send",
    );
    let unnamed = proof
        .review(
            one,
            "base.watchers.draft",
            json!({"name":"Send without terms","watch":OPENED,"do":[{"script":script(json!([]))}]}),
        )
        .await;
    check(
        unnamed["valid"] == false
            && unnamed
                .to_string()
                .contains("must name its destination and data"),
        "a script that selects an outbound action without terms is refused at draft",
    );
    let wider = proof
        .review(one, "base.watchers.draft", json!({"name":"Send an unknown field","watch":OPENED,"do":[{"script":script(json!([{"operation":"watcher_cases.cases.notify","destination":"desk@partner.example","data":["title","notes"]}]))}]}))
        .await;
    check(
        wider["valid"] == false && wider.to_string().contains("never sends notes"),
        "terms naming data the owner never sends are refused at draft",
    );
    let (id, version) = proof
        .draft(
            one,
            json!({"name":"Share urgent cases","watch":OPENED,"do":[{"script":script(terms)}]}),
        )
        .await?;
    let review = proof
        .review(
            one,
            "base.watchers.approve",
            json!({"watcher_id": id, "version": version}),
        )
        .await
        .to_string();
    check(
        review.contains("sends only title, priority to desk@partner.example"),
        "approval shows the outbound destination and data",
    );
    proof.approve(one, id, version).await.map_err(|why| {
        println!("     outbound approval refused: {why}");
        Error::Invalid
    })?;
    let open = |title: &'static str, priority: &'static str, address: &'static str| async move {
        let opened = proof
            .write(one, "watcher_cases.cases.open", json!({"company_id":1,"title":title,"priority":priority,"amount":"10","margin":"250.00","partner_address":address}))
            .await
            .map_err(|why| {
                println!("     open refused: {why}");
                Error::Invalid
            })?;
        opened["case_id"].as_i64().ok_or(Error::Invalid)
    };
    let notices = |case: i64| async move {
        let rows = proof
            .db
            .get()
            .await?
            .query(
                "SELECT destination,payload FROM watcher_cases_notices WHERE case_id=$1",
                &[&case],
            )
            .await?;
        Ok::<_, Error>(
            rows.iter()
                .map(|row| (row.get::<_, String>(0), row.get::<_, Value>(1)))
                .collect::<Vec<_>>(),
        )
    };
    let approved_send = |sent: &[(String, Value)]| {
        sent.len() == 1
            && sent[0].0 == "desk@partner.example"
            && sent[0].1.as_object().is_some_and(|payload| {
                let mut keys: Vec<&str> = payload.keys().map(String::as_str).collect();
                keys.sort_unstable();
                keys == ["priority", "title"]
            })
    };
    let refused = |case: i64| async move {
        Ok::<_, Error>(proof.receipts(case).await?.iter().any(|receipt| {
            receipt.0 == id
                && receipt.1 == "refused"
                && receipt.2.as_deref() == Some("script_outbound_refused")
        }))
    };
    // Queued watcher runs, as the watchers principal.
    let good = open("Urgent shared case", "urgent", "desk@partner.example").await?;
    let hostile = open("Redirected case", "urgent", "drop@attacker.example").await?;
    let wide = open("Wide payload case", "low", "desk@partner.example").await?;
    for case in [good, hostile, wide] {
        wait_for_terminal_run(proof, case, id).await?;
    }
    check(
        approved_send(&notices(good).await?),
        "a queued run sends only the approved data to the approved destination",
    );
    check(
        refused(hostile).await? && notices(hostile).await?.is_empty(),
        "a queued send whose resolved destination is not the approved one is refused with no effect",
    );
    check(
        refused(wide).await? && notices(wide).await?.is_empty(),
        "a queued send of data beyond the approved fields is refused with no effect",
    );
    // The person door (page action, command line, delegated agent), off the queue.
    proof
        .switch(one, id, false)
        .await
        .map_err(|_| Error::Invalid)?;
    let person = open("Person-shared case", "urgent", "desk@partner.example").await?;
    let person_hostile = open("Person-redirected case", "urgent", "drop@attacker.example").await?;
    let person_wide = open("Person wide payload", "low", "desk@partner.example").await?;
    let run = |case: i64| {
        proof.write(
            one,
            "base.watchers.script.run",
            json!({"watcher_id":id,"version":version,"script":1,"subject":case.to_string()}),
        )
    };
    let sent = run(person).await;
    check(
        sent.is_ok() && approved_send(&notices(person).await?),
        "a person-run script sends only the approved data to the approved destination",
    );
    for (case, what) in [(person_hostile, "destination"), (person_wide, "data")] {
        let answer = run(case).await;
        check(
            answer.is_err_and(|why| why.contains("script_outbound_refused"))
                && notices(case).await?.is_empty(),
            &format!("the person door refuses {what} outside the approved terms before any effect"),
        );
    }
    Ok(())
}
