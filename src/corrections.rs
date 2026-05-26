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
use std::path::{Path, PathBuf};

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

    /// The resolved corrections path: `DICTATE_CORRECTIONS_PATH` if set, else
    /// `~/.config/local-dictation/corrections.json`. Shared by load + save so the
    /// menu-bar Dictionary editor writes exactly where the refiner/cleaner read.
    pub fn config_path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("DICTATE_CORRECTIONS_PATH") {
            return Some(PathBuf::from(p));
        }
        crate::app_paths::config_file("corrections.json")
    }

    /// Render the map as lines for the Dictionary editor. An identity entry
    /// (key equals value, case-insensitively) shows as just the word — a "keep
    /// this spelling" hint; a real fix shows as `from → to`. Sorted for a stable,
    /// readable list.
    pub fn to_editor_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .map
            .iter()
            .map(|(k, v)| {
                if k.eq_ignore_ascii_case(v) {
                    v.clone()
                } else {
                    format!("{k} → {v}")
                }
            })
            .collect();
        lines.sort_by_key(|l| l.to_lowercase());
        lines
    }

    /// Parse Dictionary-editor text into a corrections map. Each non-blank,
    /// non-`_`-comment line is either `from → to` (also accepts `->`) — a
    /// replacement with a lowercased key — or a bare `Word`, stored as an
    /// identity entry so the spelling is both preserved and fed to the model.
    pub fn from_editor_text(text: &str) -> Self {
        let mut map = HashMap::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('_') {
                continue;
            }
            let (from, to) = match split_arrow(line) {
                Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
                None => (line.to_string(), line.to_string()), // bare word = identity
            };
            if from.is_empty() || to.is_empty() {
                continue;
            }
            map.insert(from.to_lowercase(), to);
        }
        Self { map }
    }

    /// Write the map to `path` as a pretty, key-sorted JSON object.
    pub fn save_to(&self, path: &Path) -> eyre::Result<()> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut keys: Vec<&String> = self.map.keys().collect();
        keys.sort();
        let obj: serde_json::Map<String, serde_json::Value> = keys
            .into_iter()
            .map(|k| (k.clone(), serde_json::Value::String(self.map[k].clone())))
            .collect();
        let body = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
            .map_err(|e| eyre::eyre!("serialize corrections: {e}"))?;
        std::fs::write(path, format!("{body}\n"))
            .map_err(|e| eyre::eyre!("write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Save to the default corrections path (or `DICTATE_CORRECTIONS_PATH`).
    pub fn save(&self) -> eyre::Result<()> {
        let path = Self::config_path().ok_or_else(|| eyre::eyre!("no config dir for corrections.json"))?;
        self.save_to(&path)
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

/// Split a Dictionary-editor line on the first `→` or `->`, returning
/// `(from, to)`. `None` for a bare word (no arrow).
fn split_arrow(line: &str) -> Option<(&str, &str)> {
    if let Some(i) = line.find('→') {
        return Some((&line[..i], &line[i + '→'.len_utf8()..]));
    }
    if let Some(i) = line.find("->") {
        return Some((&line[..i], &line[i + 2..]));
    }
    None
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

    #[test]
    fn editor_lines_render_fixes_and_bare_words() {
        let c = dict(&[("lings", "Lingzi"), ("github", "GitHub"), ("parakeet", "Parakeet")]);
        let lines = c.to_editor_lines();
        // Casing-only fixes are identical ignoring case → clean bare words.
        assert!(lines.contains(&"Parakeet".to_string()), "{lines:?}");
        assert!(lines.contains(&"GitHub".to_string()), "{lines:?}");
        // Only a genuine phonetic fix (different letters) shows "from → to".
        assert!(lines.contains(&"lings → Lingzi".to_string()), "{lines:?}");
    }

    #[test]
    fn from_editor_text_parses_arrows_and_bare_words() {
        let c = Corrections::from_editor_text(
            "_a comment\nfoo -> Bar\nBaz\nmacos → macOS\n  \n",
        );
        assert_eq!(c.apply("foo"), "Bar"); // -> arrow
        assert_eq!(c.apply("macos"), "macOS"); // → arrow
        assert_eq!(c.apply("baz"), "Baz"); // bare word = identity, keeps casing
        assert_eq!(c.len(), 3); // comment + blank dropped
    }

    #[test]
    fn editor_round_trip_preserves_behavior() {
        let c = dict(&[("lings", "Lingzi"), ("macos", "macOS")]);
        let text = c.to_editor_lines().join("\n");
        let c2 = Corrections::from_editor_text(&text);
        assert_eq!(c2.apply("lings"), "Lingzi");
        assert_eq!(c2.apply("macos"), "macOS");
    }
}
