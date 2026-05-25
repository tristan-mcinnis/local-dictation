//! Dictation refinement — the deterministic tail of the pipeline.
//!
//! Once the raw transcript has been (optionally) run through the LLM cleaner,
//! the same two transforms always follow, in the same order, for every entry
//! point that injects text:
//!
//!   1. **Corrections** — apply the user's personal substitution dictionary
//!      (proper nouns, domain casing, common mis-transcriptions).
//!   2. **Trailing voice command** — strip a recognised suffix like
//!      "press enter" and turn it into an action the caller executes after
//!      injecting the body.
//!
//! The ordering is load-bearing: corrections run **before** the command parse
//! so that a correction can never rewrite (and accidentally fire, or destroy)
//! the trigger phrase. Previously this sequence — plus the empty-string
//! handling around it — was inlined in the daemon's event loop and only
//! *partially* re-implemented by the `bench`/`dictate` CLI subcommands (they
//! skipped corrections and voice commands entirely). Concentrating it here
//! gives every caller identical behaviour and makes the whole transform
//! unit-testable with no audio, models, or AX.
//!
//! Cleanup itself (the async, feature-gated Gemma pass) stays with the caller
//! because it owns the heavy engine; the contract is simply that `refine` is
//! handed the *cleaned* text.

use crate::corrections::Corrections;
use crate::voice_commands::{parse_trailing_command, TrailingAction};

/// The result of refining one utterance: the body to inject plus any trailing
/// action to perform afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinedDictation {
    /// Text to inject at the cursor (may be empty — see `is_empty`).
    pub text: String,
    /// Action to run after injection, e.g. synthesize a Return keystroke.
    pub action: TrailingAction,
}

impl RefinedDictation {
    /// True when there is no body to inject (after trimming).
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// True when the only thing to do is synthesize a keystroke — no body text.
    /// (e.g. the user said just "press enter" or "new paragraph".) `Cancel` is
    /// deliberately excluded: it means "do nothing", not "press a key".
    pub fn is_bare_action(&self) -> bool {
        self.is_empty()
            && matches!(
                self.action,
                TrailingAction::PressEnter
                    | TrailingAction::NewParagraph
                    | TrailingAction::PressTab
                    | TrailingAction::PressEscape
                    | TrailingAction::Undo
            )
    }

    /// Collapse this refined dictation into the single terminal decision the
    /// caller should act on. This is the four-way branch the daemon used to
    /// open-code (cancel → discard, bare command → keystroke, empty → skip,
    /// else inject); concentrating it here makes it unit-testable without
    /// audio, models, or AX, and keeps the daemon's hot path a flat `match`.
    pub fn outcome(self) -> DictationOutcome {
        if matches!(self.action, TrailingAction::Cancel) {
            // "scratch that" — inject nothing AND synthesize no key.
            return DictationOutcome::Discard;
        }
        if self.is_bare_action() {
            return DictationOutcome::BareAction(self.action);
        }
        if self.is_empty() {
            return DictationOutcome::Skip;
        }
        DictationOutcome::Inject {
            text: self.text,
            action: self.action,
        }
    }
}

/// The terminal decision for one refined utterance — what the caller actually
/// does with it. `Discard` (a cancel phrase) and `Skip` (cleaned to nothing)
/// both inject nothing, but stay distinct so callers can log/observe them
/// differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictationOutcome {
    /// User cancelled ("scratch that"): inject nothing, press no key.
    Discard,
    /// No body, just a keystroke to synthesize ("press enter" on its own).
    BareAction(TrailingAction),
    /// Inject `text`, then run `action` afterwards (`action` may be `None`).
    Inject { text: String, action: TrailingAction },
    /// Nothing left to do — empty body with no action.
    Skip,
}

/// Applies corrections then voice-command parsing to cleaned transcripts.
/// Holds the corrections dictionary so callers build it once at startup.
pub struct Refiner {
    corrections: Corrections,
}

impl Refiner {
    pub fn new(corrections: Corrections) -> Self {
        Self { corrections }
    }

    /// Refine already-cleaned text into a body + trailing action.
    ///
    /// Corrections are applied first so they cannot alter the voice-command
    /// trigger phrase; the command parse then runs on the corrected text.
    pub fn refine(&self, cleaned_text: &str) -> RefinedDictation {
        let corrected = self.corrections.apply(cleaned_text);
        let (text, action) = parse_trailing_command(&corrected);
        RefinedDictation { text, action }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn refiner(pairs: &[(&str, &str)]) -> Refiner {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.to_string()))
            .collect();
        Refiner::new(Corrections::from_map(map))
    }

    #[test]
    fn corrections_then_command_compose() {
        let r = refiner(&[("teh", "the")]);
        let out = r.refine("teh cat press enter");
        assert_eq!(out.text, "the cat");
        assert_eq!(out.action, TrailingAction::PressEnter);
        assert!(!out.is_empty());
        assert!(!out.is_bare_action());
    }

    #[test]
    fn corrections_run_before_command_parse() {
        // The correction fixes a word in the body; the trailing command still
        // parses off the corrected text, not the raw input.
        let r = refiner(&[("postgres", "Postgres")]);
        let out = r.refine("restart postgres press return");
        assert_eq!(out.text, "restart Postgres");
        assert_eq!(out.action, TrailingAction::PressEnter);
    }

    #[test]
    fn no_command_is_passthrough_with_corrections() {
        let r = refiner(&[("macos", "macOS")]);
        let out = r.refine("ship the macos build");
        assert_eq!(out.text, "ship the macOS build");
        assert_eq!(out.action, TrailingAction::None);
    }

    #[test]
    fn bare_command_has_empty_body() {
        let r = refiner(&[]);
        let out = r.refine("press enter");
        assert!(out.is_empty());
        assert!(out.is_bare_action());
        assert_eq!(out.action, TrailingAction::PressEnter);
    }

    #[test]
    fn empty_input_is_empty_output() {
        let r = refiner(&[]);
        let out = r.refine("   ");
        assert!(out.is_empty());
        assert!(!out.is_bare_action());
        assert_eq!(out.action, TrailingAction::None);
    }

    #[test]
    fn cancel_is_empty_but_not_a_bare_action() {
        // "scratch that" must inject nothing AND must not synthesize a key.
        let r = refiner(&[]);
        let out = r.refine("scratch that");
        assert!(out.is_empty());
        assert!(!out.is_bare_action());
        assert_eq!(out.action, TrailingAction::Cancel);
    }

    #[test]
    fn bare_new_paragraph_is_a_bare_action() {
        let r = refiner(&[]);
        let out = r.refine("new paragraph");
        assert!(out.is_empty());
        assert!(out.is_bare_action());
        assert_eq!(out.action, TrailingAction::NewParagraph);
    }

    #[test]
    fn outcome_inject_carries_body_and_action() {
        let r = refiner(&[("teh", "the")]);
        let out = r.refine("teh cat press enter").outcome();
        assert_eq!(
            out,
            DictationOutcome::Inject {
                text: "the cat".to_string(),
                action: TrailingAction::PressEnter,
            }
        );
    }

    #[test]
    fn outcome_inject_with_no_trailing_action() {
        let r = refiner(&[]);
        let out = r.refine("just some text").outcome();
        assert_eq!(
            out,
            DictationOutcome::Inject {
                text: "just some text".to_string(),
                action: TrailingAction::None,
            }
        );
    }

    #[test]
    fn outcome_cancel_is_discard() {
        let r = refiner(&[]);
        assert_eq!(r.refine("scratch that").outcome(), DictationOutcome::Discard);
    }

    #[test]
    fn outcome_bare_command_is_bare_action() {
        let r = refiner(&[]);
        assert_eq!(
            r.refine("press tab").outcome(),
            DictationOutcome::BareAction(TrailingAction::PressTab)
        );
    }

    #[test]
    fn outcome_empty_is_skip() {
        let r = refiner(&[]);
        assert_eq!(r.refine("   ").outcome(), DictationOutcome::Skip);
    }
}
