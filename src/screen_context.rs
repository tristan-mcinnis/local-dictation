//! Screen-context vocabulary harvesting.
//!
//! The cleanup model spells proper nouns blind: it has no idea the person you're
//! dictating about is "Lingzi" not "Lindsay", or that the project is "Parakeet".
//! But the correct spelling is usually *already on screen* — in the text field
//! you're editing and the window title. This module turns that visible text into
//! a short "known vocabulary" list fed to the cleanup prompt (via
//! [`crate::prompts::vocabulary_suffix`]), so Gemma preserves on-screen spellings
//! instead of substituting similar-sounding common words.
//!
//! [`extract_terms`] is the pure, dependency-free core (unit-tested here). The AX
//! read that produces the raw screen text lives in `injector.rs` (it shares the
//! same Accessibility plumbing as injection) and is gated behind `ax-inject`.

use std::collections::HashSet;

/// Cap on how much on-screen text we scan. A focused field can hold a whole
/// document; we only need a representative sample of the visible proper nouns,
/// and this keeps extraction cheap even though it already runs off the critical
/// path (in the parallel focus-capture thread).
const MAX_SCAN_CHARS: usize = 8_000;

/// Common English words that are frequently capitalised (sentence starts,
/// pronouns, function words). They're never the proper nouns the ASR mangles,
/// so they'd only waste slots in the capped vocabulary list.
const STOPWORDS: &[&str] = &[
    "the", "this", "that", "these", "those", "and", "but", "or", "for", "with",
    "from", "into", "your", "you", "our", "their", "his", "her", "its", "we",
    "they", "he", "she", "it", "a", "an", "of", "to", "in", "on", "at", "is",
    "are", "was", "were", "be", "been", "being", "will", "would", "could",
    "should", "can", "may", "might", "must", "what", "when", "where", "who",
    "whom", "why", "how", "here", "there", "then", "now", "yes", "no", "ok",
    "okay", "so", "well", "just", "like", "also", "if", "as", "by", "do",
    "does", "did", "not", "all", "any", "some", "more", "most", "such", "than",
    "too", "very", "i'm", "i'll", "i've", "i'd", "we're", "don't", "it's",
    "let's", "that's", "there's", "hello", "hi", "thanks", "thank",
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
    let end_b = if end_ci >= n { text.len() } else { chars[end_ci].0 };
    &text[start_b..end_b]
}

/// Extract up to `cap` proper-noun / domain-term candidates from a blob of
/// on-screen text, preferring tokens the ASR is most likely to get wrong
/// (mixed-case like `macOS`, identifiers, then plain proper nouns). The
/// on-screen casing is preserved verbatim — that's the spelling we want the
/// model to keep. Deterministic: stable order, case-insensitive dedup.
pub fn extract_terms(text: &str, cap: usize) -> Vec<String> {
    if cap == 0 {
        return Vec::new();
    }
    let mut seen: HashSet<String> = HashSet::new();
    // (score, insertion_index, term) — sort by score desc, then index asc for a
    // deterministic tie-break that keeps document order among equal-score terms.
    let mut scored: Vec<(i32, usize, String)> = Vec::new();

    for raw in text.chars().take(MAX_SCAN_CHARS).collect::<String>().split_whitespace() {
        let Some(tok) = trim_token(raw) else { continue };
        if !is_candidate(tok) {
            continue;
        }
        let key = tok.to_lowercase();
        if STOPWORDS.contains(&key.as_str()) {
            continue;
        }
        if !seen.insert(key) {
            continue;
        }
        let idx = scored.len();
        scored.push((score(tok), idx, tok.to_string()));
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(cap).map(|(_, _, t)| t).collect()
}

/// Strip surrounding punctuation/brackets and a trailing possessive (`'s`) from
/// a whitespace-split token. Returns `None` if nothing meaningful is left.
fn trim_token(raw: &str) -> Option<&str> {
    let t = raw.trim_matches(|c: char| !c.is_alphanumeric());
    if t.is_empty() {
        return None;
    }
    // Drop a trailing possessive so "Lingzi's" contributes "Lingzi".
    let t = t.strip_suffix("'s").or_else(|| t.strip_suffix("'S")).unwrap_or(t);
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// A token worth feeding to the model: 2–40 chars, contains a letter, isn't a
/// URL/path/email, and looks like a proper noun or domain term (initial capital,
/// internal capital, or an identifier mixing letters with digits/`_`/`.`/`-`).
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
    let first_upper = tok.chars().next().is_some_and(|c| c.is_uppercase());
    let internal_upper = tok.chars().skip(1).any(|c| c.is_uppercase());
    let has_digit = tok.chars().any(|c| c.is_ascii_digit());
    let identifier_punct = tok.contains('_') || tok.contains('.') || tok.contains('-');
    first_upper || internal_upper || (has_digit) || identifier_punct
}

/// Rank candidates so the strongest "ASR will mangle this" signals win the
/// limited slots: mixed-case casing first, then identifiers/digits, then plain
/// capitalised proper nouns, with a small bonus for longer (rarer) tokens.
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
    s += (tok.chars().count() as i32 - 4).clamp(0, 6);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulls_proper_nouns_and_domain_terms() {
        let text = "I talked to Lingzi about the Parakeet model on macOS \
                    and we'll push it to GitHub after standup.";
        let terms = extract_terms(text, 16);
        assert!(terms.contains(&"Lingzi".to_string()));
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
        assert_eq!(terms.iter().filter(|t| t.eq_ignore_ascii_case("macos")).count(), 1);
        assert_eq!(terms[0], "macOS");
    }

    #[test]
    fn strips_punctuation_and_possessive() {
        let terms = extract_terms("(Parakeet), \"GitHub\" — Lingzi's repo", 16);
        assert!(terms.contains(&"Parakeet".to_string()));
        assert!(terms.contains(&"GitHub".to_string()));
        assert!(terms.contains(&"Lingzi".to_string()));
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
        let text = "FarawayProject discussion ... talking about Lingzi and Parakeet here";
        let caret = text.find("Lingzi").unwrap_or(0);
        // char index of the caret (find returns a byte index; ASCII here so equal)
        let win = window_around(text, caret, 20);
        let terms = extract_terms(win, 16);
        assert!(terms.contains(&"Lingzi".to_string()));
        assert!(terms.contains(&"Parakeet".to_string()));
        assert!(!terms.contains(&"FarawayProject".to_string()), "got {terms:?}");
    }
}
