//! Screen-context vocabulary harvesting.
//!
//! The cleanup model spells proper nouns blind: it has no idea the person you're
//! dictating about is "Aoife" not "Eva", or that the project is "Parakeet".
//! But the correct spelling is usually *already on screen* — in the text field
//! you're editing and the window title. This module turns that visible text into
//! a short "known vocabulary" list fed to the cleanup prompt (via
//! [`crate::prompts::vocabulary_suffix`]), so Gemma preserves on-screen spellings
//! instead of substituting similar-sounding common words.
//!
//! [`extract_terms`] is the pure, dependency-free core (unit-tested here). The AX
//! read that produces the raw screen text lives in `injector.rs` (it shares the
//! same Accessibility plumbing as injection) and is gated behind `ax-inject`.

/// Cap on how much on-screen text we scan. A focused field can hold a whole
/// document; we only need a representative sample of the visible proper nouns,
/// and this keeps extraction cheap even though it already runs off the critical
/// path (in the parallel focus-capture thread).
const MAX_SCAN_CHARS: usize = 8_000;

/// Common English words that are frequently capitalised (sentence starts,
/// pronouns, function words). They're never the proper nouns the ASR mangles,
/// so they'd only waste slots in the capped vocabulary list.
const STOPWORDS: &[&str] = &[
    "the", "this", "that", "these", "those", "and", "but", "or", "for", "with", "from", "into",
    "your", "you", "our", "their", "his", "her", "its", "we", "they", "he", "she", "it", "a", "an",
    "of", "to", "in", "on", "at", "is", "are", "was", "were", "be", "been", "being", "will",
    "would", "could", "should", "can", "may", "might", "must", "what", "when", "where", "who",
    "whom", "why", "how", "here", "there", "then", "now", "yes", "no", "ok", "okay", "so", "well",
    "just", "like", "also", "if", "as", "by", "do", "does", "did", "not", "all", "any", "some",
    "more", "most", "such", "than", "too", "very", "i'm", "i'll", "i've", "i'd", "we're", "don't",
    "it's", "let's", "that's", "there's", "hello", "hi", "thanks", "thank",
    // Email and chat boilerplate that opens a line with a capital.
    "please", "meeting", "sounds", "let", "best", "regards", "attached", "dear", "sincerely",
    "cheers", "am", "pm", "re", "fwd",
    // Days and months: always capitalised, never misheard.
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday", "january",
    "february", "march", "april", "june", "july", "august", "september", "october", "november",
    "december",
];

/// Take a window of `±radius` characters centred on `caret` from `text`. This
/// is the relevance primitive for screen-context vocabulary: the text *around
/// your cursor* in the focused field is what surrounds the words you're about to
/// dictate — far higher signal than the whole field/window, and it keeps a tiny
/// model from being cluttered with proper nouns from unrelated screen regions.
/// Snaps to char boundaries; clamps to the text bounds.
pub fn window_around(text: &str, caret: usize, radius: usize) -> &str {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = chars.len();
    if n == 0 {
        return "";
    }
    let caret = caret.min(n);
    let start_ci = caret.saturating_sub(radius);
    let end_ci = caret.saturating_add(radius).min(n);
    let start_b = chars[start_ci].0;
    let end_b = if end_ci >= n {
        text.len()
    } else {
        chars[end_ci].0
    };
    &text[start_b..end_b]
}

/// Extract up to `cap` proper-noun / domain-term candidates from a blob of
/// on-screen text, preferring tokens the ASR is most likely to get wrong
/// (mixed-case like `macOS`, identifiers, then plain proper nouns). The
/// on-screen casing is preserved verbatim — that's the spelling we want the
/// model to keep. Deterministic: stable order, case-insensitive dedup.
///
/// Ranking rules (each one is a bug that let ordinary words outrank names):
///   * a word capitalised ONLY at the start of a sentence ("Please", "Sounds")
///     ranks below a word capitalised mid-sentence; it is demoted, not dropped,
///     because a name can open a line ("Beatrix said…");
///   * a repeated term ranks higher; length gives no credit, so "Wednesday"
///     cannot beat "Amy";
///   * a run of three or more ALL-CAPS words is shouting, not vocabulary.
pub fn extract_terms(text: &str, cap: usize) -> Vec<String> {
    if cap == 0 {
        return Vec::new();
    }
    struct Seen {
        term: String,
        base: i32,
        count: i32,
        mid_sentence: bool,
    }
    let mut order: Vec<String> = Vec::new();
    let mut seen: std::collections::HashMap<String, Seen> = std::collections::HashMap::new();

    let scan: String = text.chars().take(MAX_SCAN_CHARS).collect();
    let raws: Vec<&str> = scan.split_whitespace().collect();
    let shouting = shouting_mask(&raws);
    let mut sentence_start = true;

    for (i, raw) in raws.iter().enumerate() {
        let at_start = sentence_start;
        sentence_start = ends_sentence(raw);
        if shouting[i] {
            continue;
        }
        for piece in token_pieces(raw) {
            let piece = piece.replace('\u{2019}', "'");
            let Some(tok) = trim_token(&piece) else { continue };
            if !is_candidate(tok) {
                continue;
            }
            let key = tok.to_lowercase();
            if STOPWORDS.contains(&key.as_str()) {
                continue;
            }
            let entry = seen.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                Seen { term: tok.to_string(), base: score(tok), count: 0, mid_sentence: false }
            });
            entry.count += 1;
            entry.mid_sentence |= !at_start;
        }
    }

    // (score, insertion_index, term) — sort by score desc, then index asc for a
    // deterministic tie-break that keeps document order among equal-score terms.
    let mut scored: Vec<(i32, usize, String)> = order
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let e = &seen[key];
            let mut s = e.base + (e.count - 1).clamp(0, 3);
            // Only a sentence-start capital vouches for this word: weak evidence.
            if !e.mid_sentence && is_plain_capitalised(&e.term) {
                s -= 1;
            }
            (s, idx, e.term.clone())
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(cap).map(|(_, _, t)| t).collect()
}

/// True when the raw token closes a sentence, so the next word's capital is
/// grammar rather than evidence of a proper noun.
fn ends_sentence(raw: &str) -> bool {
    raw.trim_end_matches(['"', '\'', ')', ']', '\u{201D}', '\u{2019}'])
        .ends_with(['.', '!', '?', ':', '\u{2026}'])
}

/// Marks every token that sits in a run of three or more ALL-CAPS words
/// ("THIS IS VERY IMPORTANT"). A lone acronym (NASA) is untouched.
fn shouting_mask(raws: &[&str]) -> Vec<bool> {
    let caps: Vec<bool> = raws
        .iter()
        .map(|r| {
            let letters: Vec<char> = r.chars().filter(|c| c.is_alphabetic()).collect();
            !letters.is_empty()
                && letters.iter().all(|c| c.is_uppercase())
                && !r.chars().any(|c| c.is_ascii_digit() || c == '_')
        })
        .collect();
    let mut mask = vec![false; raws.len()];
    let mut i = 0;
    while i < caps.len() {
        if caps[i] {
            let mut j = i;
            while j < caps.len() && caps[j] {
                j += 1;
            }
            if j - i >= 3 {
                mask[i..j].iter_mut().for_each(|m| *m = true);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    mask
}

/// The candidate pieces inside one whitespace-split token. Usually the token
/// itself; a markdown link contributes its label (`[Aoife](http://…)` →
/// `Aoife`), and a short slash pair of names contributes each side
/// (`Priya/Marcus`). URLs and paths still yield the whole token, which
/// `is_candidate` rejects.
fn token_pieces(raw: &str) -> Vec<&str> {
    if let (Some(open), Some(close)) = (raw.find('['), raw.find("](")) {
        if open < close {
            return vec![&raw[open + 1..close]];
        }
    }
    if raw.contains('/') && !raw.contains("://") && !raw.starts_with('/') {
        let parts: Vec<&str> = raw.split('/').collect();
        let name_like = |p: &&str| {
            let p = p.trim_matches(|c: char| !c.is_alphanumeric());
            p.chars().next().is_some_and(|c| c.is_uppercase())
                && p.chars().all(|c| c.is_alphabetic())
        };
        if (2..=3).contains(&parts.len()) && parts.iter().all(name_like) {
            return parts;
        }
    }
    vec![raw]
}

/// Strip surrounding punctuation/brackets and a trailing possessive (`'s`) from
/// a whitespace-split token. The caller has already normalised the curly
/// apostrophe (U+2019, the macOS default) to `'`. Returns `None` if nothing
/// meaningful is left.
fn trim_token(raw: &str) -> Option<&str> {
    let t = raw.trim_matches(|c: char| !c.is_alphanumeric());
    if t.is_empty() {
        return None;
    }
    // Drop a trailing possessive so "Aoife's" contributes "Aoife".
    let t = t
        .strip_suffix("'s")
        .or_else(|| t.strip_suffix("'S"))
        .unwrap_or(t);
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// An initial capital and nothing else (no internal capital, digit or
/// identifier punctuation): the shape a sentence-start word shares with a name.
fn is_plain_capitalised(tok: &str) -> bool {
    tok.chars().next().is_some_and(|c| c.is_uppercase())
        && !tok.chars().skip(1).any(|c| c.is_uppercase())
        && !tok.chars().any(|c| c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
}

/// A token worth feeding to the model: 2–40 chars, contains a letter, isn't a
/// URL/path/email, and looks like a proper noun or domain term: an initial or
/// internal capital, a `snake_case` identifier, or letters mixed with digits.
/// A hyphen or dot alone is not evidence ("well-known", "e.g.", "file.txt"),
/// and neither is a number with a unit suffix ("3pm", "2nd").
fn is_candidate(tok: &str) -> bool {
    let n = tok.chars().count();
    if !(2..=40).contains(&n) {
        return false;
    }
    if tok.contains('/') || tok.contains('@') || tok.contains('\\') {
        return false;
    }
    if !tok.chars().any(|c| c.is_alphabetic()) {
        return false;
    }
    let any_upper = tok.chars().any(|c| c.is_uppercase());
    let has_digit = tok.chars().any(|c| c.is_ascii_digit());
    let number_with_suffix = {
        let digits = tok.chars().take_while(|c| c.is_ascii_digit()).count();
        let rest: Vec<char> = tok.chars().skip(digits).collect();
        digits > 0 && rest.len() <= 2 && rest.iter().all(|c| c.is_lowercase())
    };
    any_upper || tok.contains('_') || (has_digit && !number_with_suffix)
}

/// Rank candidates so the strongest "ASR will mangle this" signals win the
/// limited slots: mixed-case casing first, then identifiers/digits, then plain
/// capitalised proper nouns. Length earns nothing: it used to give a long
/// common word (+6) far more than a short name (+1). Repeat count and the
/// sentence-start demotion are applied in [`extract_terms`].
fn score(tok: &str) -> i32 {
    let mut s = 0;
    let internal_upper = tok.chars().skip(1).any(|c| c.is_uppercase());
    if internal_upper {
        // Internal capitals (macOS, GitHub, CGEventTap) are the single strongest
        // "the ASR will get this wrong" signal — rank them above plain proper nouns.
        s += 4;
    }
    if tok.chars().any(|c| c.is_ascii_digit()) {
        s += 2;
    }
    if tok.contains('_') || tok.contains('.') {
        s += 2;
    }
    if tok.chars().next().is_some_and(|c| c.is_uppercase()) {
        s += 1;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulls_proper_nouns_and_domain_terms() {
        let text = "I talked to Aoife about the Parakeet model on macOS \
                    and we'll push it to GitHub after standup.";
        let terms = extract_terms(text, 16);
        assert!(terms.contains(&"Aoife".to_string()));
        assert!(terms.contains(&"Parakeet".to_string()));
        assert!(terms.contains(&"macOS".to_string()));
        assert!(terms.contains(&"GitHub".to_string()));
    }

    #[test]
    fn drops_common_capitalised_stopwords() {
        // Sentence-start "The" / "This" must not eat vocabulary slots.
        let terms = extract_terms("The cat sat. This is fine.", 16);
        assert!(!terms.iter().any(|t| t.eq_ignore_ascii_case("the")));
        assert!(!terms.iter().any(|t| t.eq_ignore_ascii_case("this")));
    }

    #[test]
    fn preserves_on_screen_casing_and_dedups() {
        let terms = extract_terms("macOS macOS macos MACOS", 16);
        // First-seen casing wins; case-insensitive dedup collapses the rest.
        assert_eq!(
            terms
                .iter()
                .filter(|t| t.eq_ignore_ascii_case("macos"))
                .count(),
            1
        );
        assert_eq!(terms[0], "macOS");
    }

    #[test]
    fn strips_punctuation_and_possessive() {
        let terms = extract_terms("(Parakeet), \"GitHub\" — Aoife's repo", 16);
        assert!(terms.contains(&"Parakeet".to_string()));
        assert!(terms.contains(&"GitHub".to_string()));
        assert!(terms.contains(&"Aoife".to_string()));
        assert!(!terms.iter().any(|t| t.contains('\'')));
    }

    #[test]
    fn skips_urls_paths_and_plain_lowercase() {
        let terms = extract_terms(
            "see https://example.com/path and the file at /usr/local/bin done",
            16,
        );
        assert!(terms.is_empty(), "got {terms:?}");
    }

    #[test]
    fn mixed_case_outranks_plain_capital_under_a_tight_cap() {
        // macOS (internal caps) should beat plain "Standup" for the single slot.
        let text = "Standup notes about macOS";
        let terms = extract_terms(text, 1);
        assert_eq!(terms, vec!["macOS".to_string()]);
    }

    #[test]
    fn respects_cap_and_zero() {
        assert!(extract_terms("Alpha Bravo Charlie Delta Echo", 2).len() <= 2);
        assert!(extract_terms("Alpha Bravo", 0).is_empty());
    }

    #[test]
    fn window_around_caret_clamps_and_centres() {
        let text = "0123456789ABCDEFGHIJ"; // 20 chars
                                           // Around index 10, radius 3 → chars [7,13).
        assert_eq!(window_around(text, 10, 3), "789ABC");
        // Caret at start clamps low edge.
        assert_eq!(window_around(text, 0, 3), "012");
        // Caret past end clamps to text end.
        assert_eq!(window_around(text, 999, 3), "HIJ");
        // Empty text is safe.
        assert_eq!(window_around("", 5, 3), "");
    }

    #[test]
    fn window_then_extract_keeps_only_nearby_names() {
        // Names far from the caret must NOT be harvested — only the caret window.
        let text = "FarawayProject discussion ... talking about Aoife and Parakeet here";
        let caret = text.find("Aoife").unwrap_or(0);
        // char index of the caret (find returns a byte index; ASCII here so equal)
        let win = window_around(text, caret, 20);
        let terms = extract_terms(win, 16);
        assert!(terms.contains(&"Aoife".to_string()));
        assert!(terms.contains(&"Parakeet".to_string()));
        assert!(
            !terms.contains(&"FarawayProject".to_string()),
            "got {terms:?}"
        );
    }

    // ── SC-8 regressions: one test per extractor bug ─────────────────────

    #[test]
    fn curly_apostrophe_possessive_is_stripped() {
        // U+2019 is what macOS smart quotes type.
        assert_eq!(extract_terms("Aoife\u{2019}s repo", 16), vec!["Aoife".to_string()]);
    }

    #[test]
    fn sentence_start_words_rank_below_mid_sentence_names() {
        let text = "Review this today. Forward it to Amy when done. Sounds good. Reply soon.";
        let terms = extract_terms(text, 16);
        assert_eq!(terms[0], "Amy", "got {terms:?}");
        // Demoted, not dropped: a name that only ever opens a line survives.
        let chat = extract_terms("Beatrix said it works. Tobias agreed.", 16);
        assert!(chat.contains(&"Beatrix".to_string()) && chat.contains(&"Tobias".to_string()));
        // …and one mid-sentence sighting lifts the demotion.
        let both = extract_terms("Okafor called. Reply soon. I told Okafor yes.", 1);
        assert_eq!(both, vec!["Okafor".to_string()]);
    }

    #[test]
    fn email_boilerplate_and_dates_do_not_take_slots() {
        let terms = extract_terms(
            "Please review. Meeting tomorrow. Sounds good. Let me know. Best regards. \
             Attached on Wednesday and Thursday in September.",
            16,
        );
        assert!(terms.is_empty(), "got {terms:?}");
    }

    #[test]
    fn a_short_name_beats_a_long_common_word() {
        // The length bonus used to give "Documentation" +6 against +1 for "Li".
        let terms = extract_terms("we sent Li the Documentation yesterday", 1);
        assert_eq!(terms, vec!["Li".to_string()]);
    }

    #[test]
    fn repeated_terms_rank_higher() {
        let terms = extract_terms("ask Bravo then Alpha, and Alpha again, then Alpha", 1);
        assert_eq!(terms, vec!["Alpha".to_string()]);
    }

    #[test]
    fn hyphen_and_dot_words_need_a_capital_or_digit() {
        let terms = extract_terms("a well-known state-of-the-art idea, e.g. file.txt on example.com at 3pm, the 2nd", 16);
        assert!(terms.is_empty(), "got {terms:?}");
        let kept = extract_terms("use snake_case with GPT-4o and v2.1 on the M4", 16);
        for want in ["snake_case", "GPT-4o", "v2.1", "M4"] {
            assert!(kept.contains(&want.to_string()), "{want} missing from {kept:?}");
        }
    }

    #[test]
    fn shouting_runs_are_dropped_but_acronyms_survive() {
        assert!(extract_terms("THIS IS VERY IMPORTANT PLEASE READ", 16).is_empty());
        assert!(extract_terms("I AM", 16).is_empty());
        assert_eq!(extract_terms("we asked NASA about it", 16), vec!["NASA".to_string()]);
    }

    #[test]
    fn slash_pairs_and_markdown_links_yield_their_names() {
        let terms = extract_terms("cc Priya/Marcus and [Aoife](http://x.y/z) on and/or this", 16);
        for want in ["Priya", "Marcus", "Aoife"] {
            assert!(terms.contains(&want.to_string()), "{want} missing from {terms:?}");
        }
        assert_eq!(terms.len(), 3, "{terms:?}");
        // Emails stay out (low trust, and an address is not a spelling).
        assert!(extract_terms("beatrix.smith@acme.com", 16).is_empty());
    }
}
