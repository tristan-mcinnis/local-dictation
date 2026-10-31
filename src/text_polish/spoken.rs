//! Spoken forms: the token stream, repeated-fragment collapse, and the
//! spoken "at", "dot" and join normalisers.
// ---------------------------------------------------------------------------
// New deterministic enrichments (rule 2–4). Each is a small pure function, is
// conservative (never drops or re-orders a content word), and is idempotent.
// ---------------------------------------------------------------------------

/// File-extension / public-TLD words that follow a spoken "dot" and join into a
/// literal suffix ("dot js" → ".js", "dot com" → ".com"). Conservative — only
/// these well-known technical tokens, never an arbitrary word after "dot".
pub(super) const EXT_WORDS: &[&str] = &[
    "js", "ts", "rs", "py", "json", "md", "com", "dev", "io", "ai", "org", "net", "html",
    "htm", "css", "pdf", "png", "jpg", "jpeg", "gif", "svg", "xml", "txt", "log", "yml",
    "yaml", "toml", "sh", "go", "rb", "c", "h", "swift", "kt", "java", "m", "cpp", "vue",
    "svelte", "tsx", "jsx", "webp", "wasm", "db", "sql", "lock", "env", "gitignore",
];

/// Repeated words that are (almost always) intentional rather than a stutter:
/// intensifiers/emphatics ("very very"), a true reduplication ("no no"), a
/// grammatical doubling ("that that", "had had"). We keep these as-is; every
/// other immediately-repeated run is collapsed.
pub(super) const INTENTIONAL_REPEATS: &[&str] = &[
    // Intensifiers/emphatics and true reduplications or grammatical doublings.
    // Articles ("the the") and other words are NOT here — a doubled article is
    // a stutter, and we collapse it. Figures are handled separately via
    // [`is_numbery`].
    "very", "really", "no", "that", "had", "so", "too", "quite", "rather",
];

/// Simple cardinal/number words, kept verbatim when repeated ("fifty fifty" —
/// a figure, not a stutter). After `spoken_to_numerals` most cardinals are
/// digits, but ambiguous runs like "fifty fifty" stay as words.
pub(super) const NUMBER_WORDS: &[&str] = &[
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
pub(super) const PROSE_WORDS: &[&str] = &[
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
pub(super) fn join_style(word: &str) -> Option<char> {
    match word.to_lowercase().as_str() {
        "underscore" => Some('_'),
        "dash" | "hyphen" => Some('-'),
        _ => None,
    }
}

/// Returns true if `w` is a cardinal word or already-numeric token — such a
/// repeat is a figure ("fifty fifty") rather than a stutter, so prefer keeping.
pub(super) fn is_numbery(w: &str) -> bool {
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
pub(super) const TECHNICAL_REPEAT_TOKENS: &[&str] = &[
    "dot", "tilde", "slash", "underscore", "dash", "hyphen", "at", "sign",
];

/// Is the whole repeated segment "intentional" (an intensifier, a grammar
/// doubling, a figure, or a technical token sequence)? If so we keep both
/// copies; otherwise collapse. Conservative: prefer allowing, so we only
/// collapse genuinely stutter-like runs.
pub(super) fn is_intentional_repeat(segment: &[&str]) -> bool {
    segment.iter().all(|w| {
        let lower = w.to_lowercase();
        INTENTIONAL_REPEATS.contains(&lower.as_str())
            || is_numbery(&lower)
            || TECHNICAL_REPEAT_TOKENS.contains(&lower.as_str())
    })
}

/// A token from `vtokenize` — a word run of ascii alphanumerics/apostrophes, or
/// a separator run (spaces, punctuation) kept verbatim.
pub(super) enum VTok {
    W(String),
    S(String),
}

impl VTok {
    pub(super) fn text(&self) -> &str {
        match self {
            VTok::W(w) => w.as_str(),
            VTok::S(s) => s.as_str(),
        }
    }
}

/// Split `s` into alternating word / separator segments, preserving everything.
pub(super) fn vtokenize(s: &str) -> Vec<VTok> {
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

pub(super) fn flush_vtok(toks: &mut Vec<VTok>, buf: &mut String, is_word: bool) {
    let s = std::mem::take(buf);
    toks.push(if is_word { VTok::W(s) } else { VTok::S(s) });
}

/// Is `toks[idx]` a word equal (case-insensitively) to `word`?
pub(super) fn tok_is_word(toks: &[VTok], idx: usize, word: &str) -> bool {
    toks.get(idx).is_some_and(|t| matches!(t, VTok::W(w) if w.eq_ignore_ascii_case(word)))
}

/// Is `toks[idx]` a separator made purely of spaces/tabs?
pub(super) fn tok_is_ws(toks: &[VTok], idx: usize) -> bool {
    toks.get(idx).is_some_and(|t| matches!(t, VTok::S(s) if s.chars().all(|c| c == ' ' || c == '\t')))
}

/// If `toks[idx]` is a known extension/TLD word, return its lowercase form.
/// (Used by the "dot ext" rule — so "dot js" → ".js".)
pub(super) fn tok_ext(toks: &[VTok], idx: usize) -> Option<String> {
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
pub(super) fn tok_identish(toks: &[VTok], idx: usize) -> Option<String> {
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
pub(super) fn collapse_repeated_fragments(s: &str) -> String {
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
        out.push_str(toks[i].text());
        i += 1;
    }
    out
}

/// The word indices (into `toks`) of the whitespace-joined word run beginning
/// at `start` (which must be a word token).
pub(super) fn whitespace_run_words(toks: &[VTok], start: usize) -> Vec<usize> {
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
pub(super) fn repeated_run_len(toks: &[VTok], start: usize) -> Option<usize> {
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
pub(super) fn normalize_at(s: &str) -> String {
    let toks = vtokenize(s);
    let n = toks.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        // "word at domain dot tld" → "word@domain.tld" (username at start).
        // Token layout: user(i) ws(i+1) at(i+2) ws(i+3) domain(i+4) ws(i+5)
        // dot(i+6) ws(i+7) tld(i+8).
        if tok_email_user(&toks, i).is_some() && tok_identish(&toks, i + 4).is_some()
            && tok_is_ws(&toks, i + 1)
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
        // "at sign" → "@", but only when it's the symbol phrase, not a prose
        // "look at sign" (the word before "at" must be a function word or absent).
        if tok_is_word(&toks, i, "at")
            && tok_is_ws(&toks, i + 1) && tok_is_word(&toks, i + 2, "sign") {
                let preceded_by_prose = i >= 2 && tok_identish(&toks, i - 2).is_some();
                if !preceded_by_prose {
                    out.push('@');
                    i += 3;
                    continue;
                }
            }
        out.push_str(toks[i].text());
        i += 1;
    }
    out
}

/// A lowercase, non-prose username at the very start of the string (i.e. this
/// is the first word).
pub(super) fn tok_email_user(toks: &[VTok], idx: usize) -> Option<String> {
    if idx != 0 {
        return None;
    }
    tok_identish(toks, idx)
}

/// Rule 3b — "tilde slash" → "~/", "dot dot dot" → "…", and "dot <ext>" →
/// ".<ext>" (joining a preceding word: "react dot js" → "react.js").
pub(super) fn normalize_spoken_dots(s: &str) -> String {
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
        out.push_str(toks[i].text());
        i += 1;
    }
    out
}

/// Rule 3c — "underscore"/"dash"/"hyphen" between two identifier-ish words →
/// "_" / "-". Only in identifier-like context (both flanking words lowercase,
/// non-prose, 2–24 chars), never at sentence level or in ordinary prose.
pub(super) fn normalize_spoken_joins(s: &str) -> String {
    let toks = vtokenize(s);
    let n = toks.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        if tok_is_ws(&toks, i + 1) {
            if let Some(ch) = join_style(toks[i + 2].text()) {
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
        out.push_str(toks[i].text());
        i += 1;
    }
    out
}
