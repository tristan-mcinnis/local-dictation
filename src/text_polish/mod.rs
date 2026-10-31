//! Defensive post-pass on cleaner output.
//!
//! Even with "output ONLY the cleaned text" hammered into the system prompt,
//! small LLMs occasionally leak a preamble ("Here's the cleaned text:"),
//! wrap output in quotes, emit a markdown bold/italic, or end with chat
//! template artefacts. None of those should hit the user's text field.
//!
//! Pure, no I/O — easy to unit-test.

mod mechanics;
mod stream;
mod guards;
mod spoken;
pub use mechanics::{
    apply_self_correction,
    fix_speech_mechanics,
    flatten_to_line,
    looks_like_list,
    strip_trailing_filler,
};
use mechanics::*;
pub use stream::join_stream_segments;
pub use guards::{cleanup_dropped_content, needs_llm_cleanup};
use spoken::*;

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
    "assistant:",
    "assistant ",
    "utterance:",
    "utterance ",
    "utterer:",
    "utterer ",
    "users:",
    "user:",
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

/// Reasoning-block tags a thinking model can leak before its answer (Qwen/S1
/// uses `<think>…</think>`; DeepSeek-style uses `<|begin_of_thought|>…`). The
/// block and its contents are stripped before visible-text cleanup. An OPEN tag
/// with no matching CLOSE means the model was cut off mid-reason and produced no
/// usable answer — everything from the open tag on is dropped.
const REASONING_OPEN_TAGS: &[&str] = &["<think>", "<|begin_of_thought|>"];
const REASONING_CLOSE_TAGS: &[&str] = &["</think>", "<|end_of_thought|>"];

fn strip_reasoning_blocks(s: &str) -> String {
    let lower = s.to_lowercase();
    // Earliest open tag that is preceded only by whitespace (a leading block,
    // never a mid-sentence literal).
    let mut open_pos: Option<usize> = None;
    for open in REASONING_OPEN_TAGS {
        if let Some(p) = lower.find(open) {
            if s[..p].trim().is_empty() {
                open_pos = Some(open_pos.map_or(p, |cur| cur.min(p)));
            }
        }
    }
    let Some(op) = open_pos else {
        return s.to_string();
    };
    // Earliest closing tag after the open tag.
    let mut close_end: Option<usize> = None;
    for close in REASONING_CLOSE_TAGS {
        if let Some(p) = lower[op..].find(close) {
            let end = op + p + close.len();
            close_end = Some(close_end.map_or(end, |cur| cur.min(end)));
        }
    }
    match close_end {
        Some(end) => format!("{}{}", &s[..op], &s[end..]),
        None => s[..op].trim().to_string(),
    }
}

fn polish_with(raw: &str, preserve_newlines: bool) -> String {
    let mut s = raw.to_string();

    // Remove a thinking model's reasoning block before visible-text cleanup.
    s = strip_reasoning_blocks(&s);

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
    for _ in 0..3 {
        let lower = s.to_lowercase();
        let mut removed = false;
        for pre in PREAMBLE_PREFIXES {
            if lower.starts_with(pre) {
                s = s[pre.len()..].trim_start().to_string();
                removed = true;
                break;
            }
        }
        if !removed {
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
    if open
        .trim_start_matches('`')
        .trim()
        .chars()
        .any(|c| !c.is_alphanumeric())
    {
        return s.to_string();
    }
    // Closing fence: a line that is nothing but backticks.
    if lines
        .last()
        .map(|l| {
            let t = l.trim();
            t.len() >= 2 && t.chars().all(|c| c == '`')
        })
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
    "here",
    "below",
    "sure",
    "okay",
    "ok,",
    "the following",
    "these are",
    "this is",
    "i've",
    "i have",
    "certainly",
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
    let looks_intro = first.ends_with(':') || LIST_INTRO_TELLS.iter().any(|t| lower.starts_with(t));
    if !looks_intro {
        return s.to_string();
    }
    lines[first_idx + 1..].join("\n").trim_start().to_string()
}

/// Rule 4 — append a terminal period to prose that lacks one. Fires only when:
/// the text is a single line (not a list), does NOT already end in terminal
/// punctuation or a closing quote, is > 3 words, and is not a trailing voice
/// command. Conservative: it only ADDS punctuation, never touches a content word.
fn add_terminal_period(s: &str) -> String {
    let trimmed = s.trim_end();
    if trimmed.is_empty() {
        return s.to_string();
    }
    if let Some(last) = trimmed.chars().last() {
        if matches!(last, '.' | '!' | '?' | '…') {
            return s.to_string();
        }
        // Already ends in a closing quote — leave it (a period may belong inside
        // or outside; guessing is more likely to corrupt than help).
        if matches!(last, '"' | '\'' | '”' | '’') {
            return s.to_string();
        }
    }
    if looks_like_list(trimmed) || trimmed.contains('\n') {
        return s.to_string();
    }
    // A trailing voice command ("press enter", "scratch that", …) is already
    // stripped downstream; don't fight the refiner by dotting it.
    if crate::voice_commands::parse_trailing_command(trimmed).1 != crate::voice_commands::TrailingAction::None {
        return s.to_string();
    }
    if trimmed.split_whitespace().count() <= 3 {
        return s.to_string();
    }
    format!("{trimmed}.")
}

#[cfg(test)]
mod tests;
