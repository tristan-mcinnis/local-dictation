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
//! `lingsworth` — and supports multi-word keys: `to twist → Todoist` matches
//! the words "to twist" (or "to-twist") as a unit. Punctuation, whitespace, and
//! other non-alphanumeric characters delimit words; a phrase's words may be
//! joined only by spaces or hyphens, so a key never matches across a comma or
//! period. When several keys could match at a word, the longest phrase wins.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default, Debug, Clone)]
pub struct Corrections {
    /// All keys lowercased; values stored verbatim. The source of truth for the
    /// editor, `save`, and the vocabulary hint.
    map: HashMap<String, String>,
    /// Derived phrase index: each key reduced to its lowercased word tokens,
    /// joined by a single space (so `"to-doist"` and `"to doist"` both become
    /// `"to doist"`), mapped to the replacement. Built once per construction so
    /// `apply` can match multi-word keys without re-tokenizing every call.
    phrases: HashMap<String, String>,
    /// Longest key, in tokens — the max phrase length `apply` needs to probe.
    max_phrase_tokens: usize,
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
        Self::build(map)
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
        Self::build(map)
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
        Ok(Self::build(map))
    }

    /// Build a `Corrections` (with its derived phrase index) from an
    /// already-lowercased key map. The single funnel every constructor passes
    /// through, so the phrase index can't drift from `map`.
    fn build(map: HashMap<String, String>) -> Self {
        let mut phrases = HashMap::with_capacity(map.len());
        let mut max_phrase_tokens = 0;
        for (k, v) in &map {
            let tokens = key_tokens(k);
            if tokens.is_empty() {
                continue;
            }
            max_phrase_tokens = max_phrase_tokens.max(tokens.len());
            phrases.insert(tokens.join(" "), v.clone());
        }
        Self {
            map,
            phrases,
            max_phrase_tokens,
        }
    }

    /// Apply corrections to `text`, matching both single words and multi-word
    /// phrases (`to twist → Todoist`). Lookup is case-insensitive and
    /// word-boundary aware — `lings` won't match inside `lingsworth`, and a
    /// phrase only matches a run of whole words joined by spaces or hyphens
    /// (never across sentence punctuation). The longest phrase anchored at a
    /// word wins; replacements use the dictionary value verbatim.
    pub fn apply(&self, text: &str) -> String {
        if self.phrases.is_empty() {
            return text.to_string();
        }
        let segs = segment(text);
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < segs.len() {
            match &segs[i] {
                Seg::Sep(s) => {
                    out.push_str(s);
                    i += 1;
                }
                Seg::Word { orig, .. } => match self.match_phrase(&segs, i) {
                    Some((rep, consumed)) => {
                        out.push_str(rep);
                        i += consumed;
                    }
                    None => {
                        out.push_str(orig);
                        i += 1;
                    }
                },
            }
        }
        out
    }

    /// Longest-first phrase match anchored at the word segment `start`. Returns
    /// the replacement plus how many segments it consumes (the matched words
    /// and the separators joining them). Words may be joined only by spaces or
    /// hyphens, so a phrase never spans a comma or period.
    fn match_phrase(&self, segs: &[Seg], start: usize) -> Option<(&str, usize)> {
        let mut tokens: Vec<&str> = Vec::new();
        let mut consumed: Vec<usize> = Vec::new(); // segs spanned when the phrase ends at this word
        let mut j = start;
        loop {
            let Some(Seg::Word { lower, .. }) = segs.get(j) else {
                break;
            };
            tokens.push(lower.as_str());
            consumed.push(j + 1 - start);
            if tokens.len() >= self.max_phrase_tokens {
                break;
            }
            // Extend only across a joinable separator immediately followed by
            // another word; anything else ends the phrase.
            match (segs.get(j + 1), segs.get(j + 2)) {
                (Some(Seg::Sep(s)), Some(Seg::Word { .. })) if is_joinable_sep(s) => j += 2,
                _ => break,
            }
        }
        for len in (1..=tokens.len()).rev() {
            if let Some(rep) = self.phrases.get(&tokens[..len].join(" ")) {
                return Some((rep.as_str(), consumed[len - 1]));
            }
        }
        None
    }
}

/// A word (alphanumeric + apostrophe run) or a separator run. `apply` walks the
/// input as these segments so it can match phrases across words while emitting
/// the original casing and spacing verbatim for everything it doesn't replace.
enum Seg {
    Word { orig: String, lower: String },
    Sep(String),
}

/// Split text into alternating word/separator segments, preserving everything.
fn segment(text: &str) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut word = String::new();
    let mut sep = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '\'' {
            if !sep.is_empty() {
                segs.push(Seg::Sep(std::mem::take(&mut sep)));
            }
            word.push(c);
        } else {
            if !word.is_empty() {
                let lower = word.to_lowercase();
                segs.push(Seg::Word {
                    orig: std::mem::take(&mut word),
                    lower,
                });
            }
            sep.push(c);
        }
    }
    if !word.is_empty() {
        let lower = word.to_lowercase();
        segs.push(Seg::Word { orig: word, lower });
    }
    if !sep.is_empty() {
        segs.push(Seg::Sep(sep));
    }
    segs
}

/// Tokens of a correction *key*: lowercased alphanumeric/apostrophe runs. Both
/// `"to-doist"` and `"to doist"` reduce to `["to", "doist"]`, so either spoken
/// spacing matches the same entry.
fn key_tokens(key: &str) -> Vec<String> {
    key.split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// A separator is "joinable" (can sit inside a phrase) only if it's entirely
/// spaces, tabs, or hyphens — so `to twist` and `t-twist` match, but `to. twist`
/// and `to, twist` do not.
fn is_joinable_sep(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c == ' ' || c == '\t' || c == '-')
}

/// Build a single Dictionary-editor line from the quick-add fields, formatted
/// exactly the way [`Corrections::to_editor_lines`] renders an entry so the two
/// stay in lock-step: `None` when there's no target spelling; a bare word when
/// `heard` is blank or matches `correct` case-insensitively (an identity/keep
/// entry); else `heard → correct`. Inputs are trimmed.
pub fn quick_add_line(heard: &str, correct: &str) -> Option<String> {
    let heard = heard.trim();
    let correct = correct.trim();
    if correct.is_empty() {
        return None;
    }
    if heard.is_empty() || heard.eq_ignore_ascii_case(correct) {
        Some(correct.to_string())
    } else {
        Some(format!("{heard} → {correct}"))
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
        Corrections::build(map)
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
    fn matches_multi_word_phrase() {
        // The whole point of the rewrite: multi-word keys actually fire now.
        let c = dict(&[("to twist", "Todoist")]);
        assert_eq!(c.apply("send articles to twist now"), "send articles Todoist now");
        // Case-insensitive across the whole phrase; surrounding spacing kept.
        assert_eq!(c.apply("To Twist"), "Todoist");
    }

    #[test]
    fn hyphen_and_space_spelling_match_same_phrase_key() {
        // A key written with a hyphen matches both hyphen and space input.
        let c = dict(&[("t-twist", "Todoist")]);
        assert_eq!(c.apply("my T-Twist"), "my Todoist");
        assert_eq!(c.apply("my t twist"), "my Todoist");
    }

    #[test]
    fn phrase_does_not_span_sentence_punctuation() {
        // "to" ends a sentence and "Twist" starts the next — not the phrase.
        let c = dict(&[("to twist", "Todoist")]);
        assert_eq!(c.apply("get to. Twist it"), "get to. Twist it");
    }

    #[test]
    fn longest_phrase_anchored_at_a_word_wins() {
        let c = dict(&[("new york", "NYC"), ("new york city", "NYC metro")]);
        assert_eq!(c.apply("from new york city today"), "from NYC metro today");
        assert_eq!(c.apply("from new york today"), "from NYC today");
    }

    #[test]
    fn single_word_behavior_unchanged_alongside_phrases() {
        let c = dict(&[("macos", "macOS"), ("to twist", "Todoist")]);
        assert_eq!(c.apply("ship macos to twist"), "ship macOS Todoist");
        // A phrase key does not enable partial-word matches.
        assert_eq!(c.apply("macostuff"), "macostuff");
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
    fn quick_add_line_formats_like_editor_lines() {
        // A real mishearing fix → arrow line, keyed on what was heard.
        assert_eq!(quick_add_line("Julian", "Julien"), Some("Julian → Julien".to_string()));
        // Blank "heard" → a bare keep-this-spelling word.
        assert_eq!(quick_add_line("", "GitHub"), Some("GitHub".to_string()));
        // Same word, different case → identity/keep, rendered bare (no arrow).
        assert_eq!(quick_add_line("github", "GitHub"), Some("GitHub".to_string()));
        // No target spelling → nothing to add.
        assert_eq!(quick_add_line("Julian", "   "), None);
        // Inputs are trimmed.
        assert_eq!(quick_add_line("  lings ", " Lingzi "), Some("lings → Lingzi".to_string()));
    }

    #[test]
    fn quick_add_line_round_trips_through_corrections() {
        // The line the quick-add fields produce must parse back to the intended fix.
        let line = quick_add_line("Julian", "Julien").unwrap();
        let c = Corrections::from_editor_text(&line);
        assert_eq!(c.apply("Julian"), "Julien");
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
