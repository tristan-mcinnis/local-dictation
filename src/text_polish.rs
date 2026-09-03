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

/// Interjection tokens that are never real English words — always filler.
/// Kept deliberately conservative: "ah"/"er" are excluded (they shade into
/// real interjections a speaker might want), and removal is whole-word only.
const FILLER_INTERJECTIONS: &[&str] = &["um", "umm", "uhm", "uh", "uhh", "erm", "hmm", "mm", "mhm"];

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
    // Mid-sentence self-correction and spoken-list formatting run LAST, after
    // all casing/punctuation mechanics, so their word-boundary surgery sees the
    // final text. Both are conservative (never drop or reorder a content word).
    let corrected = apply_self_correction(&terminated);
    format_spoken_list(&corrected)
}

/// Byte spans of every alphanumeric run in `s` — the shared primitive for the
/// deterministic self-correction and list-formatting passes, which need to
/// locate words in the original-cased string.
fn word_spans(s: &str) -> Vec<(usize, usize)> {
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

/// Ordinal words that mark a spoken list, mapped to their 1-based number.
/// (Deterministic: `numbers::spoken_to_numerals` deliberately leaves ordinals
/// alone, so these survive the earlier mechanics untouched.)
const ORDINAL_WORDS: &[(&str, usize)] = &[
    ("first", 1),
    ("second", 2),
    ("third", 3),
    ("fourth", 4),
    ("fifth", 5),
    ("sixth", 6),
    ("seventh", 7),
    ("eighth", 8),
    ("ninth", 9),
    ("tenth", 10),
];

fn ordinal_number(word: &str) -> Option<usize> {
    ORDINAL_WORDS
        .iter()
        .find(|(w, _)| *w == word)
        .map(|(_, n)| *n)
}

/// Deterministic spoken-list formatting: when the utterance enumerates items
/// with ordinal markers ("first … second … finally …"), split it into a
/// numbered list, one item per line. Requires ≥2 ordinal markers (so prose that
/// merely mentions "first" or "second" is left alone) and preserves the
/// speaker's wording — items are only split, never rewritten. "finally"/"lastly"
/// closes the list with the next number.
pub fn format_spoken_list(s: &str) -> String {
    let spans = word_spans(s);
    if spans.len() < 4 {
        return s.to_string();
    }
    let words: Vec<String> = spans.iter().map(|&(a, b)| s[a..b].to_lowercase()).collect();
    let mut markers: Vec<(usize, usize)> = Vec::new(); // (word index, number)
    let mut finally: Option<usize> = None;
    for (idx, w) in words.iter().enumerate() {
        if let Some(num) = ordinal_number(w) {
            markers.push((idx, num));
        } else if w == "finally" || w == "lastly" {
            finally = Some(idx);
        }
    }
    if markers.len() < 2 {
        return s.to_string();
    }
    if let Some(fi) = finally {
        markers.push((fi, markers.len() + 1));
    }
    markers.sort_by_key(|m| m.0);

    let mut items: Vec<String> = Vec::new();
    for (mi, &(idx, num)) in markers.iter().enumerate() {
        let item_start = idx + 1;
        let item_end = markers.get(mi + 1).map(|m| m.0).unwrap_or(spans.len());
        if item_start >= item_end {
            continue;
        }
        let a = spans[item_start].0;
        // Extend to the next marker (or end of string) so a trailing period or
        // comma that belongs to this item is kept, not dropped.
        let b = markers
            .get(mi + 1)
            .map(|m| spans[m.0].0)
            .unwrap_or(s.len());
        let mut text = s[a..b].trim().trim_start_matches(',').trim().to_string();
        if let Some(first) = text.chars().next() {
            if first.is_ascii_lowercase() {
                let cap = first.to_ascii_uppercase();
                text = format!("{cap}{}", &text[first.len_utf8()..]);
            }
        }
        items.push(format!("{num}. {text}"));
    }
    if items.len() < 2 {
        return s.to_string();
    }
    items.join("\n")
}

/// Words that restart a self-corrected utterance, immediately before "i mean" /
/// "i meant". ("scratch that" is a whole-utterance cancel command handled
/// elsewhere; "let me rephrase" is rare enough to skip.)
const REPAIR_RESTARTS: &[&str] = &["no", "wait", "actually", "rather", "instead", "sorry"];

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
fn capitalize_sentence_starts(s: &str) -> String {
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

fn starts_like_list_item(line: &str) -> bool {
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

/// Join streamed ASR segments into one utterance, repairing the boundary
/// artifact the segmenter creates: the VAD closes a segment at every pause and
/// the ASR punctuates each segment independently, so a pause before the final
/// word yields "…it's like instant." + "connection." — a spurious period that
/// `fix_speech_mechanics` then sentence-capitalizes into "instant. Connection."
/// Words that cannot end an English sentence. When a streamed segment ends on
/// one of these (plus the period the ASR appended), the pause was a hesitation
/// inside the clause, not a sentence boundary.
const NON_TERMINAL_TAIL: &[&str] = &[
    "a", "an", "the", "of", "in", "on", "at", "to", "for", "with", "from", "by",
    "into", "onto", "about", "and", "but", "or", "nor", "so", "than", "that",
    "which", "who", "whom", "whose", "where", "when", "while", "because", "if",
    "unless", "like", "as", "my", "your", "his", "her", "its", "our", "their",
    "this", "these", "those", "very", "really", "just", "also", "not", "is",
    "are", "was", "were", "be", "been", "am", "has", "have", "had", "do", "does",
    "did", "can", "could", "will", "would", "should", "may", "might", "must",
    "i'm", "it's", "that's", "there's", "we're", "they're", "you're",
];

/// Words safe to lowercase when the ASR capitalised them only because a
/// segment happened to start there. Deliberately common words only: a name
/// or acronym ("RTI", "Claude") keeps its capital.
const SAFE_TO_LOWER: &[&str] = &[
    "a", "an", "the", "of", "in", "on", "at", "to", "for", "with", "from", "by",
    "into", "about", "and", "but", "or", "so", "than", "that", "this", "these",
    "those", "which", "what", "when", "where", "who", "how", "why", "it", "its",
    "it's", "they", "them", "we", "us", "you", "he", "she", "him", "her", "his",
    "my", "your", "our", "their", "is", "are", "was", "were", "be", "been",
    "being", "have", "has", "had", "do", "does", "did", "can", "could", "will",
    "would", "should", "may", "might", "must", "not", "no", "yes", "like",
    "just", "also", "then", "there", "here", "some", "any", "all", "more",
    "most", "very", "really", "actually", "kind", "sort", "maybe", "probably",
    "if", "because", "up", "down", "out", "off", "over", "under", "again",
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    "ten", "hundred", "thousand", "something", "anything", "everything",
    "nothing", "now", "still", "even", "only", "too", "much", "many", "little",
    "bit", "way", "thing", "things", "stuff", "people", "time", "know", "think",
    "want", "need", "get", "go", "make", "see", "say", "said", "use", "using",
];

fn last_word_lower(s: &str) -> Option<String> {
    let trimmed = s.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let word = trimmed
        .rsplit(|c: char| c.is_whitespace())
        .next()?
        .trim_matches(|c: char| !(c.is_alphanumeric() || c == '\''));
    if word.is_empty() {
        None
    } else {
        Some(word.to_lowercase())
    }
}

fn first_word(s: &str) -> &str {
    s.split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim_end_matches(|c: char| !(c.is_alphanumeric() || c == '\''))
}

/// Streaming ASR transcribes each pause-delimited segment as its own request
/// and punctuates it as a standalone utterance, so a hesitation inside a clause
/// comes back as ". Capital". Two repairs, both conservative:
///
/// 1. A segment that BEGINS lowercase is the ASR's own signal that it heard a
///    mid-sentence continuation: drop the previous segment's single trailing
///    period and keep the sentence flowing.
/// 2. A segment that ENDS on a word no sentence can end on ("of", "which",
///    "the") was cut mid-clause: drop its period and, when the next word is an
///    everyday word, lowercase it. Names and acronyms keep their capital.
///
/// A "..." trail-off, '!'/'?', and other capitalized starts are left untouched.
pub fn join_stream_segments(segs: &[String]) -> String {
    let mut out = String::new();
    for seg in segs {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if out.is_empty() {
            out.push_str(seg);
            continue;
        }
        let single_period = out.ends_with('.') && !out.ends_with("..");
        let starts_lower = seg.chars().next().is_some_and(|c| c.is_lowercase());
        let cut_mid_clause = single_period
            && last_word_lower(&out).is_some_and(|w| NON_TERMINAL_TAIL.contains(&w.as_str()));
        if single_period && (starts_lower || cut_mid_clause) {
            out.pop();
        }
        out.push(' ');
        if cut_mid_clause && !starts_lower {
            let head = first_word(seg);
            let lowered = head.to_lowercase();
            let is_acronym = head.len() > 1 && head.chars().all(|c| !c.is_lowercase());
            if !is_acronym && SAFE_TO_LOWER.contains(&lowered.as_str()) {
                out.push_str(&lowered);
                out.push_str(&seg[head.len()..]);
                continue;
            }
        }
        out.push_str(seg);
    }
    out
}

/// Trailing acknowledgement fillers a speaker tacks on while thinking — kept as
/// its own clause, e.g. "…we can use. Yeah." Only stripped when it's a STANDALONE
/// trailing clause (after a sentence/comma boundary), so "I told him yeah" and a
/// whole-utterance "Yeah." are left intact. (Leading versions are handled by the
/// cleanup prompt; this is the deterministic trailing case — measured at ~1% of
/// real dictations but consistently a verbatim verbal tic.)
const TRAILING_FILLERS: &[&str] = &["yeah", "yep", "yup", "you know"];

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
const LEADING_FILLERS: &[&str] = &[
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

fn strip_leading_filler(s: &str) -> String {
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

/// Quote characters that delimit a quoted (verbatim) span. A filler inside
/// such a span is someone's quoted speech, not the speaker's own filler, so it
/// must survive — and the quoting character must stay balanced.
const QUOTE_CHARS: &[char] = &['"', '“', '”'];

/// Does `tok` carry any quote character of its own (so a filler that also opens
/// or closes a quote must be kept, not dropped)?
fn tok_has_quote(tok: &str) -> bool {
    tok.chars().any(|c| QUOTE_CHARS.contains(&c))
}

/// Toggle `*in_quote` on the odd-count of quote characters in `tok`, so the
/// caller can tell whether the next token still sits inside a quoted region.
fn update_quote_state(tok: &str, in_quote: &mut bool) {
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
fn drop_filler_interjections(s: &str) -> String {
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
    out.trim_start_matches(|c: char| c == ',' || c == ' ')
        .to_string()
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

/// Lowercased alphanumeric word tokens of `s`.
fn word_tokens(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
}

/// Discourse / hedge / contraction words (≥4 chars) that legitimate cleanup
/// routinely drops or rewrites ("like", "you know", "gonna" → "going to"). They
/// must NOT count as dropped *content* when judging whether the model gutted a
/// clause, or heavy-but-correct filler removal would trip the guard.
const DROPPABLE_DISCOURSE: &[&str] = &[
    "like",
    "just",
    "yeah",
    "yep",
    "yup",
    "okay",
    "well",
    "right",
    "anyway",
    "actually",
    "basically",
    "literally",
    "seriously",
    "honestly",
    "really",
    "mean",
    "know",
    "sort",
    "kind",
    "guess",
    "maybe",
    "stuff",
    "very",
    "even",
    "gonna",
    "wanna",
    "gotta",
    "kinda",
    "sorta",
    "dunno",
    "lemme",
    "gimme",
];

/// Heuristic guard: did the generative cleanup **drop real content** (rewrite
/// instead of edit) rather than just remove filler? Because the cleanup model
/// regenerates the whole utterance, it can silently delete a whole clause — e.g.
/// "Hey Chris, just blocking your time to talk about basketball. Let me know if
/// this works." → "Hey Chris, let me know if this works." This catches that so
/// the daemon can fall back to the (faithful) raw transcript instead of
/// injecting a gutted message.
///
/// The signal is **content words dropped**: substantial words (≥4 chars,
/// excluding the discourse/hedge/contraction set the model legitimately rewrites)
/// that are in the raw but absent from the cleaned text. Two ways it fires:
///   (A) Wholesale gutting — output collapsed to < 55% of the raw's characters
///       AND ≥3 content words gone (a heavy rewrite, not an edit).
///   (B) Dropped clause in a longer utterance — the overall length barely moves
///       (one sentence deleted from several), so the char ratio misses it, but a
///       run of ≥5 content words (≈ a short clause) has vanished.
/// Short utterances (< 24 raw chars) are never judged — too little to tell.
pub fn cleanup_dropped_content(raw: &str, cleaned: &str) -> bool {
    let raw_chars = raw.chars().count();
    if raw_chars < 24 {
        return false;
    }
    let clean_chars = cleaned.chars().count();
    let clean_words: std::collections::HashSet<String> = word_tokens(cleaned).collect();
    let dropped_content = word_tokens(raw)
        .filter(|w| w.chars().count() >= 4)
        .filter(|w| !DROPPABLE_DISCOURSE.contains(&w.as_str()))
        .filter(|w| !clean_words.contains(w))
        .count();
    // (A) Wholesale gutting: big char loss + a few content words gone.
    if clean_chars * 100 < raw_chars * 55 && dropped_content >= 3 {
        return true;
    }
    // (B) Dropped clause that the char ratio alone can't see.
    dropped_content >= 5
}

/// Decide whether a transcript is messy enough to be worth the generative
/// cleanup LLM, or clean enough to take the deterministic pass alone.
///
/// Measured motivation: ~87% of real dictations from a clear speaker pass
/// through the LLM unchanged (it re-emits the same text for ~150 ms–1 s), and a
/// chunk of the rest are *unwanted* rewrites — e.g. "open pencil" → "open source"
/// (the model swapping an unusual word for a common phrase it assumes you meant).
/// So we only invoke the model when the text has mess the deterministic pass
/// (filler interjections, casing, numbers) can't already handle:
///   - colloquial contractions that need expanding (wanna/gonna/…),
///   - a false start / stutter (adjacent duplicate word),
///   - ≥2 "soft" fillers the deterministic pass leaves in (like / you know /
///     i mean / sort of / kind of),
///   - a long run-on (≥40 words) with almost no sentence punctuation.
/// Otherwise the deterministic pass is enough — faster, and it can't substitute
/// or drop a word. `< 3` words is always "clean" (nothing for the model to do).
pub fn needs_llm_cleanup(raw: &str) -> bool {
    let lower = raw.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.len() < 3 {
        return false;
    }
    const CONTRACTIONS: &[&str] = &[
        "wanna", "gonna", "gotta", "kinda", "sorta", "gimme", "lemme", "dunno", "outta", "lotta",
        "shoulda", "coulda", "woulda",
    ];
    if words.iter().any(|w| CONTRACTIONS.contains(w)) {
        return true;
    }
    // False start / stutter: the same word twice in a row ("the the", "I I").
    if words.windows(2).any(|w| w[0] == w[1] && w[0].len() > 1) {
        return true;
    }
    // Soft fillers the deterministic pass won't safely strip (context-dependent).
    const SOFT: &[&str] = &["like", "basically", "literally", "seriously", "honestly"];
    let mut score = words.iter().filter(|w| SOFT.contains(w)).count();
    for w in words.windows(2) {
        if (w[0] == "you" && w[1] == "know")
            || (w[0] == "i" && w[1] == "mean")
            || (w[0] == "sort" && w[1] == "of")
            || (w[0] == "kind" && w[1] == "of")
        {
            score += 1;
        }
    }
    if score >= 2 {
        return true;
    }
    // Long run-on with almost no sentence breaks — the model may need to
    // re-segment it into sentences.
    let terminators = raw.matches(['.', '!', '?']).count();
    words.len() >= 40 && terminators <= 1
}

// ---------------------------------------------------------------------------
// New deterministic enrichments (rule 2–4). Each is a small pure function, is
// conservative (never drops or re-orders a content word), and is idempotent.
// ---------------------------------------------------------------------------

/// File-extension / public-TLD words that follow a spoken "dot" and join into a
/// literal suffix ("dot js" → ".js", "dot com" → ".com"). Conservative — only
/// these well-known technical tokens, never an arbitrary word after "dot".
const EXT_WORDS: &[&str] = &[
    "js", "ts", "rs", "py", "json", "md", "com", "dev", "io", "ai", "org", "net", "html",
    "htm", "css", "pdf", "png", "jpg", "jpeg", "gif", "svg", "xml", "txt", "log", "yml",
    "yaml", "toml", "sh", "go", "rb", "c", "h", "swift", "kt", "java", "m", "cpp", "vue",
    "svelte", "tsx", "jsx", "webp", "wasm", "db", "sql", "lock", "env", "gitignore",
];

/// Repeated words that are (almost always) intentional rather than a stutter:
/// intensifiers/emphatics ("very very"), a true reduplication ("no no"), a
/// grammatical doubling ("that that", "had had"). We keep these as-is; every
/// other immediately-repeated run is collapsed.
const INTENTIONAL_REPEATS: &[&str] = &[
    // Intensifiers/emphatics and true reduplications or grammatical doublings.
    // Articles ("the the") and other words are NOT here — a doubled article is
    // a stutter, and we collapse it. Figures are handled separately via
    // [`is_numbery`].
    "very", "really", "no", "that", "had", "so", "too", "quite", "rather",
];

/// Simple cardinal/number words, kept verbatim when repeated ("fifty fifty" —
/// a figure, not a stutter). After `spoken_to_numerals` most cardinals are
/// digits, but ambiguous runs like "fifty fifty" stay as words.
const NUMBER_WORDS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
    "nineteen", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    "hundred", "thousand", "million", "billion", "point",
];

/// Ordinary English function/determiner/preposition words (and a few common
/// verbs). Used to keep the identifier-ish join from firing on prose: a "dash"
/// or "underscore" only becomes a joiner when BOTH flanking words are outside
/// this set (i.e. look like identifier parts, not function words). Also guards
/// the spoken-email conversion from treating a sentence opener as a username.
const PROSE_WORDS: &[&str] = &[
    "the", "a", "an", "and", "of", "to", "in", "on", "at", "for", "with", "from", "by", "as",
    "is", "are", "was", "were", "be", "been", "being", "am", "do", "does", "did", "have", "has",
    "had", "not", "no", "yes", "but", "or", "so", "if", "then", "else", "when", "how", "what",
    "which", "who", "whom", "why", "can", "could", "will", "would", "shall", "should", "may",
    "might", "must", "let", "me", "my", "your", "our", "us", "him", "her", "them", "their",
    "there", "here", "this", "that", "these", "those", "it", "its", "we", "they", "you", "he",
    "she", "i", "one", "two", "both", "each", "some", "any", "all", "more", "less", "most",
    "over", "under", "into", "onto", "off", "out", "up", "down", "than", "just", "only", "also",
    "very", "really", "about", "around", "before", "after", "between", "during", "without",
    "toward", "near", "like", "well", "now", "still", "yet", "too", "again", "once", "where",
    "went", "came", "saw", "met", "called", "said", "told", "let", "got", "get", "made", "make",
    "found", "took", "put", "set", "ran", "run", "please", "thanks", "yes", "okay", "ok",
];

/// The two join-words, mapping to the punctuation they spell out.
fn join_style(word: &str) -> Option<char> {
    match word.to_lowercase().as_str() {
        "underscore" => Some('_'),
        "dash" | "hyphen" => Some('-'),
        _ => None,
    }
}

/// Returns true if `w` is a cardinal word or already-numeric token — such a
/// repeat is a figure ("fifty fifty") rather than a stutter, so prefer keeping.
fn is_numbery(w: &str) -> bool {
    if w.is_empty() {
        return false;
    }
    if w.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    NUMBER_WORDS.contains(&w.to_lowercase().as_str())
}

/// Technical tokens whose repetition is a meaningful sequence/phrase (not a
/// stutter): "dot dot dot" → "…", "tilde slash" → "~/", "underscore
/// underscore" would be a run. Collapse must leave these for the spoken-
/// technical rules (which run after) to translate.
const TECHNICAL_REPEAT_TOKENS: &[&str] = &[
    "dot", "tilde", "slash", "underscore", "dash", "hyphen", "at", "sign",
];

/// Is the whole repeated segment "intentional" (an intensifier, a grammar
/// doubling, a figure, or a technical token sequence)? If so we keep both
/// copies; otherwise collapse. Conservative: prefer allowing, so we only
/// collapse genuinely stutter-like runs.
fn is_intentional_repeat(segment: &[&str]) -> bool {
    segment.iter().all(|w| {
        let lower = w.to_lowercase();
        INTENTIONAL_REPEATS.contains(&lower.as_str())
            || is_numbery(&lower)
            || TECHNICAL_REPEAT_TOKENS.contains(&lower.as_str())
    })
}

/// A token from `vtokenize` — a word run of ascii alphanumerics/apostrophes, or
/// a separator run (spaces, punctuation) kept verbatim.
enum VTok {
    W(String),
    S(String),
}

impl VTok {
    fn text(&self) -> &str {
        match self {
            VTok::W(w) => w.as_str(),
            VTok::S(s) => s.as_str(),
        }
    }
}

/// Split `s` into alternating word / separator segments, preserving everything.
fn vtokenize(s: &str) -> Vec<VTok> {
    let mut toks = Vec::new();
    let mut buf = String::new();
    let mut buf_is_word = false;
    for c in s.chars() {
        let is_word = c.is_ascii_alphanumeric() || c == '\'';
        if buf.is_empty() {
            buf.push(c);
            buf_is_word = is_word;
        } else if is_word == buf_is_word {
            buf.push(c);
        } else {
            flush_vtok(&mut toks, &mut buf, buf_is_word);
            buf.push(c);
            buf_is_word = is_word;
        }
    }
    if !buf.is_empty() {
        flush_vtok(&mut toks, &mut buf, buf_is_word);
    }
    toks
}

fn flush_vtok(toks: &mut Vec<VTok>, buf: &mut String, is_word: bool) {
    let s = std::mem::take(buf);
    toks.push(if is_word { VTok::W(s) } else { VTok::S(s) });
}

/// Is `toks[idx]` a word equal (case-insensitively) to `word`?
fn tok_is_word(toks: &[VTok], idx: usize, word: &str) -> bool {
    toks.get(idx).is_some_and(|t| matches!(t, VTok::W(w) if w.eq_ignore_ascii_case(word)))
}

/// Is `toks[idx]` a separator made purely of spaces/tabs?
fn tok_is_ws(toks: &[VTok], idx: usize) -> bool {
    toks.get(idx).is_some_and(|t| matches!(t, VTok::S(s) if s.chars().all(|c| c == ' ' || c == '\t')))
}

/// If `toks[idx]` is a known extension/TLD word, return its lowercase form.
/// (Used by the "dot ext" rule — so "dot js" → ".js".)
fn tok_ext(toks: &[VTok], idx: usize) -> Option<String> {
    match toks.get(idx) {
        Some(VTok::W(w)) if w.chars().all(|c| c.is_ascii_alphabetic()) => {
            let lower = w.to_lowercase();
            if EXT_WORDS.contains(&lower.as_str()) {
                Some(lower)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Is `toks[idx]` an identifier-ish word: lowercased ascii alnum, 2–24 chars,
/// and not a plain English function word. (Used to keep the dash/underscore
/// join from firing on prose like "the dash is".)
fn tok_identish(toks: &[VTok], idx: usize) -> Option<String> {
    match toks.get(idx) {
        Some(VTok::W(w)) if w.len() >= 2 && w.len() <= 24 => {
            let lower = w.to_lowercase();
            let ok_chars = w.chars().all(|c| c.is_ascii_alphanumeric());
            let not_prose = !PROSE_WORDS.contains(&lower.as_str());
            if ok_chars && not_prose {
                Some(lower)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Rule 2 — collapse immediately-duplicated token runs of 1–8 words, keeping
/// one copy, unless the repetition is intentional ("very very", "no no",
/// "that that", "had had", figures like "fifty fifty"). Only fires within a
/// run of words joined purely by whitespace (never across a comma/period), so
/// sentence boundaries are never crossed and nothing is re-ordered.
fn collapse_repeated_fragments(s: &str) -> String {
    let toks = vtokenize(s);
    let n = toks.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        if let VTok::W(_) = &toks[i] {
            if let Some(len) = repeated_run_len(&toks, i) {
                let run = whitespace_run_words(&toks, i);
                // Emit copy 1 (words + their interleaving whitespace), then skill
                // the bridge separator and copy 2 by jumping past its last word.
                for t in &toks[run[0]..run[len - 1] + 1] {
                    out.push_str(t.text());
                }
                i = run[2 * len - 1] + 1;
                continue;
            }
        }
        out.push_str(&toks[i].text());
        i += 1;
    }
    out
}

/// The word indices (into `toks`) of the whitespace-joined word run beginning
/// at `start` (which must be a word token).
fn whitespace_run_words(toks: &[VTok], start: usize) -> Vec<usize> {
    let mut idxs = Vec::new();
    let mut j = start;
    while j < toks.len() {
        match &toks[j] {
            VTok::W(_) => idxs.push(j),
            VTok::S(s) if s.chars().all(|c| c == ' ' || c == '\t') => {}
            VTok::S(_) => break,
        }
        j += 1;
    }
    idxs
}

/// If the word run starting at `start` begins with an immediately-repeated
/// group of 1–8 words (the same order, case-insensitive), return its length;
/// otherwise `None`. The repeated copy must be non-intentional (see
/// [`is_intentional_repeat`]). Only a whitespace-joined run is considered, so
/// punctuation/case boundaries are never crossed.
fn repeated_run_len(toks: &[VTok], start: usize) -> Option<usize> {
    let word_idxs = whitespace_run_words(toks, start);
    if word_idxs.len() < 2 {
        return None;
    }
    let max_len = (word_idxs.len() / 2).min(8);
    for len in (1..=max_len).rev() {
        let equal = (0..len).all(|k| {
            toks[word_idxs[k]]
                .text()
                .eq_ignore_ascii_case(toks[word_idxs[k + len]].text())
        });
        if !equal {
            continue;
        }
        let segment: Vec<&str> = (0..len).map(|k| toks[word_idxs[k]].text()).collect();
        if is_intentional_repeat(&segment) {
            continue;
        }
        return Some(len);
    }
    None
}

/// Rule 3a — "at sign" → "@" and the unambiguous spoken-email
/// "user at domain dot tld" → "user@domain.tld". The email form fires only
/// when the username is the very first word, lowercased, and not an English
/// sentence opener, so ordinary prose ("I looked at domain dot com") is never
/// mangled.
fn normalize_at(s: &str) -> String {
    let toks = vtokenize(s);
    let n = toks.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        // "word at domain dot tld" → "word@domain.tld" (username at start).
        // Token layout: user(i) ws(i+1) at(i+2) ws(i+3) domain(i+4) ws(i+5)
        // dot(i+6) ws(i+7) tld(i+8).
        if tok_email_user(&toks, i).is_some() && tok_identish(&toks, i + 4).is_some() {
            if tok_is_ws(&toks, i + 1)
                && tok_is_word(&toks, i + 2, "at")
                && tok_is_ws(&toks, i + 3)
                && tok_is_ws(&toks, i + 5)
                && tok_is_word(&toks, i + 6, "dot")
                && tok_is_ws(&toks, i + 7)
            {
                if let Some(tld) = tok_ext(&toks, i + 8) {
                    // Preserve the user/domain spelling as spoken; the TLD is
                    // normalized lowercase (domains are case-insensitive).
                    out.push_str(toks[i].text());
                    out.push('@');
                    out.push_str(toks[i + 4].text());
                    out.push('.');
                    out.push_str(&tld);
                    i += 9;
                    continue;
                }
            }
        }
        // "at sign" → "@", but only when it's the symbol phrase, not a prose
        // "look at sign" (the word before "at" must be a function word or absent).
        if tok_is_word(&toks, i, "at") {
            if tok_is_ws(&toks, i + 1) && tok_is_word(&toks, i + 2, "sign") {
                let preceded_by_prose = i >= 2 && tok_identish(&toks, i - 2).is_some();
                if !preceded_by_prose {
                    out.push('@');
                    i += 3;
                    continue;
                }
            }
        }
        out.push_str(&toks[i].text());
        i += 1;
    }
    out
}

/// A lowercase, non-prose username at the very start of the string (i.e. this
/// is the first word).
fn tok_email_user(toks: &[VTok], idx: usize) -> Option<String> {
    if idx != 0 {
        return None;
    }
    tok_identish(toks, idx)
}

/// Rule 3b — "tilde slash" → "~/", "dot dot dot" → "…", and "dot <ext>" →
/// ".<ext>" (joining a preceding word: "react dot js" → "react.js").
fn normalize_spoken_dots(s: &str) -> String {
    let toks = vtokenize(s);
    let n = toks.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        // "dot dot dot" → "…"
        if tok_is_word(&toks, i, "dot") && tok_is_ws(&toks, i + 1) && tok_is_word(&toks, i + 2, "dot") && tok_is_ws(&toks, i + 3) && tok_is_word(&toks, i + 4, "dot") {
            out.push('…');
            i += 5;
            continue;
        }
        // "<word> dot js" → "<word>.js" (join with a preceding non-prose word)
        if let VTok::W(_) = toks[i] {
            if tok_is_ws(&toks, i + 1) && tok_is_word(&toks, i + 2, "dot") && tok_is_ws(&toks, i + 3) {
                if let Some(ext) = tok_ext(&toks, i + 4) {
                    let lower = toks[i].text().to_lowercase();
                    if !PROSE_WORDS.contains(&lower.as_str()) {
                        out.push_str(toks[i].text());
                        out.push('.');
                        out.push_str(&ext);
                        i += 5;
                        continue;
                    }
                }
            }
        }
        // "dot js" (no preceding word) → ".js"
        if tok_is_word(&toks, i, "dot") && tok_is_ws(&toks, i + 1) {
            if let Some(ext) = tok_ext(&toks, i + 2) {
                out.push('.');
                out.push_str(&ext);
                i += 3;
                continue;
            }
        }
        // "tilde slash" → "~/"
        if tok_is_word(&toks, i, "tilde") && tok_is_ws(&toks, i + 1) && tok_is_word(&toks, i + 2, "slash") {
            out.push_str("~/");
            i += 3;
            continue;
        }
        out.push_str(&toks[i].text());
        i += 1;
    }
    out
}

/// Rule 3c — "underscore"/"dash"/"hyphen" between two identifier-ish words →
/// "_" / "-". Only in identifier-like context (both flanking words lowercase,
/// non-prose, 2–24 chars), never at sentence level or in ordinary prose.
fn normalize_spoken_joins(s: &str) -> String {
    let toks = vtokenize(s);
    let n = toks.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        if tok_is_ws(&toks, i + 1) {
            if let Some(ch) = join_style(&toks[i + 2].text()) {
                if tok_is_ws(&toks, i + 3) {
                    // Preserve the flanking words' casing as spoken (never
                    // lowercase a content word); only the join char is added.
                    if tok_identish(&toks, i).is_some() && tok_identish(&toks, i + 4).is_some() {
                        out.push_str(toks[i].text());
                        out.push(ch);
                        out.push_str(toks[i + 4].text());
                        i += 5;
                        continue;
                    }
                }
            }
        }
        out.push_str(&toks[i].text());
        i += 1;
    }
    out
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
mod tests {
    use super::*;

    #[test]
    fn join_stream_segments_repairs_mid_sentence_boundaries() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The reported bug: a pause before the last word → spurious period.
        assert_eq!(
            join_stream_segments(&s(&["it's like instant.", "connection."])),
            "it's like instant connection."
        );
        // Capitalized start = a genuine new sentence — period stays.
        assert_eq!(
            join_stream_segments(&s(&["The...", "They can be paired."])),
            "The... They can be paired."
        );
        // Trail-off ellipsis is deliberate — never popped.
        assert_eq!(
            join_stream_segments(&s(&["I was thinking...", "maybe later."])),
            "I was thinking... maybe later."
        );
        // '!'/'?' are real terminals — untouched even before a lowercase start.
        assert_eq!(
            join_stream_segments(&s(&["Ready?", "yes."])),
            "Ready? yes."
        );
        // Empty / whitespace segments are dropped; single segment passes through.
        assert_eq!(join_stream_segments(&s(&["", "  ", "Hello."])), "Hello.");
        assert_eq!(join_stream_segments(&s(&["Hello."])), "Hello.");
    }

    #[test]
    fn join_stream_segments_repairs_cuts_after_non_terminal_words() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // 2026-09-03 live dictation: a breath after "of" before an acronym.
        // The period goes; the acronym keeps its capitals.
        assert_eq!(
            join_stream_segments(&s(&["The chat window at the bottom of.", "Of RTI."])),
            "The chat window at the bottom of of RTI."
        );
        // Everyday next word is lowercased.
        assert_eq!(
            join_stream_segments(&s(&["The light mode, which.", "I know right now."])),
            "The light mode, which I know right now."
        );
        assert_eq!(
            join_stream_segments(&s(&["It shifts up a little bit, which.", "Is weird."])),
            "It shifts up a little bit, which is weird."
        );
        // A name after a preposition keeps its capital.
        assert_eq!(
            join_stream_segments(&s(&["Send it to.", "Claude tomorrow."])),
            "Send it to Claude tomorrow."
        );
        // A real sentence end on a content word is untouched.
        assert_eq!(
            join_stream_segments(&s(&["Also look at the dictation.", "It keeps breaking."])),
            "Also look at the dictation. It keeps breaking."
        );
        // Trail-off after a function word is deliberate.
        assert_eq!(
            join_stream_segments(&s(&["I just want...", "Kath, I just want to know."])),
            "I just want... Kath, I just want to know."
        );
    }

    #[test]
    fn strips_chat_template_artefacts() {
        assert_eq!(polish("Hello world.<end_of_turn>"), "Hello world.");
        assert_eq!(polish("Hello world.</s>"), "Hello world.");
    }

    #[test]
    fn strips_reasoning_blocks_closed_and_unclosed() {
        // Closed block: keep what follows the closing tag.
        assert_eq!(
            polish("<think>the user wants a greeting</think>\nHello"),
            "Hello"
        );
        // Unclosed block (model cut off mid-reason): drop everything.
        assert_eq!(polish("<think>"), "");
        assert_eq!(polish("<think>let me consider"), "");
        // DeepSeek-style tags, case-insensitive.
        assert_eq!(
            polish("<Think>reasoning</Think>Answer"),
            "Answer"
        );
        // No tag → untouched.
        assert_eq!(polish("plain answer"), "plain answer");
        // A mid-sentence literal is never a reasoning block.
        assert_eq!(polish("I think this is fine"), "I think this is fine");
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
    fn strips_repeated_s1_role_labels() {
        assert_eq!(polish("Assistant Assistant: I sent it."), "I sent it.");
        assert_eq!(polish("Utterance: test this."), "test this.");
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
            "So I think we should ship it you know."
        );
        // "Um" dropped as an interjection; leading "yeah," stripped as
        // throat-clearing; "so" (no comma) is a real connective, kept.
        assert_eq!(
            fix_speech_mechanics("Um, yeah, so the cache keeps growing."),
            "So the cache keeps growing."
        );
        assert_eq!(
            fix_speech_mechanics("uh we never evict anything"),
            "We never evict anything."
        );
    }

    #[test]
    fn mechanics_capitalizes_standalone_i() {
        assert_eq!(
            fix_speech_mechanics("i don't know if i should"),
            "I don't know if I should."
        );
        assert_eq!(
            fix_speech_mechanics("i'm not sure i'll make it"),
            "I'm not sure I'll make it."
        );
    }

    #[test]
    fn mechanics_leaves_words_containing_filler_or_i_alone() {
        // "i" inside a word and filler substrings inside real words survive
        // (only the sentence-initial letter is capitalized; "api"/"main" stay).
        assert_eq!(
            fix_speech_mechanics("the api summary is in the main file"),
            "The api summary is in the main file."
        );
        // "summary" contains "um"; "main" contains "i"; none are standalone.
        // (Sentence-initial capitalization makes it "Summary".)
        assert!(fix_speech_mechanics("summary").contains("Summary"));
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
        assert_eq!(polish_multiline("```rust\nlet x = 1;\n```"), "let x = 1;");
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
        assert_eq!(
            strip_trailing_filler("the models we can use. Yeah."),
            "the models we can use."
        );
        assert_eq!(strip_trailing_filler("do it. yeah"), "do it.");
        assert_eq!(strip_trailing_filler("that's cool, yeah"), "that's cool.");
        assert_eq!(
            strip_trailing_filler("ship it tomorrow. You know."),
            "ship it tomorrow."
        );
    }

    #[test]
    fn keeps_meaningful_and_whole_utterance_yeah() {
        // no clause boundary before "yeah" → it's part of the sentence
        assert_eq!(strip_trailing_filler("I told him yeah"), "I told him yeah");
        // the entire utterance is the acknowledgement → the speaker meant it
        assert_eq!(strip_trailing_filler("Yeah."), "Yeah.");
        assert_eq!(strip_trailing_filler("yeah"), "yeah");
        // whole-word only: must not chop the end of a real word
        assert_eq!(
            strip_trailing_filler("the deal is conveyed"),
            "the deal is conveyed"
        );
    }

    #[test]
    fn detects_lists_vs_prose() {
        assert!(looks_like_list("1. Buy milk.\n2. Buy eggs.\n3. Buy flour."));
        assert!(looks_like_list("- one\n- two\n- three"));
        assert!(!looks_like_list(
            "I went to the store and bought milk, eggs, and flour."
        ));
        assert!(!looks_like_list(
            "First we set up the repo. Second we ship it."
        )); // one line
        assert!(!looks_like_list("3.11 is the version we shipped today.")); // single line, not a list
    }

    #[test]
    fn flatten_collapses_newlines() {
        assert_eq!(flatten_to_line("a\nb   c\n\n d"), "a b c d");
    }

    #[test]
    fn fix_speech_mechanics_drops_trailing_yeah_end_to_end() {
        // (leading "so" removal is the cleanup model's job, not this pass)
        assert_eq!(
            fix_speech_mechanics("i think we should ship it. yeah"),
            "I think we should ship it."
        );
    }

    #[test]
    fn dropped_content_catches_a_deleted_clause() {
        // The real failure: a whole clause silently deleted (rewrite, not edit).
        let raw = "Hey Chris, just blocking your time to talk about basketball. Let me know if this works.";
        let gutted = "Hey Chris, let me know if this works.";
        assert!(cleanup_dropped_content(raw, gutted));
    }

    #[test]
    fn dropped_content_ignores_legit_cleanup() {
        // Filler removal + light edits must NOT trip the guard.
        let raw = "um so like I think we should uh ship the the build tomorrow you know";
        let clean = "I think we should ship the build tomorrow.";
        assert!(!cleanup_dropped_content(raw, clean));

        // Contraction expansion / number formatting shouldn't fire either.
        let raw2 = "we're gonna wanna refactor this in version two point one soon";
        let clean2 = "We are going to want to refactor this in version 2.1 soon.";
        assert!(!cleanup_dropped_content(raw2, clean2));

        // A faithful, same-length clean is obviously fine.
        let raw3 = "the latency lab shows the model warms up once at boot";
        let clean3 = "The latency lab shows the model warms up once at boot.";
        assert!(!cleanup_dropped_content(raw3, clean3));
    }

    #[test]
    fn dropped_content_catches_a_clause_dropped_from_a_longer_utterance() {
        // The log smoking gun: a content sentence deleted mid-utterance. Overall
        // length barely moves (64% retained), so the old char-ratio gate missed
        // it, but a run of content words ("amazing nice looking designs") is gone.
        let raw = "So I forked this repo and now this is me I'm working on it. So actually \
            what I would like is there are a lot of amazing nice looking designs in here. \
            I would like to make an HTML template design for the example co slide master that we have";
        let gutted = "So I forked this repo and now I'm working on it. Actually, I would like \
            to create an HTML template design for the Example Co slide master that we have.";
        assert!(cleanup_dropped_content(raw, gutted));
    }

    #[test]
    fn dropped_content_skips_short_utterances() {
        // Too short to judge — never fire (avoids nuking genuinely terse edits).
        assert!(!cleanup_dropped_content(
            "yeah sounds good to me",
            "Sounds good."
        ));
    }

    #[test]
    fn messiness_gate_skips_clean_speech() {
        // The exact failure case: clean, no mess → skip the LLM, so it can't
        // turn "open pencil" into "open source".
        assert!(!needs_llm_cleanup(
            "I don't use open pencil, you can remove that."
        ));
        assert!(!needs_llm_cleanup(
            "So that we could then update the basketball room slide sketch."
        ));
        assert!(!needs_llm_cleanup(
            "Give me the key structure that would appear inside a brief."
        ));
    }

    #[test]
    fn strips_leading_throat_clearing_yeah() {
        assert_eq!(
            fix_speech_mechanics("Yeah, let's get the redesign live right now."),
            "Let's get the redesign live right now."
        );
        assert_eq!(
            fix_speech_mechanics("So, actually that might add latency."),
            "Actually that might add latency."
        );
        // Strips at most one leading filler.
        assert_eq!(
            fix_speech_mechanics("Yeah, okay, makes sense."),
            "Okay, makes sense."
        );
    }

    #[test]
    fn capitalizes_sentence_start_after_model_strips_filler() {
        // The LLM stripped "Okay," itself and left "the" lowercase — repair it.
        assert_eq!(
            fix_speech_mechanics("the issue is when you look at some of the logs."),
            "The issue is when you look at some of the logs."
        );
        // Mid-utterance sentence start after a period.
        assert_eq!(
            fix_speech_mechanics("Cool, that makes sense. the other thing is the gutters."),
            "Cool, that makes sense. The other thing is the gutters."
        );
    }

    #[test]
    fn capitalization_preserves_intercaps_and_decimals() {
        // Intercaps brand/identifier tokens at a sentence start stay verbatim.
        assert_eq!(
            capitalize_sentence_starts("iOS is fine. macOS too."),
            "iOS is fine. macOS too."
        );
        // A decimal after a period must not trigger a spurious capitalization.
        assert_eq!(
            capitalize_sentence_starts("Ship version 2.1 today."),
            "Ship version 2.1 today."
        );
        // Already-correct text is unchanged (idempotent).
        assert_eq!(
            capitalize_sentence_starts("Hello there. How are you?"),
            "Hello there. How are you?"
        );
    }

    #[test]
    fn keeps_meaningful_leading_words() {
        // No comma ⇒ "so" is a real connective, leave it.
        assert_eq!(
            fix_speech_mechanics("So we need to ship the build."),
            "So we need to ship the build."
        );
        // A whole-utterance "Yeah." (no comma) is the speaker's actual answer.
        assert_eq!(fix_speech_mechanics("Yeah."), "Yeah.");
        // "right" mid-word / as content must not be touched.
        assert_eq!(
            fix_speech_mechanics("Right now is a good time."),
            "Right now is a good time."
        );
    }

    #[test]
    fn messiness_gate_routes_messy_speech_to_llm() {
        assert!(needs_llm_cleanup(
            "we're gonna wanna refactor this kinda soon"
        )); // contractions
        assert!(needs_llm_cleanup("I think the the build is slow")); // stutter / false start
        assert!(needs_llm_cleanup(
            "it was like really slow and like you know kind of broken"
        )); // dense soft filler
            // A 40+ word run-on with no sentence breaks needs re-segmenting.
        let runon = "so then we did the thing and then the other thing and then \
            after that we went over to the place and grabbed the stuff and then \
            came back and started again with the first part of the whole process once more";
        assert!(needs_llm_cleanup(runon));
    }

    #[test]
    fn self_correction_drops_abandoned_word_and_marker() {
        // The user's exact reported cases.
        assert_eq!(
            apply_self_correction("Let's meet Monday. No wait, I mean Tuesday."),
            "Let's meet Tuesday."
        );
        assert_eq!(
            apply_self_correction("Let's meet Monday, no wait, I mean Tuesday."),
            "Let's meet Tuesday."
        );
        assert_eq!(
            apply_self_correction("The report is due Friday, actually I mean Monday."),
            "The report is due Monday."
        );
        assert_eq!(
            apply_self_correction("ship it, I mean the release"),
            "ship the release"
        );
        // No marker, or marker without content before it → unchanged.
        assert_eq!(apply_self_correction("I mean well"), "I mean well");
        assert_eq!(
            apply_self_correction("I mean, the release is ready"),
            "I mean, the release is ready"
        );
        assert_eq!(
            apply_self_correction("the report is due Monday"),
            "the report is due Monday"
        );
    }

    #[test]
    fn spoken_list_formats_ordinals_into_numbered_lines() {
        assert_eq!(
            format_spoken_list("First, check the logs. Second, run the tests. Finally, commit it."),
            "1. Check the logs.\n2. Run the tests.\n3. Commit it."
        );
        // Two ordinals, no "finally".
        assert_eq!(
            format_spoken_list("First do A. Second do B."),
            "1. Do A.\n2. Do B."
        );
        // A single "first" is prose → unchanged.
        assert_eq!(
            format_spoken_list("This is the first time we tried it"),
            "This is the first time we tried it"
        );
        // Non-list text with no ordinals → unchanged.
        assert_eq!(
            format_spoken_list("Just a normal sentence about nothing"),
            "Just a normal sentence about nothing"
        );
    }

    #[test]
    fn messiness_gate_ignores_ordinary_non_repair_uses() {
        // "actually"/"sorry"/"no" in ordinary, non-repair senses must NOT route
        // to the LLM — the deterministic pass handles them.
        assert!(!needs_llm_cleanup("actually I like that"));
        assert!(!needs_llm_cleanup("I'm sorry to hear that"));
        assert!(!needs_llm_cleanup("no problem at all"));
        // Emphasis use of "actually" (pronoun before it) isn't a repair.
        assert!(!needs_llm_cleanup("I actually like that idea"));
        assert!(!needs_llm_cleanup("we actually shipped it yesterday"));
    }

    // -------------------------------------------------------------------
    // New deterministic rules (2–4): filler quote-safety, repeated-fragment
    // collapse, spoken technical normalization, and the trailing stop.
    // -------------------------------------------------------------------

    #[test]
    fn filler_removal_is_quote_aware() {
        // Standalone filler, mid-clause, is still dropped (with seam repair).
        assert_eq!(
            fix_speech_mechanics("that, um, we should ship it"),
            "That, we should ship it."
        );
        // Filler inside a quoted span is verbatim speech → kept, quotes balanced.
        assert_eq!(
            fix_speech_mechanics("He said \"um, yeah\" and left"),
            "He said \"um, yeah\" and left."
        );
        // A filler substring inside a real word is never touched.
        assert_eq!(
            fix_speech_mechanics("the summary and the armour are here"),
            "The summary and the armour are here."
        );
    }

    #[test]
    fn collapse_repeated_fragments_collapses_stutter() {
        assert_eq!(collapse_repeated_fragments("I I think"), "I think");
        assert_eq!(
            collapse_repeated_fragments("we should we should go"),
            "we should go"
        );
        assert_eq!(
            collapse_repeated_fragments("the the build is slow"),
            "the build is slow"
        );
        // Never crosses a sentence boundary or a non-adjacent repeat.
        assert_eq!(
            collapse_repeated_fragments("we should. we should go"),
            "we should. we should go"
        );
    }

    #[test]
    fn collapse_repeated_fragments_keeps_intentional_repeats() {
        assert_eq!(collapse_repeated_fragments("very very good"), "very very good");
        assert_eq!(collapse_repeated_fragments("no no"), "no no");
        assert_eq!(collapse_repeated_fragments("that that is true"), "that that is true");
        assert_eq!(collapse_repeated_fragments("had had enough"), "had had enough");
        // A figure is kept, not collapsed — be conservative, prefer allowing.
        assert_eq!(collapse_repeated_fragments("fifty fifty"), "fifty fifty");
    }

    #[test]
    fn spoken_technical_tilde_and_dots() {
        assert_eq!(normalize_spoken_dots("redirect to tilde slash"), "redirect to ~/");
        assert_eq!(normalize_spoken_dots("open index dot js"), "open index.js");
        assert_eq!(normalize_spoken_dots("the file dot js"), "the file.js");
        assert_eq!(normalize_spoken_dots("visit example dot com"), "visit example.com");
        assert_eq!(normalize_spoken_dots("dot js"), ".js");
        assert_eq!(normalize_spoken_dots("dot dot dot"), "…");
    }

    #[test]
    fn spoken_identifier_joins() {
        assert_eq!(normalize_spoken_joins("foo underscore bar"), "foo_bar");
        assert_eq!(normalize_spoken_joins("pull dash request"), "pull-request");
        assert_eq!(normalize_spoken_joins("type hyphen hint"), "type-hint");
        // Prose / ordinary English must NOT fire (function-word flanking words).
        assert_eq!(normalize_spoken_joins("the dash is missing"), "the dash is missing");
        assert_eq!(normalize_spoken_joins("a underscore b"), "a underscore b");
        assert_eq!(normalize_spoken_joins("Foo underscore bar"), "Foo_bar");
    }

    #[test]
    fn spoken_at_sign_and_email() {
        assert_eq!(normalize_at("at sign is used"), "@ is used");
        assert_eq!(normalize_at("john at gmail dot com"), "john@gmail.com");
        // Prose "at sign" is left alone (a content word precedes "at").
        assert_eq!(normalize_at("look at sign"), "look at sign");
        // Ordinary prose describing a domain must not become an email.
        assert_eq!(normalize_at("I looked at domain dot com"), "I looked at domain dot com");
    }

    #[test]
    fn spoken_versions_are_numbers_concern() {
        // "two point one" → "2.1" is numbers.rs' job; we deliberately add no
        // version logic here. Confirm the deterministic path still renders it.
        assert_eq!(fix_speech_mechanics("version two point one"), "Version 2.1");
    }

    #[test]
    fn trailing_stop_turns_prose_into_sentences() {
        assert_eq!(add_terminal_period("The cat sat down"), "The cat sat down.");
        assert_eq!(fix_speech_mechanics("open the settings file"), "Open the settings file.");
        // Too short for a sentence → no period.
        assert_eq!(add_terminal_period("Hello world"), "Hello world");
        // Already punctuated → unchanged.
        assert_eq!(add_terminal_period("The cat sat."), "The cat sat.");
        assert_eq!(add_terminal_period("The cat sat!"), "The cat sat!");
        // A list is not prose.
        assert_eq!(
            add_terminal_period("1. Buy milk\n2. Buy eggs"),
            "1. Buy milk\n2. Buy eggs"
        );
        // A trailing voice command is left for the refiner.
        assert_eq!(
            fix_speech_mechanics("run the build press enter"),
            "Run the build press enter"
        );
    }

    #[test]
    fn new_deterministic_rules_are_idempotent() {
        for input in [
            "I I think we should ship it",
            "we should we should go",
            "very very good",
            "He said \"um, yeah\" and left",
            "foo underscore bar",
            "visit example dot com",
            "dot dot dot",
            "open the settings file",
        ] {
            let once = fix_speech_mechanics(input);
            let twice = fix_speech_mechanics(&once);
            assert_eq!(once, twice, "idempotence failed for {input:?}");
        }
    }
}
