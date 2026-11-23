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
    let foreign = proof
        .open(&proof.two, 100, "Separate company's case", "normal", "10")
        .await?;
    let foreign_target = format!("return [{{operation:'watcher_cases.cases.route',input:{{queue:'priority',case_id:{foreign}}}}}];");
    let call = json!({"operation":"watcher_cases.cases.route","input":{"queue":"priority"}});
    let approved = format!("return [{call}];");
    for (name, body, expected) in [
        ("Route an urgent case", approved.as_str(), "done"),
        (
            "File access",
            "return require('fs').readFileSync('example.txt');",
            "refused",
        ),
        (
            "Network access",
            "return fetch('https://example.invalid/');",
            "refused",
        ),
        (
            "Another company's record",
            foreign_target.as_str(),
            "refused",
        ),
        (
            "Heap limit",
            "const x=[]; for (;;) x.push(new Array(16384).fill(1));",
            "refused",
        ),
        ("Time limit", "for (;;) {}", "refused"),
    ] {
        let script = json!({"name":name,"runner":"watchers","fields":["priority"],"calls":[call],
            "source":format!("if (facts.priority !== 'urgent') return [];\n{body}"),
            "examples":[{"facts":{"priority":"normal"},"expect":[]}]});
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
        let effect_ok = if expected == "done" {
            outcome.3.len() == 1 && proof.case(record).await?.1 == 1
        } else {
            outcome.3.is_empty()
                && proof.case(record).await?.1 == 0
                && matches!(
                    outcome.2.as_deref(),
                    Some("script_refused" | "script_timeout" | "script_call_refused")
                )
        };
        check(
            outcome.1 == expected && effect_ok,
            &format!("{name}: {} {:?}", outcome.1, outcome.2),
        );
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
