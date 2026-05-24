//! Voice commands that get stripped from injected text and turned into
//! actions (synthesized keystrokes), or that cancel the whole utterance.
//!
//! Two kinds, both designed to keep false positives near zero:
//!
//!   * **Trailing key commands** — a recognised phrase at the *very end* of
//!     the utterance (allowing trailing punctuation / whitespace) is stripped
//!     and turned into a keystroke run after the body is injected:
//!       - "press enter" / "press return" / "hit enter" / "new line" → Return
//!       - "new paragraph" → two Returns
//!       - "press tab" / "hit tab" → Tab
//!     The suffix is matched on a word boundary, so "compress enter" or
//!     "I want to press enter on the keyboard" do not fire.
//!
//!   * **Whole-utterance cancel** — when the *entire* utterance is exactly a
//!     cancel phrase ("scratch that", "never mind", "cancel that",
//!     "delete that"), nothing is injected. Requiring an exact match keeps
//!     "let me scratch that itch" from being swallowed.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrailingAction {
    None,
    /// Press Return once ("press enter", "new line", …).
    PressEnter,
    /// Press Return twice ("new paragraph").
    NewParagraph,
    /// Press Tab once ("press tab").
    PressTab,
    /// Discard the whole utterance — inject nothing ("scratch that", …).
    Cancel,
}

/// Phrases that, when they form the *entire* utterance, cancel it.
const CANCEL_PHRASES: &[&str] = &["scratch that", "cancel that", "never mind", "delete that"];

/// Trailing key-command phrases, longest first so "new paragraph" is tested
/// before "new line" would ever shadow it. Each maps to the action to run
/// after the body is injected.
const TRAILING_PHRASES: &[(&str, TrailingAction)] = &[
    ("press enter", TrailingAction::PressEnter),
    ("press return", TrailingAction::PressEnter),
    ("hit enter", TrailingAction::PressEnter),
    ("hit return", TrailingAction::PressEnter),
    ("new paragraph", TrailingAction::NewParagraph),
    ("new line", TrailingAction::PressEnter),
    ("newline", TrailingAction::PressEnter),
    ("press tab", TrailingAction::PressTab),
    ("hit tab", TrailingAction::PressTab),
];

/// Inspect cleaned text. If the whole thing is a cancel phrase, or it ends
/// with a known trailing command, return the body to inject (possibly empty)
/// plus the action for the daemon to execute.
pub fn parse_trailing_command(text: &str) -> (String, TrailingAction) {
    // Work on a lower-cased copy with trailing whitespace + sentence-final
    // punctuation stripped, so "...press enter." still matches. The body
    // returned to the caller keeps its OWN trailing punctuation — only the
    // command phrase + the separating whitespace is removed.
    let trimmed = text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '.' | ',' | '!' | '?' | ';' | ':')
    });
    let lower_trimmed = trimmed.to_lowercase();

    // Whole-utterance cancel: the entire utterance must BE a cancel phrase.
    if CANCEL_PHRASES.contains(&lower_trimmed.as_str()) {
        return (String::new(), TrailingAction::Cancel);
    }

    for (phrase, action) in TRAILING_PHRASES {
        if !lower_trimmed.ends_with(phrase) {
            continue;
        }
        let phrase_start = trimmed.len() - phrase.len();
        // Word boundary check so "compress enter" doesn't fire.
        let before = trimmed[..phrase_start].chars().last();
        let on_boundary = match before {
            None => true,
            Some(c) => !c.is_alphanumeric(),
        };
        if !on_boundary {
            continue;
        }
        // Strip only the separating whitespace between body and command,
        // not the body's own sentence-ending punctuation.
        let body = trimmed[..phrase_start].trim_end().to_string();
        return (body, action.clone());
    }
    (text.to_string(), TrailingAction::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_on_simple_suffix() {
        let (body, act) = parse_trailing_command("Send this press enter");
        assert_eq!(body, "Send this");
        assert_eq!(act, TrailingAction::PressEnter);
    }

    #[test]
    fn fires_with_trailing_period() {
        let (body, act) = parse_trailing_command("Send this. Press enter.");
        assert_eq!(body, "Send this.");
        assert_eq!(act, TrailingAction::PressEnter);
    }

    #[test]
    fn case_insensitive() {
        let (_, act) = parse_trailing_command("Send this PRESS ENTER");
        assert_eq!(act, TrailingAction::PressEnter);
    }

    #[test]
    fn does_not_fire_mid_sentence() {
        let (body, act) =
            parse_trailing_command("I want to press enter on the keyboard");
        assert_eq!(body, "I want to press enter on the keyboard");
        assert_eq!(act, TrailingAction::None);
    }

    #[test]
    fn does_not_fire_on_non_word_boundary() {
        // "compress enter" should NOT match (no boundary before "press").
        let (body, act) = parse_trailing_command("compress enter");
        assert_eq!(act, TrailingAction::None);
        assert_eq!(body, "compress enter");
    }

    #[test]
    fn accepts_press_return() {
        let (_, act) = parse_trailing_command("send press return");
        assert_eq!(act, TrailingAction::PressEnter);
    }

    #[test]
    fn empty_body_is_handled() {
        let (body, act) = parse_trailing_command("press enter");
        assert_eq!(body, "");
        assert_eq!(act, TrailingAction::PressEnter);
    }

    #[test]
    fn new_line_maps_to_press_enter() {
        let (body, act) = parse_trailing_command("first item new line");
        assert_eq!(body, "first item");
        assert_eq!(act, TrailingAction::PressEnter);
    }

    #[test]
    fn new_paragraph_is_distinct_from_new_line() {
        let (body, act) = parse_trailing_command("end of section. New paragraph");
        assert_eq!(body, "end of section.");
        assert_eq!(act, TrailingAction::NewParagraph);
    }

    #[test]
    fn press_tab_fires() {
        let (body, act) = parse_trailing_command("username press tab");
        assert_eq!(body, "username");
        assert_eq!(act, TrailingAction::PressTab);
    }

    #[test]
    fn cancel_requires_whole_utterance() {
        let (body, act) = parse_trailing_command("Scratch that.");
        assert_eq!(act, TrailingAction::Cancel);
        assert_eq!(body, "");
    }

    #[test]
    fn cancel_does_not_fire_mid_sentence() {
        // A cancel word inside a real sentence must pass through untouched.
        let (body, act) = parse_trailing_command("let me scratch that itch");
        assert_eq!(act, TrailingAction::None);
        assert_eq!(body, "let me scratch that itch");
    }

    #[test]
    fn never_mind_cancels() {
        let (_, act) = parse_trailing_command("never mind");
        assert_eq!(act, TrailingAction::Cancel);
    }
}
