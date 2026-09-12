//! Resolves where the AI models live, so the *same* binary works in every
//! launch context without recompiling:
//!
//!   - `cargo run` / CLI from the repo → models under `./models`.
//!   - dev `.app` bundle → models symlinked into Application Support.
//!   - shipped `.app` bundle → models copied into `Contents/Resources/models`.
//!
//! Resolution order for the models base directory (the folder that holds the
//! `dictation/` and `llm/` trees):
//!
//!   1. `DICTATE_MODELS_DIR` env var (explicit override, wins everything).
//!   2. `<bundle>/Contents/Resources/models` — a shipped, self-contained app.
//!   3. `~/Library/Application Support/Local Dictation/models` — the dev app
//!      points here (usually a symlink back to the repo's `models/`).
//!   4. `./models` — repo-relative default for `cargo run`.

use std::path::PathBuf;

/// `Contents/Resources` when this executable lives inside a macOS `.app`
/// (`…/Local Dictation.app/Contents/MacOS/<bin>`), else `None`.
pub fn bundle_resources_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let macos = exe.parent()?; // …/Contents/MacOS
    let contents = macos.parent()?; // …/Contents
    if macos.file_name()?.to_str()? != "MacOS" || contents.file_name()?.to_str()? != "Contents" {
        return None;
    }
    Some(contents.join("Resources"))
}

/// True when running from inside a `.app` bundle. The binary uses this to
/// default to the `daemon` subcommand (a double-clicked app passes no args).
pub fn running_in_bundle() -> bool {
    bundle_resources_dir().is_some()
}

/// The daemon's structured log. A bundled app redirects stdout/stderr here
/// (no terminal to print to); the menu bar, the settings window and the
/// `logs` subcommand all open this same file.
pub const DAEMON_LOG_PATH: &str = "/tmp/dictate-daemon.log";

/// `$HOME` as a path (None when unset). Every per-user location below is
/// derived from this one accessor rather than re-reading the env var.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `~/Library/Application Support/Local Dictation` (None if `$HOME` is unset).
pub fn app_support_dir() -> Option<PathBuf> {
    Some(home_dir()?.join("Library/Application Support/Local Dictation"))
}

/// `~/Library/Application Support/House/commands` — the shared directory where
/// every house app publishes its command manifest, so one app can list what the
/// others do without knowing any of them by name. The contract lives in
/// `design-system/docs/app-commands.md`. `None` if `$HOME` is unset.
pub fn house_commands_dir() -> Option<PathBuf> {
    Some(home_dir()?.join("Library/Application Support/House/commands"))
}

/// `~/Library/Caches/Local Dictation/<sub>` (None if `$HOME` is unset).
pub fn cache_dir(sub: &str) -> Option<PathBuf> {
    Some(home_dir()?.join("Library/Caches/Local Dictation").join(sub))
}

/// `~/.config/local-dictation` — the single user-config directory shared by
/// `settings.json`, `prompts.json`, `corrections.json` and `history.db`.
/// `None` only when `$HOME` is unset. This is the one place the config
/// location is defined; every module resolves through it rather than
/// re-joining the path itself (see CLAUDE.md "Config & precedence").
pub fn config_dir() -> Option<PathBuf> {
    Some(home_dir()?.join(".config").join("local-dictation"))
}

/// A named file inside [`config_dir`], e.g. `config_file("settings.json")`.
/// `None` only when `$HOME` is unset.
pub fn config_file(name: &str) -> Option<PathBuf> {
    Some(config_dir()?.join(name))
}

/// Base directory containing the `dictation/` and `llm/` model trees.
/// See the module docs for the resolution order.
pub fn models_base_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("DICTATE_MODELS_DIR") {
        return PathBuf::from(v);
    }
    if let Some(res) = bundle_resources_dir() {
        let m = res.join("models");
        if m.exists() {
            return m;
        }
    }
    if let Some(app) = app_support_dir() {
        let m = app.join("models");
        if m.exists() {
            return m;
        }
    }
    PathBuf::from("models")
}

/// Fixed Cohere Q4 checkpoint converted for the public MLX Swift runtime.
/// Canonicalized: the model dir may be a symlink into ~/Models (the canonical
/// model home), and the native MLX bridge fails to load a symlinked model dir
/// ("Not a directory"), so hand it the resolved real path.
pub fn cohere_mlx_default_dir() -> String {
    std::env::var("DICTATE_COHERE_MLX_MODEL_DIR").unwrap_or_else(|_| {
        let dir = models_base_dir().join("dictation/cohere-mlx-q4");
        std::fs::canonicalize(&dir)
            .unwrap_or(dir)
            .to_string_lossy()
            .into_owned()
    })
}

/// Persistent native helper executable. A packaged app carries it in
/// Resources; development builds resolve the Xcode SwiftPM product.
pub fn cohere_mlx_helper_path() -> String {
    if let Ok(path) = std::env::var("DICTATE_COHERE_MLX_BRIDGE") {
        return path;
    }
    if let Some(resources) = bundle_resources_dir() {
        return resources
            .join("cohere-mlx-bridge")
            .to_string_lossy()
            .into_owned();
    }
    PathBuf::from("native/cohere-mlx/.build/xcode/Build/Products/Release/cohere-mlx-bridge")
        .to_string_lossy()
        .into_owned()
}

/// Default cleanup-LLM path (S1-mini GGUF). Overrides are resolved in
/// `settings::Settings::resolve_llm_model`.
pub fn llm_default_path() -> String {
    models_base_dir()
        .join("llm/s1-mini/s1-mini.gguf")
        .to_string_lossy()
        .into_owned()
}

/// Directory the menu's model picker scans for cleanup models (`<base>/llm`).
pub fn llm_models_dir() -> PathBuf {
    models_base_dir().join("llm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_lives_under_config_dir() {
        // Only assert the shape when $HOME is set (it is in CI / dev shells).
        if let Some(dir) = config_dir() {
            assert!(dir.ends_with("local-dictation"));
            let f = config_file("settings.json").expect("HOME set ⇒ Some");
            assert_eq!(f.parent(), Some(dir.as_path()));
            assert_eq!(
                f.file_name().and_then(|s| s.to_str()),
                Some("settings.json")
            );
        }
    }
}
