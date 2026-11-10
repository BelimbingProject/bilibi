# Language detection

`base/language_detection` provides one shared local fastText detector for applications and modules; it does not integrate conversations or choose a person's UI locale.

## Call the facade

Declare `bilibi-language-detection.workspace = true` and the descriptor dependency `base/language_detection`, then resolve `LanguageDetector` from `SERVICE` (`base/language_detection.detector`). Call `detect(client, text).await` with a `bilibi_tenancy::ReadClient`. Only global operator settings are read: no tenant records, message persistence, audit writes, agent operations or HTTP endpoints are involved. Keep enrichment outside the message intake transaction and handle every outcome without refusing the message.

| Outcome | Meaning |
| --- | --- |
| `Detection::Detected { language, confidence }` | The upstream language code (`language.as_str()`) and fastText score meet policy. Codes include Tamil `ta`, Sinhala `si`, English `en` and Malay `ms`, among 176 labels. |
| `Detection::Unknown(reason)` | The input lacks usable evidence or the model's result is insufficient. No fallback language is invented. |
| `Detection::Unavailable(reason)` | Settings, model or worker failure, or the local CPU budget is busy. Retain the message; defer or omit enrichment according to the caller's policy. |

`confidence` is the model's uncalibrated score, not a measured probability of correctness. Scores are bounded to `[0, 1]` (tiny upstream floating point overshoots are clamped). The top score must meet `min_score`, and its gap over the runner-up must meet `min_margin`, inclusively. No prediction or an invalid score stays unknown. Model labels are language codes, not regional locale tags or a translation catalogue; `base/locale` remains the owner of UI regional preferences.

## Input policy

Empty/whitespace, inputs with no alphabetic characters, control characters other than whitespace, short inputs, and oversized UTF-8 input return typed unknown reasons. Reserved model tokens (`</s>` or a token starting with `__label__`) also stay unknown, so input cannot terminate or alter model tokenization. Oversized messages are refused for detection in full, never silently truncated. Unicode whitespace is normalized to ASCII separators before inference. Substantial sentence spans (at most eight) and two halves are also scored; conflicting confident labels return `MixedLanguages`. This bounded heuristic catches common code-switching, not every mixture. A whole-message low score or small margin stays unknown.

fastText is a closed-set classifier trained on Wikipedia, Tatoeba and SETimes, not a conversation-specific or out-of-distribution detector. Shared words, names, misspellings, unsupported languages and adversarial text can still receive a confidently wrong supported label. No reliable Romanized Tamil/Sinhala detection is promised. A caller must preserve explicit user language choices and treat detection as optional evidence. No message contents are logged or retained by this module.

## Operator settings

The existing **System Settings** page exposes these global settings under **Language detection**; its normal operator authorization and audit apply. No environment-only policy or separate settings page exists.

| Key (prefix `language_detection.`) | Default | Valid values / effect |
| --- | --- | --- |
| `model_path` | empty | Empty uses `data/models/lid.176-bin-v1/lid.176.bin` relative to the process working directory; otherwise an absolute local file path, at most 4096 bytes, without control characters. Restart after changing or repairing the asset. |
| `min_score` | `0.8` | Float, `0.5..=1.0`; raising it leaves more inputs unknown. |
| `min_margin` | `0.2` | Float, `0.0..=1.0`; minimum gap between the top two scores. |
| `min_letters` | `20` | Integer, `8..=1000` Unicode alphabetic characters. |
| `max_bytes` | `8192` | Integer, `128..=65536` UTF-8 bytes; 65536 is also a hard resource ceiling checked before policy reads. |
| `max_concurrent` | `2` | Integer, `1..=32`; per-process CPU budget, fixed at first use; restart to change it. |

Score and input policy are read on each call and take effect without restart. Settings refuses a minimum letter count above the maximum byte size. The boundary validates stored policy again and returns `Unavailable(Settings)` for invalid/unreadable settings. Defaults are conservative starting points, not accuracy guarantees; evaluate representative application text before adjusting them.

## Model asset

Provision the separately versioned `lid.176-bin-v1` asset explicitly; no binary is committed, bundled into releases, downloaded at boot, or downloaded per request.

```sh
python3 /path/to/bilibi/scripts/provision-language-model.py /opt/bilibi/<instance>/data/models/lid.176-bin-v1
```

The helper needs Python 3.11 or newer and its standard library. Run as the deployment account with write access to the chosen directory, then ensure the application can read it. The [installed instance layout](../../../docs/server-install.md) uses the instance as working directory, with persistent `data/` across releases. The default path therefore survives upgrades and works inside the existing service sandbox. For another directory, set the absolute `model_path` through System Settings and ensure service read access. Offline deployments can copy the verified binary, `model.json` and `NOTICE.md` to that directory; the application independently checks the asset at load.

[`model.json`](model.json) pins the authoritative HTTPS download URL, exact size (**131266198 bytes**) and SHA-256 (**`7e69ec5451bc261cc7844e49e4792a85d7f09c06789ec800fc4a44aec362764e`**). This digest was computed from the download linked by the [fastText authors](https://fasttext.cc/docs/en/language-identification.html); upstream does not publish a signed checksum. It also matches the [original Hugging Face import's immutable LFS pointer](https://huggingface.co/julien-c/fasttext-language-id/blob/0266da4549434de56667387618bc67dc6d2670ef/lid.176.bin). The helper fails on size/hash mismatch, publishes only a verified complete file atomically, refuses to overwrite a mismatching existing file, and installs attribution beside it. A future asset update needs a new version and reviewed checksum; arbitrary replacement models and the compressed `.ftz` asset are not accepted.

The model is unchanged and licensed **CC-BY-SA-3.0**, separately from the MIT application code. Preserve [`NOTICE.md`](NOTICE.md), the model authors' attribution, license link and requested paper citations with redistributed assets; identify modifications and honor ShareAlike when distributing adaptations.

## Process and CPU behavior

The service factory performs no model I/O and a missing model never prevents boot. The first eligible call loads and verifies the asset on Tokio's blocking pool; a process-wide `OnceLock` shares the resulting read-only model across all module calls and registry installations. It parses the same verified bytes, avoiding a reopen between verification and parsing. A missing file returns `MissingModel`, access/I/O failure `UnreadableModel`, wrong size/hash/non-file `ModelIntegrity`, and parse failure `InvalidModel`. The first load failure is cached too, so fixing/provisioning the file requires process restart. Empty/short inputs do not trigger loading.

Inference uses `fasttext = 0.8.1`, a maintained [pure Rust implementation](https://docs.rs/crate/fasttext/0.8.1), locked in Cargo. Its published source uses Rust edition 2021, no build script or C/C++ binding; the workspace Rust 1.99 toolchain suffices. Single predictions run synchronously on CPU using read-only shared matrices and per-call scratch state; the facade sends that work to Tokio's blocking pool. Its semaphore bounds concurrent load/inference jobs, rejects excess calls with `Busy` without an internal queue, and holds permits through cancellation until the blocking job finishes. Loading temporarily holds the 126 MiB asset bytes plus the parsed model; steady memory holds one parsed model per process. Each eligible input performs at most eleven predictions. Multiple application processes each allocate their own copy; there is no central service, GPU requirement or outbound inference call.

## Executable walkthrough

Use a fresh disposable database with the [runtime evidence convention](../../../docs/runtime-parity.md). This walkthrough links the real registry, Settings, mutation/audit boundary and versioned walkthrough fixture; it writes only detector settings in the disposable database and never prints input text. It has no unit-test harness, mocks or assertions.

```sh
export BILIBI_DISPOSABLE_LANGUAGE_WALKTHROUGH=yes
export DATABASE_URL=<disposable-database-url>
cargo run -p bilibi-language-detection --locked --example language-detection-walkthrough -- /absolute/path/to/lid.176.bin
```

Inspect Tamil `ta`, Sinhala `si`, English `en`, Malay `ms` with scores; unknown empty/symbol/short/oversize/control/ambiguous/mixed specimens; repeat-call timings; and refused invalid thresholds, limits and paths. The separate Malay sentence with shared vocabulary demonstrates that even valid supported-language text can remain unknown under the default threshold; the Romanized specimen is observational only. Run again in fresh processes with a nonexistent path and a corrupt file: all eligible specimens must report the relevant unavailable reason on repeated calls. A same-size corrupt copy must fail the checksum too. For cache proof, append `pause` to the command, move the model file away at the prompt, and press Enter: subsequent calls still use the cached model. Restore the asset before starting a new process.

Use the running app's System Settings page to change a detector setting, reload, and inspect a refused invalid value. Record observations outside the source tree alongside the repository discovery, formatting, Clippy and asset checks. Conversations integration and application adoption are later changes.
