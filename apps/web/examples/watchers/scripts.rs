//! Run with the watcher_cases fixture mounted and --scripts on a disposable database.
use super::{check, Proof, DELIVER, DELIVERY_PRINCIPAL, OPENED, WATCHER_CAPABILITIES};
use bilibi_database::{Error, Result};
use serde_json::json;

pub(super) async fn walkthrough(proof: &Proof) -> Result<()> {
    let one = &proof.one;
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
        proof.settled().await?;
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
            proof.settled().await?;
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
    println!("the script checks passed");
    Ok(())
}
