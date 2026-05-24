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
use std::path::PathBuf;

/// Default cleanup prompt: strip disfluencies, fix punctuation, expand
/// colloquial contractions, preserve domain casing, never paraphrase. Lives
/// here (not in `cleaner.rs`) so all prompt text is in one place.
pub const DEFAULT_CLEANUP: &str =
    "You are a real-time dictation cleaner. Edit raw speech-to-text for \
     written-text readability:\n\
     - Remove disfluencies: uh, um, er, ah, hmm; 'like' only when used \
       as filler; 'you know' only when used as filler.\n\
     - Expand colloquial contractions to their written form: \
       wanna → want to, gonna → going to, kinda → kind of, sorta → sort of, \
       gimme → give me, lemme → let me, dunno → don't know, outta → out of, \
       lotta → lot of, shoulda → should have, coulda → could have, \
       woulda → would have, y'all → you all.\n\
     - KEEP standard contractions as-is: don't, it's, won't, can't, I'm, \
       I'll, we're, etc. — do not over-formalize.\n\
     - Fix punctuation and capitalization of sentence boundaries.\n\
     - Preserve domain-specific casing when it appears: Rust, macOS, ONNX, \
       GitHub, Parakeet, iOS, Apple Silicon, etc.\n\
     - CRITICAL: Do NOT substitute common words for proper nouns based on \
       guessed context. If the speaker says 'main', keep 'main' — do not \
       change to 'Maine'. If they say 'mark', keep 'mark' — do not change \
       to 'Mark'. Tech terms like 'git', 'npm', 'cd', 'ls', 'main', \
       'master', 'dev', 'prod', 'api' stay lowercase as written. Only \
       capitalize when the word is unambiguously a proper noun in context.\n\
     - Do NOT rephrase, paraphrase, summarize, or add words the speaker \
       didn't say. Preserve the speaker's voice and word choice.\n\
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
     rephrasing, expanding, or restructuring content. Do not minimize your \
     changes: if the instruction asks for more items, more detail, or a longer \
     result, produce it. Only keep the original wording and detail where the \
     instruction does not ask you to change them. If the instruction is a \
     translation, output only the translation. Return ONLY the resulting text \
     — no preamble, no explanation, no quotes, no markdown fences, no \
     commentary.";

/// The resolved prompts handed to the cleanup engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompts {
    pub cleanup: String,
    pub transform: String,
}

/// On-disk shape of `prompts.json`. Both fields optional; unknown keys (e.g. a
/// leading `_comment`) are ignored by serde.
#[derive(Debug, Default, Deserialize)]
struct PromptsFile {
    #[serde(default)]
    cleanup: Option<String>,
    #[serde(default)]
    transform: Option<String>,
}

impl Prompts {
    /// `~/.config/local-dictation/prompts.json`, or `DICTATE_PROMPTS_PATH` when
    /// set. `None` only if neither `DICTATE_PROMPTS_PATH` nor `$HOME` is set.
    pub fn config_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("DICTATE_PROMPTS_PATH") {
            return Some(PathBuf::from(p));
        }
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join(".config")
                .join("local-dictation")
                .join("prompts.json"),
        )
    }

    /// Load prompts, applying env > file > default. Never errors: a missing or
    /// corrupt file degrades to defaults (a parse error is logged, not fatal).
    pub fn load() -> Self {
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

        let resolved = Self::resolve(
            std::env::var("DICTATE_CLEANUP_PROMPT").ok(),
            file.cleanup,
            std::env::var("DICTATE_TRANSFORM_PROMPT").ok(),
            file.transform,
        );

        // Tell the user which prompts are customised — useful when testing a
        // tweaked prompt (confirms the file actually took effect).
        let src = |s: &str, default: &str| if s == default { "default" } else { "custom" };
        eprintln!(
            "[boot] prompts     cleanup={} · transform={}",
            src(&resolved.cleanup, DEFAULT_CLEANUP),
            src(&resolved.transform, DEFAULT_TRANSFORM),
        );
        resolved
    }

    /// Pure resolution so the precedence is unit-testable without touching real
    /// env vars or `$HOME`. For each prompt: first non-blank of (env, file),
    /// else the built-in default. Blank/whitespace entries are treated as unset
    /// so an empty field falls back rather than sending an empty system prompt.
    fn resolve(
        env_cleanup: Option<String>,
        file_cleanup: Option<String>,
        env_transform: Option<String>,
        file_transform: Option<String>,
    ) -> Self {
        Self {
            cleanup: pick(env_cleanup, file_cleanup, DEFAULT_CLEANUP),
            transform: pick(env_transform, file_transform, DEFAULT_TRANSFORM),
        }
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

    #[test]
    fn defaults_when_nothing_set() {
        let p = Prompts::resolve(None, None, None, None);
        assert_eq!(p.cleanup, DEFAULT_CLEANUP);
        assert_eq!(p.transform, DEFAULT_TRANSFORM);
    }

    #[test]
    fn file_overrides_default() {
        let p = Prompts::resolve(
            None,
            Some("my cleanup".into()),
            None,
            Some("my transform".into()),
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
        );
        assert_eq!(p.cleanup, DEFAULT_CLEANUP);
        assert_eq!(p.transform, DEFAULT_TRANSFORM);
    }

    #[test]
    fn one_field_overridden_other_default() {
        // A partial file (only transform set) keeps the default cleanup.
        let p = Prompts::resolve(None, None, None, Some("only transform".into()));
        assert_eq!(p.cleanup, DEFAULT_CLEANUP);
        assert_eq!(p.transform, "only transform");
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

        let p = Prompts::resolve(None, file.cleanup, None, file.transform);
        assert_eq!(p.transform, transform);
        assert_eq!(p.cleanup, cleanup);
    }
}
