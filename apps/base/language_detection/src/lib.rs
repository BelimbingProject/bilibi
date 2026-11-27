//! Shared, local language identification. See the module README for provisioning,
//! score semantics, input policy and the process lifetime of the model cache.
mod model;
mod policy;

use bilibi_module_registry::{Layer, Module, Service};
use bilibi_settings::Settings;
use bilibi_tenancy::ReadClient;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

pub const SERVICE: &str = "base/language_detection.detector";
pub const MODEL_VERSION: &str = "lid.176-bin-v1";

/// A label from the pinned model, without fastText's `__label__` prefix.
/// These are the upstream language codes, not regional locale preferences.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Language(String);

impl Language {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnknownReason {
    Empty,
    NoLetters,
    TooShort,
    TooLong,
    ControlCharacters,
    ReservedToken,
    NoPrediction,
    LowScore,
    Ambiguous,
    MixedLanguages,
    UnsupportedLabel,
}

/// Operational failure is separate from insufficient language evidence.
/// Callers can retain the message and defer enrichment for any of these.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    Settings,
    Busy,
    MissingModel,
    UnreadableModel,
    ModelIntegrity,
    InvalidModel,
    Worker,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Detection {
    Detected {
        language: Language,
        /// fastText's score in [0, 1], not calibrated certainty or accuracy.
        confidence: f32,
    },
    Unknown(UnknownReason),
    Unavailable(UnavailableReason),
}

/// Resolve this facade from `SERVICE` after declaring `base/language_detection`.
/// It stores no customer text and performs no outbound inference or downloads.
pub struct LanguageDetector {
    settings: Arc<Settings>,
}

// Even separate registry installations in one application share one model and
// one CPU budget. A failed load is cached too; provisioning/repair needs restart.
static MODEL: OnceLock<Result<Arc<fasttext::FastText>, UnavailableReason>> = OnceLock::new();
static CPU: OnceLock<Arc<Semaphore>> = OnceLock::new();

impl LanguageDetector {
    /// Pure enrichment: the client reads global operator policy only. No tenant
    /// records are read or written, so this operation needs no tenant scope.
    /// Call outside a message's intake transaction; every outcome is recoverable.
    pub async fn detect(&self, client: &dyn ReadClient, text: &str) -> Detection {
        if text.len() > policy::HARD_MAX_BYTES {
            return Detection::Unknown(UnknownReason::TooLong);
        }
        if text.trim().is_empty() {
            return Detection::Unknown(UnknownReason::Empty);
        }
        if text
            .chars()
            .any(|ch| ch.is_control() && !ch.is_whitespace())
        {
            return Detection::Unknown(UnknownReason::ControlCharacters);
        }
        if text
            .split_whitespace()
            .any(|token| token == "</s>" || token.starts_with("__label__"))
        {
            return Detection::Unknown(UnknownReason::ReservedToken);
        }
        let letters = text.chars().filter(|ch| ch.is_alphabetic()).count();
        if letters == 0 {
            return Detection::Unknown(UnknownReason::NoLetters);
        }
        let policy = match policy::Policy::read(&self.settings, client).await {
            Ok(policy) => policy,
            Err(reason) => return Detection::Unavailable(reason),
        };
        if text.len() > policy.max_bytes {
            return Detection::Unknown(UnknownReason::TooLong);
        }
        if letters < policy.min_letters {
            return Detection::Unknown(UnknownReason::TooShort);
        }
        let cpu = CPU.get_or_init(|| Arc::new(Semaphore::new(policy.max_concurrent)));
        let Ok(permit) = cpu.clone().try_acquire_owned() else {
            return Detection::Unavailable(UnavailableReason::Busy);
        };
        let path = if MODEL.get().is_none() {
            match self.settings.get_global(client, policy::MODEL_PATH).await {
                Ok(value) => match value.as_str().filter(|path| policy::valid_path(path)) {
                    Some(path) => path.to_owned(),
                    None => return Detection::Unavailable(UnavailableReason::Settings),
                },
                Err(_) => return Detection::Unavailable(UnavailableReason::Settings),
            }
        } else {
            String::new()
        };
        // Only bounded text is copied. The permit lives in the blocking job even
        // if the caller cancels its future, so cancellation cannot bypass limits.
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match MODEL.get_or_init(|| model::load(&path).map(Arc::new)) {
                Ok(model) => model::detect(model, &text, policy),
                Err(reason) => Detection::Unavailable(*reason),
            }
        })
        .await
        .unwrap_or(Detection::Unavailable(UnavailableReason::Worker))
    }
}

inventory::submit! {
    Module {
        id: "base/language_detection",
        layer: Layer::Base,
        dependencies: &["base/module_registry", "base/settings", "base/tenancy"],
        migrations: &[],
        services: &[Service { id: SERVICE, factory: |services| {
            Ok(Arc::new(LanguageDetector {
                settings: services.get::<Settings>("base/settings.store")?,
            }))
        }}],
    }
}
