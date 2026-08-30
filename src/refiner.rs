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
//! Between corrections and the command parse, two deterministic artifact
//! guards fire: a **trailing-artifact strip** (an ASR-invented closing
//! pleasantry like "…as expected. Thank you.", hallucinated from the room
//! tone tail-capture adds past release) and a whole-transcript **misfire
//! artifact** drop ("Thank you." from a near-silent accidental key press).
//! Both yield empty body + no action → `Skip` when nothing real remains, so a
//! misfire never lands text in the focused app.
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

/// Whole-transcript ASR silence artifacts. Near-silent audio (an accidental
/// key press) is hallucinated by the ASR into one of these stock phrases —
/// the classic being "Thank you." on a sub-second misfire. When the ENTIRE
/// transcript normalizes to one of these, it is dropped: inject nothing.
/// Deliberately excludes plausible one-word messages ("yes", "no", "okay",
/// "hi") — those survive.
const MISFIRE_ARTIFACTS: &[&str] = &[
    "thank you",
    "thanks",
    "thank you for watching",
    "thanks for watching",
    "thank you for listening",
    "thanks for listening",
    "thank you so much",
    "thank you very much",
    "bye",
    "goodbye",
    "bye bye",
    "you",
    "the end",
    "so",
    "music",
    "applause",
    "laughter",
    "silence",
    "subtitles",
    "hmm",
    "huh",
    "oh",
    "ah",
    "um",
    "uh",
    "mhm",
    "mm hmm",
];

/// Artifact phrases stripped when they appear at the END of an otherwise real
/// utterance. Tail-capture keeps the mic open past release and the ASR renders
/// that trailing room tone as a closing pleasantry ("…working as expected.
/// Thank you."). Deliberately narrower than [`MISFIRE_ARTIFACTS`]: single
/// words like "you"/"so" are too dangerous to strip off sentence tails.
const TRAILING_ARTIFACTS: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "thank you for listening",
    "thanks for listening",
    "thank you so much",
    "thank you very much",
    "thank you",
    "thanks",
    "goodbye",
    "bye bye",
    "bye",
    "the end",
];

/// Strip trailing artifact phrases (up to 3 repeats) off the end of `text`,
/// fixing a dangling comma left behind ("…expected, thank you" → "…expected.").
/// Word-boundary aware: matches whole words at the text tail only.
pub fn strip_trailing_artifact(text: &str) -> String {
    let mut out = text.trim_end().to_string();
    for _ in 0..3 {
        // Word spans (alphanumeric runs) over the current text.
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut start: Option<usize> = None;
        for (i, c) in out.char_indices() {
            if c.is_alphanumeric() {
                start.get_or_insert(i);
            } else if let Some(s) = start.take() {
                spans.push((s, i));
            }
        }
        if let Some(s) = start {
            spans.push((s, out.len()));
        }
        if spans.is_empty() {
            break;
        }
        // Try each artifact as a suffix of the words (multi-word first).
        let words: Vec<String> = spans
            .iter()
            .map(|&(s, e)| out[s..e].to_ascii_lowercase())
            .collect();
        let mut cut: Option<usize> = None;
        for artifact in TRAILING_ARTIFACTS {
            let aw: Vec<&str> = artifact.split_whitespace().collect();
            if aw.len() > words.len() {
                continue;
            }
            let tail = &words[words.len() - aw.len()..];
            if tail.iter().map(String::as_str).eq(aw.iter().copied()) {
                cut = Some(spans[words.len() - aw.len()].0);
                break;
            }
        }
        let Some(cut) = cut else { break };
        out = out[..cut].trim_end().to_string();
        // "…expected," after losing "thank you" reads as a truncated list —
        // close the sentence instead.
        if out.ends_with(',') {
            out.pop();
            out.push('.');
        }
    }
    out
}

/// True when the whole transcript is an ASR silence artifact (misfire).
/// Normalization: lowercase, punctuation → space, whitespace collapsed.
pub fn is_misfire_artifact(text: &str) -> bool {
    let normalized: String = text
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    let collapsed = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    !collapsed.is_empty() && MISFIRE_ARTIFACTS.contains(&collapsed.as_str())
}

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
    Inject {
        text: String,
        action: TrailingAction,
    },
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

    /// Apply only the corrections dictionary, without voice-command parsing.
    /// Used to fix known proper nouns in the *raw* transcript before LLM
    /// cleanup, so the small model sees the intended spelling (e.g. "to twist"
    /// → "Todoist") instead of mangling a garbled one. The post-cleanup
    /// [`Self::refine`] pass runs corrections again — idempotent for these
    /// proper-noun fixes (the target spelling isn't itself a correction key).
    pub fn apply_corrections(&self, text: &str) -> String {
        self.corrections.apply(text)
    }

    /// Refine already-cleaned text into a body + trailing action.
    ///
    /// Corrections are applied first so they cannot alter the voice-command
    /// trigger phrase; the command parse then runs on the corrected text.
    pub fn refine(&self, cleaned_text: &str) -> RefinedDictation {
        let corrected = self.corrections.apply(cleaned_text);
        // Strip an ASR-invented closing pleasantry ("…as expected. Thank you.")
        // before anything else sees it.
        let corrected = strip_trailing_artifact(&corrected);
        if is_misfire_artifact(&corrected) {
            // Accidental-press hallucination: inject nothing, press no key.
            return RefinedDictation {
                text: String::new(),
                action: TrailingAction::None,
            };
        }
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
        assert_eq!(
            r.refine("scratch that").outcome(),
            DictationOutcome::Discard
        );
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

    #[test]
    fn misfire_thank_you_is_dropped() {
        let r = refiner(&[]);
        assert_eq!(r.refine("Thank you.").outcome(), DictationOutcome::Skip);
        assert_eq!(r.refine("thank you").outcome(), DictationOutcome::Skip);
        assert_eq!(r.refine("  THANK   YOU ! ").outcome(), DictationOutcome::Skip);
        assert_eq!(r.refine("Thanks for watching!").outcome(), DictationOutcome::Skip);
        assert_eq!(r.refine("Bye.").outcome(), DictationOutcome::Skip);
    }

    #[test]
    fn trailing_artifact_stripped_off_real_text() {
        let r = refiner(&[]);
        // The exact pattern from the logs: tail-capture room tone rendered as a
        // closing "Thank you." appended to real dictation.
        match r.refine("It did it again. Thank you.").outcome() {
            DictationOutcome::Inject { text, .. } => assert_eq!(text, "It did it again."),
            other => panic!("expected inject, got {other:?}"),
        }
        match r.refine("Have you gotten stuck? Thank you.").outcome() {
            DictationOutcome::Inject { text, .. } => assert_eq!(text, "Have you gotten stuck?"),
            other => panic!("expected inject, got {other:?}"),
        }
        // Dangling comma closes as a sentence.
        assert_eq!(
            strip_trailing_artifact("make sure it works, thank you"),
            "make sure it works."
        );
        // Repeated artifact ("Thank you. Thank you.") collapses to nothing.
        assert_eq!(
            r.refine("Thank you. Thank you.").outcome(),
            DictationOutcome::Skip
        );
    }

    #[test]
    fn trailing_artifact_never_touches_safe_tails() {
        // "you"/"so" tails are real language — not in the trailing list.
        assert_eq!(strip_trailing_artifact("I love you"), "I love you");
        assert_eq!(strip_trailing_artifact("and so"), "and so");
        // Mid-text artifacts survive (not a tail).
        assert_eq!(
            strip_trailing_artifact("say thank you to the team"),
            "say thank you to the team"
        );
        // A bare misfire "Thank you." is still dropped by the whole-transcript
        // path even after the strip empties it.
        let r = refiner(&[]);
        assert_eq!(r.refine("Thank you.").outcome(), DictationOutcome::Skip);
    }

    #[test]
    fn misfire_never_fires_inside_real_text() {
        let r = refiner(&[]);
        // Artifact phrase inside a real utterance survives.
        let out = r.refine("say thank you to the team").outcome();
        assert!(matches!(out, DictationOutcome::Inject { .. }));
        let out = r.refine("thank you for the update").outcome();
        assert!(matches!(out, DictationOutcome::Inject { .. }));
        // Plausible one-word messages are NOT artifacts.
        for word in ["yes", "no", "okay", "hi", "sure", "done"] {
            let out = r.refine(word).outcome();
            assert!(matches!(out, DictationOutcome::Inject { .. }), "{word} must survive");
        }
    }
}
