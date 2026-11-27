use crate::UnavailableReason;
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{
    Constraint, Definition, Editor, ScopeKind, Setting, Settings, Validator, ValueType,
    CONSTRAINTS, DEFINITIONS,
};
use bilibi_tenancy::ReadClient;
use serde_json::json;

pub(crate) const HARD_MAX_BYTES: usize = 65_536;
pub(crate) const MODEL_PATH: &str = "language_detection.model_path";
const MIN_SCORE: &str = "language_detection.min_score";
const MIN_MARGIN: &str = "language_detection.min_margin";
const MIN_LETTERS: &str = "language_detection.min_letters";
const MAX_BYTES: &str = "language_detection.max_bytes";
const MAX_CONCURRENT: &str = "language_detection.max_concurrent";

pub(crate) struct Policy {
    pub min_score: f32,
    pub min_margin: f32,
    pub min_letters: usize,
    pub max_bytes: usize,
    pub max_concurrent: usize,
}

impl Policy {
    pub async fn read(
        settings: &Settings,
        client: &dyn ReadClient,
    ) -> Result<Self, UnavailableReason> {
        let (score, margin, letters, bytes, concurrent) = tokio::try_join!(
            settings.get_global(client, MIN_SCORE),
            settings.get_global(client, MIN_MARGIN),
            settings.get_global(client, MIN_LETTERS),
            settings.get_global(client, MAX_BYTES),
            settings.get_global(client, MAX_CONCURRENT),
        )
        .map_err(|_| UnavailableReason::Settings)?;
        // Validate again at the boundary, including stored values from outside
        // the normal editor. Invalid policy never silently weakens the gate.
        let number = |value: &serde_json::Value, min: f64, max: f64| {
            value
                .as_f64()
                .filter(|v| v.is_finite() && (min..=max).contains(v))
                .ok_or(UnavailableReason::Settings)
        };
        let integer = |value: &serde_json::Value, min: u64, max: u64| {
            value
                .as_u64()
                .filter(|v| (min..=max).contains(v))
                .map(|v| v as usize)
                .ok_or(UnavailableReason::Settings)
        };
        let policy = Self {
            min_score: number(&score, 0.5, 1.0)? as f32,
            min_margin: number(&margin, 0.0, 1.0)? as f32,
            min_letters: integer(&letters, 8, 1000)?,
            max_bytes: integer(&bytes, 128, HARD_MAX_BYTES as u64)?,
            max_concurrent: integer(&concurrent, 1, 32)?,
        };
        if policy.min_letters > policy.max_bytes {
            return Err(UnavailableReason::Settings);
        }
        Ok(policy)
    }
}

pub(crate) fn valid_path(path: &str) -> bool {
    path.is_empty()
        || (path.len() <= 4096
            && std::path::Path::new(path).is_absolute()
            && !path.chars().any(char::is_control))
}

macro_rules! setting {
    ($key:expr, $kind:ident, $default:expr, $min:expr, $max:expr, $validator:expr, $label:literal, $help:literal) => {
        inventory::submit! { Contribution::new(DEFINITIONS, $key,
        Provenance { module: "base/language_detection", source: file!(), line: line!() },
        &Setting::Definition(Definition {
            value_type: ValueType::$kind, scopes: &[ScopeKind::Global],
            default: || json!($default), nullable: false, encrypted: false,
            minimum: $min, maximum: $max, validator: $validator,
            editor: Some(Editor { group: "Language detection", label: $label, help: $help,
                capability: None, reveal_duration_ms: 5_000 }),
        })) }
    };
}

setting!(MODEL_PATH, String, "", None, None,
    Some(Validator { accepts: |v| v.as_str().is_some_and(valid_path), message: "must be empty or an absolute local file path without control characters" }),
    "Language model file", "Leave blank to use data/models/lid.176-bin-v1/lid.176.bin. Provision the model first, then restart the application after changing this path.");
setting!(MIN_SCORE, Float, 0.8, Some(0.5), Some(1.0), None,
    "Minimum language score", "Require at least this score from 0.5 to 1.0. Scores are model estimates, not measured certainty.");
setting!(
    MIN_MARGIN,
    Float,
    0.2,
    Some(0.0),
    Some(1.0),
    None,
    "Minimum language score gap",
    "Require this gap from 0.0 to 1.0 between the two highest language scores."
);
setting!(
    MIN_LETTERS,
    Integer,
    20,
    Some(8.0),
    Some(1000.0),
    None,
    "Minimum letters for language detection",
    "Leave short messages unknown until they contain this many letters, from 8 to 1000."
);
setting!(
    MAX_BYTES,
    Integer,
    8192,
    Some(128.0),
    Some(HARD_MAX_BYTES as f64),
    None,
    "Maximum language detection input (bytes)",
    "Leave larger messages unknown without truncating them. From 128 to 65536 UTF-8 bytes."
);
setting!(MAX_CONCURRENT, Integer, 2, Some(1.0), Some(32.0), None,
    "Concurrent language detections", "Limit local CPU work to this many calls, from 1 to 32. Extra calls return busy immediately. Restart after changing this limit.");

inventory::submit! { Contribution::new(CONSTRAINTS, "language_detection.input_bounds",
Provenance { module: "base/language_detection", source: file!(), line: line!() },
&Constraint {
    keys: &[MIN_LETTERS, MAX_BYTES],
    accepts: |values| values[0].as_u64().zip(values[1].as_u64()).is_some_and(|(letters, bytes)| letters <= bytes),
    message: "Minimum letters cannot exceed the maximum input size in bytes.",
}) }
