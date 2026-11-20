//! What a failed answer means, in words an agent can act on. The server's own
//! code and fields are always printed beside them; nothing here decides
//! anything about the request.
use crate::{wire::Answer, Problem};
use serde_json::Value;

/// An answer Bilibi did not write: it answers in JSON only, so a page of
/// anything else came from a firewall, CDN or proxy in front of it.
fn intercepted(origin: &str, status: u16) -> Problem {
    Problem::new(
        "BLOCKED_BEFORE_BILIBI",
        format!("Something in front of {origin} answered instead of Bilibi (HTTP {status}, not JSON)"),
    )
    .help("A firewall or CDN is stopping this program before Bilibi sees it; signing in again will not help")
    .help("Tell whoever runs this Bilibi: docs/server-install.md, \"A firewall or CDN in front of Bilibi\"")
}

/// Why an answer that is not a success failed.
pub(crate) fn failed(origin: &str, answer: &Answer) -> Problem {
    if answer.json {
        refusal(answer.status, &answer.body)
    } else {
        intercepted(origin, answer.status)
    }
}

/// What an answer means: the body of a success, else the refusal.
pub(crate) fn settled(origin: &str, answer: Answer) -> Result<Value, Problem> {
    if answer.json && (200..300).contains(&answer.status) {
        Ok(answer.body)
    } else {
        Err(failed(origin, &answer))
    }
}

/// The words a refusal code is explained with; the server's code and fields
/// are always printed beside them.
pub(crate) fn refusal(status: u16, body: &Value) -> Problem {
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code = if error.is_empty() {
        format!("HTTP_{status}")
    } else {
        error.to_ascii_uppercase()
    };
    let described = body
        .get("error_description")
        .or_else(|| body.get("message"))
        .and_then(Value::as_str);
    let mut problem = match (status, error) {
        (401, _) => Problem::new("SIGN_IN_NEEDED", "Bilibi did not accept this sign-in")
            .help("Run `bilibi-axi login <address>` to sign in again"),
        (_, "ai_switched_off") => Problem::new(
            &code,
            described.unwrap_or("AI access is off for this connection"),
        )
        .help("Ask the person to switch this connection on at the settings address, then run the command again"),
        (_, "step_up_required") => Problem::new(
            &code,
            described.unwrap_or("This write needs the person's current 2FA code"),
        )
        .help("Ask the person for their current 2FA code, then run the same commit with BILIBI_STEP_UP_CODE set to it"),
        (_, "forbidden") => Problem::new(&code, "The signed-in account may not do this")
            .help("`bilibi-axi hello` checks the connection itself; this answer is about permission"),
        (_, "invalid_input") => Problem::new(&code, "Bilibi refused the input"),
        (_, "plan_rejected") => Problem::new(&code, "Bilibi refused the plan; nothing was written"),
        (_, "unknown_operation" | "unknown_guide" | "not_found") => {
            Problem::new(&code, "Bilibi has nothing under that key")
                .help("Run `bilibi-axi search <words>` to find the key")
        }
        (_, "channel_not_allowed") => {
            Problem::new(&code, "This operation is not offered through the API")
        }
        (_, "rejected") => Problem::new(
            &code,
            "Bilibi refused the write; nothing was written. With reason record_changed, a record changed since the plan",
        )
        .help("Read the records again, then plan and review once more"),
        (429, _) => Problem::new("RATE_LIMITED", "Too many requests from this address")
            .help("Wait a minute, then run the command again"),
        (500.., _) => Problem::new(
            if error.is_empty() { "UNAVAILABLE" } else { code.as_str() },
            described.unwrap_or("Bilibi could not answer just now"),
        )
        .help("Run the command again shortly; a commit that failed this way may have been written, so read before planning again"),
        _ => Problem::new(
            &code,
            described.map_or_else(|| format!("Bilibi refused the request ({status})"), str::to_owned),
        ),
    };
    if let Some(fields) = body.as_object() {
        for (name, value) in fields {
            if !matches!(name.as_str(), "error" | "error_description" | "message") {
                problem = problem.with(name, value.clone());
            }
        }
    }
    problem
}
