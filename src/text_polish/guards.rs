//! Guards around the cleanup model: did it drop content, and does this text
//! need it at all.
use super::*;

/// Discourse / hedge / contraction words (≥4 chars) that legitimate cleanup
/// routinely drops or rewrites ("like", "you know", "gonna" → "going to"). They
/// must NOT count as dropped *content* when judging whether the model gutted a
/// clause, or heavy-but-correct filler removal would trip the guard.
pub(super) const DROPPABLE_DISCOURSE: &[&str] = &[
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
