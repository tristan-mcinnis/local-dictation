//! Deterministic spoken-number → numerals conversion (the reliable half of the
//! numerals-heavy number policy; the cleanup prompt handles the contextual rest).
//!
//! Converts the cases a 1B model does inconsistently but a parser nails:
//!   * decimals / versions: "three point one one" → "3.11", "point five" → "0.5"
//!   * cardinals: "twenty three" → "23", "three hundred and five" → "305",
//!     "fifteen" → "15", standalone "two".."nineteen"/tens → digits
//!
//! Design for SAFETY (high precision, low false-positives):
//!   * A run that doesn't parse as ONE well-formed cardinal is left untouched
//!     (e.g. years "nineteen eighty four", phone numbers) — Gemma/the prompt
//!     handle those; we never emit a wrong sum.
//!   * A lone "one" is left as a word ("one of them" must NOT become "1 of
//!     them"); the prompt converts "one" when context says it's a count.
//!   * Whole-word matching only, so "someone"/"alone" are never touched.
//!
//! Known non-goals (deferred to the prompt / future work): ordinals
//! (first→1st), "oh" as zero, year and phone-number grouping.

/// Convert spoken numbers in `text` to numerals per the rules above. Preserves
/// all surrounding text, punctuation, and spacing verbatim.
pub fn spoken_to_numerals(text: &str) -> String {
    let toks = tokenize(text);
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < toks.len() {
        // Try to consume a number run starting at a number word.
        if let Tok::Word { lower, .. } = &toks[i] {
            if is_number_start(lower) {
                if let Some((rendered, consumed)) = parse_run(&toks[i..]) {
                    out.push_str(&rendered);
                    i += consumed;
                    continue;
                }
            }
        }
        match &toks[i] {
            Tok::Word { orig, .. } => out.push_str(orig),
            Tok::Sep(s) => out.push_str(s),
        }
        i += 1;
    }
    out
}

#[derive(Debug, Clone)]
enum Tok {
    Word { orig: String, lower: String },
    Sep(String),
}

/// Split into alternating word / separator tokens. A "word" is a run of ascii
/// letters or apostrophes; everything else (spaces, digits, punctuation) is a
/// separator token kept verbatim.
fn tokenize(text: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut buf = String::new();
    let mut buf_is_word = false;
    for c in text.chars() {
        let is_word_char = c.is_ascii_alphabetic() || c == '\'';
        if buf.is_empty() {
            buf.push(c);
            buf_is_word = is_word_char;
        } else if is_word_char == buf_is_word {
            buf.push(c);
        } else {
            flush(&mut toks, &buf, buf_is_word);
            buf.clear();
            buf.push(c);
            buf_is_word = is_word_char;
        }
    }
    if !buf.is_empty() {
        flush(&mut toks, &buf, buf_is_word);
    }
    toks
}

fn flush(toks: &mut Vec<Tok>, buf: &str, is_word: bool) {
    if is_word {
        toks.push(Tok::Word { orig: buf.to_string(), lower: buf.to_lowercase() });
    } else {
        toks.push(Tok::Sep(buf.to_string()));
    }
}

fn unit(w: &str) -> Option<u64> {
    Some(match w {
        "zero" => 0, "one" => 1, "two" => 2, "three" => 3, "four" => 4,
        "five" => 5, "six" => 6, "seven" => 7, "eight" => 8, "nine" => 9,
        "ten" => 10, "eleven" => 11, "twelve" => 12, "thirteen" => 13,
        "fourteen" => 14, "fifteen" => 15, "sixteen" => 16, "seventeen" => 17,
        "eighteen" => 18, "nineteen" => 19,
        _ => return None,
    })
}

fn tens(w: &str) -> Option<u64> {
    Some(match w {
        "twenty" => 20, "thirty" => 30, "forty" => 40, "fifty" => 50,
        "sixty" => 60, "seventy" => 70, "eighty" => 80, "ninety" => 90,
        _ => return None,
    })
}

fn scale(w: &str) -> Option<u64> {
    Some(match w {
        "hundred" => 100, "thousand" => 1_000, "million" => 1_000_000,
        "billion" => 1_000_000_000,
        _ => return None,
    })
}

fn single_digit(w: &str) -> Option<u8> {
    unit(w).filter(|&v| v <= 9).map(|v| v as u8)
}

fn is_number_start(w: &str) -> bool {
    unit(w).is_some() || tens(w).is_some() || w == "point"
}

/// Try to parse a number run starting at `toks[0]`. Returns the rendered
/// numeral string and how many tokens (words + interleaving seps) it consumed,
/// or `None` to leave the run untouched. Only "simple" separators (a single
/// space or hyphen) may sit *inside* a run.
fn parse_run(toks: &[Tok]) -> Option<(String, usize)> {
    // Collect the number words and the exact token span, stopping at the first
    // separator that isn't a lone space/hyphen or the first non-number word.
    let mut words: Vec<&str> = Vec::new();
    let mut idx = 0usize;
    loop {
        match toks.get(idx) {
            Some(Tok::Word { lower, .. }) if is_run_word(lower) => {
                words.push(lower.as_str());
                idx += 1;
            }
            Some(Tok::Sep(s)) if (s == " " || s == "-") => {
                // Only keep the separator if the NEXT token continues the run.
                match toks.get(idx + 1) {
                    Some(Tok::Word { lower, .. }) if is_run_word(lower) => idx += 1,
                    _ => break,
                }
            }
            _ => break,
        }
    }
    if words.is_empty() {
        return None;
    }
    // Trim a trailing "point"/"and" with no following digits, then try to render.
    let mut trimmed = words.clone();
    while matches!(trimmed.last(), Some(&"point") | Some(&"and")) {
        trimmed.pop();
    }
    if let Some(value) = trimmed.first().and_then(|_| render_number(&trimmed)) {
        // Consume up to the last KEPT word (a trailing bare "point" stays text).
        return Some((value, span_for_words(toks, trimmed.len())));
    }
    // The run is a sequence of number words that ISN'T one valid cardinal/decimal
    // (year-style "nineteen eighty four", "three four", a lone "one"). Consume the
    // WHOLE run verbatim so we never partially re-convert a sub-run; a single
    // leftover word is handed back to the caller untouched.
    if words.len() <= 1 {
        return None;
    }
    let full_span = span_for_words(toks, words.len());
    let original: String = toks[..full_span].iter().map(tok_str).collect();
    Some((original, full_span))
}

fn tok_str(t: &Tok) -> &str {
    match t {
        Tok::Word { orig, .. } => orig,
        Tok::Sep(s) => s,
    }
}

/// A word that can appear inside a number run.
fn is_run_word(w: &str) -> bool {
    unit(w).is_some() || tens(w).is_some() || scale(w).is_some() || w == "point" || w == "and"
}

/// Token span (words + interior seps) covering the first `n_words` number words.
fn span_for_words(toks: &[Tok], n_words: usize) -> usize {
    let mut seen = 0usize;
    let mut span = 0usize;
    for (i, t) in toks.iter().enumerate() {
        if let Tok::Word { .. } = t {
            seen += 1;
            if seen <= n_words {
                span = i + 1;
            }
            if seen == n_words {
                break;
            }
        }
    }
    span
}

/// Render a validated word list to a numeral string, or `None` if it isn't a
/// single well-formed cardinal/decimal (caller then leaves it untouched).
fn render_number(words: &[&str]) -> Option<String> {
    // Split on "point" for a decimal.
    if let Some(p) = words.iter().position(|&w| w == "point") {
        let int_words = &words[..p];
        let frac_words = &words[p + 1..];
        if frac_words.is_empty() {
            return None;
        }
        // Fractional part is a sequence of single digit words.
        let mut frac = String::new();
        for w in frac_words {
            frac.push(char::from(b'0' + single_digit(w)?));
        }
        let int_part = if int_words.is_empty() {
            0
        } else {
            parse_cardinal(int_words)?
        };
        return Some(format!("{int_part}.{frac}"));
    }

    // Plain cardinal. Refuse to convert a lone "one" (pronoun risk); the prompt
    // decides. Other lone small numbers are safe to convert.
    if words == ["one"] {
        return None;
    }
    let v = parse_cardinal(words)?;
    Some(v.to_string())
}

/// Standard left-to-right cardinal parser. Returns `None` on any ill-formed
/// sequence (so years / phone numbers / "and four" fall through unconverted).
fn parse_cardinal(words: &[&str]) -> Option<u64> {
    let mut total: u64 = 0; // accumulates across thousand/million groups
    let mut group: u64 = 0; // current sub-1000 group
    let mut had_any = false;
    let mut last_was_value = false; // for validating "and" / ordering

    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if w == "and" {
            // "and" only valid right after a scale word, between number parts.
            if !last_was_value {
                return None;
            }
            last_was_value = false;
            i += 1;
            continue;
        }
        if let Some(u) = unit(w) {
            if u < 10 {
                // a plain unit needs an empty units place: ok after a tens word
                // ("twenty three") or a hundred ("hundred five"), not after a
                // unit/teen ("three four" is two numbers).
                if group % 10 != 0 {
                    return None;
                }
            } else {
                // a teen needs empty tens+units: ok alone or after a hundred
                // ("one hundred fifteen"), not after a tens ("twenty fifteen").
                if group % 100 != 0 {
                    return None;
                }
            }
            group += u;
            had_any = true;
            last_was_value = true;
        } else if let Some(t) = tens(w) {
            // a tens word needs the sub-100 part empty: ok alone or after a
            // hundred ("one hundred twenty"), not after another tens/unit.
            if group % 100 != 0 {
                return None;
            }
            group += t;
            had_any = true;
            last_was_value = true;
        } else if let Some(s) = scale(w) {
            if !had_any {
                return None; // scale with nothing before it ("hundred")
            }
            if s == 100 {
                if group == 0 || group >= 100 {
                    return None;
                }
                group *= 100;
            } else {
                // thousand / million / billion: fold the current group in.
                let g = if group == 0 { 1 } else { group };
                total += g * s;
                group = 0;
            }
            last_was_value = true;
        } else {
            return None;
        }
        i += 1;
    }
    if !had_any {
        return None;
    }
    Some(total + group)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(s: &str) -> String {
        spoken_to_numerals(s)
    }

    #[test]
    fn decimals_and_versions() {
        assert_eq!(conv("three point one one"), "3.11");
        assert_eq!(conv("version three point one one"), "version 3.11");
        assert_eq!(conv("pi is three point one four"), "pi is 3.14");
        assert_eq!(conv("point five"), "0.5");
        assert_eq!(conv("zero point five"), "0.5");
    }

    #[test]
    fn cardinals() {
        assert_eq!(conv("twenty three"), "23");
        assert_eq!(conv("fifteen"), "15");
        assert_eq!(conv("three hundred and five"), "305");
        assert_eq!(conv("one hundred twenty three"), "123");
        assert_eq!(conv("two thousand twenty four"), "2024");
        assert_eq!(conv("ten"), "10");
    }

    #[test]
    fn lone_one_is_left_for_the_prompt() {
        assert_eq!(conv("one of them"), "one of them");
        assert_eq!(conv("no one knows"), "no one knows");
        // but "one" inside a compound converts
        assert_eq!(conv("one hundred"), "100");
    }

    #[test]
    fn ambiguous_runs_are_left_untouched() {
        // year-style: not a single well-formed cardinal → leave it
        assert_eq!(conv("nineteen eighty four"), "nineteen eighty four");
        // two separate small numbers → leave (we don't guess spacing)
        assert_eq!(conv("three four"), "three four");
        assert_eq!(conv("twenty thirty"), "twenty thirty");
    }

    #[test]
    fn whole_word_only_and_text_preserved() {
        assert_eq!(conv("someone alone"), "someone alone"); // contains 'one'
        assert_eq!(conv("I have fifteen apples."), "I have 15 apples.");
        assert_eq!(conv("hello"), "hello");
        assert_eq!(conv(""), "");
    }

    #[test]
    fn punctuation_breaks_runs() {
        // a comma between number words breaks the cardinal run: 20 and 3 are
        // two separate numbers, NOT 23 (both still convert under numerals-heavy).
        assert_eq!(conv("twenty, three"), "20, 3");
    }

    #[test]
    fn standalone_small_numbers_convert() {
        assert_eq!(conv("give me five"), "give me 5");
        assert_eq!(conv("two of them"), "2 of them");
    }
}
