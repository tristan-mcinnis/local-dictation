//! Context-aware padding & capitalization for injected dictation text.
//!
//! Given the character immediately before the caret and the character
//! immediately after, decide whether to prepend a space, append a space, and
//! whether to capitalize the first letter.
//!
//! Pure & unit-tested. The injector wraps this around the AX read of the
//! focused element's value + selected range.

/// Render `text` for injection at a caret with the given surrounding chars.
/// `char_before` is the last non-whitespace char *to the left* of the caret
/// (walking past intervening whitespace), or None at start of field.
/// `immediate_before` and `char_after` are the characters right at the caret
/// boundary (used to decide whitespace insertion).
pub fn smart_pad(
    text: &str,
    immediate_before: Option<char>,
    last_non_ws_before: Option<char>,
    char_after: Option<char>,
) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let needs_leading_space = match immediate_before {
        None => false,
        Some(c) if c.is_whitespace() => false,
        // After opening punctuation, don't add a space.
        Some('(') | Some('[') | Some('{') | Some('"') | Some('\'')
        | Some('“') | Some('‘') | Some('—') => false,
        Some(_) => true,
    };

    let should_capitalize = match last_non_ws_before {
        // Start of field: capitalize.
        None => true,
        // Mid-sentence after these terminators: capitalize.
        Some('.') | Some('!') | Some('?') => true,
        // After opening quote at start ("|hello world") — don't force.
        _ => false,
    };

    let needs_trailing_space = match char_after {
        None => false,
        Some(c) if c.is_whitespace() => false,
        // Don't pad before closing punctuation.
        Some('.') | Some(',') | Some(';') | Some(':') | Some('!') | Some('?')
        | Some(')') | Some(']') | Some('}') | Some('"') | Some('\'')
        | Some('”') | Some('’') => false,
        Some(_) => true,
    };

    let mut out = String::with_capacity(trimmed.len() + 2);
    if needs_leading_space {
        out.push(' ');
    }
    let mut chars = trimmed.chars();
    if let Some(first) = chars.next() {
        if should_capitalize {
            for u in first.to_uppercase() {
                out.push(u);
            }
        } else {
            out.push(first);
        }
        out.extend(chars);
    }
    if needs_trailing_space {
        out.push(' ');
    }
    out
}

/// Look back from `cursor` (a char index, NOT byte index) through any
/// whitespace and return the first non-whitespace char to the left.
pub fn last_non_ws_before(value: &str, cursor_chars: usize) -> Option<char> {
    value
        .chars()
        .take(cursor_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .find(|c| !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_start_of_empty_field_capitalizes_no_space() {
        assert_eq!(
            smart_pad("hello world", None, None, None),
            "Hello world"
        );
    }

    #[test]
    fn after_period_space_capitalizes_no_extra_space() {
        // "Hello. |"  cursor right after the space → capitalize, no leading space
        assert_eq!(
            smart_pad("world", Some(' '), Some('.'), None),
            "World"
        );
    }

    #[test]
    fn mid_word_pads_both_sides() {
        // "abc|def" — cursor in middle of a word (rare but possible).
        // Treat dictation as a word, so split: "abc xyz def" rather than
        // mashing "abcxyzdef".
        assert_eq!(
            smart_pad("xyz", Some('c'), Some('c'), Some('d')),
            " xyz "
        );
    }

    #[test]
    fn after_word_with_no_space_adds_leading_space() {
        // "Hello|" cursor right after 'o' with no space → add space before
        assert_eq!(
            smart_pad("world", Some('o'), Some('o'), None),
            " world"
        );
    }

    #[test]
    fn after_opening_paren_no_leading_space() {
        assert_eq!(
            smart_pad("hello", Some('('), Some('('), None),
            "hello"
        );
    }

    #[test]
    fn before_closing_punctuation_no_trailing_space() {
        // "say |." — cursor before period, don't add trailing space
        assert_eq!(
            smart_pad("hi", Some(' '), Some('y'), Some('.')),
            "hi"
        );
    }

    #[test]
    fn before_word_adds_trailing_space() {
        // "I said |world" → "I said hi world"
        assert_eq!(
            smart_pad("hi", Some(' '), Some('d'), Some('w')),
            "hi "
        );
    }

    #[test]
    fn full_sandwich_word_in_middle_of_sentence() {
        // "The cat |sat on" → cursor between two words, both space-padded
        // immediate_before is ' ', char_after is 's' (word).
        assert_eq!(
            smart_pad("quickly", Some(' '), Some('t'), Some('s')),
            "quickly "
        );
    }

    #[test]
    fn last_non_ws_walks_past_spaces() {
        assert_eq!(last_non_ws_before("Hello.  ", 8), Some('.'));
        assert_eq!(last_non_ws_before("", 0), None);
        assert_eq!(last_non_ws_before("   ", 3), None);
    }

    #[test]
    fn strips_caller_provided_whitespace() {
        // The caller may pass text with leading/trailing whitespace from the
        // cleaner; smart_pad should normalize and then re-pad itself.
        assert_eq!(
            smart_pad("  hello  ", Some(' '), Some('.'), None),
            "Hello"
        );
    }
}
