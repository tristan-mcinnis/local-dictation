//! User-editable LLM prompts (`~/.config/local-dictation/prompts.json`).
//!
//! The cleanup and transform *system prompts* are the two strings that most
//! shape what Gemma does to your text. Both ship with sensible built-in
//! defaults; drop a `prompts.json` next to `settings.json` to override either.
//! A missing file, a missing field, or an empty/blank string all fall back to
//! the built-in default, so a partial file is fine:
//!
//! ```json
//! { "transform": "Rewrite the selection however I ask...", "cleanup": "..." }
//! ```
//!
//! Precedence (highest first): env var > `prompts.json` field > built-in default.
//!   - cleanup:    `DICTATE_CLEANUP_PROMPT`
//!   - transform:  `DICTATE_TRANSFORM_PROMPT`
//!   - file path:  `DICTATE_PROMPTS_PATH` (default
//!     `~/.config/local-dictation/prompts.json`)
//!
//! Prompts are read once when the cleanup engine starts, so edits take effect
//! on the next daemon launch — same as switching the model or hotkey.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Default cleanup prompt. Lives here (not in `cleaner.rs`) so all prompt text
/// is in one place.
///
/// Shape matters more than wording. "Reformat the user's message" (the previous
/// opening) reads to a 1B model as a licence to rewrite, and it took it: over
/// 429 real transcripts it deleted a content word in 25 of the 120 utterances
/// that reached it, 52 words in all, once dropping fourteen words of a
/// half-finished sentence to make the rest read tidily. Framing the job as the
/// *smallest* change, listing what may be touched, and forbidding the tidy-up
/// explicitly cuts that to 18 utterances and 29 words, and is slightly faster
/// (419 ms vs 453 ms). Measured with `examples/cleanup_lab.rs`; see
/// `docs/cleanup-behaviour-20260909.md`.
pub const DEFAULT_CLEANUP: &str =
    "Clean up the user's dictated message with the smallest change that works.\n\
     You may only:\n\
     - fix punctuation, capitalization, obvious misspellings, and homophones\n\
     - write clearly spoken numbers and dates as digits\n\
     - delete \"um\", \"uh\", an accidentally repeated word, and an explicit \
       self-correction\n\
     - start a new paragraph where the message is long or turns to a new \
       subject\n\
     Do not rephrase, tighten, summarize, or improve the wording. Keep every \
     other word the speaker said, and keep their tone, hesitation, slang, and \
     force. Never soften or cut profanity. Never drop a clause, an aside, or a \
     trailing thought to make the rest read as a tidy sentence: an unfinished \
     sentence stays unfinished.\n\
     Return only the cleaned message. Do not answer it, and add nothing.";

/// Default transform prompt (Shift + push-to-talk). Deliberately permissive:
/// the user spoke an explicit instruction, so follow it even when that means
/// *adding* or restructuring content. The previous wording told a 1B model to
/// "preserve the original meaning" and "return the original text unchanged" if
/// unsure — which made expansion-type instructions (e.g. "turn these 2 bullets
/// into 4") silently no-op. We keep the strict *output-shape* rules but drop
/// the conservatism so the instruction wins.
pub const DEFAULT_TRANSFORM: &str =
    "You are a precise text editor. Apply the user's instruction to the text \
     below and follow it faithfully — even when that means adding, removing, \
     rephrasing, expanding, restructuring, or translating. Do not minimize \
     your changes: if the instruction asks for more items, more detail, or a \
     longer result, produce it.\n\
     - Honor the output shape the instruction implies: if it asks for \
       bullets, items, or steps — or the text is already a list — return one \
       item per line ('- ' for bullets, '1.', '2.', '3.' for numbered \
       steps), never a single run-on paragraph.\n\
     - If the instruction names a specific number of items (e.g. 'four \
       bullets'), produce exactly that many.\n\
     - If the instruction is a translation, output only the translation.\n\
     - Keep the original wording only where the instruction does not ask you \
       to change it.\n\
     Return ONLY the resulting text — no preamble, no explanation, no quotes, \
     no markdown fences, no commentary.";

/// Built-in output-format presets, available out of the box (the menu bar's
/// "Output format" submenu). When one is active it REPLACES the cleanup prompt
/// for normal dictation. A user's `prompts.json` `formats` map is overlaid on
/// top of these (same key overrides; new keys add), so these are sensible
/// defaults rather than a locked set. Tuned in
/// `prompts-lab/` — see that directory's README for the methodology.
pub const DEFAULT_FORMATS: &[(&str, &str)] = &[
    (
        "numbered",
        "Rewrite this dictation as a numbered list. Put each distinct item or \
         step the speaker actually mentioned on its own line, numbered '1.', \
         '2.', '3.' and so on. Fix grammar and punctuation, remove filler, and \
         keep the speaker's own wording. Do NOT add, merge, summarize, or \
         invent items, and do NOT add an introductory or closing line. Output \
         ONLY the numbered list — no preamble, no heading, no commentary.",
    ),
    (
        "bullets",
        "Rewrite this dictation as a tight bulleted list. Put each distinct \
         idea the speaker actually mentioned exactly once on its own line, \
         starting with '- '. Fix grammar, remove filler, use parallel \
         phrasing, and keep the speaker's wording. Do NOT repeat, add, or \
         invent items. Output ONLY the bullets — no preamble, no heading, no \
         commentary.",
    ),
    (
        "email",
        "Rewrite this dictation as a clear, concise email body. Fix grammar \
         and punctuation, use complete sentences and short paragraphs, and \
         keep a professional but natural tone. Do NOT add a subject line, a \
         greeting, a sign-off, or any bracketed placeholder like [Your Name] \
         — write the body only. Output ONLY the email body — no preamble, no \
         quotes, no markdown.",
    ),
    (
        "code",
        "Clean this dictation into a terse technical note suitable for a \
         commit message or code comment. Fix punctuation; preserve every \
         identifier, path, and domain casing exactly (macOS, ONNX, GitHub, \
         snake_case, CamelCase). Do NOT add explanation, headings, quotes, or \
         markdown code fences. Output ONLY the cleaned text.",
    ),
];

/// The built-in format presets as an owned map (lowercased keys), used as the
/// base that a user's `prompts.json` `formats` overlays onto.
pub fn default_formats() -> HashMap<String, String> {
    DEFAULT_FORMATS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Cap on the number of vocabulary terms appended to the cleanup prompt, so a
/// large corrections dictionary can't balloon the prompt (and the KV cache).
pub const MAX_VOCAB_TERMS: usize = 64;

/// Slots of [`MAX_VOCAB_TERMS`] reserved for per-utterance screen terms, so a
/// large dictionary (CSV import makes 64+ terms easy) cannot evict them all.
pub const SCREEN_VOCAB_SLOTS: usize = 16;

/// The curated (corrections + dictionary) terms that go in the prompt: trimmed,
/// de-duplicated case-insensitively, first-seen order, capped at
/// `MAX_VOCAB_TERMS - SCREEN_VOCAB_SLOTS`. Computed ONCE at boot so this block
/// of the prompt is byte-identical on every utterance and its KV-cache prefix
/// is reused.
pub fn cap_static_vocab(terms: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    terms
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty() && seen.insert(t.to_lowercase()))
        .take(MAX_VOCAB_TERMS - SCREEN_VOCAB_SLOTS)
        .map(str::to_string)
        .collect()
}

/// The curated block in priority order, before the cap: `dictionary.json`
/// terms FIRST, then the corrections' target spellings. When the two together
/// pass the cap, something is cut. A correction target is still enforced after
/// cleanup by the deterministic substitution; a dictionary term has no such
/// second chance, so it must not be the one evicted. Both inputs keep their
/// own (stable) order, so the block is identical on every launch.
pub fn curated_vocabulary(dictionary: &[String], correction_targets: &[String]) -> Vec<String> {
    let mut all = Vec::with_capacity(dictionary.len() + correction_targets.len());
    all.extend_from_slice(dictionary);
    all.extend_from_slice(correction_targets);
    cap_static_vocab(&all)
}

/// Curated terms first (constant prefix), then up to [`SCREEN_VOCAB_SLOTS`]
/// screen terms that are not already curated. `static_capped` must come from
/// [`cap_static_vocab`]. Screen terms go LAST on purpose: the cleaner reuses
/// the longest common token prefix, and a per-utterance term placed first
/// would force the whole vocabulary to be re-decoded on every dictation.
pub fn combined_vocabulary(static_capped: &[String], screen: &[String]) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> =
        static_capped.iter().map(|t| t.to_lowercase()).collect();
    let mut out = static_capped.to_vec();
    out.extend(
        screen
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty() && seen.insert(t.to_lowercase()))
            .take(SCREEN_VOCAB_SLOTS)
            .map(str::to_string),
    );
    out
}

/// Build a "known vocabulary" suffix for the cleanup system prompt from the
/// user's personal vocabulary (the target spellings in `corrections.json`).
/// Returns `None` when there's nothing worth adding.
///
/// This is the dictionary-aware-cleanup borrow: telling the model the user's
/// proper nouns, names, and domain casing up front stops it from "correcting"
/// them away (lowercasing `macOS`, turning `Aoife` into `Eva`) — a
/// complement to the post-cleanup substitution in [`crate::corrections`].
/// Terms are de-duplicated case-insensitively and capped at [`MAX_VOCAB_TERMS`].
pub fn vocabulary_suffix(terms: &[String]) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<&str> = Vec::new();
    for t in terms {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.to_lowercase()) {
            kept.push(t);
            if kept.len() >= MAX_VOCAB_TERMS {
                break;
            }
        }
    }
    if kept.is_empty() {
        return None;
    }
    Some(format!(
        "\n\nKnown vocabulary — these are correctly-spelled proper nouns, names, \
         and domain terms the speaker uses. When the speech clearly refers to one \
         of them, preserve this exact spelling and casing and never substitute a \
         similar common word: {}.",
        kept.join(", ")
    ))
}

/// The resolved prompts handed to the cleanup engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompts {
    pub cleanup: String,
    pub transform: String,
    /// Named output-shape presets (e.g. `email`, `bullets`, `code`), keyed by
    /// lowercased name. Each value is a full cleanup-style system prompt used
    /// in place of `cleanup` when that preset is active. The built-in presets
    /// are always present; a user's `formats` object overlays them.
    pub formats: HashMap<String, String>,
    /// Lowercased names of presets the user's `prompts.json` supplied or
    /// overrode. Such a preset is a free-form instruction that S1-mini was not
    /// trained to follow, so the engine routes it to an instruction model.
    pub custom_formats: HashSet<String>,
    /// True when `cleanup` came from an env var or `prompts.json` rather than
    /// the built-in default. A custom cleanup prompt is a free-form instruction
    /// that S1-mini was not trained to follow.
    pub cleanup_is_custom: bool,
    /// True when `transform` came from an env var or `prompts.json`.
    pub transform_is_custom: bool,
}

/// On-disk shape of `prompts.json`. All fields optional; unknown keys (e.g. a
/// leading `_comment`) are ignored by serde.
#[derive(Debug, Default, Deserialize)]
struct PromptsFile {
    #[serde(default)]
    cleanup: Option<String>,
    #[serde(default)]
    transform: Option<String>,
    #[serde(default)]
    formats: HashMap<String, String>,
}

impl Prompts {
    /// `~/.config/local-dictation/prompts.json`, or `DICTATE_PROMPTS_PATH` when
    /// set. `None` only if neither `DICTATE_PROMPTS_PATH` nor `$HOME` is set.
    pub fn config_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("DICTATE_PROMPTS_PATH") {
            return Some(PathBuf::from(p));
        }
        crate::app_paths::config_file("prompts.json")
    }

    /// Load prompts, applying env > file > default, and log a one-line boot
    /// summary. Never errors: a missing or corrupt file degrades to defaults (a
    /// parse error is logged, not fatal). Use [`Self::load_quiet`] for readers
    /// (e.g. the menu bar) that just need the values without re-logging.
    pub fn load() -> Self {
        let resolved = Self::load_quiet();

        // Tell the user which prompts are customised — useful when testing a
        // tweaked prompt (confirms the file actually took effect).
        let src = |s: &str, default: &str| if s == default { "default" } else { "custom" };
        let formats = if resolved.formats.is_empty() {
            String::from("formats=none")
        } else {
            let mut names: Vec<&str> = resolved.formats.keys().map(|s| s.as_str()).collect();
            names.sort_unstable();
            format!("formats={}", names.join(","))
        };
        eprintln!(
            "[boot] prompts     cleanup={} · transform={} · {formats}",
            src(&resolved.cleanup, DEFAULT_CLEANUP),
            src(&resolved.transform, DEFAULT_TRANSFORM),
        );
        resolved
    }

    /// Same resolution as [`Self::load`] but without the boot-summary log line.
    /// A parse error is still surfaced (it's a real problem, not noise).
    pub fn load_quiet() -> Self {
        let file = Self::config_path()
            .and_then(|p| std::fs::read_to_string(&p).ok().map(|body| (p, body)))
            .and_then(
                |(p, body)| match serde_json::from_str::<PromptsFile>(&body) {
                    Ok(f) => Some(f),
                    Err(e) => {
                        eprintln!("[warn] prompts parse failed ({}): {e}", p.display());
                        None
                    }
                },
            )
            .unwrap_or_default();

        Self::resolve(
            std::env::var("DICTATE_CLEANUP_PROMPT").ok(),
            file.cleanup,
            std::env::var("DICTATE_TRANSFORM_PROMPT").ok(),
            file.transform,
            file.formats,
        )
    }

    /// Pure resolution so the precedence is unit-testable without touching real
    /// env vars or `$HOME`. For each prompt: first non-blank of (env, file),
    /// else the built-in default. Blank/whitespace entries are treated as unset
    /// so an empty field falls back rather than sending an empty system prompt.
    /// Format presets are lowercased on their keys and have blank values
    /// dropped so an empty preset can never blank the cleanup prompt.
    fn resolve(
        env_cleanup: Option<String>,
        file_cleanup: Option<String>,
        env_transform: Option<String>,
        file_transform: Option<String>,
        file_formats: HashMap<String, String>,
    ) -> Self {
        // Start from the built-in presets and overlay the user's file formats:
        // a matching key overrides the default, a new key adds a preset. Blank
        // values and `_`-comment keys are dropped (they can't blank a preset).
        // File-supplied names are remembered so the engine can tell a free-form
        // custom preset from a built-in one.
        let mut formats = default_formats();
        let mut custom_formats = HashSet::new();
        for (k, v) in file_formats {
            if k.starts_with('_') || v.trim().is_empty() {
                continue;
            }
            let k = k.trim().to_lowercase();
            if !k.is_empty() {
                custom_formats.insert(k.clone());
                formats.insert(k, v);
            }
        }
        let (cleanup, cleanup_is_custom) = pick_tracked(env_cleanup, file_cleanup, DEFAULT_CLEANUP);
        let (transform, transform_is_custom) =
            pick_tracked(env_transform, file_transform, DEFAULT_TRANSFORM);
        Self {
            cleanup,
            transform,
            formats,
            custom_formats,
            cleanup_is_custom,
            transform_is_custom,
        }
    }

    /// True when `format` names a preset the user supplied or overrode in
    /// `prompts.json`. Such a preset is a free-form instruction S1-mini cannot
    /// follow, and must not be silently replaced by a trained axis.
    pub fn is_custom_format(&self, format: &str) -> bool {
        self.custom_formats.contains(&format.trim().to_lowercase())
    }

    /// The cleanup system prompt to use for the active formatting preset. When
    /// `format` names a preset present in `self.formats` (matched
    /// case-insensitively), its prompt is returned; otherwise the default
    /// cleanup prompt. This is the formatting-presets borrow (email / bullets /
    /// code): quick output-shape modes layered on top of normal cleanup.
    pub fn cleanup_for(&self, format: Option<&str>) -> &str {
        if let Some(name) = format {
            let name = name.trim().to_lowercase();
            if let Some(p) = self.formats.get(&name) {
                return p;
            }
        }
        &self.cleanup
    }

    /// True when `format` names a preset that exists, by the same rule
    /// [`Prompts::cleanup_for`] uses to pick its prompt. This is the ONE
    /// definition of "a format is active": an unknown, deleted or mistyped
    /// name falls back to no format everywhere, instead of switching off the
    /// deterministic fast path while the default prompt runs.
    pub fn format_is_defined(&self, format: Option<&str>) -> bool {
        format.is_some_and(|name| self.formats.contains_key(&name.trim().to_lowercase()))
    }

    /// Sorted list of defined preset names, for the menu-bar picker.
    pub fn format_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.formats.keys().cloned().collect();
        names.sort_unstable();
        names
    }
}

/// First non-blank of `env`, then `file`, else `default`, plus whether an
/// override (env or file) supplied the value.
fn pick_tracked(
    env: Option<String>,
    file: Option<String>,
    default: &str,
) -> (String, bool) {
    match [env, file]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
    {
        Some(value) => (value, true),
        None => (default.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_formats() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn a_big_dictionary_cannot_evict_the_screen_terms() {
        // Regression: 64+ curated terms filled the cap and no screen term was
        // ever sent, while the log still printed them.
        let curated: Vec<String> = (0..100).map(|i| format!("Curated{i}")).collect();
        let screen: Vec<String> = (0..20).map(|i| format!("Screen{i}")).collect();
        let capped = cap_static_vocab(&curated);
        assert_eq!(capped.len(), MAX_VOCAB_TERMS - SCREEN_VOCAB_SLOTS);
        let combined = combined_vocabulary(&capped, &screen);
        assert_eq!(combined.len(), MAX_VOCAB_TERMS);
        assert_eq!(&combined[..capped.len()], &capped[..], "curated block stays first");
        assert_eq!(combined[capped.len()], "Screen0");
        assert_eq!(combined.last().unwrap(), "Screen15");
        let suffix = vocabulary_suffix(&combined).unwrap();
        assert!(suffix.contains("Screen15") && !suffix.contains("Screen16"));
    }

    #[test]
    fn dictionary_terms_survive_the_cap_before_correction_targets() {
        // Regression: corrections came first in HashMap order, so a big
        // corrections file evicted every dictionary.json term, and which
        // corrections survived changed per launch.
        let targets: Vec<String> = (0..100).map(|i| format!("Target{i:03}")).collect();
        let dictionary = vec!["Okafor".to_string(), "Aoife".to_string()];
        let block = curated_vocabulary(&dictionary, &targets);
        assert_eq!(block.len(), MAX_VOCAB_TERMS - SCREEN_VOCAB_SLOTS);
        assert_eq!(&block[..2], &dictionary[..]);
        assert_eq!(block[2], "Target000");
        assert_eq!(block, curated_vocabulary(&dictionary, &targets), "stable");
    }

    #[test]
    fn the_curated_prefix_is_identical_across_utterances() {
        // KV prefix reuse depends on this: only the tail may vary.
        let capped = cap_static_vocab(&["Todoist".into(), " todoist ".into(), "Aoife".into(), "".into()]);
        assert_eq!(capped, vec!["Todoist".to_string(), "Aoife".to_string()]);
        let a = vocabulary_suffix(&combined_vocabulary(&capped, &["Beatrix".into()])).unwrap();
        let b = vocabulary_suffix(&combined_vocabulary(&capped, &["Okafor".into(), "AOIFE".into()])).unwrap();
        let none = vocabulary_suffix(&combined_vocabulary(&capped, &[])).unwrap();
        let prefix = none.trim_end_matches('.');
        assert!(a.starts_with(prefix), "{a}");
        assert!(b.starts_with(prefix), "{b}");
        assert!(!b.contains("AOIFE"), "a screen term already curated is not repeated");
    }

    #[test]
    fn only_a_defined_preset_counts_as_an_active_format() {
        let p = Prompts::resolve(None, None, None, None, no_formats());
        assert!(p.format_is_defined(Some("numbered")));
        assert!(p.format_is_defined(Some("  Bullets ")));
        // Regression: a stale or mistyped name used to read as "active".
        assert!(!p.format_is_defined(Some("my-deleted-preset")));
        assert!(!p.format_is_defined(Some("")));
        assert!(!p.format_is_defined(None));
        // …and it agrees with the prompt that is actually picked.
        assert_eq!(p.cleanup_for(Some("my-deleted-preset")), p.cleanup_for(None));
        assert_ne!(p.cleanup_for(Some("numbered")), p.cleanup_for(None));
    }

    #[test]
    fn defaults_when_nothing_set() {
        let p = Prompts::resolve(None, None, None, None, no_formats());
        assert_eq!(p.cleanup, DEFAULT_CLEANUP);
        assert_eq!(p.transform, DEFAULT_TRANSFORM);
        // The built-in format presets are available out of the box.
        assert_eq!(
            p.format_names(),
            vec![
                "bullets".to_string(),
                "code".to_string(),
                "email".to_string(),
                "numbered".to_string(),
            ]
        );
        assert!(p.cleanup_for(Some("numbered")).contains("numbered list"));
    }

    #[test]
    fn file_overrides_default() {
        let p = Prompts::resolve(
            None,
            Some("my cleanup".into()),
            None,
            Some("my transform".into()),
            no_formats(),
        );
        assert_eq!(p.cleanup, "my cleanup");
        assert_eq!(p.transform, "my transform");
    }

    #[test]
    fn env_beats_file() {
        let p = Prompts::resolve(
            Some("env cleanup".into()),
            Some("file cleanup".into()),
            Some("env transform".into()),
            Some("file transform".into()),
            no_formats(),
        );
        assert_eq!(p.cleanup, "env cleanup");
        assert_eq!(p.transform, "env transform");
    }

    #[test]
    fn blank_entries_fall_back_to_default() {
        // An empty or whitespace-only override must not blank the system prompt.
        let p = Prompts::resolve(
            Some("   ".into()),
            Some("".into()),
            None,
            Some("\n\t ".into()),
            no_formats(),
        );
        assert_eq!(p.cleanup, DEFAULT_CLEANUP);
        assert_eq!(p.transform, DEFAULT_TRANSFORM);
    }

    #[test]
    fn one_field_overridden_other_default() {
        // A partial file (only transform set) keeps the default cleanup.
        let p = Prompts::resolve(
            None,
            None,
            None,
            Some("only transform".into()),
            no_formats(),
        );
        assert_eq!(p.cleanup, DEFAULT_CLEANUP);
        assert_eq!(p.transform, "only transform");
    }

    #[test]
    fn custom_overrides_are_tracked_for_routing() {
        // A built-in preset is not custom; a file-supplied one is. The engine
        // needs this to tell a free-form instruction from a trained axis.
        let mut formats = HashMap::new();
        formats.insert("email".into(), "Write it as an email.".into());
        formats.insert("legal".into(), "Make it legalese.".into());
        let p = Prompts::resolve(
            None,
            Some("custom cleanup".into()),
            None,
            Some("custom transform".into()),
            formats,
        );
        assert!(p.cleanup_is_custom);
        assert!(p.transform_is_custom);
        assert!(p.is_custom_format("legal"));
        assert!(p.is_custom_format("EMAIL"), "case-insensitive");
        // Present only because it is built in.
        assert!(!p.is_custom_format("bullets"));
        assert!(!p.is_custom_format("numbered"));

        let d = Prompts::resolve(None, None, None, None, HashMap::new());
        assert!(!d.cleanup_is_custom);
        assert!(!d.transform_is_custom);
        assert!(!d.is_custom_format("bullets"));
        assert!(d.custom_formats.is_empty());
    }

    #[test]
    fn file_formats_overlay_builtin_defaults() {
        let mut formats = HashMap::new();
        formats.insert("Email".into(), "Write it as an email.".into()); // overrides default
        formats.insert("legal".into(), "Make it legalese.".into()); // adds a new preset
        formats.insert("blank".into(), "   ".into()); // dropped: blank value
        formats.insert("_note".into(), "ignored".into()); // dropped: comment key
        let p = Prompts::resolve(None, None, None, None, formats);

        // File key (case-insensitively) overrides the built-in default.
        assert_eq!(p.cleanup_for(Some("email")), "Write it as an email.");
        assert_eq!(p.cleanup_for(Some("EMAIL")), "Write it as an email.");
        // A new file preset is added alongside the built-ins.
        assert_eq!(p.cleanup_for(Some("legal")), "Make it legalese.");
        // Untouched built-ins still resolve to their preset (not the default).
        assert!(p.cleanup_for(Some("numbered")).contains("numbered list"));
        assert!(p.cleanup_for(Some("bullets")).contains("bulleted list"));
        // Unknown name / blank-valued file key / no preset → default cleanup.
        assert_eq!(p.cleanup_for(Some("nonexistent")), DEFAULT_CLEANUP);
        assert_eq!(p.cleanup_for(Some("blank")), DEFAULT_CLEANUP);
        assert_eq!(p.cleanup_for(None), DEFAULT_CLEANUP);
        // Built-ins + the added "legal" preset; blank/`_` keys filtered out.
        assert_eq!(
            p.format_names(),
            vec![
                "bullets".to_string(),
                "code".to_string(),
                "email".to_string(),
                "legal".to_string(),
                "numbered".to_string(),
            ]
        );
    }

    #[test]
    fn vocabulary_suffix_dedups_caps_and_skips_empty() {
        assert_eq!(vocabulary_suffix(&[]), None);
        assert_eq!(vocabulary_suffix(&["   ".to_string()]), None);

        let terms = vec![
            "macOS".to_string(),
            "macos".to_string(), // case-insensitive duplicate of macOS
            "Aoife".to_string(),
        ];
        let s = vocabulary_suffix(&terms).expect("non-empty");
        assert!(s.contains("macOS"));
        assert!(s.contains("Aoife"));
        // Duplicate folded away: "macOS" appears once, "macos" not at all.
        assert_eq!(s.matches("macOS").count(), 1);
        assert!(!s.contains("macos,") && !s.contains(", macos"));

        // Cap holds. Use a token ("Vocab") that never appears in the suffix
        // prose so the count reflects only the kept terms.
        let many: Vec<String> = (0..MAX_VOCAB_TERMS + 20)
            .map(|i| format!("Vocab{i}"))
            .collect();
        let s = vocabulary_suffix(&many).unwrap();
        assert_eq!(s.matches("Vocab").count(), MAX_VOCAB_TERMS);
    }

    #[test]
    fn shipped_example_file_parses_and_resolves() {
        // The real artifact users copy. Proves it's valid JSON, that the
        // documentation-only `_`-prefixed keys are ignored, and that both
        // fields flow through resolve unchanged.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/prompts.example.json");
        let body = std::fs::read_to_string(path).expect("read prompts.example.json");
        let file: PromptsFile =
            serde_json::from_str(&body).expect("prompts.example.json must parse into PromptsFile");

        let transform = file.transform.clone().expect("example sets transform");
        let cleanup = file.cleanup.clone().expect("example sets cleanup");
        assert!(transform.contains("Apply the user's instruction"));
        assert_eq!(cleanup, DEFAULT_CLEANUP);

        let p = Prompts::resolve(None, file.cleanup, None, file.transform, file.formats);
        assert_eq!(p.transform, transform);
        assert_eq!(p.cleanup, cleanup);
    }
}
