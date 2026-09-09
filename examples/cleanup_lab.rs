//! Cleanup lab — run the *production* cleanup decision over a corpus of real
//! raw transcripts and report what actually changes.
//!
//! Answers the questions the daemon log cannot: does the LLM ever run, what
//! does it do when it does, and does the diff-guard catch it when it eats a
//! clause. The gating below mirrors `daemon/worker.rs` exactly (terminal skip
//! aside, which is a property of the target app, not the text).
//!
//! ```bash
//! cargo run --release --features full --example cleanup_lab -- /tmp/raw_corpus.json
//! ```
//!
//! The corpus is a JSON array of raw transcript strings. Prints a per-path
//! tally, then every case where the LLM changed the text, before and after.

#[cfg(not(feature = "cleaner"))]
fn main() {
    eprintln!("cleanup_lab needs the `cleaner` feature: cargo run --release --features full --example cleanup_lab");
    std::process::exit(1);
}

#[cfg(feature = "cleaner")]
#[tokio::main]
async fn main() -> eyre::Result<()> {
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::text_polish;
    use std::time::Instant;

    let path = std::env::args().nth(1).unwrap_or("/tmp/raw_corpus.json".into());
    let corpus: Vec<String> = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let llm = fast_dictate_backend::settings::Settings::load()
        .resolve_llm_model(&fast_dictate_backend::app_paths::llm_default_path());
    eprintln!("[lab] corpus {} utterances · llm {llm}", corpus.len());
    let cleaner = TextCleanupEngine::initialize(&llm)?;

    // Words the cleanup prompt is *supposed* to remove. Anything else that
    // disappears is content the speaker said and did not get back.
    const FILLER: &[&str] = &[
        "um", "uh", "erm", "ah", "like", "basically", "literally", "seriously",
        "honestly", "you", "know", "i", "mean", "sort", "of", "kind", "just",
        "really", "actually", "so", "well", "yeah", "gonna", "going", "to",
        "wanna", "want", "gotta", "got", "kinda", "sorta", "a", "the",
        // Spelled numbers are deliberately rewritten to digits, so their
        // disappearance is the feature, not a loss.
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight",
        "nine", "ten", "eleven", "twelve", "twenty", "thirty", "forty", "fifty",
        "hundred", "thousand", "first", "second", "third",
    ];
    fn dropped_content(raw: &str, cleaned: &str) -> Vec<String> {
        let norm = |s: &str| -> Vec<String> {
            s.to_lowercase()
                .split(|c: char| !c.is_alphanumeric() && c != '\'')
                .filter(|w| !w.is_empty())
                .map(|w| w.to_string())
                .collect()
        };
        let mut after: Vec<String> = norm(cleaned);
        let mut out = Vec::new();
        for w in norm(raw) {
            match after.iter().position(|x| *x == w) {
                Some(i) => {
                    after.remove(i);
                }
                None => {
                    if !FILLER.contains(&w.as_str()) {
                        out.push(w);
                    }
                }
            }
        }
        out
    }

    let (mut too_short, mut clean_enough, mut llm_ran) = (0, 0, 0);
    let (mut drop_events, mut dropped_words) = (0usize, 0usize);
    let mut worst: Vec<(usize, String, String, Vec<String>)> = Vec::new();
    let (mut llm_changed, mut llm_identical, mut guard_fired) = (0, 0, 0);
    let mut total_ms = 0_u128;
    let mut changes: Vec<(String, String, String)> = Vec::new();

    for raw in &corpus {
        let word_count = raw.split_whitespace().count();
        let deterministic = text_polish::fix_speech_mechanics(raw);
        // Same two conditions as daemon/worker.rs, with no output format active.
        if word_count < 3 {
            too_short += 1;
            continue;
        }
        if !text_polish::needs_llm_cleanup(raw) {
            clean_enough += 1;
            continue;
        }
        llm_ran += 1;
        let t = Instant::now();
        let cleaned = cleaner.process_transcript_with_vocab_tone(raw, &[], None).await?;
        total_ms += t.elapsed().as_millis();
        let lost = dropped_content(raw, &cleaned);
        if !lost.is_empty() {
            drop_events += 1;
            dropped_words += lost.len();
            worst.push((lost.len(), raw.clone(), cleaned.clone(), lost));
        }
        if text_polish::cleanup_dropped_content(raw, &cleaned) {
            guard_fired += 1;
            changes.push((raw.clone(), cleaned, "GUARD → deterministic".into()));
        } else if cleaned.trim() == deterministic.trim() || cleaned.trim() == raw.trim() {
            llm_identical += 1;
        } else {
            llm_changed += 1;
            changes.push((raw.clone(), cleaned, "used".into()));
        }
    }

    println!("\n=== path taken ({} utterances) ===", corpus.len());
    println!("  under 3 words → deterministic only : {too_short}");
    println!("  looked clean  → deterministic only : {clean_enough}");
    println!("  sent to the LLM                    : {llm_ran}");
    if llm_ran > 0 {
        println!("      LLM output used (changed text) : {llm_changed}");
        println!("      LLM output identical to input  : {llm_identical}");
        println!("      diff-guard rejected the output : {guard_fired}");
        println!("      mean LLM latency               : {} ms", total_ms / llm_ran as u128);
        println!("\n=== content loss (non-filler words the LLM deleted) ===");
        println!("  utterances that lost a content word : {drop_events} of {llm_ran}");
        println!("  content words lost in total         : {dropped_words}");
        worst.sort_by(|a, b| b.0.cmp(&a.0));
        for (n, raw, cleaned, lost) in worst.iter().take(5) {
            println!("\n  -{n}: {lost:?}");
            println!("   B: {}", &raw[..raw.len().min(4000)]);
            println!("   A: {}", &cleaned[..cleaned.len().min(4000)]);
        }
    }

    println!("\n=== every LLM edit ===");
    for (i, (raw, cleaned, verdict)) in changes.iter().enumerate() {
        println!("\n--- {} [{verdict}]", i + 1);
        println!("BEFORE: {raw}");
        println!("AFTER : {cleaned}");
    }
    Ok(())
}
