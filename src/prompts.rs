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
use std::collections::HashMap;
use std::path::PathBuf;

/// Default cleanup prompt: strip disfluencies, fix punctuation, expand
/// colloquial contractions, preserve domain casing, never paraphrase. Lives
/// here (not in `cleaner.rs`) so all prompt text is in one place.
pub const DEFAULT_CLEANUP: &str =
    "You are a real-time dictation cleaner. Turn raw speech-to-text into clean \
     written text.\n\
     PRIME DIRECTIVE: output the speaker's OWN words. You only fix mechanics — \
     you never improve, reword, or restate. If the raw text is already \
     grammatical, the only changes you make are removing filler, fixing \
     punctuation, and fixing casing.\n\
     Make ONLY these edits:\n\
     - Remove ALL filler and disfluencies wherever they occur, INCLUDING at \
       the very start of the text: uh, um, er, ah, hmm; a leading 'so', \
       'okay', 'well', 'yeah', or 'right' used only as throat-clearing; \
       'like' and 'you know' when used as filler.\n\
     - Remove false starts and stray repeated words ('the the' → 'the').\n\
     - Expand colloquial contractions: wanna → want to, gonna → going to, \
       gotta → got to, kinda → kind of, sorta → sort of, gimme → give me, \
       lemme → let me, dunno → don't know, outta → out of, lotta → lot of, \
       shoulda → should have, coulda → could have, woulda → would have, \
       y'all → you all.\n\
     - KEEP standard contractions as-is: don't, it's, won't, can't, I'm, \
       I'll, we're.\n\
     - Add sentence punctuation, capitalize the first letter of each \
       sentence, and ALWAYS capitalize the standalone pronoun 'I'.\n\
     - Preserve domain casing when present: Rust, macOS, ONNX, GitHub, \
       Parakeet, iOS, Apple Silicon.\n\
     - CRITICAL: never swap a common word for a similar-sounding proper \
       noun. 'main' stays 'main' (not 'Maine'); 'mark' stays 'mark'. Tech \
       tokens like git, npm, cd, ls, main, master, dev, prod, api stay \
       lowercase exactly as written. Only capitalize a word that is \
       unambiguously a proper noun.\n\
     FORBIDDEN (these change intent and are never allowed): dropping or \
     omitting any clause, sentence, or phrase the speaker said — keep EVERY \
     one, even a greeting, an aside, or a line that seems like unimportant \
     small talk (e.g. 'just blocking your time to talk about X' must survive \
     in full); changing verb tense, mood, or word order to 'read better'; \
     replacing any word with a synonym; merging or splitting the speaker's \
     sentences for style; completing a half-finished thought; answering a \
     question instead of transcribing it; summarizing, translating, or adding \
     anything not spoken; replacing an unusual or unexpected word with a more \
     common one you assume the speaker 'meant' (transcribe the ACTUAL words — \
     'open pencil' stays 'open pencil', never 'open source'; 'Saoirse' is not \
     'Seamus'). When in doubt, keep the original words exactly.\n\
     Output ONLY the cleaned text — no preamble, no commentary, no quotes, \
     no markdown.";

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
/// defaults rather than a locked set. Tuned against Gemma 3 1B in
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

/// Build a "known vocabulary" suffix for the cleanup system prompt from the
/// user's personal vocabulary (the target spellings in `corrections.json`).
/// Returns `None` when there's nothing worth adding.
///
/// This is the dictionary-aware-cleanup borrow: telling the model the user's
/// proper nouns, names, and domain casing up front stops it from "correcting"
/// them away (lowercasing `macOS`, turning `Lingzi` into `Lindsay`) — a
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
    /// in place of `cleanup` when that preset is active. Empty unless the user
    /// defines a `formats` object in `prompts.json`.
    pub formats: HashMap<String, String>,
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
            .and_then(|(p, body)| match serde_json::from_str::<PromptsFile>(&body) {
                Ok(f) => Some(f),
                Err(e) => {
                    eprintln!("[warn] prompts parse failed ({}): {e}", p.display());
                    None
                }
            })
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
        let mut formats = default_formats();
        for (k, v) in file_formats {
            if k.starts_with('_') || v.trim().is_empty() {
                continue;
            }
            let k = k.trim().to_lowercase();
            if !k.is_empty() {
                formats.insert(k, v);
            }
        }
        Self {
            cleanup: pick(env_cleanup, file_cleanup, DEFAULT_CLEANUP),
            transform: pick(env_transform, file_transform, DEFAULT_TRANSFORM),
            formats,
        }
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

    /// Sorted list of defined preset names, for the menu-bar picker.
    pub fn format_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.formats.keys().cloned().collect();
        names.sort_unstable();
        names
    }
}

/// First non-blank of `env`, then `file`, else `default`.
fn pick(env: Option<String>, file: Option<String>, default: &str) -> String {
    [env, file]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_formats() -> HashMap<String, String> {
        HashMap::new()
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
        let p = Prompts::resolve(None, None, None, Some("only transform".into()), no_formats());
        assert_eq!(p.cleanup, DEFAULT_CLEANUP);
        assert_eq!(p.transform, "only transform");
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
            "Lingzi".to_string(),
        ];
        let s = vocabulary_suffix(&terms).expect("non-empty");
        assert!(s.contains("macOS"));
        assert!(s.contains("Lingzi"));
        // Duplicate folded away: "macOS" appears once, "macos" not at all.
        assert_eq!(s.matches("macOS").count(), 1);
        assert!(!s.contains("macos,") && !s.contains(", macos"));

        // Cap holds. Use a token ("Vocab") that never appears in the suffix
        // prose so the count reflects only the kept terms.
        let many: Vec<String> = (0..MAX_VOCAB_TERMS + 20).map(|i| format!("Vocab{i}")).collect();
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
        assert!(cleanup.contains("dictation cleaner"));

        let p = Prompts::resolve(None, file.cleanup, None, file.transform, file.formats);
        assert_eq!(p.transform, transform);
        assert_eq!(p.cleanup, cleanup);
    }
}
