//! Joining streamed ASR segments into one transcript.
/// Join streamed ASR segments into one utterance, repairing the boundary
/// artifact the segmenter creates: the VAD closes a segment at every pause and
/// the ASR punctuates each segment independently, so a pause before the final
/// word yields "…it's like instant." + "connection." — a spurious period that
/// `fix_speech_mechanics` then sentence-capitalizes into "instant. Connection."
/// Words that cannot end an English sentence. When a streamed segment ends on
/// one of these (plus the period the ASR appended), the pause was a hesitation
/// inside the clause, not a sentence boundary.
pub(super) const NON_TERMINAL_TAIL: &[&str] = &[
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
pub(super) const SAFE_TO_LOWER: &[&str] = &[
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

pub(super) fn last_word_lower(s: &str) -> Option<String> {
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

pub(super) fn first_word(s: &str) -> &str {
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
