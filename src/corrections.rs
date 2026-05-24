//! Personal corrections dictionary.
//!
//! Lets the user define word-level substitutions that get applied after
//! the LLM cleanup, before injection. Use case: proper nouns that
//! Parakeet doesn't know (`Lings → Lingzi`), domain-specific casing
//! (`postgres → Postgres`), common transcription errors (`teh → the`),
//! or any personal vocabulary tweak.
//!
//! File format (JSON):
//!
//! ```json
//! {
//!     "lings": "Lingzi",
//!     "tristan": "Tristan",
//!     "postgres": "Postgres",
//!     "macos": "macOS",
//!     "github": "GitHub"
//! }
//! ```
//!
//! Keys are lowercased on load; matching is **case-insensitive**.
//! Replacements are written verbatim from the value (so put `"macOS"`
//! not `"macos"` in the value).
//!
//! Path priority:
//!   1. `DICTATE_CORRECTIONS_PATH` env var (if set, must exist)
//!   2. `~/.config/local-dictation/corrections.json`
//!   3. otherwise: no corrections (returns an empty dictionary)
//!
//! Matching is word-boundary aware — `lings` won't match inside
//! `lingsworth`. Punctuation, whitespace, and other non-alphanumeric
//! characters delimit words.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Default, Debug, Clone)]
pub struct Corrections {
    /// All keys lowercased; values stored verbatim.
    map: HashMap<String, String>,
}

#[derive(Deserialize)]
struct RawMap(HashMap<String, String>);

impl Corrections {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build directly from a key→replacement map. Keys are lowercased so
    /// matching stays case-insensitive (mirroring `load_from`). Useful for
    /// tests and for callers that source corrections from somewhere other
    /// than the JSON file.
    pub fn from_map(map: HashMap<String, String>) -> Self {
        let map = map.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
        Self { map }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The distinct replacement values — i.e. the *target* spellings the user
    /// cares about (`Lingzi`, `macOS`, `GitHub`, …). Feeding these to the
    /// cleanup model as a "known vocabulary" hint stops it from "correcting"
    /// them away before the post-cleanup substitution even runs. Order is
    /// unspecified; duplicates are collapsed.
    pub fn replacement_values(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for v in self.map.values() {
            if seen.insert(v.as_str()) {
                out.push(v.clone());
            }
        }
        out
    }

    /// Load from the default path. Honors `DICTATE_CORRECTIONS_PATH` if
    /// set, else `~/.config/local-dictation/corrections.json`. Returns
    /// an empty dictionary (without error) if the file doesn't exist.
    pub fn load_default() -> eyre::Result<Self> {
        if let Ok(p) = std::env::var("DICTATE_CORRECTIONS_PATH") {
            return Self::load_from(Path::new(&p));
        }
        let Some(path) = crate::app_paths::config_file("corrections.json") else {
            return Ok(Self::empty());
        };
        if !path.exists() {
            return Ok(Self::empty());
        }
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> eyre::Result<Self> {
        let body = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("read {}: {e}", path.display()))?;
        let raw: RawMap = serde_json::from_str(&body)
            .map_err(|e| eyre::eyre!("parse {}: {e}", path.display()))?;
        // Filter `_`-prefixed keys — convention for JSON-comment fields
        // (a common workaround since standard JSON has no comment syntax).
        let map = raw
            .0
            .into_iter()
            .filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();
        Ok(Self { map })
    }

    /// Apply corrections to `text` with word-boundary matching.
    /// Lookup is case-insensitive; replacement uses the dictionary
    /// value verbatim (so `lings → Lingzi` works regardless of how
    /// "lings" was capitalized in the input).
    pub fn apply(&self, text: &str) -> String {
        if self.map.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut word = String::new();
        for c in text.chars() {
            if c.is_alphanumeric() || c == '\'' {
                word.push(c);
            } else {
                self.flush_word(&mut word, &mut out);
                out.push(c);
            }
        }
        self.flush_word(&mut word, &mut out);
        out
    }

    fn flush_word(&self, word: &mut String, out: &mut String) {
        if word.is_empty() {
            return;
        }
        let lower = word.to_lowercase();
        if let Some(rep) = self.map.get(&lower) {
            out.push_str(rep);
        } else {
            out.push_str(word);
        }
        word.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(pairs: &[(&str, &str)]) -> Corrections {
        let map = pairs
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.to_string()))
            .collect();
        Corrections { map }
    }

    #[test]
    fn empty_dict_returns_input_unchanged() {
        let c = Corrections::empty();
        assert_eq!(c.apply("anything goes here"), "anything goes here");
    }

    #[test]
    fn basic_word_replacement() {
        let c = dict(&[("lings", "Lingzi")]);
        assert_eq!(c.apply("Hello lings how are you"), "Hello Lingzi how are you");
    }

    #[test]
    fn replacement_is_case_insensitive_match() {
        let c = dict(&[("lings", "Lingzi")]);
        assert_eq!(c.apply("LINGS Lings lings"), "Lingzi Lingzi Lingzi");
    }

    #[test]
    fn does_not_match_inside_other_words() {
        let c = dict(&[("lings", "Lingzi")]);
        assert_eq!(c.apply("lingsworth lingsmith"), "lingsworth lingsmith");
    }

    #[test]
    fn punctuation_is_a_word_boundary() {
        let c = dict(&[("lings", "Lingzi")]);
        assert_eq!(c.apply("Hi, lings!"), "Hi, Lingzi!");
        assert_eq!(c.apply("(lings)"), "(Lingzi)");
    }

    #[test]
    fn multiple_replacements_in_one_pass() {
        let c = dict(&[("teh", "the"), ("macos", "macOS")]);
        assert_eq!(c.apply("teh macos thing"), "the macOS thing");
    }

    #[test]
    fn preserves_apostrophes_in_contractions() {
        let c = dict(&[("dont", "don't")]);
        // "dont" should match; "don't" itself should not become "don'\\'t"
        assert_eq!(c.apply("I dont know"), "I don't know");
        assert_eq!(c.apply("I don't know"), "I don't know");
    }

    #[test]
    fn punctuation_only_passes_through() {
        let c = dict(&[("a", "b")]);
        assert_eq!(c.apply("... !!! ???"), "... !!! ???");
    }
}
