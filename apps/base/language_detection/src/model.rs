use crate::{policy::Policy, Detection, Language, UnavailableReason, UnknownReason};
use fasttext::FastText;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Cursor, Read},
    path::Path,
};

#[derive(Deserialize)]
struct Asset {
    bytes: u64,
    sha256: String,
}

pub(crate) fn load(path: &str) -> Result<FastText, UnavailableReason> {
    let asset: Asset = serde_json::from_str(include_str!("../model.json"))
        .map_err(|_| UnavailableReason::InvalidModel)?;
    let path = Path::new(if path.is_empty() {
        "data/models/lid.176-bin-v1/lid.176.bin"
    } else {
        path
    });
    let metadata = std::fs::metadata(path).map_err(file_error)?;
    if !metadata.is_file() || metadata.len() != asset.bytes {
        return Err(UnavailableReason::ModelIntegrity);
    }
    let mut bytes = Vec::with_capacity(asset.bytes as usize);
    File::open(path)
        .map_err(file_error)?
        .take(asset.bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(file_error)?;
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if bytes.len() as u64 != asset.bytes || digest != asset.sha256 {
        return Err(UnavailableReason::ModelIntegrity);
    }
    // Parse exactly the bytes that were verified; never reopen the path after
    // hashing. Untrusted/truncated/quantized assets never reach the parser.
    FastText::load(&mut Cursor::new(bytes)).map_err(|_| UnavailableReason::InvalidModel)
}

fn file_error(error: std::io::Error) -> UnavailableReason {
    if error.kind() == std::io::ErrorKind::NotFound {
        UnavailableReason::MissingModel
    } else {
        UnavailableReason::UnreadableModel
    }
}

fn candidate(
    model: &FastText,
    text: &str,
    policy: &Policy,
) -> Result<(Language, f32), UnknownReason> {
    // Normalize Unicode whitespace to the model's ASCII token separators;
    // preserve punctuation/case and the caller's original string.
    let normalized: String = text
        .chars()
        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
        .collect();
    let predictions = model.predict(&normalized, 2, 0.0);
    let first = predictions.first().ok_or(UnknownReason::NoPrediction)?;
    let score = |value: f32| {
        // Upstream's log/exp approximation can exceed 1 by a few ulps.
        if value.is_finite() && (0.0..=1.0001).contains(&value) {
            Ok(value.min(1.0))
        } else {
            Err(UnknownReason::NoPrediction)
        }
    };
    let confidence = score(first.prob)?;
    let runner_up = predictions
        .get(1)
        .map(|p| score(p.prob))
        .transpose()?
        .unwrap_or(0.0);
    if confidence < policy.min_score {
        return Err(UnknownReason::LowScore);
    }
    if confidence - runner_up < policy.min_margin {
        return Err(UnknownReason::Ambiguous);
    }
    let language = first
        .label
        .strip_prefix("__label__")
        .filter(|code| {
            (2..=3).contains(&code.len()) && code.bytes().all(|ch| ch.is_ascii_lowercase())
        })
        .ok_or(UnknownReason::UnsupportedLabel)?;
    Ok((Language(language.to_owned()), confidence))
}

pub(crate) fn detect(model: &FastText, text: &str, policy: Policy) -> Detection {
    let whole = candidate(model, text, &policy);
    // Check bounded, substantial spans as well as the whole input. This catches
    // common code-switching that a dominant whole-message score can conceal.
    // It is a refusal heuristic, not language segmentation or a mixed-text proof.
    let midpoint = text
        .char_indices()
        .find(|(index, _)| *index >= text.len() / 2)
        .map_or(text.len(), |(index, _)| index);
    let split = text[midpoint..]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(midpoint, |(index, _)| midpoint + index);
    let halves = [&text[..split], &text[split..]];
    let mut language = None;
    for span in text
        .split(['\n', '.', '!', '?', '。', '！', '？'])
        .take(8)
        .chain(halves)
    {
        if span.chars().filter(|ch| ch.is_alphabetic()).count() < policy.min_letters {
            continue;
        }
        if let Ok((found, _)) = candidate(model, span, &policy) {
            if language.as_ref().is_some_and(|previous| previous != &found)
                || whole.as_ref().is_ok_and(|(whole, _)| whole != &found)
            {
                return Detection::Unknown(UnknownReason::MixedLanguages);
            }
            language = Some(found);
        }
    }
    match whole {
        Ok((language, confidence)) => Detection::Detected {
            language,
            confidence,
        },
        Err(reason) => Detection::Unknown(reason),
    }
}
