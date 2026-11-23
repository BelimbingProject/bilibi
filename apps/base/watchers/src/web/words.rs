//! A watcher run's receipt in words: why it ended as it did, what an
//! action's refusal means and what its AI observation answered.
use crate::store::RunView;

/// Why a run ended as it did, in words.
pub(super) fn reason(code: &str) -> &'static str {
    match code {
        "catalog_changed" => "Not run: the installed fields or actions changed. Draft a new version for review.",
        "record_changed" => "Skipped: the record changed after the watcher matched.",
        "event_before_enabled" => "Skipped: this event was recorded before the watcher was switched on.",
        "stale_activation" => "Skipped: the watcher was switched off and on after this run was queued.",
        "event_before_approval" => "Skipped: this event was recorded before the watcher version was approved.",
        "chain_budget" => "Stopped: another watcher started it, and the chain reached the limit of watchers in a row.",
        "watcher_off" => "Not run: the watcher was switched off.",
        "stale_version" => "Not run: a newer version was approved after this event, so this one no longer runs.",
        "watcher_limit" => "Not run: the watcher exceeds a configured size or fact limit in System Settings.",
        "attempt_limit" => "Not run: the watchers job reached its configured attempt limit.",
        "action_limit" => "Not run: it has more actions than allowed per run.",
        "script_run_refused" => "Automation lacks permission to run this script.",
        "script_memory" => "The script reached its memory limit.",
        "script_timeout" => "The script reached its time limit.",
        "script_refused" => "The script failed inside its sandbox.",
        "script_call_refused" => "The script requested a call outside its approved list.",
        "script_lookup_refused" => "The script requested a read outside its approved lookups, or the read owner refused it.",
        "script_permission_refused" => "This watcher is not permitted to run that particular script version.",
        "script_output" | "script_input" => "The script input or result exceeded its contract.",
        "script_unavailable" => "The script worker could not start.",
        "read_refused" => "Not run: automation may not read these records yet. An administrator allows it in Automation access.",
        "restricted_field" => "Not run: a condition reads a field that is hidden from automation.",
        "subject_unavailable" => "Not run: the record is gone or out of reach.",
        "observation_missing" => "Not run: its AI observation was not asked.",
        "action_refused" => "An action was refused; see its outcome.",
        "actor_refused" => "Not run: automation was stopped here (the workspace is suspended or Automation access was withdrawn).",
        _ => "Not run.",
    }
}
pub(super) fn effect_reason(code: &str) -> &'static str {
    match code {
        "forbidden" => "refused: automation is not allowed to do this",
        "invalid" => "refused by the record's owner",
        "refused" => "refused by the record's owner",
        "not_found" => "refused: the record is gone",
        "record_changed" => "skipped: the record changed after the watcher matched",
        "conflict" => "refused: the record changed",
        "actor_refused" => "refused: automation was stopped",
        _ => "refused",
    }
}
/// What the AI answered for a run, in words; nothing when none was asked.
pub(super) fn observed(run: &RunView) -> Option<String> {
    let observation = Some(&run.observation).filter(|o| !o.is_null())?;
    if observation.get("restricted").is_some() {
        return Some("The AI observation is hidden: you may not read the fields it sent.".into());
    }
    Some(match observation["state"].as_str() {
        Some("observed") => format!(
            "AI answered {} ({}, {}).",
            observation["label"].as_str().unwrap_or_default(),
            observation["provider"].as_str().unwrap_or_default(),
            observation["model"].as_str().unwrap_or_default(),
        ),
        Some("pending") => "Waiting for the AI observation.".into(),
        _ => format!(
            "No AI answer: {}. Conditions on it saw no answer.",
            observation_reason(observation["reason"].as_str().unwrap_or_default())
        ),
    })
}
fn observation_reason(code: &str) -> &'static str {
    match code {
        "disabled" => "AI is off for automation here",
        "not_configured" => "the AI provider is not configured",
        "timeout" => "the AI provider did not answer in time",
        "provider_error" => "the AI provider failed",
        "invalid_answer" => "the answer was not one of the labels",
        "budget_exhausted" => "today's AI observations are used up",
        "interrupted" => "the call was interrupted and is not asked twice",
        "not_observed" => "this event was not asked about",
        "read_refused" | "restricted_field" | "subject_unavailable" => {
            "automation could not read the fields it sends"
        }
        _ => "unavailable",
    }
}
