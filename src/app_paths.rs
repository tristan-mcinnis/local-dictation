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

/// `<bundle>/Contents/Info.plist`, when running from a `.app`.
pub fn bundle_info_plist() -> Option<PathBuf> {
    Some(bundle_resources_dir()?.parent()?.join("Info.plist"))
}

/// The git commit this bundle was built from, as `scripts/build-app.sh` writes
/// it into `Info.plist` under `LocalDictationBuiltFromCommit`.
///
/// Read from the plist rather than baked in with a `build.rs`, so a `cargo
/// build` needs no git and the value can never be stale relative to the bundle
/// it is shown in. `None` for a non-bundled run.
pub fn build_commit() -> Option<String> {
    let text = std::fs::read_to_string(bundle_info_plist()?).ok()?;
    plist_string_value(&text, "LocalDictationBuiltFromCommit")
}

/// The value of a `<string>` entry in an XML property list, by key.
///
/// A hand-rolled scan rather than a plist crate or a `plutil` child process:
/// one key, one format, and `build-app.sh` writes the file, so the shape is
/// known. Returns `None` rather than guessing when the shape is not the
/// expected `<key>…</key><string>…</string>` pair.
pub fn plist_string_value(xml: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let after = &xml[xml.find(&needle)? + needle.len()..];
    let open = after.find("<string>")?;
    // Anything other than whitespace between the key and its value means the
    // key's own value is not a string (the next key started first).
    if !after[..open].trim().is_empty() {
        return None;
    }
    let value = &after[open + "<string>".len()..];
    let end = value.find("</string>")?;
    Some(value[..end].trim().to_string())
}

/// `$HOME` as a path (None when unset). Every per-user location below is
/// derived from this one accessor rather than re-reading the env var.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `~/Library/Logs/Local Dictation` — the per-user log directory (created
/// `0700` by `daemon_log::open`). `None` if `$HOME` is unset.
pub fn logs_dir() -> Option<PathBuf> {
    Some(home_dir()?.join("Library/Logs/Local Dictation"))
}

/// The daemon's structured log, `~/Library/Logs/Local Dictation/daemon.log`.
/// A bundled app redirects stdout/stderr here (no terminal to print to); the
/// menu bar, the settings window and the `logs` subcommand all open this same
/// file. Opening, permissions and rotation live in `daemon_log.rs`. It used
/// to be the shared, world-readable `/tmp/dictate-daemon.log`.
pub fn daemon_log_path() -> Option<PathBuf> {
    Some(logs_dir()?.join("daemon.log"))
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

/// Private original-audio archive beside the history database.
pub fn recovery_audio_dir() -> Option<PathBuf> {
    Some(recovery_audio_dir_at(&config_dir()?))
}

/// Injectable counterpart used by isolated history stores and tests.
pub fn recovery_audio_dir_at(config: &std::path::Path) -> PathBuf {
    config.join("recovery-audio")
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

/// Optional instruction-model slot. Installing a model here (or a symlink to
/// verified shared weights) enables instruction work without changing cleanup.
/// Missing is a valid state: plain dictation remains usable.
pub fn instruction_default_path() -> Option<String> {
    let mut candidates = vec![models_base_dir().join("llm/instruction-default.gguf")];
    // A model downloaded after installing a self-contained app belongs in the
    // writable sidecar, never inside the signed app bundle.
    if let Some(support) = app_support_dir() {
        candidates.push(support.join("models/llm/instruction-default.gguf"));
    }
    candidates.into_iter().find(|path| path.is_file()).map(|path| {
        std::fs::canonicalize(&path).unwrap_or(path).to_string_lossy().into_owned()
    })
}

/// Directory the menu's model picker scans for cleanup models (`<base>/llm`).
pub fn llm_models_dir() -> PathBuf {
    models_base_dir().join("llm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_log_is_per_user_and_never_in_tmp() {
        if let Some(log) = daemon_log_path() {
            assert!(log.ends_with("Library/Logs/Local Dictation/daemon.log"));
            assert!(log.starts_with(home_dir().unwrap()));
            assert!(!log.starts_with("/tmp"));
        }
    }

    #[test]
    fn the_build_commit_is_read_from_the_bundles_own_plist() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleShortVersionString</key>
    <string>0.4.0</string>
    <key>LocalDictationBuiltFromCommit</key>
    <string>a6b9410</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>"#;
        assert_eq!(
            plist_string_value(plist, "LocalDictationBuiltFromCommit").as_deref(),
            Some("a6b9410")
        );
        assert_eq!(
            plist_string_value(plist, "CFBundleShortVersionString").as_deref(),
            Some("0.4.0")
        );
        // A key that is not there, and a key whose value is not a string,
        // report nothing rather than the next key's value.
        assert_eq!(plist_string_value(plist, "CFBundleVersion"), None);
        assert_eq!(plist_string_value(plist, "LSUIElement"), None);
        assert_eq!(plist_string_value("", "anything"), None);
    }

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
