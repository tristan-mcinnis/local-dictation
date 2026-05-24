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
    if macos.file_name()?.to_str()? != "MacOS"
        || contents.file_name()?.to_str()? != "Contents"
    {
        return None;
    }
    Some(contents.join("Resources"))
}

/// True when running from inside a `.app` bundle. The binary uses this to
/// default to the `daemon` subcommand (a double-clicked app passes no args).
pub fn running_in_bundle() -> bool {
    bundle_resources_dir().is_some()
}

/// `~/Library/Application Support/Local Dictation` (None if `$HOME` is unset).
pub fn app_support_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Local Dictation"))
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

/// Default Parakeet ASR model directory (`<base>/dictation/parakeet-tdt-v3-int8`).
pub fn parakeet_default_dir() -> String {
    models_base_dir()
        .join("dictation/parakeet-tdt-v3-int8")
        .to_string_lossy()
        .into_owned()
}

/// Default Gemma cleanup model path
/// (`<base>/llm/gemma-3-1b-it/gemma-3-1b-it-Q4_K_M.gguf`).
pub fn gemma_default_path() -> String {
    models_base_dir()
        .join("llm/gemma-3-1b-it/gemma-3-1b-it-Q4_K_M.gguf")
        .to_string_lossy()
        .into_owned()
}

/// Directory the menu's model picker scans for cleanup models (`<base>/llm`).
pub fn llm_models_dir() -> PathBuf {
    models_base_dir().join("llm")
}
