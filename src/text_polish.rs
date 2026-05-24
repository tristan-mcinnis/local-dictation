//! Defensive post-pass on cleaner output.
//!
//! Even with "output ONLY the cleaned text" hammered into the system prompt,
//! small LLMs occasionally leak a preamble ("Here's the cleaned text:"),
//! wrap output in quotes, emit a markdown bold/italic, or end with chat
//! template artefacts. None of those should hit the user's text field.
//!
//! Pure, no I/O — easy to unit-test.

const PREAMBLE_PREFIXES: &[&str] = &[
    "here's the cleaned text:",
    "here is the cleaned text:",
    "here's the cleaned version:",
    "here is the cleaned version:",
    "here's the cleaned transcript:",
    "here is the cleaned transcript:",
    "cleaned text:",
    "cleaned transcript:",
    "polished text:",
    "sure, here's",
    "sure, here is",
    "sure!",
    "okay,",
    "ok,",
];

const TRAILING_ARTEFACTS: &[&str] = &[
    "<end_of_turn>",
    "<eos>",
    "</s>",
    "<|im_end|>",
    "<|endoftext|>",
];

/// Strip LLM artefacts, trim whitespace, collapse internal runs to single
/// spaces (newlines included), peel wrapping quotes. For single-utterance
/// dictation cleanup, where a stray newline would inject an accidental break.
pub fn polish(raw: &str) -> String {
    polish_with(raw, false)
}

/// Like [`polish`] but preserves line structure — intra-line space runs are
/// collapsed and runs of 3+ blank lines squeezed to one, but single/double
/// newlines survive. For transform mode, where the result may legitimately be
/// a bullet list or multiple paragraphs.
pub fn polish_multiline(raw: &str) -> String {
    polish_with(raw, true)
}

fn polish_with(raw: &str, preserve_newlines: bool) -> String {
    let mut s = raw.to_string();

    // 1. Strip known trailing chat-template tokens.
    for marker in TRAILING_ARTEFACTS {
        if let Some(idx) = s.find(marker) {
            s.truncate(idx);
        }
    }

    s = s.trim().to_string();

    // 2. Strip a known preamble prefix if the output starts with one.
    let lower = s.to_lowercase();
    for pre in PREAMBLE_PREFIXES {
        if lower.starts_with(pre) {
            s = s[pre.len()..].trim_start().to_string();
            break;
        }
    }

    // 3. Peel symmetric wrapping quotes if the *entire* output is wrapped.
    s = peel_wrapping_quotes(&s);

    // 4. Strip markdown emphasis around the whole string (**foo** / *foo* / _foo_).
    s = peel_markdown_emphasis(&s);

    // 5. Collapse whitespace. Single-utterance cleanup flattens everything to
    //    spaces; transform mode keeps line structure (bullet lists, paragraphs).
    s = if preserve_newlines {
        collapse_whitespace_keep_newlines(&s)
    } else {
        collapse_whitespace(&s)
    };

    s.trim().to_string()
}

/// Collapse intra-line space/tab runs to a single space and trim each line,
/// but keep newlines — squeezing any run of 3+ into a single blank line so the
/// model can't inject runaway vertical gaps.
fn collapse_whitespace_keep_newlines(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut blank_run = 0usize;
    for line in s.lines() {
        let mut collapsed = String::with_capacity(line.len());
        let mut prev_ws = false;
        for c in line.chars() {
            if c == ' ' || c == '\t' {
                if !prev_ws {
                    collapsed.push(' ');
                    prev_ws = true;
                }
            } else {
                collapsed.push(c);
                prev_ws = false;
            }
        }
        let collapsed = collapsed.trim().to_string();
        if collapsed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                lines.push(collapsed); // at most one blank line in a row
            }
        } else {
            blank_run = 0;
            lines.push(collapsed);
        }
    }
    lines.join("\n")
}

fn peel_wrapping_quotes(s: &str) -> String {
    let pairs = [('"', '"'), ('\'', '\''), ('“', '”'), ('‘', '’'), ('`', '`')];
    for (open, close) in pairs {
        if s.starts_with(open) && s.ends_with(close) && s.chars().count() >= 2 {
            let trimmed: String = s.chars().skip(1).take(s.chars().count() - 2).collect();
            return trimmed;
        }
    }
    s.to_string()
}

fn peel_markdown_emphasis(s: &str) -> String {
    for marker in ["**", "*", "_"] {
        if s.starts_with(marker) && s.ends_with(marker) && s.len() > marker.len() * 2 {
            return s[marker.len()..s.len() - marker.len()].to_string();
        }
    }
    s.to_string()
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_chat_template_artefacts() {
        assert_eq!(polish("Hello world.<end_of_turn>"), "Hello world.");
        assert_eq!(polish("Hello world.</s>"), "Hello world.");
    }

    #[test]
    fn strips_known_preambles() {
        assert_eq!(
            polish("Here's the cleaned text: We should ship."),
            "We should ship."
        );
        assert_eq!(polish("Sure! We should ship."), "We should ship.");
    }

    #[test]
    fn peels_wrapping_quotes() {
        assert_eq!(polish("\"We should ship.\""), "We should ship.");
        assert_eq!(polish("'Hello world'"), "Hello world");
    }

    #[test]
    fn peels_markdown_emphasis() {
        assert_eq!(polish("**We should ship.**"), "We should ship.");
        assert_eq!(polish("*hello*"), "hello");
    }

    #[test]
    fn collapses_internal_whitespace() {
        assert_eq!(polish("  We  should   ship.  "), "We should ship.");
        assert_eq!(polish("line one\n\nline two"), "line one line two");
    }

    #[test]
    fn preserves_intentional_content() {
        assert_eq!(
            polish("We should ship Rust + macOS code."),
            "We should ship Rust + macOS code."
        );
    }

    #[test]
    fn empty_in_empty_out() {
        assert_eq!(polish(""), "");
        assert_eq!(polish("   \n  "), "");
    }

    #[test]
    fn multiline_preserves_list_newlines() {
        // The transform path must keep line structure that `polish` flattens.
        assert_eq!(
            polish_multiline("* Buy milk\n* Buy eggs\n* Buy bread"),
            "* Buy milk\n* Buy eggs\n* Buy bread"
        );
    }

    #[test]
    fn multiline_collapses_intra_line_spaces_and_extra_blanks() {
        assert_eq!(
            polish_multiline("para one\n\n\n\npara  two"),
            "para one\n\npara two"
        );
    }

    #[test]
    fn multiline_still_strips_preamble_and_artefacts() {
        assert_eq!(
            polish_multiline("Sure! line one\nline two<end_of_turn>"),
            "line one\nline two"
        );
    }
}
