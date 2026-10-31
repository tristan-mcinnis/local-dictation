//! Personal corrections dictionary.
//!
//! Lets the user define word-level substitutions that get applied after
//! the LLM cleanup, before injection. Use case: proper nouns that
//! Parakeet doesn't know (`Eefa → Aoife`), domain-specific casing
//! (`postgres → Postgres`), common transcription errors (`teh → the`),
//! or any personal vocabulary tweak.
//!
//! File format (JSON):
//!
//! ```json
//! {
//!     "eefa": "Aoife",
//!     "todoist": "Todoist",
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
//! Matching is word-boundary aware — `eefa` won't match inside
//! `eefaworth` — and supports multi-word keys: `to twist → Todoist` matches
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

/// The result of a Dictionary-pane Add/rename. `NoTarget` means the "Correct
/// to" field was blank, which is a nudge, not an error.
#[derive(Debug, PartialEq, Eq)]
pub enum QuickAdd {
    /// Written to disk. Carries the new entry count.
    Saved(usize),
    /// Nothing to add: no target spelling was typed.
    NoTarget,
}

impl Corrections {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build directly from a key→replacement map. Keys are lowercased so
    /// matching stays case-insensitive (mirroring `load_from`). Useful for
    /// tests and for callers that source corrections from somewhere other
    /// than the JSON file.
    pub fn from_map(map: HashMap<String, String>) -> Self {
        let map = map
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();
        Self::build(map)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The distinct replacement values — i.e. the *target* spellings the user
    /// cares about (`Aoife`, `macOS`, `GitHub`, …). Feeding these to the
    /// cleanup model as a "known vocabulary" hint stops it from "correcting"
    /// them away before the post-cleanup substitution even runs. Sorted
    /// (case-insensitive) so the result is stable across launches; duplicates
    /// are collapsed.
    pub fn replacement_values(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for v in self.map.values() {
            if seen.insert(v.as_str()) {
                out.push(v.clone());
            }
        }
        // Sorted, so the list is the same on every launch. It feeds a capped
        // prompt block: with HashMap order, WHICH terms survived the cap
        // changed from one launch to the next.
        out.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
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

    /// Every entry as `(key, replacement)`, key-sorted — the list the
    /// Dictionary pane draws, one row per entry. Keys are already lowercased,
    /// so the sort is the same order `to_editor_lines` produces.
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The entries whose key or replacement contains `needle`, case-insensitively
    /// — the Dictionary pane's search. A blank needle matches everything, so the
    /// list and the filtered list are one code path.
    pub fn search(&self, needle: &str) -> Vec<(String, String)> {
        let needle = needle.trim().to_lowercase();
        self.entries()
            .into_iter()
            .filter(|(k, v)| {
                needle.is_empty() || k.contains(&needle) || v.to_lowercase().contains(&needle)
            })
            .collect()
    }

    /// Drop the entry keyed by `key` (matched case-insensitively, like every
    /// other lookup). Returns whether anything was removed. Rebuilds through
    /// `build` so the phrase index cannot drift.
    pub fn remove(&mut self, key: &str) -> bool {
        let mut map = self.map.clone();
        if map.remove(&key.to_lowercase()).is_none() {
            return false;
        }
        *self = Self::build(map);
        true
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

    /// Merge CSV rows into the dictionary, returning the number of net-new
    /// entries added (an existing key that gets overwritten is not counted).
    ///
    /// Row semantics:
    ///   * `from,to`  → a substitution (`from` keyed lowercased, `to` verbatim).
    ///   * `word`     → an identity entry (word → word), preserving the spelling.
    ///   * Blank lines and empty cells are skipped. A header row — one of
    ///     `from,to`, `heard,correct`, `word`, `from`, `phrase,replacement`
    ///     (case-insensitive, first data row only) — is skipped too.
    ///
    /// The merge is done by reading the existing `map`, recombining it with the
    /// parsed rows, and rebuilding through [`Self::build`] so the phrase index
    /// can't drift.
    pub fn import_csv(&mut self, csv_body: &str) -> eyre::Result<usize> {
        // Excel writes a UTF-8 BOM; without this the header cell reads
        // "\u{feff}from" and is imported as a term.
        let records = parse_csv(csv_body.trim_start_matches('\u{feff}'));
        let mut map: HashMap<String, String> = self.map.clone();
        let mut added = 0usize;
        let mut first_data_row = true;

        for cells in records {
            // Blank line: every cell empty.
            if cells.iter().all(|c| c.trim().is_empty()) {
                continue;
            }
            // Header detection applies only to the first non-blank row.
            if first_data_row {
                first_data_row = false;
                if is_header_row(&cells) {
                    continue;
                }
            }
            let fields: Vec<&str> = cells
                .iter()
                .map(|c| c.trim())
                .filter(|c| !c.is_empty())
                .collect();
            match fields.len() {
                0 => {}                    // all empty cells (already blank)
                1 => {
                    // Identity entry: word → word, so the spelling is preserved.
                    let word = fields[0];
                    if map.insert(word.to_lowercase(), word.to_string()).is_none() {
                        added += 1;
                    }
                }
                2 => {
                    // Substitution: from → to, key lowercased via `build`'s funnel.
                    let (from, to) = (fields[0], fields[1]);
                    if map.insert(from.to_lowercase(), to.to_string()).is_none() {
                        added += 1;
                    }
                }
                _ => {}                    // >2 columns: ignore the extra cells
            }
        }

        *self = Self::build(map);
        Ok(added)
    }

    /// Read the CSV file at `path` and merge it via [`Self::import_csv`],
    /// returning the number of entries added. (The menu bar will use this.)
    pub fn import_csv_path(&mut self, path: &Path) -> eyre::Result<usize> {
        let body = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!(r#"read {}: {e}"#, path.display()))?;
        self.import_csv(&body)
    }

    /// Write the map to `path` as a pretty, key-sorted JSON object.
    pub fn save_to(&self, path: &Path) -> eyre::Result<()> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // A corrections file this build cannot parse is the user's whole
        // dictionary. It loads as "empty", so saving would replace it with
        // whatever little the editor holds. Refuse instead.
        crate::config_io::refuse_if_unreadable(path, |body| {
            serde_json::from_str::<RawMap>(body).is_ok()
        })?;
        // `_`-prefixed keys are the user's JSON comments; carry them over.
        let comments: Vec<(String, String)> = std::fs::read_to_string(path)
            .ok()
            .and_then(|body| serde_json::from_str::<RawMap>(&body).ok())
            .map(|raw| {
                let mut c: Vec<_> = raw.0.into_iter().filter(|(k, _)| k.starts_with('_')).collect();
                c.sort();
                c
            })
            .unwrap_or_default();
        let mut keys: Vec<&String> = self.map.keys().collect();
        keys.sort();
        let obj: serde_json::Map<String, serde_json::Value> = comments
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .chain(
                keys.into_iter()
                    .map(|k| (k.clone(), serde_json::Value::String(self.map[k].clone()))),
            )
            .collect();
        let body = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
            .map_err(|e| eyre::eyre!("serialize corrections: {e}"))?;
        crate::config_io::write_atomic(path, format!("{body}\n").as_bytes())
            .map_err(|e| eyre::eyre!("write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Save to the default corrections path (or `DICTATE_CORRECTIONS_PATH`).
    pub fn save(&self) -> eyre::Result<()> {
        let path =
            Self::config_path().ok_or_else(|| eyre::eyre!("no config dir for corrections.json"))?;
        self.save_to(&path)
    }

    /// Add (or rename) one entry exactly as the Dictionary pane's Add button
    /// does, then persist it **immediately** so the edit survives closing the
    /// window without Apply & Restart. `editing` is the key a row click pulled
    /// into the fields; it is dropped before the new line is added so renaming
    /// does not leave the old spelling behind beside the new one. On a write
    /// failure nothing is committed: both the in-memory dictionary and the file
    /// are left untouched, and the error comes back for the pane to show.
    pub fn add_and_save_to(
        &mut self,
        editing: Option<&str>,
        heard: &str,
        correct: &str,
        path: &Path,
    ) -> eyre::Result<QuickAdd> {
        let Some(line) = quick_add_line(heard, correct) else {
            return Ok(QuickAdd::NoTarget);
        };
        let mut next = self.clone();
        if let Some(key) = editing {
            next.remove(key);
        }
        let mut lines = next.to_editor_lines();
        lines.push(line);
        next = Self::from_editor_text(&lines.join("\n"));
        next.save_to(path)?;
        *self = next;
        Ok(QuickAdd::Saved(self.len()))
    }

    /// Remove the entry keyed by `key` and persist **immediately**, so the
    /// delete survives closing the window without Apply & Restart. Returns
    /// whether an entry was dropped. A write failure changes nothing: the
    /// in-memory dictionary and the file are both left as they were.
    pub fn remove_and_save_to(&mut self, key: &str, path: &Path) -> eyre::Result<bool> {
        let mut next = self.clone();
        if !next.remove(key) {
            return Ok(false);
        }
        next.save_to(path)?;
        *self = next;
        Ok(true)
    }

    pub fn load_from(path: &Path) -> eyre::Result<Self> {
        let body = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("read {}: {e}", path.display()))?;
        // A blank file is an empty dictionary, not a corrupt one: `save_to`
        // will write over it, so the editors must not lock the user out.
        if body.trim().is_empty() {
            return Ok(Self::empty());
        }
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
    /// word-boundary aware — `eefa` won't match inside `eefaworth`, and a
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

/// Is `cells` a CSV header row? Matched on the first data row only, and
/// case-insensitively. Recognized headers: `from,to`, `heard,correct`,
/// `word`, `from`, `phrase,replacement`.
fn is_header_row(cells: &[String]) -> bool {
    let norm: Vec<String> = cells.iter().map(|c| c.trim().to_lowercase()).collect();
    match norm.len() {
        1 => norm[0] == "word" || norm[0] == "from",
        2 => (norm[0] == "from" && norm[1] == "to")
            || (norm[0] == "heard" && norm[1] == "correct")
            || (norm[0] == "phrase" && norm[1] == "replacement"),
        _ => false,
    }
}

/// Minimal RFC4180-style CSV parser: fields separated by commas, records by
/// `\n` or `\r\n`. A field may be double-quoted — a quoted field can contain
/// commas and newlines, and a doubled `""` inside quotes is a literal `"`.
/// Surrounding whitespace is trimmed on unquoted fields; quoted fields keep
/// their bytes verbatim (so interior spacing isn't lost). Returns one record
/// per row; physical lines inside a quoted field stay part of that field.
fn parse_csv(body: &str) -> Vec<Vec<String>> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut quoted = false; // whether the field currently being built is quoted
    let mut chars = body.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                // A doubled quote inside quotes is a literal `"`; a single
                // one closes the quoted field.
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else {
            match c {
                '"' => {
                    in_quotes = true;
                    quoted = true;
                }
                ',' => {
                    record.push(finish_field(&field, quoted));
                    field.clear();
                    quoted = false;
                }
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    record.push(finish_field(&field, quoted));
                    field.clear();
                    quoted = false;
                    records.push(std::mem::take(&mut record));
                }
                '\n' => {
                    record.push(finish_field(&field, quoted));
                    field.clear();
                    quoted = false;
                    records.push(std::mem::take(&mut record));
                }
                _ => field.push(c),
            }
        }
    }
    // Flush the last record (handles a missing trailing newline, including a
    // trailing comma's empty field, e.g. `a,b,`).
    if !field.is_empty() || !record.is_empty() {
        record.push(finish_field(&field, quoted));
        records.push(record);
    }
    records
}

/// Finalize a field: quoted fields are kept verbatim; unquoted fields have
/// surrounding whitespace trimmed.
fn finish_field(field: &str, quoted: bool) -> String {
    if quoted {
        field.to_string()
    } else {
        field.trim().to_string()
    }
}

#[cfg(test)]
mod tests;
