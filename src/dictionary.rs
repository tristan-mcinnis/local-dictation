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

use std::path::Path;

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

/// Load the user's dictionary from the default location (or
/// `DICTATE_DICTIONARY_PATH`). Best-effort: a missing file is silent, a corrupt
/// one logs a warning and yields an empty list.
pub fn load_default() -> Vec<String> {
    if let Ok(p) = std::env::var("DICTATE_DICTIONARY_PATH") {
        return load_from(Path::new(&p));
    }
    let Some(path) = crate::app_paths::config_file("dictionary.json") else {
        return Vec::new();
    };
    if !path.exists() {
        return Vec::new();
    }
    load_from(&path)
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
}
