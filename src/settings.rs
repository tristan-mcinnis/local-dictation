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
//!     "gemma_model": "/abs/path/to/models/llm/gemma-3-1b-it/gemma-3-1b-it-Q4_K_M.gguf",
//!     "hotkey_keycode": 61,
//!     "cleanup_enabled": true
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Built-in default cleanup model, relative to the project tree. Shared by
/// the CLI resolver and the menu so "default" means the same thing in both.
pub const DEFAULT_GEMMA_REL: &str = "models/llm/gemma-3-1b-it/gemma-3-1b-it-Q4_K_M.gguf";

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
}

impl Settings {
    /// `~/.config/local-dictation/settings.json` (None if `$HOME` is unset).
    pub fn config_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join(".config")
                .join("local-dictation")
                .join("settings.json"),
        )
    }

    /// Load settings, returning an empty (all-`None`) struct when the file is
    /// missing or unparseable. Never errors — a corrupt settings file should
    /// degrade to defaults, not stop the daemon from booting.
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
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
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| eyre::eyre!("create {}: {e}", dir.display()))?;
        }
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, body).map_err(|e| eyre::eyre!("write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Resolve the cleanup model path: `GEMMA_MODEL_PATH` env > settings >
    /// `default`.
    pub fn resolve_gemma<'a>(&'a self, default: &'a str) -> String {
        if let Ok(v) = std::env::var("GEMMA_MODEL_PATH") {
            return v;
        }
        self.gemma_model.clone().unwrap_or_else(|| default.to_string())
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
}

/// Enumerate cleanup models under `models/llm/<name>/*.gguf` (relative to the
/// current working directory, matching how the default paths resolve). Each
/// subdirectory contributes its first `.gguf`. Returns paths canonicalized to
/// absolute so a selection survives a daemon relaunch regardless of CWD.
pub fn discover_llm_models() -> Vec<ModelChoice> {
    discover_llm_models_in(Path::new("models/llm"))
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
}
