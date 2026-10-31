//! Speech mechanics: self-correction, sentence capitals, the standalone "I",
//! list detection, and filler and interjection removal.
use super::*;

/// Interjection tokens that are never real English words — always filler.
/// Kept deliberately conservative: "ah"/"er" are excluded (they shade into
/// real interjections a speaker might want), and removal is whole-word only.
pub(super) const FILLER_INTERJECTIONS: &[&str] = &["um", "umm", "uhm", "uh", "uhh", "erm", "hmm", "mm", "mhm"];

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
    let deled = strip_leading_filler(&despoken);
    let detrailed = strip_trailing_filler(&deled);
    let i_capped = capitalize_standalone_i(&detrailed);
    // Numerals-heavy number formatting: the deterministic half (decimals,
    // versions, well-formed cardinals). The cleanup prompt handles contextual
    // singles; ambiguous runs (years, "three four") are left for it / the user.
    let numbered = crate::numbers::spoken_to_numerals(&i_capped);
    // Sentence-start casing: when the model strips a leading filler itself
    // ("Okay, the issue…" → "the issue…") it leaves a lowercase start nothing
    // else repairs.
    let capitalized = capitalize_sentence_starts(&numbered);
    // --- New deterministic enrichments, AFTER the existing mechanics fix ---
    // Each is conservative (never drops or re-orders a content word); together
    // they make the no-LLM fast path cover still more real dictation and make
    // spoken technical text (paths, extensions, identifiers, versions) more
    // accurate for code-heavy dictation.
    let collapsed = collapse_repeated_fragments(&capitalized);
    let at_handled = normalize_at(&collapsed);
    let dots_handled = normalize_spoken_dots(&at_handled);
    let joined = normalize_spoken_joins(&dots_handled);
    let terminated = add_terminal_period(&joined);
    // Mid-sentence self-correction runs LAST, after all casing/punctuation
    // mechanics, so its word-boundary surgery sees the final text. Structural
    // formatting is never inferred here: lists require an explicit output-format
    // preset, because ordinal words also occur naturally in faithful prose.
    apply_self_correction(&terminated)
}

/// Byte spans of every alphanumeric run in `s` — the primitive used by the
/// deterministic self-correction pass to locate words in the original-cased
/// string.
pub(super) fn word_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(si) = start.take() {
            spans.push((si, i));
        }
    }
    if let Some(si) = start {
        spans.push((si, s.len()));
    }
    spans
}

/// Words that restart a self-corrected utterance, immediately before "i mean" /
/// "i meant". ("scratch that" is a whole-utterance cancel command handled
/// elsewhere; "let me rephrase" is rare enough to skip.)
pub(super) const REPAIR_RESTARTS: &[&str] = &["no", "wait", "actually", "rather", "instead", "sorry"];

/// Deterministic mid-sentence self-correction. When the speaker abandons a
/// fragment and restates it — "Let's meet Monday, no wait, I mean Tuesday" — the
/// abandoned word and the repair marker are dropped, keeping only the correction:
/// "Let's meet Tuesday." Conservative: only fires on an explicit "i mean"/"i
/// meant" with an optional restart word, requires real words on both sides, and
/// drops only the single word immediately before the marker. This is a
/// deterministic stand-in because the cleanup LLM (S1-mini) frequently returns
/// empty on these inputs; a multi-word abandoned phrase is a known limitation.
pub fn apply_self_correction(s: &str) -> String {
    let spans = word_spans(s);
    if spans.len() < 4 {
        return s.to_string();
    }
    let words: Vec<String> = spans.iter().map(|&(a, b)| s[a..b].to_lowercase()).collect();
    let n = words.len();
    // (marker start word index, marker end word index) of the rightmost match.
    let mut best: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < n {
        let is_mean =
            words[i] == "i" && i + 1 < n && (words[i + 1] == "mean" || words[i + 1] == "meant");
        if is_mean {
            let mean_end = i + 2;
            let mut start = i;
            if i >= 1 && REPAIR_RESTARTS.contains(&words[i - 1].as_str()) {
                start = i - 1;
                // "no wait" / "wait no" collapse to a two-word restart.
                if i >= 2 && matches!(words[i - 2].as_str(), "no" | "wait") {
                    start = i - 2;
                }
            }
            if start >= 1 && mean_end < n {
                best = Some((start, mean_end));
            }
            i = mean_end;
        } else {
            i += 1;
        }
    }
    let (m_start, m_end) = match best {
        Some(b) => b,
        None => return s.to_string(),
    };
    // Drop the single word immediately before the marker (the abandoned
    // fragment) plus the marker itself; keep the correction after it.
    let frag_start = spans[m_start - 1].0;
    let marker_end = spans[m_end - 1].1;
    let mut out = String::from(&s[..frag_start]);
    let rest = s[marker_end..].trim_start_matches(|c: char| c.is_whitespace() || c == ',');
    out.push_str(rest);
    out
}

/// Capitalize the first letter of the utterance and of each sentence that
/// follows terminal punctuation (`.`/`!`/`?`). The cleanup model, when it strips
/// a leading filler on its own, leaves a lowercase sentence start that no other
/// pass fixes. Conservative: only acts on a word that begins with a lowercase
/// ASCII letter AND carries no *interior* uppercase, so intercaps brand /
/// identifier tokens ("iOS", "eBay", "macOS", "iPhone") at a sentence start are
/// preserved verbatim. Decimals/versions are untouched (the char after the
/// period isn't a letter). Idempotent.
pub(super) fn capitalize_sentence_starts(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut at_sentence_start = true;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if at_sentence_start && c.is_ascii_lowercase() {
            // Consume the whole word; skip capitalizing if it's intercaps.
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_alphanumeric() {
                j += 1;
            }
            if chars[i + 1..j].iter().any(|c| c.is_uppercase()) {
                out.extend(&chars[i..j]);
            } else {
                out.push(c.to_ascii_uppercase());
                out.extend(&chars[i + 1..j]);
            }
            i = j;
            at_sentence_start = false;
            continue;
        }
        out.push(c);
        if c.is_alphanumeric() {
            at_sentence_start = false;
        } else if matches!(c, '.' | '!' | '?') {
            // A terminator only ends a sentence when it's followed by whitespace
            // or end-of-string. A period glued to a letter/digit ("example.com",
            // "version 2.1", "e.g.") is a domain/decimal/abbreviation, not a
            // sentence break — so what follows is NOT capitalized. This keeps the
            // spoken-technical "dot" joins ("example dot com" → "example.com")
            // from being re-capitalized on a second pass.
            at_sentence_start = match chars.get(i + 1) {
                None => true,
                Some(next) => next.is_whitespace(),
            };
        }
        i += 1;
    }
    out
}

/// Does this text look like a list the model intentionally formatted — i.e. ≥2
/// non-empty lines, a majority of which open with a list marker (`1.` / `1)` /
/// `- ` / `* ` / `• `)? Used by the cleanup path to decide whether to PRESERVE
/// the model's line breaks (auto numbered/bulleted lists) instead of flattening
/// normal dictation to a single line.
pub fn looks_like_list(s: &str) -> bool {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() < 2 {
        return false;
    }
    let listish = lines.iter().filter(|l| starts_like_list_item(l)).count();
    listish >= 2 && listish * 2 >= lines.len()
}

pub(super) fn starts_like_list_item(line: &str) -> bool {
    if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("• ") {
        return true;
    }
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    i > 0 && i < b.len() && (b[i] == b'.' || b[i] == b')')
}

/// Collapse all whitespace (including newlines) to single spaces — the
/// single-line form normal dictation injects.
pub fn flatten_to_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Trailing acknowledgement fillers a speaker tacks on while thinking — kept as
/// its own clause, e.g. "…we can use. Yeah." Only stripped when it's a STANDALONE
/// trailing clause (after a sentence/comma boundary), so "I told him yeah" and a
/// whole-utterance "Yeah." are left intact. (Leading versions are handled by the
/// cleanup prompt; this is the deterministic trailing case — measured at ~1% of
/// real dictations but consistently a verbatim verbal tic.)
pub(super) const TRAILING_FILLERS: &[&str] = &["yeah", "yep", "yup", "you know"];

/// Leading throat-clearing the speaker tacks on before the real sentence —
/// "Yeah, let's ship it." → "Let's ship it." Removing leading filler used to be
/// the cleanup LLM's job, but clear dictation now skips the model (see
/// `needs_llm_cleanup`), so this deterministic pass handles it instead.
///
/// Conservative: only strips a leading filler word **immediately followed by a
/// comma** ("Yeah, …", "So, …", "Okay, …"), which is reliably throat-clearing.
/// A bare leading "So I think…" (no comma) is left alone — "so" there is a real
/// connective. Strips at most ONE (so "Yeah, okay, sure." → "Okay, sure.", not
/// "Sure."), and a whole-utterance "Yeah." has no comma so it's kept.
pub(super) const LEADING_FILLERS: &[&str] = &[
    "yeah",
    "yep",
    "yup",
    "so",
    "okay",
    "ok",
    "well",
    "right",
    "um",
    "uh",
    "er",
    "anyway",
    "look",
    "like",
    "basically",
    "honestly",
];

pub(super) fn strip_leading_filler(s: &str) -> String {
    let trimmed = s.trim_start();
    for f in LEADING_FILLERS {
        if trimmed.len() <= f.len() {
            continue;
        }
        // Byte compare (filler words are ASCII) so a leading multi-byte char
        // can't panic a str slice; a match guarantees f.len() is a char boundary.
        if !trimmed.as_bytes()[..f.len()].eq_ignore_ascii_case(f.as_bytes()) {
            continue;
        }
        // The char right after the word must be a comma (optionally preceded by
        // nothing) — that's the throat-clearing signal. Word boundary too.
        let rest = &trimmed[f.len()..];
        if !rest.starts_with(',') {
            continue;
        }
        let after = rest[1..].trim_start();
        if after.is_empty() {
            continue; // "Yeah," with nothing after — leave it
        }
        // Capitalize the first letter of what remains.
        let mut chars = after.chars();
        let first = chars.next().unwrap();
        return format!("{}{}", first.to_uppercase(), chars.as_str());
    }
    s.to_string()
}

pub fn strip_trailing_filler(s: &str) -> String {
    let trimmed = s.trim_end();
    // Bare text with trailing terminal punctuation / spaces removed, for matching.
    let core = trimmed.trim_end_matches(['.', '!', '?', ',', ' ']);
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

/// Quote characters that delimit a quoted (verbatim) span. A filler inside
/// such a span is someone's quoted speech, not the speaker's own filler, so it
/// must survive — and the quoting character must stay balanced.
pub(super) const QUOTE_CHARS: &[char] = &['"', '“', '”'];

/// Does `tok` carry any quote character of its own (so a filler that also opens
/// or closes a quote must be kept, not dropped)?
pub(super) fn tok_has_quote(tok: &str) -> bool {
    tok.chars().any(|c| QUOTE_CHARS.contains(&c))
}

/// Toggle `*in_quote` on the odd-count of quote characters in `tok`, so the
/// caller can tell whether the next token still sits inside a quoted region.
pub(super) fn update_quote_state(tok: &str, in_quote: &mut bool) {
    let count = tok.chars().filter(|c| QUOTE_CHARS.contains(c)).count();
    if count % 2 == 1 {
        *in_quote = !*in_quote;
    }
}

/// Remove whole-word filler interjections anywhere in the text, then repair
/// the punctuation/space seams the removal leaves (" ," → ",", doubled
/// spaces, a stray leading comma). Quote-aware: a filler inside a quoted span
/// ("He said \"um, sure\"") is verbatim speech and is left intact, and a
/// filler token carrying its own quote char is kept so quotes stay balanced.
pub(super) fn drop_filler_interjections(s: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut in_quote = false;
    for tok in s.split(' ') {
        let core = tok
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if FILLER_INTERJECTIONS.contains(&core.as_str()) && !in_quote && !tok_has_quote(tok) {
            continue; // standalone filler, not quoted → drop it
        }
        kept.push(tok);
        update_quote_state(tok, &mut in_quote);
    }
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
    out.trim_start_matches([',', ' '])
        .to_string()
}

/// Capitalize the standalone pronoun "i" (word-bounded), including its
/// contractions ("i'm", "i'll", "i've", "i'd"). Never touches "i" inside a
/// larger word.
pub(super) fn capitalize_standalone_i(s: &str) -> String {
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

pub(super) fn peel_wrapping_quotes(s: &str) -> String {
    let pairs = [('"', '"'), ('\'', '\''), ('“', '”'), ('‘', '’'), ('`', '`')];
    for (open, close) in pairs {
        if s.starts_with(open) && s.ends_with(close) && s.chars().count() >= 2 {
            let trimmed: String = s.chars().skip(1).take(s.chars().count() - 2).collect();
            return trimmed;
        }
    }
    s.to_string()
}

pub(super) fn peel_markdown_emphasis(s: &str) -> String {
    for marker in ["**", "*", "_"] {
        if s.starts_with(marker) && s.ends_with(marker) && s.len() > marker.len() * 2 {
            return s[marker.len()..s.len() - marker.len()].to_string();
        }
    }
    s.to_string()
}

pub(super) fn collapse_whitespace(s: &str) -> String {
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

/// Lowercased alphanumeric word tokens of `s`.
pub(super) fn word_tokens(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
}
