//! Trailing voice commands that get stripped from injected text and
//! turned into actions (synthesized keystrokes).
//!
//! Currently supports:
//!   * "press enter" / "press return" / "hit enter" → posts a Return key
//!     after injecting the rest of the text.
//!
//! Strict suffix match — the command must be at the very end of the
//! cleaned transcript (allowing trailing punctuation / whitespace), so
//! sentences like "I want to press enter on the keyboard" don't trigger.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrailingAction {
    None,
    PressEnter,
}

/// Inspect cleaned text. If it ends with a known voice command, strip the
/// command (and only its own trailing punctuation) and return the body +
/// the action for the daemon to execute.
pub fn parse_trailing_command(text: &str) -> (String, TrailingAction) {
    // The match works on a lower-cased copy that has trailing whitespace +
    // sentence-final punctuation stripped, so "...press enter." still
    // matches. The body returned to the caller, though, keeps its OWN
    // trailing punctuation — only the command phrase + the body/command
    // separator (whitespace) is removed.
    let trimmed = text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '.' | ',' | '!' | '?' | ';' | ':')
    });
    let lower_trimmed = trimmed.to_lowercase();

    for phrase in &["press enter", "press return", "hit enter", "hit return"] {
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
        return (body, TrailingAction::PressEnter);
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
}
