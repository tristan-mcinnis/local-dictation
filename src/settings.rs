//! Persistent user settings (`~/.config/local-dictation/settings.json`).
//!
//! The menu-bar UI writes this file; the daemon reads it at boot. It exists
//! so non-technical users can change the cleanup model, the push-to-talk
//! hotkey, and whether cleanup runs at all — without editing a launch
//! script or exporting env vars.
//!
//! Precedence at resolve time is always **env var > settings file > built-in
//! default**. That keeps the documented env knobs (`DICTATE_LLM_MODEL_PATH`,
//! `DICTATE_HOTKEY_KEYCODE`) authoritative for power users / scripted
//! launches: if one of those is set, the matching menu item is ignored.
//!
//! File format (all fields optional; the full key list is in
//! `docs/architecture.md`):
//!
//! ```json
//! {
//!     "llm_model": "/abs/path/to/models/llm/s1-mini/s1-mini.gguf",
//!     "hotkey_keycode": 61,
//!     "cleanup_enabled": true
//! }
//! ```
//!
//! `llm_model` is the path to whichever cleanup model is selected (the name is
//! model-neutral on purpose; the shipped default is S1-mini). The pre-2026-09
//! key `gemma_model` is still read once and migrated; see
//! `migrate_legacy_keys`. A single malformed value only resets that key
//! (`parse_lenient`), and writes are atomic (`config_io`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillSize {
    Compact,
    Standard,
}

impl PillSize {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Standard => "standard",
        }
    }

    fn resolve(saved: Option<&str>, override_value: Option<&str>) -> Self {
        match override_value
            .or(saved)
            .unwrap_or("compact")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "standard" => Self::Standard,
            _ => Self::Compact,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveformSensitivity {
    Low,
    Balanced,
    High,
}

impl WaveformSensitivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Balanced => "balanced",
            Self::High => "high",
        }
    }

    fn resolve(saved: Option<&str>, override_value: Option<&str>) -> Self {
        match override_value
            .or(saved)
            .unwrap_or("balanced")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "low" => Self::Low,
            "high" => Self::High,
            _ => Self::Balanced,
        }
    }
}

/// Derived UI state; the saved streaming preference remains unchanged when
/// always-on capture temporarily makes preview unavailable.
#[derive(Debug, PartialEq, Eq)]
pub struct LivePreviewControl {
    pub enabled: bool,
    pub editable: bool,
    pub hint: &'static str,
}

impl LivePreviewControl {
    fn resolve(saved: Option<bool>, override_value: Option<&str>, preroll_ms: u32) -> Self {
        let enabled = resolve_enabled_flag(saved, override_value, true);
        if preroll_ms > 0 {
            Self {
                enabled: false,
                editable: false,
                hint: "Turn off Always-on mic to use live preview.",
            }
        } else if override_value.is_some() {
            Self {
                enabled,
                editable: false,
                hint: "Controlled by DICTATE_STREAMING_ASR.",
            }
        } else {
            Self {
                enabled,
                editable: true,
                hint: "Draft phrases appear as you pause.",
            }
        }
    }
}

/// One rule for every on/off env override: `1`/`true`/`on`/`yes` and
/// `0`/`false`/`off`/`no`, any case, trimmed. Anything else is not a value and
/// falls through to the saved setting, as a bad number does.
fn parse_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

fn resolve_enabled_flag(saved: Option<bool>, override_value: Option<&str>, default: bool) -> bool {
    override_value.and_then(parse_flag).or(saved).unwrap_or(default)
}

fn resolve_audio_retention_value(
    saved: Option<&str>,
    override_value: Option<&str>,
) -> &'static str {
    let raw = override_value
        .or(saved)
        .unwrap_or("1month")
        .trim()
        .to_ascii_lowercase();
    match raw.as_str() {
        "24hours" | "24 hours" | "1day" | "1 day" | "day" => "24hours",
        "1week" | "1 week" | "week" => "1week",
        "1month" | "1 month" | "month" => "1month",
        "1year" | "1 year" | "year" => "1year",
        "never" => "never",
        // Invalid values must not silently shorten an existing retention choice.
        _ => "forever",
    }
}

fn resolve_audio_cap(saved: Option<u64>, override_value: Option<&str>) -> u64 {
    override_value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .or(saved)
        .unwrap_or(0)
}

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
/// One naming table lives in `hotkeys`; this is a thin alias for menu code.
pub fn hotkey_name(keycode: i64) -> String {
    crate::hotkeys::modifier_keycode_name(keycode)
}

/// Env var that pins the cleanup model path (wins over `settings.json`).
pub const LLM_MODEL_PATH_ENV: &str = "DICTATE_LLM_MODEL_PATH";
/// Optional instruction-model override. Empty explicitly disables the second model.
pub const INSTRUCTION_MODEL_PATH_ENV: &str = "DICTATE_INSTRUCTION_MODEL_PATH";

/// Languages actually supported by the fixed Cohere checkpoint/tokenizer.
pub const COHERE_LANGUAGE_CHOICES: &[(&str, &str)] = &[
    ("en", "English (en)"), ("fr", "French (fr)"), ("de", "German (de)"),
    ("es", "Spanish (es)"), ("it", "Italian (it)"), ("pt", "Portuguese (pt)"),
    ("nl", "Dutch (nl)"), ("pl", "Polish (pl)"), ("el", "Greek (el)"),
    ("ar", "Arabic (ar)"), ("ja", "Japanese (ja)"), ("zh", "Chinese (zh)"),
    ("vi", "Vietnamese (vi)"), ("ko", "Korean (ko)"),
];

/// Resolve one optional model selection without reading process-global state.
/// An explicit empty override/configuration means disabled, not "use default".
pub fn resolve_instruction_model_from(
    environment: Option<&str>, configured: Option<&str>, default: Option<&str>,
) -> Option<String> {
    environment.or(configured).or(default)
        .map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

/// Env var that pins the completion sound id (wins over `settings.json`).
pub const COMPLETION_SOUND_ENV: &str = "DICTATE_COMPLETION_SOUND";
/// Env var that pins the start sound id (wins over `settings.json`).
pub const START_SOUND_ENV: &str = "DICTATE_START_SOUND";
/// Still-accepted spellings of `LLM_MODEL_PATH_ENV`, checked in order after
/// it. `S1_MODEL_PATH` is a documented alias (the model is S1-mini);
/// `GEMMA_MODEL_PATH` is the pre-rename name and logs a one-time deprecation
/// warning. Drop `GEMMA_MODEL_PATH` after one release.
pub const LLM_MODEL_PATH_ENV_ALIASES: &[&str] = &["S1_MODEL_PATH", "GEMMA_MODEL_PATH"];

/// The cleanup-model env override, if any: `(var name, value)`. New name wins;
/// aliases are accepted; the deprecated `GEMMA_MODEL_PATH` warns once.
pub fn llm_model_env() -> Option<(&'static str, String)> {
    if let Ok(v) = std::env::var(LLM_MODEL_PATH_ENV) {
        return Some((LLM_MODEL_PATH_ENV, v));
    }
    for &name in LLM_MODEL_PATH_ENV_ALIASES {
        if let Ok(v) = std::env::var(name) {
            if name == "GEMMA_MODEL_PATH" {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    eprintln!(
                        "[warn] GEMMA_MODEL_PATH is deprecated; set {LLM_MODEL_PATH_ENV} instead"
                    )
                });
            }
            return Some((name, v));
        }
    }
    None
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Absolute path to the cleanup model `.gguf`. `None` ⇒ use the default.
    /// On-disk key `llm_model` (since 2026-09). The pre-rename key
    /// `gemma_model` is still read as a fallback for one release and moved
    /// here on load (see `migrate_legacy_keys`); it is never written back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    /// GGUF for rewrites, translations and custom output instructions. None
    /// uses an available default; an empty string explicitly disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_model: Option<String>,
    /// Legacy spelling of `llm_model`. Read-only: `load_from` folds it into
    /// `llm_model` and `save` drops it. Remove after one release.
    #[serde(default, skip_serializing)]
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
    /// Recording pill size. Defaults to compact; DICTATE_PILL_SIZE overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pill_size: Option<String>,
    /// Visual waveform response, never microphone gain or recognition volume.
    /// Defaults to balanced; DICTATE_WAVEFORM_SENSITIVITY overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waveform_sensitivity: Option<String>,
    /// Name the current processing stage in the pill (default true).
    /// DICTATE_SHOW_PROCESSING_DETAIL overrides; this never gates processing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_processing_detail: Option<bool>,
    /// Preserve recordings locally for recovery. Enabled by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_history_enabled: Option<bool>,
    /// Audio retention is independent of text history. Default: 30 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_retention: Option<String>,
    /// Optional budget for recovery audio in MiB. Default: 0 (retention only).
    /// An explicit cap can remove older audio before its time window expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_storage_cap_mb: Option<u64>,
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
    /// Warm-window (ms): how long an idle capture resource is retained with IO
    /// paused, so a follow-up dictation can resume without rebuilding the stream.
    /// Explicit always-on pre-roll remains a separate mode. 0 disables reuse (each
    /// utterance opens fresh). `None` ⇒ 60000. Override: `DICTATE_WARM_WINDOW_MS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_window_ms: Option<u32>,
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
    /// Language hint for the Cohere decoder (it has no auto-detect), e.g.
    /// `"en"`, `"de"`, `"zh"`, `"ja"`. `None`/blank ⇒ `"en"`. Override:
    /// `DICTATE_COHERE_LANG`. This is the only surviving ASR setting — the
    /// engine itself is fixed to Cohere MLX Q4. (Legacy `asr_backend` /
    /// `cohere_quant` / `moonshine_variant` keys in an old settings file are
    /// silently ignored by serde — see `load_from`.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cohere_language: Option<String>,

    /// Modifier mask for the push-to-talk hotkey (a CGEventFlags bitmask of
    /// Shift/Control/Option/Command). 0 means the hotkey is a bare modifier key
    /// (the historical default, Right Option). Non-zero turns the hotkey into a
    /// chord: `<modifiers> + <hotkey_keycode>`. Override:
    /// `DICTATE_HOTKEY_MODIFIERS` (decimal or `0x`-hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey_modifiers: Option<u64>,

    /// Ordered list of target languages for Translate mode, e.g.
    /// `["Spanish", "Chinese"]`. Empty ⇒ bare "translate" is not available.
    /// Override: `DICTATE_TRANSLATION_TARGETS` (comma-separated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation_targets: Option<Vec<String>>,

    /// The active Translate-mode target language (must appear in
    /// `translation_targets`). `None` ⇒ the first target is used. Override:
    /// `DICTATE_ACTIVE_TRANSLATION_TARGET`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_translation_target: Option<String>,

    /// How long to keep dictation history: `"forever"` (default), `"1year"`,
    /// `"1month"`, `"1week"`, `"24hours"`, or `"never"`. Override:
    /// `DICTATE_HISTORY_RETENTION`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_retention: Option<String>,

    /// Whisper mode: lower the speech-detection threshold so quiet dictation is
    /// captured. `None`/false ⇒ off. Override: `DICTATE_WHISPER_MODE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whisper_mode: Option<bool>,

    /// Double-tap the hotkey to enter hands-free (latched) recording — an
    /// alternative to the Space chord for remotes with no Spacebar (e.g. a
    /// pocket gadget that only emits Right Option). A quick tap released and
    /// re-pressed within the double-tap window latches the mic. `None`/true ⇒
    /// enabled. Override: `DICTATE_DOUBLE_TAP_LATCH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_tap_latch: Option<bool>,

    /// Show the "Landed · N words" result pill after a dictation lands.
    /// `None`/false ⇒ off (the text arriving at the cursor is the feedback).
    /// When on, the pill stays for [`RESULT_PILL_MS`]. Override:
    /// `DICTATE_SHOW_RESULT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_result: Option<bool>,

    /// The completion sound that plays after a successful injection: a stable
    /// id from [`crate::cues::COMPLETION_SOUNDS`] (e.g. `"glass-tick"`).
    /// `None`/unknown ⇒ the default tight thud, so an upgrade with no saved
    /// selection keeps the sound it already had. Override:
    /// `DICTATE_COMPLETION_SOUND`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_sound: Option<String>,

    /// The start sound that plays when a recording begins: a stable id from
    /// [`crate::cues::START_SOUNDS`] (e.g. `"glass-rise"`). `None`/unknown ⇒
    /// the default rising pair, so an upgrade with no saved selection keeps the
    /// sound it already had. Override: `DICTATE_START_SOUND`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_sound: Option<String>,

    /// Put the text on the clipboard when a finished dictation could not be
    /// inserted (no text field, a failed paste, the recording limit), so ⌘V
    /// works at once. A plain clipboard write: clipboard managers keep it.
    /// `None`/true ⇒ on. Override: `DICTATE_COPY_ON_PASTE_FAILURE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_on_paste_failure: Option<bool>,

    /// Global "Copy last dictation" chord as a parseable string
    /// (`"ctrl+cmd+v"`). Absent ⇒ the built-in default
    /// ([`crate::hotkeys::COPY_LAST_DEFAULTS`]); `""` or `"off"` ⇒ no hotkey.
    /// Override: `DICTATE_COPY_LAST_HOTKEY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_last_hotkey: Option<String>,

    /// Stop a recording that has run this many minutes, so a lost key-up or a
    /// forgotten hands-free session cannot record without end. 0 ⇒ no limit.
    /// `None` ⇒ [`DEFAULT_MAX_RECORDING_MINUTES`]. Override:
    /// `DICTATE_MAX_RECORDING_MINUTES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_recording_minutes: Option<u32>,

    /// Window appearance: `"system"`, `"light"`, or `"dark"`. `None` ⇒
    /// [`DEFAULT_APPEARANCE`] (dark — the house design's default). Applied with
    /// `NSApplication.setAppearance`, so every window follows it; nothing is
    /// pinned to Aqua any more. Override: `DICTATE_APPEARANCE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appearance: Option<String>,
}

/// How long the optional "Landed · N words" result pill stays on screen. Long
/// enough to read; the old 420 ms vanished before the eye reached it.
pub const RESULT_PILL_MS: u64 = 2000;

/// Default recording limit. Long enough for any real dictation; short enough
/// that a stuck key is caught the same morning.
pub const DEFAULT_MAX_RECORDING_MINUTES: u32 = 10;

/// Recording-limit choices offered in Settings, as `(minutes, label)`.
pub const MAX_RECORDING_CHOICES: &[(u32, &str)] = &[
    (5, "5 minutes"),
    (10, "10 minutes"),
    (30, "30 minutes"),
    (60, "1 hour"),
    (0, "No limit"),
];

/// Appearance choices offered in the menu and the Settings window, as
/// `(on-disk value, label)`.
pub const APPEARANCE_CHOICES: &[(&str, &str)] = &[
    ("dark", "Dark"),
    ("light", "Light"),
    ("system", "System"),
];

/// The house design is dark-first; light is first-class but not the default.
pub const DEFAULT_APPEARANCE: &str = "dark";

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
        let (settings, migrated) = Self::load_from_migrating(&path);
        if migrated {
            // One-time rewrite so the file carries the new key from now on.
            match settings.save_to(&path) {
                Ok(()) => eprintln!("[settings] migrated gemma_model -> llm_model in {}", path.display()),
                Err(e) => eprintln!("[warn] settings migration not written ({}): {e}", path.display()),
            }
        }
        settings
    }

    /// Load settings from an explicit path. Missing or corrupt files degrade to
    /// defaults; this is the test entry point. Legacy keys are folded into
    /// their current names in memory (the file is not rewritten here).
    pub fn load_from(path: &Path) -> Self {
        Self::load_from_migrating(path).0
    }

    /// `load_from` plus a flag saying whether a legacy key was migrated (so the
    /// caller can decide to rewrite the file).
    fn load_from_migrating(path: &Path) -> (Self, bool) {
        let mut settings: Self = match std::fs::read_to_string(path) {
            Ok(body) => Self::parse_lenient(&body, &path.display().to_string()),
            Err(_) => Self::default(), // missing file is the common case
        };
        let migrated = settings.migrate_legacy_keys();
        (settings, migrated)
    }

    /// Parse a settings document one key at a time. A single bad value (for
    /// example `"preroll_ms": "400"` or `-1`) used to fail the whole document
    /// and reset EVERY setting; now only that key falls back to its default,
    /// with a warning, and the rest survive. A document that is not a JSON
    /// object at all still degrades to defaults (and `save_to` keeps a `.bak`
    /// of it before writing).
    fn parse_lenient(body: &str, origin: &str) -> Self {
        if let Ok(s) = serde_json::from_str::<Self>(body) {
            return s;
        }
        let map = match serde_json::from_str::<serde_json::Value>(body) {
            Ok(serde_json::Value::Object(map)) => map,
            Ok(_) => {
                eprintln!("[warn] settings ignored ({origin}): not a JSON object");
                return Self::default();
            }
            Err(e) => {
                eprintln!("[warn] settings parse failed ({origin}): {e}");
                return Self::default();
            }
        };
        let mut good = serde_json::Map::new();
        for (key, value) in map {
            let mut one = serde_json::Map::new();
            one.insert(key.clone(), value.clone());
            match serde_json::from_value::<Self>(serde_json::Value::Object(one)) {
                Ok(_) => {
                    good.insert(key, value);
                }
                Err(e) => eprintln!(
                    "[warn] settings: ignoring \"{key}\" ({origin}): {e}; using its default"
                ),
            }
        }
        serde_json::from_value(serde_json::Value::Object(good)).unwrap_or_default()
    }

    /// Fold renamed keys into their current fields. Returns true when
    /// something moved. Rule: the new key wins when both are present.
    fn migrate_legacy_keys(&mut self) -> bool {
        let mut migrated = false;
        if let Some(legacy) = self.gemma_model.take() {
            if self.llm_model.is_none() {
                self.llm_model = Some(legacy);
            }
            migrated = true;
        }
        migrated
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
        // A file this build could not fully read (not JSON at all, or an
        // object with a value of the wrong type, which `parse_lenient` loaded
        // as "unset") is about to lose what it held. Keep a copy first.
        if let Ok(existing) = std::fs::read_to_string(path) {
            let fully_read = serde_json::from_str::<Self>(&existing).is_ok();
            if !existing.trim().is_empty() && !fully_read {
                match crate::config_io::backup(path) {
                    Ok(bak) => eprintln!(
                        "[settings] {} was not fully readable; kept a copy at {}",
                        path.display(),
                        bak.display()
                    ),
                    Err(e) => {
                        return Err(eyre::eyre!(
                            "{} is not fully readable and could not be backed up ({e}); not overwriting it",
                            path.display()
                        ))
                    }
                }
            }
        }
        let body = serde_json::to_string_pretty(self)?;
        crate::config_io::write_atomic(path, body.as_bytes())
            .map_err(|e| eyre::eyre!("write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Resolve the cleanup model path: env (`llm_model_env`) > settings
    /// `llm_model` > `default`.
    pub fn resolve_llm_model<'a>(&'a self, default: &'a str) -> String {
        if let Some((_, v)) = llm_model_env() {
            return v;
        }
        self.llm_model
            .clone()
            .unwrap_or_else(|| default.to_string())
    }

    /// Optional instruction model, independently selected from normalization.
    pub fn resolve_instruction_model(&self, default: Option<&str>) -> Option<String> {
        let environment = std::env::var(INSTRUCTION_MODEL_PATH_ENV).ok();
        resolve_instruction_model_from(environment.as_deref(), self.instruction_model.as_deref(), default)
    }

    /// Resolve the fixed Cohere MLX ASR configuration with the usual
    /// **env > settings > default** precedence. The engine is hard-pinned to
    /// Cohere MLX Q4; the only selectable knob is the decoder language.
    /// Env knob: `DICTATE_COHERE_LANG`.
    pub fn resolve_asr(&self) -> crate::transcriber::AsrConfig {
        use crate::transcriber::AsrConfig;

        let cohere_language = std::env::var("DICTATE_COHERE_LANG")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                self.cohere_language
                    .clone()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_else(|| "en".to_string());

        let normalized = cohere_language.trim().to_ascii_lowercase();
        let language = if COHERE_LANGUAGE_CHOICES.iter().any(|(code, _)| *code == normalized) {
            normalized
        } else {
            eprintln!("[warn] unsupported Cohere language; using English (choose one of the supported languages in Settings)");
            "en".to_string()
        };

        AsrConfig {
            helper_path: crate::app_paths::cohere_mlx_helper_path(),
            model_dir: crate::app_paths::cohere_mlx_default_dir(),
            language,
            max_tokens: 256,
        }
    }

    /// Resolve incremental streaming ASR: `DICTATE_STREAMING_ASR` env > settings
    /// > true (on by default). Returns false when always-on pre-roll is active,
    /// > because the two drain the capture ring differently and a later workstream
    /// > reconciles capture. Env accepts `0`/`false`/`off`/`no` (case-insensitive)
    /// > to disable.
    pub fn resolve_streaming_asr(&self) -> bool {
        if self.resolve_preroll_ms() > 0 {
            eprintln!(
                "[boot] streaming asr disabled (pre-roll active — capture reconciliation pending)"
            );
            return false;
        }
        resolve_enabled_flag(self.streaming_asr, std::env::var("DICTATE_STREAMING_ASR").ok().as_deref(), true)
    }

    pub fn live_preview_control(&self) -> LivePreviewControl {
        LivePreviewControl::resolve(
            self.streaming_asr,
            std::env::var("DICTATE_STREAMING_ASR").ok().as_deref(),
            self.resolve_preroll_ms(),
        )
    }

    pub fn resolve_pill_size(&self) -> PillSize {
        PillSize::resolve(
            self.pill_size.as_deref(),
            std::env::var("DICTATE_PILL_SIZE").ok().as_deref(),
        )
    }

    pub fn resolve_waveform_sensitivity(&self) -> WaveformSensitivity {
        WaveformSensitivity::resolve(
            self.waveform_sensitivity.as_deref(),
            std::env::var("DICTATE_WAVEFORM_SENSITIVITY")
                .ok()
                .as_deref(),
        )
    }

    pub fn resolve_show_processing_detail(&self) -> bool {
        resolve_enabled_flag(
            self.show_processing_detail,
            std::env::var("DICTATE_SHOW_PROCESSING_DETAIL")
                .ok()
                .as_deref(),
            true,
        )
    }

    pub fn resolve_audio_history_enabled(&self) -> bool {
        resolve_enabled_flag(
            self.audio_history_enabled,
            std::env::var("DICTATE_AUDIO_HISTORY").ok().as_deref(),
            true,
        )
    }

    pub fn resolve_audio_retention(&self) -> String {
        resolve_audio_retention_value(
            self.audio_retention.as_deref(),
            std::env::var("DICTATE_AUDIO_RETENTION").ok().as_deref(),
        )
        .to_string()
    }

    pub fn resolve_audio_storage_cap_mb(&self) -> u64 {
        resolve_audio_cap(
            self.audio_storage_cap_mb,
            std::env::var("DICTATE_AUDIO_STORAGE_CAP_MB")
                .ok()
                .as_deref(),
        )
    }

    /// Resolve the pre-roll lookback in ms: `DICTATE_PREROLL_MS` env > settings
    /// > 0 (off). 0 means open-on-press (current behaviour); any positive value
    /// > enables the always-on warm stream with that much lookback.
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
        resolve_enabled_flag(self.tail_capture, std::env::var("DICTATE_TAIL_CAPTURE").ok().as_deref(), true)
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

    /// Resolve the warm-window in ms: `DICTATE_WARM_WINDOW_MS` env > settings >
    /// [`crate::audio::DEFAULT_WARM_WINDOW_MS`] (60000). 0 disables warm-window
    /// reuse (each utterance opens the mic fresh). No-op when always-on pre-roll
    /// is active — the always-on stream is already warm for the daemon's life.
    pub fn resolve_warm_window_ms(&self) -> u32 {
        if let Ok(v) = std::env::var("DICTATE_WARM_WINDOW_MS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                return n;
            }
        }
        self.warm_window_ms
            .unwrap_or(crate::audio::DEFAULT_WARM_WINDOW_MS)
    }

    /// Resolve listen mode: `DICTATE_LISTEN_MODE` env > settings > false. Note
    /// the daemon additionally requires `resolve_preroll_ms() > 0` to actually
    /// engage it (listen mode needs the always-on stream).
    pub fn resolve_listen_mode(&self) -> bool {
        resolve_enabled_flag(self.listen_mode, std::env::var("DICTATE_LISTEN_MODE").ok().as_deref(), false)
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

    /// Resolve the hotkey modifier mask: `DICTATE_HOTKEY_MODIFIERS` env >
    /// settings > 0 (bare modifier key). Env value may be decimal or `0x`-hex.
    pub fn resolve_hotkey_modifiers(&self) -> u64 {
        if let Some(mask) = std::env::var("DICTATE_HOTKEY_MODIFIERS")
            .ok()
            .and_then(|s| parse_u64(&s))
        {
            return mask;
        }
        self.hotkey_modifiers.unwrap_or(0)
    }

    /// Resolve the ordered Translate-mode target languages:
    /// `DICTATE_TRANSLATION_TARGETS` env (comma-separated) > settings > empty.
    /// Blank entries are dropped; leading/trailing whitespace is trimmed.
    pub fn resolve_translation_targets(&self) -> Vec<String> {
        if let Ok(v) = std::env::var("DICTATE_TRANSLATION_TARGETS") {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            if !list.is_empty() {
                return list;
            }
        }
        self.translation_targets
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Resolve the active Translate-mode target: `DICTATE_ACTIVE_TRANSLATION_TARGET`
    /// env > settings > the first entry of `targets` (if any). Falls back to the
    /// first target when the configured one isn't in the list.
    pub fn resolve_active_translation_target(&self, targets: &[String]) -> Option<String> {
        let pick = |candidate: Option<String>| -> Option<String> {
            let candidate = candidate.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())?;
            // Return the canonical spelling from `targets` (case-insensitive
            // match) so a lowercased setting resolves to the list's casing.
            targets
                .iter()
                .find(|t| t.eq_ignore_ascii_case(&candidate))
                .cloned()
        };
        if let Ok(v) = std::env::var("DICTATE_ACTIVE_TRANSLATION_TARGET") {
            if let Some(c) = pick(Some(v)) {
                return Some(c);
            }
        }
        if let Some(c) = pick(self.active_translation_target.clone()) {
            return Some(c);
        }
        targets.first().cloned()
    }

    /// Resolve the history retention window: `DICTATE_HISTORY_RETENTION` env >
    /// settings > `"forever"`. Returns the raw lowercased string; the concrete
    /// cutoff is computed by [`crate::history`].
    pub fn resolve_history_retention(&self) -> String {
        if let Ok(v) = std::env::var("DICTATE_HISTORY_RETENTION") {
            let v = v.trim().to_lowercase();
            if !v.is_empty() {
                return v;
            }
        }
        self.history_retention
            .as_ref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "forever".to_string())
    }

    /// Resolve the window appearance: `DICTATE_APPEARANCE` env > settings >
    /// [`DEFAULT_APPEARANCE`]. Anything unrecognised falls back to the default
    /// rather than leaving the app in an undefined appearance.
    pub fn resolve_appearance(&self) -> String {
        let raw = std::env::var("DICTATE_APPEARANCE")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                self.appearance
                    .as_ref()
                    .map(|v| v.trim().to_ascii_lowercase())
                    .filter(|v| !v.is_empty())
            });
        match raw.as_deref() {
            Some("light") => "light".to_string(),
            Some("dark") => "dark".to_string(),
            Some("system") | Some("auto") => "system".to_string(),
            _ => DEFAULT_APPEARANCE.to_string(),
        }
    }

    /// Resolve whisper mode: `DICTATE_WHISPER_MODE` env > settings > false.
    pub fn resolve_whisper_mode(&self) -> bool {
        resolve_enabled_flag(self.whisper_mode, std::env::var("DICTATE_WHISPER_MODE").ok().as_deref(), false)
    }

    /// Resolve the result pill: `DICTATE_SHOW_RESULT` env > settings > off.
    pub fn resolve_show_result(&self) -> bool {
        resolve_enabled_flag(self.show_result, std::env::var("DICTATE_SHOW_RESULT").ok().as_deref(), false)
    }

    /// Resolve the completion sound: `DICTATE_COMPLETION_SOUND` env > settings
    /// > the default tight thud. Blank or unknown ids fall back to the default,
    /// > so a hand-edited file can never leave the app with a missing cue.
    pub fn resolve_completion_sound(&self) -> String {
        let raw = std::env::var(COMPLETION_SOUND_ENV)
            .ok()
            .or_else(|| self.completion_sound.clone())
            .unwrap_or_default();
        crate::cues::normalize_completion_sound(&raw).to_string()
    }

    /// Resolve the start sound: `DICTATE_START_SOUND` env > settings > the
    /// default rising pair. Blank or unknown ids fall back to the default, so a
    /// hand-edited file can never leave the app with a missing cue.
    pub fn resolve_start_sound(&self) -> String {
        let raw = std::env::var(START_SOUND_ENV)
            .ok()
            .or_else(|| self.start_sound.clone())
            .unwrap_or_default();
        crate::cues::normalize_start_sound(&raw).to_string()
    }

    /// Resolve copy-on-failure: `DICTATE_COPY_ON_PASTE_FAILURE` env >
    /// settings > on.
    pub fn resolve_copy_on_paste_failure(&self) -> bool {
        resolve_enabled_flag(self.copy_on_paste_failure, std::env::var("DICTATE_COPY_ON_PASTE_FAILURE").ok().as_deref(), true)
    }

    /// The configured "Copy last dictation" chord string:
    /// `DICTATE_COPY_LAST_HOTKEY` env > settings. `None` means nothing was
    /// configured, so the caller applies the built-in default; an empty string
    /// means the user turned the hotkey off. Parsing and validation against the
    /// dictation hotkey live in [`crate::hotkeys::copy_last_hotkey`].
    pub fn resolve_copy_last_hotkey(&self) -> Option<String> {
        if let Ok(v) = std::env::var("DICTATE_COPY_LAST_HOTKEY") {
            return Some(v.trim().to_string());
        }
        self.copy_last_hotkey.as_ref().map(|s| s.trim().to_string())
    }

    /// Resolve the recording limit in minutes: `DICTATE_MAX_RECORDING_MINUTES`
    /// env > settings > [`DEFAULT_MAX_RECORDING_MINUTES`]. 0 = no limit.
    pub fn resolve_max_recording_minutes(&self) -> u32 {
        if let Ok(v) = std::env::var("DICTATE_MAX_RECORDING_MINUTES") {
            if let Ok(n) = v.trim().parse::<u32>() {
                return n;
            }
        }
        self.max_recording_minutes
            .unwrap_or(DEFAULT_MAX_RECORDING_MINUTES)
    }

    /// Resolve double-tap-to-latch (hands-free from a hotkey double-tap):
    /// `DICTATE_DOUBLE_TAP_LATCH` env > settings > enabled.
    pub fn resolve_double_tap_latch(&self) -> bool {
        resolve_enabled_flag(self.double_tap_latch, std::env::var("DICTATE_DOUBLE_TAP_LATCH").ok().as_deref(), true)
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

/// Parse an unsigned integer string that may be decimal (`1048576`) or hex
/// (`0x00100000`). Used for the hotkey modifier mask.
pub fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(rest, 16).ok()
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
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("s1-mini") {
        return Some("English normalization".to_string());
    }
    if normalized.contains("tiny-aya") {
        return Some("NonCommercial licence · personal use only".to_string());
    }
    parse_param_billions(name).map(|size| format!("{size}B parameters"))
}

/// Enumerate cleanup models under `<models base>/llm/<name>/*.gguf`, where the
/// base is resolved bundle-aware (see [`crate::app_paths`]). Each subdirectory
/// contributes its first `.gguf`. Returns paths canonicalized to absolute so a
/// selection survives a daemon relaunch regardless of CWD.
pub fn discover_llm_models() -> Vec<ModelChoice> {
    let mut choices = discover_llm_models_in(&crate::app_paths::llm_models_dir());
    if let Some(path) = crate::app_paths::instruction_default_path() {
        if !choices.iter().any(|choice| choice.path == path) {
            let label = Path::new(&path).file_stem().and_then(|name| name.to_str())
                .unwrap_or("Instruction model").to_string();
            choices.push(ModelChoice { hint: model_hint(&label), label, path });
        }
    }
    choices.sort_by(|a, b| a.label.cmp(&b.label));
    choices
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
    fn instruction_model_precedence_and_explicit_disable() {
        assert_eq!(resolve_instruction_model_from(None, None, Some("default.gguf")).as_deref(), Some("default.gguf"));
        assert_eq!(resolve_instruction_model_from(None, Some("saved.gguf"), Some("default.gguf")).as_deref(), Some("saved.gguf"));
        assert_eq!(resolve_instruction_model_from(Some(" env.gguf "), Some("saved.gguf"), None).as_deref(), Some("env.gguf"));
        assert_eq!(resolve_instruction_model_from(Some(""), Some("saved.gguf"), Some("default.gguf")), None);
        assert_eq!(resolve_instruction_model_from(None, Some("  "), Some("default.gguf")), None);
        assert_eq!(resolve_instruction_model_from(None, None, None), None);
    }

    #[test]
    fn instruction_model_survives_settings_round_trip() {
        let settings: Settings = serde_json::from_str(r#"{"instruction_model":"local.gguf","llm_model":"normalize.gguf"}"#).unwrap();
        assert_eq!(settings.instruction_model.as_deref(), Some("local.gguf"));
        assert_eq!(serde_json::from_str::<Settings>(&serde_json::to_string(&settings).unwrap()).unwrap(), settings);
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap().instruction_model, None);
    }

    #[test]
    fn language_menu_matches_the_checkpoint_supported_set() {
        let mut codes: Vec<_> = COHERE_LANGUAGE_CHOICES.iter().map(|(code, _)| *code).collect();
        codes.sort_unstable();
        assert_eq!(codes, vec!["ar", "de", "el", "en", "es", "fr", "it", "ja", "ko", "nl", "pl", "pt", "vi", "zh"]);
    }

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
    fn model_hints_describe_capability_without_claiming_untested_accuracy() {
        assert_eq!(
            model_hint("smollm2-360m-it").as_deref(),
            Some("0.36B parameters")
        );
        assert_eq!(
            model_hint("gemma-3-1b-it").as_deref(),
            Some("1B parameters")
        );
        assert_eq!(
            model_hint("gemma-3-4b-it").as_deref(),
            Some("4B parameters")
        );
        assert_eq!(model_hint("mystery-model").as_deref(), None);
        assert_eq!(model_hint("s1-mini").as_deref(), Some("English normalization"));
        assert_eq!(model_hint("tiny-aya-global").as_deref(), Some("NonCommercial licence · personal use only"));
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
    fn parse_u64_decimal_and_hex() {
        assert_eq!(parse_u64("0"), Some(0));
        assert_eq!(parse_u64("1048576"), Some(1 << 20));
        assert_eq!(parse_u64("0x00100000"), Some(1 << 20));
        assert_eq!(parse_u64("0X20000"), Some(1 << 17));
        assert_eq!(parse_u64("junk"), None);
    }

    #[test]
    fn hotkey_modifiers_default_zero_and_from_settings() {
        if std::env::var_os("DICTATE_HOTKEY_MODIFIERS").is_none() {
            assert_eq!(Settings::default().resolve_hotkey_modifiers(), 0);
            let mut s = Settings::default();
            s.hotkey_modifiers = Some(1 << 20); // Command
            assert_eq!(s.resolve_hotkey_modifiers(), 1 << 20);
        }
    }

    #[test]
    fn translation_targets_parse_trim_and_drop_blanks() {
        if std::env::var_os("DICTATE_TRANSLATION_TARGETS").is_none() {
            assert!(Settings::default().resolve_translation_targets().is_empty());
            let mut s = Settings::default();
            s.translation_targets = Some(vec![
                " Spanish ".into(),
                "".into(),
                "Chinese".into(),
            ]);
            assert_eq!(s.resolve_translation_targets(), vec!["Spanish", "Chinese"]);
        }
    }

    #[test]
    fn active_translation_target_falls_back_to_first() {
        if std::env::var_os("DICTATE_ACTIVE_TRANSLATION_TARGET").is_none() {
            let targets = vec!["Spanish".to_string(), "Chinese".to_string()];
            assert_eq!(
                Settings::default().resolve_active_translation_target(&targets),
                Some("Spanish".to_string())
            );
            let mut s = Settings::default();
            s.active_translation_target = Some("chinese".into());
            assert_eq!(
                s.resolve_active_translation_target(&targets),
                Some("Chinese".to_string())
            );
            // A configured target not in the list falls back to the first.
            let mut s2 = Settings::default();
            s2.active_translation_target = Some("Klingon".into());
            assert_eq!(
                s2.resolve_active_translation_target(&targets),
                Some("Spanish".to_string())
            );
            // No targets at all ⇒ None.
            assert_eq!(
                Settings::default().resolve_active_translation_target(&[]),
                None
            );
        }
    }

    #[test]
    fn history_retention_defaults_forever_and_normalizes() {
        if std::env::var_os("DICTATE_HISTORY_RETENTION").is_none() {
            assert_eq!(Settings::default().resolve_history_retention(), "forever");
            let mut s = Settings::default();
            s.history_retention = Some(" 1Month ".into());
            assert_eq!(s.resolve_history_retention(), "1month");
        }
    }

    #[test]
    fn whisper_mode_off_by_default() {
        if std::env::var_os("DICTATE_WHISPER_MODE").is_none() {
            assert!(!Settings::default().resolve_whisper_mode());
            let mut s = Settings::default();
            s.whisper_mode = Some(true);
            assert!(s.resolve_whisper_mode());
        }
    }

    #[test]
    fn copy_on_paste_failure_is_on_by_default() {
        if std::env::var_os("DICTATE_COPY_ON_PASTE_FAILURE").is_none() {
            assert!(Settings::default().resolve_copy_on_paste_failure());
            let mut s = Settings::default();
            s.copy_on_paste_failure = Some(false);
            assert!(!s.resolve_copy_on_paste_failure());
        }
    }

    #[test]
    fn copy_last_hotkey_distinguishes_unset_from_off() {
        if std::env::var_os("DICTATE_COPY_LAST_HOTKEY").is_none() {
            assert_eq!(Settings::default().resolve_copy_last_hotkey(), None);
            let mut s = Settings::default();
            s.copy_last_hotkey = Some("".into());
            assert_eq!(s.resolve_copy_last_hotkey().as_deref(), Some(""));
            s.copy_last_hotkey = Some(" ctrl+cmd+v ".into());
            assert_eq!(s.resolve_copy_last_hotkey().as_deref(), Some("ctrl+cmd+v"));
        }
    }

    #[test]
    fn max_recording_defaults_to_ten_minutes_and_zero_means_no_limit() {
        if std::env::var_os("DICTATE_MAX_RECORDING_MINUTES").is_none() {
            assert_eq!(Settings::default().resolve_max_recording_minutes(), 10);
            let mut s = Settings::default();
            s.max_recording_minutes = Some(0);
            assert_eq!(s.resolve_max_recording_minutes(), 0);
            assert!(MAX_RECORDING_CHOICES.iter().any(|(m, _)| *m == DEFAULT_MAX_RECORDING_MINUTES));
        }
    }

    #[test]
    fn an_old_settings_file_without_the_new_keys_still_loads() {
        let s = Settings::parse_lenient(r#"{"hotkey_keycode": 61, "show_result": true}"#, "test");
        assert_eq!(s.hotkey_keycode, Some(61));
        assert_eq!(s.copy_last_hotkey, None);
        assert_eq!(s.copy_on_paste_failure, None);
        assert_eq!(s.max_recording_minutes, None);
        assert_eq!(s.completion_sound, None);
        assert_eq!(s.start_sound, None);
        // An old file with no completion_sound key still resolves the default.
        assert_eq!(
            s.resolve_completion_sound(),
            crate::cues::DEFAULT_COMPLETION_SOUND
        );
        // The same for the start sound key.
        assert_eq!(
            s.resolve_start_sound(),
            crate::cues::DEFAULT_START_SOUND
        );
        // A bad value in a new key costs only that key.
        let s = Settings::parse_lenient(r#"{"max_recording_minutes": "ten", "show_result": true}"#, "test");
        assert_eq!(s.max_recording_minutes, None);
        assert_eq!(s.show_result, Some(true));
    }

    #[test]
    fn completion_sound_defaults_and_unknown_falls_back() {
        if std::env::var_os(COMPLETION_SOUND_ENV).is_none() {
            // Unset ⇒ the currently installed tight thud, not an audition cue.
            assert_eq!(
                Settings::default().resolve_completion_sound(),
                crate::cues::DEFAULT_COMPLETION_SOUND
            );
            // A saved audition cue is honored.
            let mut s = Settings::default();
            s.completion_sound = Some("glass-tick".into());
            assert_eq!(s.resolve_completion_sound(), "glass-tick");
            // Unknown, blank and whitespace all fall back rather than go silent.
            s.completion_sound = Some("not-a-real-cue".into());
            assert_eq!(
                s.resolve_completion_sound(),
                crate::cues::DEFAULT_COMPLETION_SOUND
            );
            s.completion_sound = Some("   ".into());
            assert_eq!(
                s.resolve_completion_sound(),
                crate::cues::DEFAULT_COMPLETION_SOUND
            );
            // A padded but valid id is trimmed to the canonical spelling.
            s.completion_sound = Some("  glass-tick  ".into());
            assert_eq!(s.resolve_completion_sound(), "glass-tick");
        }
    }

    #[test]
    fn start_sound_defaults_and_unknown_falls_back() {
        if std::env::var_os(START_SOUND_ENV).is_none() {
            // Unset ⇒ the currently installed rising pair, not an audition cue.
            assert_eq!(
                Settings::default().resolve_start_sound(),
                crate::cues::DEFAULT_START_SOUND
            );
            // A saved audition cue is honored.
            let mut s = Settings::default();
            s.start_sound = Some("glass-rise".into());
            assert_eq!(s.resolve_start_sound(), "glass-rise");
            // Unknown, blank and whitespace all fall back rather than go silent.
            s.start_sound = Some("not-a-real-cue".into());
            assert_eq!(
                s.resolve_start_sound(),
                crate::cues::DEFAULT_START_SOUND
            );
            s.start_sound = Some("   ".into());
            assert_eq!(
                s.resolve_start_sound(),
                crate::cues::DEFAULT_START_SOUND
            );
            // A completion id is not a start id: it falls back too.
            s.start_sound = Some("glass-tick".into());
            assert_eq!(
                s.resolve_start_sound(),
                crate::cues::DEFAULT_START_SOUND
            );
            // A padded but valid id is trimmed to the canonical spelling.
            s.start_sound = Some("  glass-rise  ".into());
            assert_eq!(s.resolve_start_sound(), "glass-rise");
        }
    }

    #[test]
    fn completion_sound_round_trips_through_the_settings_file() {
        let dir = std::env::temp_dir().join(format!("fd-settings-sound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");

        let mut s = Settings::default();
        s.completion_sound = Some("glass-tick".into());
        s.start_sound = Some("glass-rise".into());
        s.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.completion_sound.as_deref(), Some("glass-tick"));
        assert_eq!(loaded.start_sound.as_deref(), Some("glass-rise"));
        if std::env::var_os(COMPLETION_SOUND_ENV).is_none() {
            assert_eq!(loaded.resolve_completion_sound(), "glass-tick");
        }
        if std::env::var_os(START_SOUND_ENV).is_none() {
            assert_eq!(loaded.resolve_start_sound(), "glass-rise");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn double_tap_latch_on_by_default() {
        if std::env::var_os("DICTATE_DOUBLE_TAP_LATCH").is_none() {
            assert!(Settings::default().resolve_double_tap_latch());
            let mut s = Settings::default();
            s.double_tap_latch = Some(false);
            assert!(!s.resolve_double_tap_latch());
        }
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
    fn llm_model_falls_back_to_default_when_unset() {
        if llm_model_env().is_none() {
            let s = Settings::default();
            assert_eq!(s.resolve_llm_model("default.gguf"), "default.gguf");
            let mut s2 = Settings::default();
            s2.llm_model = Some("chosen.gguf".to_string());
            assert_eq!(s2.resolve_llm_model("default.gguf"), "chosen.gguf");
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
    fn one_bad_value_does_not_reset_the_other_settings() {
        // Regression: any single type error failed the whole document and
        // every setting silently reverted to its default.
        let dir = std::env::temp_dir().join(format!("fd-settings-lenient-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "hotkey_keycode": 54,
                "preroll_ms": "400",
                "warm_window_ms": -1,
                "tail_capture_cap_ms": 400.5,
                "cleanup_enabled": "yes",
                "appearance": "light",
                "cohere_language": "de",
                "translation_targets": ["Spanish", 7],
                "history_retention": "1week",
                "some_future_key": {"nested": true}
            }"#,
        )
        .unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.hotkey_keycode, Some(54));
        assert_eq!(s.appearance.as_deref(), Some("light"));
        assert_eq!(s.cohere_language.as_deref(), Some("de"));
        assert_eq!(s.history_retention.as_deref(), Some("1week"));
        // The bad ones fall back to "unset" (their built-in default).
        assert_eq!(s.preroll_ms, None);
        assert_eq!(s.warm_window_ms, None);
        assert_eq!(s.tail_capture_cap_ms, None);
        assert_eq!(s.cleanup_enabled, None);
        assert_eq!(s.translation_targets, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_with_one_bad_value_is_backed_up_before_it_is_rewritten() {
        // The bad value loads as "unset", so the next save drops it. The user
        // typed it; keep the original beside the rewritten file.
        let dir = std::env::temp_dir().join(format!("fd-settings-badkey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let original = r#"{ "hotkey_keycode": 54, "preroll_ms": "400" }"#;
        std::fs::write(&path, original).unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.hotkey_keycode, Some(54));
        s.save_to(&path).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("settings.json.bak")).unwrap(), original);
        assert_eq!(Settings::load_from(&path).hotkey_keycode, Some(54));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lenient_parse_still_migrates_the_legacy_model_key() {
        let s = {
            let mut s = Settings::parse_lenient(
                r#"{ "gemma_model": "/tmp/legacy.gguf", "preroll_ms": "bad" }"#,
                "test",
            );
            s.migrate_legacy_keys();
            s
        };
        assert_eq!(s.llm_model.as_deref(), Some("/tmp/legacy.gguf"));
        assert_eq!(s.preroll_ms, None);
    }

    #[test]
    fn unparseable_settings_are_backed_up_before_the_first_save() {
        let dir = std::env::temp_dir().join(format!("fd-settings-bak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let broken = "{ \"hotkey_keycode\": 54,, }";
        std::fs::write(&path, broken).unwrap();
        // Unparseable → defaults, as before.
        let mut s = Settings::load_from(&path);
        assert_eq!(s, Settings::default());
        // The menu's load → mutate → save must not destroy the user's file.
        s.cleanup_enabled = Some(false);
        s.save_to(&path).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("settings.json.bak")).unwrap(), broken);
        assert_eq!(Settings::load_from(&path).cleanup_enabled, Some(false));
        // A healthy file is not backed up again.
        s.save_to(&path).unwrap();
        let baks = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".bak"))
            .count();
        assert_eq!(baks, 1);
        let _ = std::fs::remove_dir_all(&dir);
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
        s.cohere_language = Some("de".into());
        s.completion_sound = Some("glass-tick".into());
        s.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.hotkey_keycode, Some(0x36));
        assert_eq!(loaded.cleanup_enabled, Some(false));
        assert_eq!(loaded.active_format, Some("bullets".into()));
        assert_eq!(loaded.preroll_ms, Some(400));
        assert_eq!(loaded.cohere_language, Some("de".into()));
        assert_eq!(loaded.completion_sound.as_deref(), Some("glass-tick"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_settings_keys_still_deserialize() {
        // An old settings file carries the removed multi-backend keys. serde
        // must drop them silently (the struct has no deny_unknown_fields)
        // while keeping the surviving live field(s).
        let dir = std::env::temp_dir().join(format!("fd-settings-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{
                "gemma_model": "/tmp/legacy.gguf",
                "cohere_language": "ja",
                "asr_backend": "cohere",
                "cohere_quant": "int4",
                "moonshine_variant": "tiny"
            }"#,
        )
        .unwrap();

        let mut expected = Settings::default();
        // `gemma_model` is folded into `llm_model` on load.
        expected.llm_model = Some("/tmp/legacy.gguf".into());
        expected.cohere_language = Some("ja".into());
        // Legacy-only keys are dropped (not preserved as noise); live fields
        // survive. Equality against `expected` proves this.
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, expected);
        assert!(loaded.gemma_model.is_none());
        // A save writes the new key only.
        loaded.save_to(&path).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"llm_model\""), "{body}");
        assert!(!body.contains("gemma_model"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn llm_model_key_wins_over_legacy_gemma_model() {
        let dir = std::env::temp_dir().join(format!("fd-settings-llmwins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{ "llm_model": "/tmp/new.gguf", "gemma_model": "/tmp/old.gguf" }"#,
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.llm_model.as_deref(), Some("/tmp/new.gguf"));
        assert!(loaded.gemma_model.is_none());
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
    fn live_preview_ui_preserves_saved_choice_while_preroll_blocks_it() {
        let blocked = LivePreviewControl::resolve(Some(true), None, 400);
        assert!(!blocked.enabled && !blocked.editable);
        assert!(blocked.hint.contains("Always-on mic"));
        let restored = LivePreviewControl::resolve(Some(true), None, 0);
        assert!(restored.enabled && restored.editable);
        assert!(!LivePreviewControl::resolve(Some(false), None, 0).enabled);
        let overridden = LivePreviewControl::resolve(Some(false), Some("true"), 0);
        assert!(overridden.enabled && !overridden.editable);
        assert!(overridden.hint.contains("DICTATE_STREAMING_ASR"));
    }

    #[test]
    fn live_preview_control_matches_runtime_without_clearing_preferences() {
        for preference in [None, Some(false), Some(true)] {
            for preroll in [0, 400] {
                let mut settings = Settings::default();
                settings.streaming_asr = preference;
                settings.preroll_ms = Some(preroll);
                assert_eq!(
                    settings.live_preview_control().enabled,
                    settings.resolve_streaming_asr()
                );
                assert_eq!(settings.streaming_asr, preference);
                assert_eq!(settings.preroll_ms, Some(preroll));
            }
        }
    }

    #[test]
    fn feedback_defaults_and_environment_precedence_are_explicit() {
        assert_eq!(PillSize::resolve(None, None), PillSize::Compact);
        assert_eq!(
            PillSize::resolve(Some("standard"), None),
            PillSize::Standard
        );
        assert_eq!(
            PillSize::resolve(Some("standard"), Some("compact")),
            PillSize::Compact
        );
        assert_eq!(PillSize::resolve(Some("unknown"), None), PillSize::Compact);
        assert_eq!(
            WaveformSensitivity::resolve(None, None),
            WaveformSensitivity::Balanced
        );
        assert_eq!(
            WaveformSensitivity::resolve(Some("high"), None),
            WaveformSensitivity::High
        );
        assert_eq!(
            WaveformSensitivity::resolve(Some("high"), Some("low")),
            WaveformSensitivity::Low
        );
        assert_eq!(
            WaveformSensitivity::resolve(Some("unknown"), None),
            WaveformSensitivity::Balanced
        );
        assert!(resolve_enabled_flag(None, None, true));
        assert!(!resolve_enabled_flag(Some(true), Some("off"), true));
        assert!(resolve_enabled_flag(Some(false), Some("1"), true));
    }

    #[test]
    fn a_bad_flag_value_falls_through_like_a_bad_number() {
        // DICTATE_TAIL_CAPTURE=garbage used to mean on and
        // DICTATE_WHISPER_MODE=garbage used to mean off. Now neither is a value.
        for garbage in ["garbage", "", "  ", "2", "yes please"] {
            assert!(!resolve_enabled_flag(Some(false), Some(garbage), true), "{garbage:?}");
            assert!(resolve_enabled_flag(None, Some(garbage), true), "{garbage:?}");
            assert!(!resolve_enabled_flag(None, Some(garbage), false), "{garbage:?}");
        }
        for (value, expected) in [(" ON ", true), ("No", false), ("TRUE", true), ("0", false)] {
            assert_eq!(resolve_enabled_flag(Some(!expected), Some(value), !expected), expected, "{value:?}");
        }
    }

    #[test]
    fn recording_feedback_choices_survive_save_and_reload() {
        let dir = std::env::temp_dir().join(format!("fd-feedback-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        let mut saved = Settings::default();
        saved.streaming_asr = Some(false);
        saved.pill_size = Some("standard".into());
        saved.waveform_sensitivity = Some("high".into());
        saved.show_processing_detail = Some(false);
        saved.audio_history_enabled = Some(false);
        saved.audio_retention = Some("1week".into());
        saved.audio_storage_cap_mb = Some(4096);
        saved.show_result = Some(true);
        saved.cleanup_enabled = Some(false);
        saved.preroll_ms = Some(400);
        saved.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), saved);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn audio_recovery_defaults_to_thirty_days_without_shortening_text_history() {
        assert_eq!(resolve_audio_retention_value(None, None), "1month");
        assert_eq!(
            resolve_audio_retention_value(Some("forever"), Some("1 week")),
            "1week"
        );
        assert_eq!(
            resolve_audio_retention_value(Some("unknown"), None),
            "forever"
        );
        assert_eq!(resolve_audio_cap(None, None), 0);
        assert_eq!(resolve_audio_cap(Some(4096), Some("2048")), 2048);
        assert_eq!(resolve_audio_cap(Some(4096), Some("0")), 0);
        let old: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(old.audio_history_enabled, None);
        assert_eq!(old.history_retention, None);
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
    fn warm_window_defaults_and_can_be_zeroed() {
        if std::env::var_os("DICTATE_WARM_WINDOW_MS").is_none() {
            assert_eq!(Settings::default().resolve_warm_window_ms(), 60_000);
            let mut s = Settings::default();
            s.warm_window_ms = Some(0);
            assert_eq!(s.resolve_warm_window_ms(), 0);
            let mut s2 = Settings::default();
            s2.warm_window_ms = Some(2500);
            assert_eq!(s2.resolve_warm_window_ms(), 2500);
        }
    }

    #[test]
    fn resolve_cohere_language_defaults_and_from_settings() {
        if std::env::var_os("DICTATE_COHERE_LANG").is_none() {
            let s = Settings::default();
            assert_eq!(s.resolve_asr().language, "en");

            let mut s2 = Settings::default();
            s2.cohere_language = Some("de".into());
            assert_eq!(s2.resolve_asr().language, "de");
        }
    }
}
