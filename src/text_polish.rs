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

    // 1b. Peel a wrapping ``` code fence (```lang … ```), which small models
    //     sometimes emit despite "no markdown". Only when the whole output is
    //     fenced — an opening ``` line and a closing ``` line.
    s = peel_code_fence(&s);

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

    // 5. For multi-line (list / paragraph) output, drop a leaked intro line
    //    that precedes a list ("Here's a bulleted list:\n- a\n- b") — small
    //    models slip these in across runs despite "no preamble".
    if preserve_newlines {
        s = strip_leading_list_intro(&s);
    }

    // 6. Collapse whitespace. Single-utterance cleanup flattens everything to
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

/// Peel a wrapping ``` fence. Handles the whole-output case the models emit:
/// an opening line that is ``` (optionally with a language tag) and a final
/// line that is just ```. Leaves un-fenced text and inline code untouched.
fn peel_code_fence(s: &str) -> String {
    let trimmed = s.trim();
    if !trimmed.starts_with("``") {
        return s.to_string();
    }
    let mut lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() < 2 {
        return s.to_string();
    }
    // Opening fence: a run of backticks (2+) then an optional language token.
    let open = lines[0].trim();
    if open.trim_start_matches('`').trim().chars().any(|c| !c.is_alphanumeric()) {
        return s.to_string();
    }
    // Closing fence: a line that is nothing but backticks.
    if lines
        .last()
        .map(|l| { let t = l.trim(); t.len() >= 2 && t.chars().all(|c| c == '`') })
        .unwrap_or(false)
    {
        lines.remove(0);
        lines.pop();
        return lines.join("\n").trim().to_string();
    }
    s.to_string()
}

/// True if `line` begins a markdown list item: "- ", "* ", "• ", or "1." /
/// "2)" style numbering.
fn is_list_marker(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("• ") {
        return true;
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    !digits.is_empty() && t[digits.len()..].starts_with(['.', ')'])
}

/// Chatty list-intro openers a small model emits before a list.
const LIST_INTRO_TELLS: &[&str] = &[
    "here", "below", "sure", "okay", "ok,", "the following", "these are",
    "this is", "i've", "i have", "certainly",
];

/// Drop a leaked intro line that sits before a list body. Fires only when the
/// first non-empty line is NOT itself a list item, a later line IS, and the
/// first line looks like an intro (ends with ':' or starts with a known tell).
/// Conservative by design — real leading content is left alone.
fn strip_leading_list_intro(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let Some(first_idx) = lines.iter().position(|l| !l.trim().is_empty()) else {
        return s.to_string();
    };
    let first = lines[first_idx].trim();
    if is_list_marker(first) {
        return s.to_string();
    }
    let body_is_list = lines[first_idx + 1..].iter().any(|l| is_list_marker(l));
    if !body_is_list {
        return s.to_string();
    }
    let lower = first.to_lowercase();
    let looks_intro =
        first.ends_with(':') || LIST_INTRO_TELLS.iter().any(|t| lower.starts_with(t));
    if !looks_intro {
        return s.to_string();
    }
    lines[first_idx + 1..].join("\n").trim_start().to_string()
}

/// Interjection tokens that are never real English words — always filler.
/// Kept deliberately conservative: "ah"/"er" are excluded (they shade into
/// real interjections a speaker might want), and removal is whole-word only.
const FILLER_INTERJECTIONS: &[&str] =
    &["um", "umm", "uhm", "uh", "uhh", "erm", "hmm", "mm", "mhm"];

/// Mechanical fixes for dictation-cleanup output that a 1B model does
/// unreliably but a deterministic pass nails every time:
///   1. drop standalone filler interjections the model left in ("um", "uh",
///      "hmm" …) — especially the leading one Gemma stubbornly keeps;
///   2. capitalize the standalone pronoun "i" and its contractions.
///
/// SPEECH cleanup only — NOT applied to transform output, where a lowercase
/// "i" can legitimately be a loop variable and a leading "um" won't occur.
/// Pure & unit-tested.
pub fn fix_speech_mechanics(s: &str) -> String {
    let despoken = drop_filler_interjections(s);
    let detrailed = strip_trailing_filler(&despoken);
    let i_capped = capitalize_standalone_i(&detrailed);
    // Numerals-heavy number formatting: the deterministic half (decimals,
    // versions, well-formed cardinals). The cleanup prompt handles contextual
    // singles; ambiguous runs (years, "three four") are left for it / the user.
    crate::numbers::spoken_to_numerals(&i_capped)
}

/// Trailing acknowledgement fillers a speaker tacks on while thinking — kept as
/// its own clause, e.g. "…we can use. Yeah." Only stripped when it's a STANDALONE
/// trailing clause (after a sentence/comma boundary), so "I told him yeah" and a
/// whole-utterance "Yeah." are left intact. (Leading versions are handled by the
/// cleanup prompt; this is the deterministic trailing case — measured at ~1% of
/// real dictations but consistently a verbatim verbal tic.)
const TRAILING_FILLERS: &[&str] = &["yeah", "yep", "yup", "you know"];

fn strip_trailing_filler(s: &str) -> String {
    let trimmed = s.trim_end();
    // Bare text with trailing terminal punctuation / spaces removed, for matching.
    let core = trimmed.trim_end_matches(|c: char| matches!(c, '.' | '!' | '?' | ',' | ' '));
    for f in TRAILING_FILLERS {
        if core.len() < f.len() || !core[core.len() - f.len()..].eq_ignore_ascii_case(f) {
            continue;
        }
        let before = &core[..core.len() - f.len()];
        // Whole-word: the char before the filler must be a boundary, not a letter
        // ("conveyed" must not match trailing "yed", "okay" not match "ay", etc.).
        if before.chars().last().is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        let before_trim = before.trim_end();
        if before_trim.is_empty() {
            // The whole utterance is just the filler — the speaker meant it. Keep.
            return trimmed.to_string();
        }
        // Only strip when a clause boundary precedes the filler (so "I said yeah"
        // — no boundary — is preserved as meaningful).
        if before_trim.ends_with(['.', '!', '?', ',']) {
            let kept = before_trim.trim_end_matches([',', ' ']);
            return if kept.ends_with(['.', '!', '?']) {
                kept.to_string()
            } else {
                format!("{kept}.")
            };
        }
    }
    trimmed.to_string()
}

/// Remove whole-word filler interjections anywhere in the text, then repair
/// the punctuation/space seams the removal leaves (" ," → ",", doubled
/// spaces, a stray leading comma).
fn drop_filler_interjections(s: &str) -> String {
    let kept: Vec<&str> = s
        .split(' ')
        .filter(|tok| {
            let core = tok
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            !FILLER_INTERJECTIONS.contains(&core.as_str())
        })
        .collect();
    let joined = kept.join(" ");
    // Repair seams: " ," / " ." → ",", collapse double spaces, drop a leading
    // comma or stray punctuation left where a leading filler used to be.
    let mut out = String::with_capacity(joined.len());
    let mut prev_space = false;
    for c in joined.chars() {
        if c == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
            out.push(c);
        } else {
            // " ," → ",": if we just pushed a space and now hit punctuation,
            // pop the space.
            if matches!(c, ',' | '.' | ';' | ':' | '!' | '?') && out.ends_with(' ') {
                out.pop();
            }
            prev_space = false;
            out.push(c);
        }
    }
    out.trim_start_matches(|c: char| c == ',' || c == ' ').to_string()
}

/// Capitalize the standalone pronoun "i" (word-bounded), including its
/// contractions ("i'm", "i'll", "i've", "i'd"). Never touches "i" inside a
/// larger word.
fn capitalize_standalone_i(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let prev_boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        if c == 'i' && prev_boundary {
            // The char after a standalone "i" must be a non-letter (word end)
            // or a contraction apostrophe.
            let next = chars.get(i + 1).copied();
            let standalone = match next {
                None => true,
                Some(n) if !n.is_alphabetic() && n != '\'' && n != '\u{2019}' => true,
                Some('\'') | Some('\u{2019}') => true,
                _ => false,
            };
            if standalone {
                out.push('I');
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
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
    fn mechanics_drops_leading_and_inline_interjections() {
        assert_eq!(
            fix_speech_mechanics("so um I think we should ship it you know"),
            "so I think we should ship it you know"
        );
        assert_eq!(
            fix_speech_mechanics("Um, yeah, so the cache keeps growing."),
            "yeah, so the cache keeps growing."
        );
        assert_eq!(fix_speech_mechanics("uh we never evict anything"), "we never evict anything");
    }

    #[test]
    fn mechanics_capitalizes_standalone_i() {
        assert_eq!(
            fix_speech_mechanics("i don't know if i should"),
            "I don't know if I should"
        );
        assert_eq!(fix_speech_mechanics("i'm not sure i'll make it"), "I'm not sure I'll make it");
    }

    #[test]
    fn mechanics_leaves_words_containing_filler_or_i_alone() {
        // "i" inside a word and filler substrings inside real words survive.
        assert_eq!(
            fix_speech_mechanics("the api summary is in the main file"),
            "the api summary is in the main file"
        );
        // "summary" contains "um"; "main" contains "i"; none are standalone.
        assert!(fix_speech_mechanics("summary").contains("summary"));
    }

    #[test]
    fn strips_leaked_list_intro() {
        assert_eq!(
            polish_multiline("Here's a bulleted list of the points:\n\n* The timeline is tight.\n* The budget is unclear."),
            "* The timeline is tight.\n* The budget is unclear."
        );
        assert_eq!(
            polish_multiline("Sure! Steps:\n1. Build it.\n2. Ship it."),
            "1. Build it.\n2. Ship it."
        );
    }

    #[test]
    fn keeps_real_leading_line_before_list() {
        // A genuine sentence that isn't a chatty intro stays put.
        let input = "We have three options to weigh carefully.\n- Option A\n- Option B";
        assert_eq!(polish_multiline(input), input);
    }

    #[test]
    fn peels_triple_backtick_fence() {
        assert_eq!(
            polish_multiline("```\n1. First\n2. Second\n```"),
            "1. First\n2. Second"
        );
        // With a language tag on the opening fence.
        assert_eq!(
            polish_multiline("```rust\nlet x = 1;\n```"),
            "let x = 1;"
        );
    }

    #[test]
    fn peels_double_backtick_fence() {
        assert_eq!(
            polish("``\nfix: cache growth on macOS.\n``"),
            "fix: cache growth on macOS."
        );
    }

    #[test]
    fn leaves_inline_backticks_untouched() {
        // Not a whole-output fence — must survive.
        assert_eq!(polish("run `git status` first"), "run `git status` first");
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

    #[test]
    fn strips_standalone_trailing_filler() {
        assert_eq!(strip_trailing_filler("the models we can use. Yeah."), "the models we can use.");
        assert_eq!(strip_trailing_filler("do it. yeah"), "do it.");
        assert_eq!(strip_trailing_filler("that's cool, yeah"), "that's cool.");
        assert_eq!(strip_trailing_filler("ship it tomorrow. You know."), "ship it tomorrow.");
    }

    #[test]
    fn keeps_meaningful_and_whole_utterance_yeah() {
        // no clause boundary before "yeah" → it's part of the sentence
        assert_eq!(strip_trailing_filler("I told him yeah"), "I told him yeah");
        // the entire utterance is the acknowledgement → the speaker meant it
        assert_eq!(strip_trailing_filler("Yeah."), "Yeah.");
        assert_eq!(strip_trailing_filler("yeah"), "yeah");
        // whole-word only: must not chop the end of a real word
        assert_eq!(strip_trailing_filler("the deal is conveyed"), "the deal is conveyed");
    }

    #[test]
    fn fix_speech_mechanics_drops_trailing_yeah_end_to_end() {
        // (leading "so" removal is the cleanup model's job, not this pass)
        assert_eq!(
            fix_speech_mechanics("i think we should ship it. yeah"),
            "I think we should ship it."
        );
    }
}
