//! Flat known-vocabulary list (`~/.config/local-dictation/dictionary.json`).
//!
//! Companion to [`crate::corrections`]. Where `corrections.json` does *from→to*
//! replacement (`lings → Lingzi`), this is a plain list of words/phrases that
//! are *already spelled the way you want* — names, products, domain terms — that
//! you just want the cleanup model to preserve rather than "correct" into a
//! similar common word. It's a soft hint (fed to the prompt), not a hard
//! substitution, so it never rewrites text on its own.
//!
//! File format: a JSON array of strings. A string beginning with `_` is treated
//! as a comment and skipped (JSON has no comment syntax):
//!
//! ```json
//! ["_ names + domain terms the model should keep verbatim",
//!  "Lingzi", "Parakeet", "CGEventTap", "Anthropic"]
//! ```
//!
//! Path priority: `DICTATE_DICTIONARY_PATH` env var, else
//! `~/.config/local-dictation/dictionary.json`. Missing/invalid file → empty
//! list (never fatal — degrades to "no extra vocabulary").

use std::path::{Path, PathBuf};

/// Parse a `dictionary.json` body into the cleaned word list: trim each entry,
/// drop blanks and `_`-prefixed comment strings. Returns an empty list on
/// invalid JSON (the caller logs). Pure, so it's unit-testable without a file.
fn parse(body: &str) -> Vec<String> {
    match serde_json::from_str::<Vec<String>>(body) {
        Ok(v) => v
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && !s.starts_with('_'))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The resolved dictionary path: `DICTATE_DICTIONARY_PATH` if set, else
/// `~/.config/local-dictation/dictionary.json`. Shared by load + save so the
/// menu-bar editor writes exactly where the cleaner reads.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DICTATE_DICTIONARY_PATH") {
        return Some(PathBuf::from(p));
    }
    crate::app_paths::config_file("dictionary.json")
}

/// Load the user's dictionary from the default location (or
/// `DICTATE_DICTIONARY_PATH`). Best-effort: a missing file is silent, a corrupt
/// one logs a warning and yields an empty list.
pub fn load_default() -> Vec<String> {
    match config_path() {
        Some(p) if p.exists() => load_from(&p),
        _ => Vec::new(),
    }
}

/// Write the dictionary to `path` as a pretty JSON array, after the same
/// cleaning `parse` applies (trim, drop blanks + `_`-comment strings). Creates
/// the parent dir if needed. Pure w.r.t. the path, so it's unit-testable.
pub fn save_to(path: &Path, terms: &[String]) -> eyre::Result<()> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let cleaned: Vec<String> = terms
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.starts_with('_'))
        .collect();
    let body = serde_json::to_string_pretty(&cleaned)
        .map_err(|e| eyre::eyre!("serialize dictionary: {e}"))?;
    std::fs::write(path, format!("{body}\n"))
        .map_err(|e| eyre::eyre!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Save the dictionary to the default location (or `DICTATE_DICTIONARY_PATH`).
pub fn save(terms: &[String]) -> eyre::Result<()> {
    let path = config_path().ok_or_else(|| eyre::eyre!("no config dir for dictionary.json"))?;
    save_to(&path, terms)
}

/// Load + parse a dictionary file, logging (but not failing) on a parse error.
pub fn load_from(path: &Path) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let terms = parse(&body);
    if terms.is_empty() && !body.trim().is_empty() && serde_json::from_str::<Vec<String>>(&body).is_err() {
        eprintln!("[warn] dictionary parse failed ({}): expected a JSON array of strings", path.display());
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_array_trims_and_drops_blanks_and_comments() {
        let body = r#"["_ a comment", "Parakeet", "  Lingzi  ", "", "  ", "CGEventTap"]"#;
        assert_eq!(
            parse(body),
            vec!["Parakeet".to_string(), "Lingzi".to_string(), "CGEventTap".to_string()]
        );
    }

    #[test]
    fn invalid_json_yields_empty() {
        assert!(parse("not json").is_empty());
        assert!(parse(r#"{"not":"an array"}"#).is_empty());
        assert!(parse("").is_empty());
    }

    #[test]
    fn empty_array_is_empty() {
        assert!(parse("[]").is_empty());
    }

    #[test]
    fn save_then_load_round_trips_and_cleans() {
        let dir = std::env::temp_dir().join(format!("dict-test-{}", std::process::id()));
        let path = dir.join("dictionary.json");
        // Blanks and _-comments are dropped on save.
        save_to(&path, &[
            "Parakeet".into(),
            "  Lingzi  ".into(),
            "".into(),
            "_ignore".into(),
        ])
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded, vec!["Parakeet".to_string(), "Lingzi".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
