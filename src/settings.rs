//! Persistent user settings (`~/.config/local-dictation/settings.json`).
//!
//! The menu-bar UI writes this file; the daemon reads it at boot. It exists
//! so non-technical users can change the cleanup model, the push-to-talk
//! hotkey, and whether cleanup runs at all — without editing a launch
//! script or exporting env vars.
//!
//! Precedence at resolve time is always **env var > settings file > built-in
//! default**. That keeps the documented env knobs (`GEMMA_MODEL_PATH`,
//! `DICTATE_HOTKEY_KEYCODE`) authoritative for power users / scripted
//! launches: if one of those is set, the matching menu item is ignored.
//!
//! File format (all fields optional):
//!
//! ```json
//! {
//!     "gemma_model": "/abs/path/to/models/llm/qwen-2.5-1.5b-it/qwen2.5-1.5b-instruct-q4_k_m.gguf",
//!     "hotkey_keycode": 61,
//!     "cleanup_enabled": true
//! }
//!
//! (The `gemma_model` key name is historical — it holds the path to whatever
//! cleanup model is selected; the recommended default is now Qwen 2.5 1.5B.)
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Built-in default cleanup model, relative to the project tree. Shared by
/// the CLI resolver and the menu so "default" means the same thing in both.
/// Qwen 2.5 1.5B is the recommended cleanup model (best quality-per-ms in the
/// `examples/model_bakeoff.rs` lineup); the `GEMMA_*` names are historical.
pub const DEFAULT_GEMMA_REL: &str = "models/llm/s1-mini/s1-mini.gguf";

/// Push-to-talk hotkey options offered in the menu, as (label, keycode).
/// Each must be a modifier key handled by `keycode_to_modifier_flag` in the
/// daemon, so the press/release edge is detected correctly.
pub const HOTKEY_CHOICES: &[(&str, i64)] = &[
    ("Right Option", 0x3D),
    ("Right Command", 0x36),
    ("Right Control", 0x3E),
    ("Right Shift", 0x3C),
];

/// Human-readable name for a hotkey keycode (falls back to the hex code).
pub fn hotkey_name(keycode: i64) -> String {
    HOTKEY_CHOICES
        .iter()
        .find(|(_, code)| *code == keycode)
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| format!("key 0x{keycode:x}"))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Absolute path to the cleanup model `.gguf`. `None` ⇒ use the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemma_model: Option<String>,
    /// CGEvent keycode watched for push-to-talk. `None` ⇒ use the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey_keycode: Option<i64>,
    /// Whether LLM cleanup runs. `None` ⇒ enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_enabled: Option<bool>,
    /// Active formatting preset name (matches a key in `prompts.json`'s
    /// `formats` map). `None`/blank ⇒ the default cleanup prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_format: Option<String>,
    /// Incremental streaming ASR (default ON): transcribe completed sentences
    /// during the hold (VAD-segmented) for a live preview, then reassemble the
    /// cached segments at release. Whole-utterance cleanup still runs at release.
    /// Disabled while always-on pre-roll is active (they drain the capture ring
    /// differently). Override: `DICTATE_STREAMING_ASR=0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming_asr: Option<bool>,
    /// Release tail-capture (default ON): on key release, keep the mic open for a
    /// brief adaptive grace loop so the last word of an utterance isn't clipped,
    /// then stop. `None`/true ⇒ enabled. Override: `DICTATE_TAIL_CAPTURE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_capture: Option<bool>,
    /// Hard ceiling (ms) on the tail-capture grace loop — the longest the daemon
    /// will keep listening after release for a trailing word. `None` ⇒ 1500.
    /// Override: `DICTATE_TAIL_CAPTURE_CAP_MS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_capture_cap_ms: Option<u32>,
    /// Always-on mic: keep the input stream warm and retain this many ms of
    /// pre-roll, so the first words aren't clipped and there's no stream-open
    /// latency on key-press. `None`/absent ⇒ off (open-on-press, current
    /// behaviour). A sensible on value is `400`. Override: `DICTATE_PREROLL_MS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preroll_ms: Option<u32>,
    /// EXPERIMENTAL wake-word listen mode: while idle, transcribe speech and, on
    /// hearing the wake word, dictate the rest into the focused field (no key).
    /// Requires always-on mic (`preroll_ms` > 0). `None`/false ⇒ off. Override:
    /// `DICTATE_LISTEN_MODE=1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_mode: Option<bool>,
    /// The phrase that triggers listen-mode dictation. `None`/blank ⇒
    /// [`DEFAULT_WAKE_WORD`]. Override: `DICTATE_WAKE_WORD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_word: Option<String>,
    /// Which ASR engine transcribes audio: `"parakeet"` (default, lowest
    /// latency) or `"cohere"` (Cohere Transcribe 2B, higher accuracy + 14
    /// languages). `None`/unknown ⇒ Parakeet. Override: `DICTATE_ASR_BACKEND`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_backend: Option<String>,
    /// Cohere weight quantization: `"int4"` (default) or `"int8"`. Only used
    /// when `asr_backend` is `"cohere"`. Override: `DICTATE_COHERE_QUANT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cohere_quant: Option<String>,
    /// Language hint for the Cohere decoder (it has no auto-detect), e.g.
    /// `"en"`, `"de"`, `"zh"`, `"ja"`. `None`/blank ⇒ `"en"`. Override:
    /// `DICTATE_COHERE_LANG`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cohere_language: Option<String>,
    /// Moonshine model size: `"tiny"` (27M, fastest, ~12.6% WER) or `"base"`
    /// (61M, recommended, ~9-10% WER). `None`/unknown ⇒ Base. Only used when
    /// `asr_backend` is `"moonshine"`. Override: `DICTATE_MOONSHINE_VARIANT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moonshine_variant: Option<String>,
}

/// Default wake word for listen mode. Two distinct syllables, uncommon as a
/// sentence opener, so false triggers in normal speech are rare.
pub const DEFAULT_WAKE_WORD: &str = "computer";

impl Settings {
    /// `~/.config/local-dictation/settings.json` (None if `$HOME` is unset).
    pub fn config_path() -> Option<PathBuf> {
        crate::app_paths::config_file("settings.json")
    }

    /// Load settings, returning an empty (all-`None`) struct when the file is
    /// missing or unparseable. Never errors — a corrupt settings file should
    /// degrade to defaults, not stop the daemon from booting.
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    /// Load settings from an explicit path. Missing or corrupt files degrade to
    /// defaults; this is the test entry point.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(body) => serde_json::from_str(&body).unwrap_or_else(|e| {
                eprintln!("[warn] settings parse failed ({}): {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(), // missing file is the common case
        }
    }

    /// Write settings to the config path, creating the parent directory.
    pub fn save(&self) -> eyre::Result<()> {
        let path = Self::config_path()
            .ok_or_else(|| eyre::eyre!("cannot resolve settings path ($HOME unset)"))?;
        self.save_to(&path)
    }

    /// Write settings to an explicit path. Test entry point.
    pub fn save_to(&self, path: &Path) -> eyre::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| eyre::eyre!("create {}: {e}", dir.display()))?;
        }
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(path, body).map_err(|e| eyre::eyre!("write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Resolve the cleanup model path: `GEMMA_MODEL_PATH` env > settings >
    /// `default`.
    pub fn resolve_gemma<'a>(&'a self, default: &'a str) -> String {
        if let Ok(v) = std::env::var("S1_MODEL_PATH") {
            return v;
        }
        if let Ok(v) = std::env::var("GEMMA_MODEL_PATH") {
            return v;
        }
        self.gemma_model
            .clone()
            .unwrap_or_else(|| default.to_string())
    }

    /// Resolve the full ASR selection (backend + each model's directory) with
    /// the usual **env > settings > default** precedence. This is the single
    /// place backend choice is decided; the daemon and the CLI both go through
    /// it so they always agree.
    ///
    /// Env knobs: `DICTATE_ASR_BACKEND` (`parakeet`|`cohere`|`moonshine`|`whisper`),
    /// `DICTATE_COHERE_QUANT` (`int4`|`int8`), `DICTATE_COHERE_LANG`,
    /// `DICTATE_MOONSHINE_VARIANT` (`tiny`|`base`), `PARAKEET_MODEL_DIR`,
    /// `DICTATE_COHERE_MODEL_DIR`, `DICTATE_MOONSHINE_MODEL_DIR`,
    /// `DICTATE_WHISPER_MODEL_PATH`.
    pub fn resolve_asr(&self) -> crate::transcriber::AsrConfig {
        use crate::transcriber::{AsrBackend, AsrConfig, CohereQuant, MoonshineSize};

        // Existing backend settings are deliberately ignored: this is now a
        // fixed Cohere MLX appliance. Keep decoding old fields for migration.
        let backend = AsrBackend::Cohere;

        let cohere_quant = std::env::var("DICTATE_COHERE_QUANT")
            .ok()
            .and_then(|v| CohereQuant::parse(&v))
            .or_else(|| self.cohere_quant.as_deref().and_then(CohereQuant::parse))
            .unwrap_or_default();

        let cohere_language = std::env::var("DICTATE_COHERE_LANG")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                self.cohere_language
                    .clone()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_else(|| "en".to_string());

        let moonshine_size = std::env::var("DICTATE_MOONSHINE_VARIANT")
            .ok()
            .and_then(|v| MoonshineSize::parse(&v))
            .or_else(|| {
                self.moonshine_variant
                    .as_deref()
                    .and_then(MoonshineSize::parse)
            })
            .unwrap_or_default();

        let parakeet_dir = std::env::var("PARAKEET_MODEL_DIR")
            .unwrap_or_else(|_| crate::app_paths::parakeet_default_dir());

        let cohere_dir = std::env::var("DICTATE_COHERE_MODEL_DIR")
            .unwrap_or_else(|_| crate::app_paths::cohere_default_dir(cohere_quant.as_str()));

        let moonshine_dir = std::env::var("DICTATE_MOONSHINE_MODEL_DIR")
            .unwrap_or_else(|_| crate::app_paths::moonshine_default_dir(moonshine_size.as_str()));

        let whisper_model_path = std::env::var("DICTATE_WHISPER_MODEL_PATH")
            .unwrap_or_else(|_| crate::app_paths::whisper_default_path());

        AsrConfig {
            backend,
            helper_path: crate::app_paths::cohere_mlx_helper_path(),
            model_dir: crate::app_paths::cohere_mlx_default_dir(),
            language: cohere_language.clone(),
            max_tokens: 256,
            parakeet_dir,
            cohere_dir,
            cohere_quant,
            cohere_language,
            moonshine_dir,
            moonshine_size,
            whisper_model_path,
        }
    }

    /// Resolve incremental streaming ASR: `DICTATE_STREAMING_ASR` env > settings
    /// > true (on by default). Returns false when always-on pre-roll is active,
    /// because the two drain the capture ring differently and a later workstream
    /// reconciles capture. Env accepts `0`/`false`/`off`/`no` (case-insensitive)
    /// to disable.
    pub fn resolve_streaming_asr(&self) -> bool {
        if self.resolve_preroll_ms() > 0 {
            eprintln!(
                "[boot] streaming asr disabled (pre-roll active — capture reconciliation pending)"
            );
            return false;
        }
        if let Ok(v) = std::env::var("DICTATE_STREAMING_ASR") {
            let v = v.trim().to_ascii_lowercase();
            return !matches!(v.as_str(), "0" | "false" | "off" | "no");
        }
        self.streaming_asr.unwrap_or(true)
    }

    /// Resolve the pre-roll lookback in ms: `DICTATE_PREROLL_MS` env > settings
    /// > 0 (off). 0 means open-on-press (current behaviour); any positive value
    /// enables the always-on warm stream with that much lookback.
    pub fn resolve_preroll_ms(&self) -> u32 {
        if let Ok(v) = std::env::var("DICTATE_PREROLL_MS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                return n;
            }
        }
        self.preroll_ms.unwrap_or(0)
    }

    /// Resolve release tail-capture: `DICTATE_TAIL_CAPTURE` env > settings > true
    /// (on). Turned OFF when always-on pre-roll is active, because the two drain
    /// the capture ring differently and pre-roll's always-on stream already keeps
    /// rolling past the release.
    pub fn resolve_tail_capture(&self) -> bool {
        if self.resolve_preroll_ms() > 0 {
            return false;
        }
        if let Ok(v) = std::env::var("DICTATE_TAIL_CAPTURE") {
            let v = v.trim().to_ascii_lowercase();
            return !matches!(v.as_str(), "0" | "false" | "off" | "no");
        }
        self.tail_capture.unwrap_or(true)
    }

    /// Resolve the tail-capture grace cap in ms: `DICTATE_TAIL_CAPTURE_CAP_MS` env
    /// > settings > [`crate::audio::DEFAULT_TAIL_CAP_MS`] (1500).
    pub fn resolve_tail_capture_cap_ms(&self) -> u32 {
        if let Ok(v) = std::env::var("DICTATE_TAIL_CAPTURE_CAP_MS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                return n;
            }
        }
        self.tail_capture_cap_ms
            .unwrap_or(crate::audio::DEFAULT_TAIL_CAP_MS as u32)
    }

    /// Resolve listen mode: `DICTATE_LISTEN_MODE` env > settings > false. Note
    /// the daemon additionally requires `resolve_preroll_ms() > 0` to actually
    /// engage it (listen mode needs the always-on stream).
    pub fn resolve_listen_mode(&self) -> bool {
        if let Ok(v) = std::env::var("DICTATE_LISTEN_MODE") {
            let v = v.trim().to_ascii_lowercase();
            return matches!(v.as_str(), "1" | "true" | "on" | "yes");
        }
        self.listen_mode.unwrap_or(false)
    }

    /// Resolve the wake word: `DICTATE_WAKE_WORD` env > settings > default.
    /// Blank/whitespace falls back to [`DEFAULT_WAKE_WORD`].
    pub fn resolve_wake_word(&self) -> String {
        if let Ok(v) = std::env::var("DICTATE_WAKE_WORD") {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
        self.wake_word
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| DEFAULT_WAKE_WORD.to_string())
    }

    /// Resolve the hotkey keycode: `DICTATE_HOTKEY_KEYCODE` env > settings >
    /// `default`. Env value may be decimal or `0x`-prefixed hex.
    pub fn resolve_hotkey(&self, default: i64) -> i64 {
        if let Some(code) = std::env::var("DICTATE_HOTKEY_KEYCODE")
            .ok()
            .and_then(|s| parse_keycode(&s))
        {
            return code;
        }
        self.hotkey_keycode.unwrap_or(default)
    }

    /// Resolve the active formatting preset: `DICTATE_FORMAT` env > settings >
    /// none. Blank/whitespace is treated as unset. The returned name is matched
    /// (case-insensitively) against `prompts.json`'s `formats` keys by the
    /// cleanup engine; an unknown name simply falls back to default cleanup.
    pub fn resolve_format(&self) -> Option<String> {
        if let Ok(v) = std::env::var("DICTATE_FORMAT") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
        self.active_format
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    /// Resolve whether cleanup should run, given a CLI `--no-cleanup` flag.
    /// The flag is a hard override (always wins); otherwise the settings
    /// toggle decides, defaulting to enabled.
    pub fn resolve_cleanup_enabled(&self, cli_no_cleanup: bool) -> bool {
        if cli_no_cleanup {
            return false;
        }
        self.cleanup_enabled.unwrap_or(true)
    }
}

/// Parse a keycode string that may be decimal (`61`) or hex (`0x3d`).
pub fn parse_keycode(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(rest, 16).ok()
    } else {
        s.parse().ok()
    }
}

/// A discovered cleanup-model candidate for the menu picker.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    /// Human label — the model's directory name, e.g. `gemma-3-1b-it`.
    pub label: String,
    /// Absolute path to the `.gguf` file.
    pub path: String,
    /// Qualitative speed/accuracy hint for the menu, inferred from the model's
    /// parameter count (e.g. "fast · recommended", "most accurate · ~2.5× slower").
    /// `None` when the size can't be parsed from the name.
    pub hint: Option<String>,
}

/// Parse a parameter count (in billions) from a model directory name by looking
/// for a `<n>b` or `<n>m` token (e.g. `gemma-3-1b-it` → 1.0, `smollm2-360m-it`
/// → 0.36, `qwen-2.5-0.5b-it` → 0.5). `None` if no size token is present.
fn parse_param_billions(name: &str) -> Option<f32> {
    let lower = name.to_lowercase();
    for tok in lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.')) {
        // A size token ends in 'b' or 'm' with a numeric body, e.g. "1b", "0.5b",
        // "360m". Guard against the "3" in "gemma-3" (no unit suffix).
        let (unit, body) = match tok.strip_suffix('b') {
            Some(b) => ('b', b),
            None => match tok.strip_suffix('m') {
                Some(m) => ('m', m),
                None => continue,
            },
        };
        if body.is_empty() {
            continue;
        }
        if let Ok(n) = body.parse::<f32>() {
            return Some(if unit == 'm' { n / 1000.0 } else { n });
        }
    }
    None
}

/// Map a parameter count to a one-line speed/accuracy hint for the picker.
/// Thresholds are grounded in the measured ~2.5× cleanup-latency gap between
/// Gemma 1B and 4B on this machine (see examples/latency_lab).
fn model_hint(name: &str) -> Option<String> {
    let b = parse_param_billions(name)?;
    Some(
        if b < 0.8 {
            "fastest · roughest cleanup"
        } else if b <= 1.5 {
            "fast · recommended"
        } else if b < 3.0 {
            "balanced · slower"
        } else {
            "most accurate · ~2.5× slower"
        }
        .to_string(),
    )
}

/// Enumerate cleanup models under `<models base>/llm/<name>/*.gguf`, where the
/// base is resolved bundle-aware (see [`crate::app_paths`]). Each subdirectory
/// contributes its first `.gguf`. Returns paths canonicalized to absolute so a
/// selection survives a daemon relaunch regardless of CWD.
pub fn discover_llm_models() -> Vec<ModelChoice> {
    discover_llm_models_in(&crate::app_paths::llm_models_dir())
}

fn discover_llm_models_in(root: &Path) -> Vec<ModelChoice> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let label = match dir.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // First *.gguf inside this model directory.
        if let Ok(files) = std::fs::read_dir(&dir) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("gguf") {
                    let abs = std::fs::canonicalize(&p).unwrap_or(p);
                    out.push(ModelChoice {
                        hint: model_hint(&label),
                        label: label.clone(),
                        path: abs.to_string_lossy().into_owned(),
                    });
                    break;
                }
            }
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_billions_parsing() {
        assert_eq!(parse_param_billions("gemma-3-1b-it"), Some(1.0));
        assert_eq!(parse_param_billions("gemma-3-4b-it"), Some(4.0));
        assert_eq!(parse_param_billions("qwen-2.5-0.5b-it"), Some(0.5));
        assert_eq!(parse_param_billions("smollm2-360m-it"), Some(0.36));
        assert_eq!(parse_param_billions("llama-3.2-1b-it"), Some(1.0));
        // No size token → None (the "3" in "gemma-3" must not be misread).
        assert_eq!(parse_param_billions("some-model-it"), None);
    }

    #[test]
    fn model_hints_bucket_by_size() {
        assert_eq!(
            model_hint("smollm2-360m-it").as_deref(),
            Some("fastest · roughest cleanup")
        );
        assert_eq!(
            model_hint("gemma-3-1b-it").as_deref(),
            Some("fast · recommended")
        );
        assert_eq!(
            model_hint("gemma-3-4b-it").as_deref(),
            Some("most accurate · ~2.5× slower")
        );
        assert_eq!(model_hint("mystery-model").as_deref(), None);
    }

    #[test]
    fn parse_keycode_decimal_and_hex() {
        assert_eq!(parse_keycode("61"), Some(61));
        assert_eq!(parse_keycode("0x3d"), Some(0x3d));
        assert_eq!(parse_keycode("0X3D"), Some(0x3d));
        assert_eq!(parse_keycode(" 0x36 "), Some(0x36));
        assert_eq!(parse_keycode("nonsense"), None);
    }

    #[test]
    fn cleanup_flag_overrides_settings() {
        let mut s = Settings::default();
        s.cleanup_enabled = Some(true);
        assert!(!s.resolve_cleanup_enabled(true)); // CLI --no-cleanup wins
        assert!(s.resolve_cleanup_enabled(false));
    }

    #[test]
    fn cleanup_defaults_enabled_when_unset() {
        let s = Settings::default();
        assert!(s.resolve_cleanup_enabled(false));
    }

    #[test]
    fn hotkey_falls_back_to_default_when_unset() {
        // No env, no settings ⇒ default. (Env is process-global; we avoid
        // setting it here so the test stays hermetic.)
        if std::env::var_os("DICTATE_HOTKEY_KEYCODE").is_none() {
            let s = Settings::default();
            assert_eq!(s.resolve_hotkey(0x3d), 0x3d);
            let mut s2 = Settings::default();
            s2.hotkey_keycode = Some(0x36);
            assert_eq!(s2.resolve_hotkey(0x3d), 0x36);
        }
    }

    #[test]
    fn gemma_falls_back_to_default_when_unset() {
        if std::env::var_os("GEMMA_MODEL_PATH").is_none() {
            let s = Settings::default();
            assert_eq!(s.resolve_gemma("default.gguf"), "default.gguf");
            let mut s2 = Settings::default();
            s2.gemma_model = Some("chosen.gguf".to_string());
            assert_eq!(s2.resolve_gemma("default.gguf"), "chosen.gguf");
        }
    }

    #[test]
    fn resolve_format_from_settings_when_no_env() {
        // Hermetic: only assert the settings/blank behaviour when the env knob
        // isn't set (env is process-global).
        if std::env::var_os("DICTATE_FORMAT").is_none() {
            let s = Settings::default();
            assert_eq!(s.resolve_format(), None);

            let mut s2 = Settings::default();
            s2.active_format = Some("email".into());
            assert_eq!(s2.resolve_format(), Some("email".into()));

            let mut s3 = Settings::default();
            s3.active_format = Some("   ".into()); // blank ⇒ unset
            assert_eq!(s3.resolve_format(), None);
        }
    }

    #[test]
    fn preroll_resolves_off_by_default_and_from_settings() {
        if std::env::var_os("DICTATE_PREROLL_MS").is_none() {
            assert_eq!(Settings::default().resolve_preroll_ms(), 0);
            let mut s = Settings::default();
            s.preroll_ms = Some(400);
            assert_eq!(s.resolve_preroll_ms(), 400);
        }
    }

    #[test]
    fn listen_mode_off_by_default() {
        if std::env::var_os("DICTATE_LISTEN_MODE").is_none() {
            assert!(!Settings::default().resolve_listen_mode());
            let mut s = Settings::default();
            s.listen_mode = Some(true);
            assert!(s.resolve_listen_mode());
        }
    }

    #[test]
    fn wake_word_falls_back_to_default_when_blank() {
        if std::env::var_os("DICTATE_WAKE_WORD").is_none() {
            assert_eq!(Settings::default().resolve_wake_word(), DEFAULT_WAKE_WORD);
            let mut s = Settings::default();
            s.wake_word = Some("  ".into());
            assert_eq!(s.resolve_wake_word(), DEFAULT_WAKE_WORD);
            let mut s2 = Settings::default();
            s2.wake_word = Some("jarvis".into());
            assert_eq!(s2.resolve_wake_word(), "jarvis");
        }
    }

    #[test]
    fn discover_returns_empty_for_missing_dir() {
        let v = discover_llm_models_in(Path::new("/nonexistent/models/llm"));
        assert!(v.is_empty());
    }

    #[test]
    fn discover_finds_gguf_and_sorts() {
        let tmp = std::env::temp_dir().join(format!("fd-models-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for name in ["zeta-model", "alpha-model"] {
            let d = tmp.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("weights.gguf"), b"x").unwrap();
        }
        // A dir with no gguf should be skipped.
        std::fs::create_dir_all(tmp.join("empty-model")).unwrap();

        let v = discover_llm_models_in(&tmp);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].label, "alpha-model"); // sorted
        assert_eq!(v[1].label, "zeta-model");
        assert!(v[0].path.ends_with("weights.gguf"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("fd-settings-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");

        let mut s = Settings::default();
        s.hotkey_keycode = Some(0x36);
        s.cleanup_enabled = Some(false);
        s.active_format = Some("bullets".into());
        s.preroll_ms = Some(400);
        s.asr_backend = Some("cohere".into());
        s.moonshine_variant = Some("tiny".into());
        s.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.hotkey_keycode, Some(0x36));
        assert_eq!(loaded.cleanup_enabled, Some(false));
        assert_eq!(loaded.active_format, Some("bullets".into()));
        assert_eq!(loaded.preroll_ms, Some(400));
        assert_eq!(loaded.asr_backend, Some("cohere".into()));
        assert_eq!(loaded.moonshine_variant, Some("tiny".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let path = std::env::temp_dir().join(format!("fd-settings-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let s = Settings::load_from(&path);
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn load_corrupt_file_returns_defaults() {
        let dir = std::env::temp_dir().join(format!("fd-settings-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{ this is not valid json").unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s, Settings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_streaming_asr_defaults_on_and_settings_can_disable() {
        if std::env::var_os("DICTATE_STREAMING_ASR").is_none() {
            assert!(Settings::default().resolve_streaming_asr());
            let mut s = Settings::default();
            s.streaming_asr = Some(false);
            assert!(!s.resolve_streaming_asr());
        }
    }

    #[test]
    fn resolve_streaming_asr_disabled_by_preroll() {
        // Pre-roll and streaming are mutually exclusive for now (both drain the
        // capture ring differently); pre-roll wins. Only assert when no env knob
        // pins the outcome (env is process-global).
        if std::env::var_os("DICTATE_STREAMING_ASR").is_none()
            && std::env::var_os("DICTATE_PREROLL_MS").is_none()
        {
            let mut s = Settings::default();
            s.preroll_ms = Some(400);
            assert!(!s.resolve_streaming_asr());
        }
    }

    #[test]
    fn tail_capture_defaults_on_and_can_be_disabled() {
        if std::env::var_os("DICTATE_TAIL_CAPTURE").is_none() {
            assert!(Settings::default().resolve_tail_capture());
            let mut s = Settings::default();
            s.tail_capture = Some(false);
            assert!(!s.resolve_tail_capture());
        }
    }

    #[test]
    fn tail_capture_disabled_by_preroll() {
        // Always-on pre-roll keeps rolling past release, so release grace is off.
        if std::env::var_os("DICTATE_TAIL_CAPTURE").is_none()
            && std::env::var_os("DICTATE_PREROLL_MS").is_none()
        {
            let mut s = Settings::default();
            s.preroll_ms = Some(400);
            assert!(!s.resolve_tail_capture());
        }
    }

    #[test]
    fn tail_capture_cap_defaults_and_parses() {
        if std::env::var_os("DICTATE_TAIL_CAPTURE_CAP_MS").is_none() {
            assert_eq!(Settings::default().resolve_tail_capture_cap_ms(), 1500);
            let mut s = Settings::default();
            s.tail_capture_cap_ms = Some(800);
            assert_eq!(s.resolve_tail_capture_cap_ms(), 800);
        }
    }

    #[test]
    fn resolve_cohere_quant_and_language() {
        if std::env::var_os("DICTATE_COHERE_QUANT").is_none()
            && std::env::var_os("DICTATE_COHERE_LANG").is_none()
        {
            let s = Settings::default();
            let cfg = s.resolve_asr();
            assert_eq!(cfg.cohere_quant, crate::transcriber::CohereQuant::Int4);
            assert_eq!(cfg.cohere_language, "en");
        }
    }

    #[test]
    fn resolve_moonshine_size_and_dir() {
        if std::env::var_os("DICTATE_MOONSHINE_VARIANT").is_none()
            && std::env::var_os("DICTATE_MOONSHINE_MODEL_DIR").is_none()
            && std::env::var_os("DICTATE_ASR_BACKEND").is_none()
        {
            // Default: Base.
            let s = Settings::default();
            let cfg = s.resolve_asr();
            assert_eq!(cfg.moonshine_size, crate::transcriber::MoonshineSize::Base);
            assert!(cfg.moonshine_dir.ends_with("moonshine-base"));

            // Settings-driven Tiny.
            let mut s = Settings::default();
            s.moonshine_variant = Some("tiny".into());
            let cfg = s.resolve_asr();
            assert_eq!(cfg.moonshine_size, crate::transcriber::MoonshineSize::Tiny);
            assert!(cfg.moonshine_dir.ends_with("moonshine-tiny"));

            // Unknown size falls back to Base.
            let mut s = Settings::default();
            s.moonshine_variant = Some("medium".into());
            let cfg = s.resolve_asr();
            assert_eq!(cfg.moonshine_size, crate::transcriber::MoonshineSize::Base);
        }
    }
}
