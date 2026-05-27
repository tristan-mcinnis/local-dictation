//! Wake-word detection for the always-on listen mode.
//!
//! This is the *decision* primitive only — pure string work, no model and no
//! audio. The daemon's listen loop transcribes each VAD-bounded speech segment
//! with Parakeet (the model already loaded for push-to-talk), then asks this
//! module: "does this segment open with the wake word, and if so what's the
//! dictation body?" Keeping the decision deterministic and model-free means the
//! always-on path never pays a second model on the hot loop, and the matching
//! rules are unit-testable without a mic (per the project's design principle:
//! cheapest primitive that works, tested before it ships).
//!
//! Matching is deliberately forgiving because it runs on raw ASR text, not
//! clean typing:
//!   * case- and punctuation-insensitive ("Computer," == "computer")
//!   * an optional polite lead-in ("hey", "ok", "okay", "yo") may precede the
//!     wake word, so "hey computer open settings" works with wake word
//!     "computer"
//!   * each wake-word token tolerates a single-character ASR slip for tokens of
//!     four or more characters ("computers" still matches "computer"), so the
//!     trigger doesn't depend on Parakeet spelling the magic word perfectly
//!
//! The body returned is the *original* text after the matched span (original
//! casing preserved, leading punctuation/space trimmed) so it flows into the
//! normal cleanup + inject pipeline unchanged.

/// A successful wake-word match: the spoken text that followed the wake word.
/// Empty `body` means the user said only the wake word (e.g. to "arm" dictation
/// for the next segment) — the caller decides what that means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeMatch {
    /// Dictation text after the wake word, original casing, trimmed.
    pub body: String,
}

/// Polite lead-ins allowed before the wake word ("hey computer ...").
const LEAD_INS: &[&str] = &["hey", "ok", "okay", "yo", "hi", "hello"];

/// Lowercased alphanumeric tokens of `s`, each with its byte range in the
/// original string. Punctuation and whitespace are separators and are dropped;
/// the ranges let us recover the original (cased, punctuated) tail after a match.
fn tokenize(s: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ch) in s.char_indices() {
        if ch.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(st) = start.take() {
            out.push((s[st..i].to_lowercase(), st..i));
        }
    }
    if let Some(st) = start {
        out.push((s[st..].to_lowercase(), st..s.len()));
    }
    out
}

/// Levenshtein distance, capped early at 2 (we only ever care about ≤1).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return 2;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Fuzzy token equality: exact for short tokens, one-edit slack for tokens of
/// four or more characters (absorbs ASR plural/tense slips on the magic word).
fn token_matches(spoken: &str, want: &str) -> bool {
    if spoken == want {
        return true;
    }
    want.len() >= 4 && edit_distance(spoken, want) <= 1
}

/// Detect a leading wake word in `transcript`. Returns the dictation body
/// (text after the wake word) when the segment opens with the wake word
/// (optionally after a polite lead-in); `None` otherwise. `wake_word` may be
/// multiple words ("hey jarvis"); blank/whitespace wake words never match.
pub fn detect(transcript: &str, wake_word: &str) -> Option<WakeMatch> {
    let want: Vec<String> = tokenize(wake_word).into_iter().map(|(t, _)| t).collect();
    if want.is_empty() {
        return None;
    }
    let spoken = tokenize(transcript);
    if spoken.is_empty() {
        return None;
    }

    // Try matching the wake phrase at token 0, or after a single lead-in token
    // when that lead-in isn't itself the first word of the wake phrase.
    let mut starts = vec![0usize];
    if spoken.len() > 1 && LEAD_INS.contains(&spoken[0].0.as_str()) && spoken[0].0 != want[0] {
        starts.push(1);
    }

    for &start in &starts {
        if start + want.len() > spoken.len() {
            continue;
        }
        let matched = want
            .iter()
            .enumerate()
            .all(|(k, w)| token_matches(&spoken[start + k].0, w));
        if !matched {
            continue;
        }
        // Body = original text after the last matched token, with leading
        // punctuation/space stripped. Preserves the body's casing untouched.
        let end = spoken[start + want.len() - 1].1.end;
        let body = transcript[end..]
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        return Some(WakeMatch { body });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(t: &str, w: &str) -> Option<String> {
        detect(t, w).map(|m| m.body)
    }

    #[test]
    fn basic_match_strips_wake_word_and_punctuation() {
        assert_eq!(
            body("Computer, open the settings", "computer").as_deref(),
            Some("open the settings")
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(body("COMPUTER send it", "computer").as_deref(), Some("send it"));
        assert_eq!(body("computer Send It", "Computer").as_deref(), Some("Send It"));
    }

    #[test]
    fn optional_lead_in_before_wake_word() {
        assert_eq!(body("hey computer what time is it", "computer").as_deref(), Some("what time is it"));
        assert_eq!(body("okay, computer. begin", "computer").as_deref(), Some("begin"));
    }

    #[test]
    fn multi_word_wake_phrase() {
        assert_eq!(body("hey jarvis turn on the lights", "hey jarvis").as_deref(), Some("turn on the lights"));
        // lead-in logic must not double-count "hey" that is part of the phrase
        assert_eq!(body("Hey Jarvis, dictate this", "hey jarvis").as_deref(), Some("dictate this"));
    }

    #[test]
    fn fuzzy_absorbs_one_char_asr_slip() {
        // Parakeet hears a trailing plural — still triggers.
        assert_eq!(body("computers open the door", "computer").as_deref(), Some("open the door"));
    }

    #[test]
    fn no_match_when_wake_word_absent() {
        assert_eq!(body("just some normal dictation", "computer"), None);
    }

    #[test]
    fn no_match_when_wake_word_is_mid_sentence() {
        // Wake word must lead the segment, not appear in the middle.
        assert_eq!(body("please tell the computer to stop", "computer"), None);
    }

    #[test]
    fn bare_wake_word_yields_empty_body() {
        assert_eq!(body("Computer.", "computer").as_deref(), Some(""));
    }

    #[test]
    fn blank_wake_word_never_matches() {
        assert_eq!(body("computer do something", "   "), None);
        assert_eq!(body("computer do something", ""), None);
    }

    #[test]
    fn short_wake_word_requires_exact_token() {
        // 3-char wake word "abe" must not fuzzy-match "ace" (no slack for short).
        assert_eq!(body("ace do it", "abe"), None);
        assert_eq!(body("abe do it", "abe").as_deref(), Some("do it"));
    }

    #[test]
    fn empty_transcript() {
        assert_eq!(body("", "computer"), None);
    }
}
