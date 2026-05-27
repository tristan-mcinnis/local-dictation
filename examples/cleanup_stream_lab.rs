//! Cleanup-stream lab (ROUND 2) — can we hide Gemma cleanup latency on long
//! dictations WITHOUT wrecking quality or the dictionary?
//!
//! Round 1 (examples/streaming_lab.rs) proved the post-release wall is the
//! cleanup model, not ASR: Gemma regenerates the whole transcript, so its decode
//! time scales with length (~6.8s on a 143s clip). Naively chunking cleanup
//! cratered quality (37.6% divergence). This lab tests the fix in TEXT SPACE
//! (no audio): clean COMPLETED SENTENCES during the hold, each with the prior
//! cleaned text as left-context, so by release only the last sentence is unclean.
//!
//! ```bash
//! cargo run --release --features cleaner --example cleanup_stream_lab
//! ```
//!
//! Corpus: prompts-lab/cleanup_stream.json — GOLD is real history.db text
//! (verbatim), RAW is a de-cleaned messy version with dictionary names corrupted
//! so corrections + screen-vocab are actually exercised. Segmentation is ORACLE
//! (gold sentence boundaries) = the quality CEILING; real segmentation is round 3.
//!
//! Strategies compared per entry:
//!   * whole               — one cleanup call on the full raw (today's behaviour)
//!   * per-sentence (none)  — clean each fragment independently (the naive path)
//!   * per-sentence (+ctx)  — clean each fragment with prior CLEANED text as
//!                            left-context (the hypothesis)
//!   * word-window (+ctx)   — same, but segment by ~25-word windows ignoring
//!                            sentence boundaries (the realistic FLOOR absent VAD)
//! Metrics: WER vs gold, WER vs whole-output, name-recall (dictionary test),
//! total latency, residual-at-release (last fragment only), output chars (≈token cost).

#[cfg(not(feature = "cleaner"))]
fn main() {
    eprintln!("cleanup_stream_lab needs the `cleaner` feature: cargo run --release --features cleaner --example cleanup_stream_lab");
    std::process::exit(1);
}

#[cfg(feature = "cleaner")]
use serde::Deserialize;

#[cfg(feature = "cleaner")]
#[derive(Deserialize)]
struct Corpus {
    entries: Vec<Entry>,
}

#[cfg(feature = "cleaner")]
#[derive(Deserialize)]
struct Entry {
    name: String,
    tier: String,
    #[serde(default)]
    vocab: Vec<String>,
    #[serde(default)]
    names: Vec<String>,
    fragments: Vec<Fragment>,
}

#[cfg(feature = "cleaner")]
#[derive(Deserialize)]
struct Fragment {
    raw: String,
    gold: String,
}

#[cfg(feature = "cleaner")]
struct Outcome {
    text: String,
    total_ms: f64,
    /// Latency unavoidable AFTER key release (whole = all of it; incremental =
    /// only the final fragment, since earlier ones finish during the hold).
    residual_ms: f64,
}

#[cfg(feature = "cleaner")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    use fast_dictate_backend::app_paths;
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::settings::Settings;
    use std::time::Instant;

    let corpus: Corpus =
        serde_json::from_str(&std::fs::read_to_string("prompts-lab/cleanup_stream.json")?)?;

    let gemma = Settings::load().resolve_gemma(&app_paths::gemma_default_path());
    eprintln!("[lab] gemma: {gemma}");
    let t = Instant::now();
    let engine = TextCleanupEngine::initialize(&gemma)?;
    eprintln!("[lab] loaded in {:?}", t.elapsed());

    // Production applies the user's corrections dictionary AFTER cleanup
    // (refiner.refine → corrections.apply). It's a verbatim, position-independent
    // word substitution, so it fixes names identically whether the text was
    // cleaned whole or per-fragment. We mirror it here so the dictionary test is
    // faithful — the gold (real history) is already post-corrections.
    let corrections = fast_dictate_backend::corrections::Corrections::load_default()
        .unwrap_or_else(|_| fast_dictate_backend::corrections::Corrections::from_map(Default::default()));
    let fin = |o: Outcome| Outcome { text: corrections.apply(&o.text), ..o };
    eprintln!("[lab] corrections loaded\n");

    // warm
    let _ = run_whole(&engine, "warm up the cleaner with a short sentence", &[]).await?;

    let mut agg: Vec<Agg> = Vec::new();

    for e in &corpus.entries {
        let full_raw = e
            .fragments
            .iter()
            .map(|f| f.raw.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let full_gold = e
            .fragments
            .iter()
            .map(|f| f.gold.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let sentences: Vec<String> = e.fragments.iter().map(|f| f.raw.clone()).collect();
        let windows = word_windows(&full_raw, 25);

        let whole = fin(run_whole(&engine, &full_raw, &e.vocab).await?);
        let per_none = fin(run_incremental(&engine, &sentences, &e.vocab, false).await?);
        let per_ctx = fin(run_incremental(&engine, &sentences, &e.vocab, true).await?);
        let win_ctx = fin(run_incremental(&engine, &windows, &e.vocab, true).await?);

        println!("\n══════════ {} [{}]  ({} frags, {} chars raw) ══════════",
            e.name, e.tier, e.fragments.len(), full_raw.len());
        let gnorm = normalize(&full_gold);
        let wnorm = normalize(&whole.text);
        let rows = [
            ("whole", &whole),
            ("per-sent (none)", &per_none),
            ("per-sent (+ctx)", &per_ctx),
            ("word-win (+ctx)", &win_ctx),
        ];
        println!("{:<17} {:>9} {:>11} {:>9} {:>9} {:>8} {:>7}",
            "strategy", "WERvGold", "WERvWhole", "names", "total_ms", "resid", "chars");
        for (label, o) in rows {
            let onorm = normalize(&o.text);
            let wer_gold = wer(&gnorm, &onorm) * 100.0;
            let wer_whole = if label == "whole" { 0.0 } else { wer(&wnorm, &onorm) * 100.0 };
            let (found, total) = name_recall(&o.text, &e.names);
            println!("{:<17} {:>8.1}% {:>10.1}% {:>6}/{:<2} {:>9.1} {:>8.1} {:>7}",
                label, wer_gold, wer_whole, found, total, o.total_ms, o.residual_ms, o.text.chars().count());
            agg.push(Agg {
                tier: e.tier.clone(),
                strategy: label.to_string(),
                wer_gold,
                wer_whole,
                names_found: found,
                names_total: total,
                total_ms: o.total_ms,
                residual_ms: o.residual_ms,
            });
        }
        // Show the actual long-clip outputs so quality is eyeballable, not just scored.
        if e.tier == "long" {
            println!("  gold      : {:?}", truncate(&full_gold, 200));
            println!("  whole     : {:?}", truncate(&whole.text, 200));
            println!("  per+ctx   : {:?}", truncate(&per_ctx.text, 200));
            println!("  per-none  : {:?}", truncate(&per_none.text, 200));
        }
        // Name failures are the dictionary test — surface them explicitly.
        for (label, o) in [("per-sent (+ctx)", &per_ctx), ("whole", &whole)] {
            let missing: Vec<&String> = e.names.iter().filter(|n| !o.text.contains(n.as_str())).collect();
            if !missing.is_empty() {
                println!("  ⚠ {label} dropped names: {missing:?}");
            }
        }
    }

    // ---- summary by tier × strategy ----
    println!("\n\n══════════ SUMMARY (mean by tier × strategy) ══════════");
    println!("{:<8} {:<17} {:>9} {:>10} {:>9} {:>10} {:>9}",
        "tier", "strategy", "WERvGold", "WERvWhole", "name%", "total_ms", "resid_ms");
    for tier in ["short", "medium", "long"] {
        for strat in ["whole", "per-sent (none)", "per-sent (+ctx)", "word-win (+ctx)"] {
            let rows: Vec<&Agg> = agg.iter().filter(|a| a.tier == tier && a.strategy == strat).collect();
            if rows.is_empty() {
                continue;
            }
            let n = rows.len() as f64;
            let mean = |f: &dyn Fn(&Agg) -> f64| rows.iter().map(|a| f(a)).sum::<f64>() / n;
            let nf: usize = rows.iter().map(|a| a.names_found).sum();
            let nt: usize = rows.iter().map(|a| a.names_total).sum();
            let name_pct = if nt == 0 { f64::NAN } else { 100.0 * nf as f64 / nt as f64 };
            println!("{:<8} {:<17} {:>8.1}% {:>9.1}% {:>8} {:>10.1} {:>9.1}",
                tier, strat,
                mean(&|a| a.wer_gold),
                mean(&|a| a.wer_whole),
                if name_pct.is_nan() { "n/a".to_string() } else { format!("{name_pct:.0}%") },
                mean(&|a| a.total_ms),
                mean(&|a| a.residual_ms));
        }
    }
    println!("\nKEY: resid_ms for per-sent/word-win = latency left AFTER release (earlier\n     fragments cleaned during the hold). Compare to `whole` total_ms = today.\n     WERvWhole = how far incremental drifts from current behaviour (0 = same).\n     name% = dictionary targets correctly spelled (the corrections/vocab test).");

    Ok(())
}

// Build the system prompt the way production does: base instructions + the
// known-vocabulary block (corrections + screen terms).
#[cfg(feature = "cleaner")]
fn base_prompt(vocab: &[String]) -> String {
    use fast_dictate_backend::prompts::{vocabulary_suffix, DEFAULT_CLEANUP};
    let mut p = DEFAULT_CLEANUP.to_string();
    if let Some(suffix) = vocabulary_suffix(vocab) {
        p.push_str(&suffix);
    }
    p
}

// The +context variant tells the model what's already been cleaned (tail only)
// so it keeps casing/flow consistent and cleans ONLY the new fragment.
#[cfg(feature = "cleaner")]
fn ctx_prompt(vocab: &[String], ctx: &str) -> String {
    let mut p = base_prompt(vocab);
    if !ctx.trim().is_empty() {
        let chars: Vec<char> = ctx.chars().collect();
        let start = chars.len().saturating_sub(240);
        let tail: String = chars[start..].iter().collect();
        p.push_str(&format!(
            "\n\nThe transcript so far has already been cleaned as:\n\"\"\"\n…{tail}\n\"\"\"\nClean ONLY the new fragment below and output just the cleaned new fragment — do not repeat the text above."
        ));
    }
    p
}

// All strategies go through eval_cleanup so the only variable is
// segmentation/context (eval_cleanup mirrors production framing + budget).
#[cfg(feature = "cleaner")]
async fn run_whole(
    engine: &fast_dictate_backend::cleaner::TextCleanupEngine,
    raw: &str,
    vocab: &[String],
) -> eyre::Result<Outcome> {
    use std::time::Instant;
    let prompt = base_prompt(vocab);
    let t = Instant::now();
    let text = engine.eval_cleanup(&prompt, raw, false).await?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    Ok(Outcome { text, total_ms: ms, residual_ms: ms })
}

#[cfg(feature = "cleaner")]
async fn run_incremental(
    engine: &fast_dictate_backend::cleaner::TextCleanupEngine,
    segments: &[String],
    vocab: &[String],
    use_ctx: bool,
) -> eyre::Result<Outcome> {
    use std::time::Instant;
    let mut cleaned_so_far = String::new();
    let mut total_ms = 0.0;
    let mut last_ms = 0.0;
    let mut pieces: Vec<String> = Vec::new();
    for seg in segments {
        if seg.trim().is_empty() {
            continue;
        }
        let prompt = if use_ctx {
            ctx_prompt(vocab, &cleaned_so_far)
        } else {
            base_prompt(vocab)
        };
        let t = Instant::now();
        let piece = engine.eval_cleanup(&prompt, seg, false).await?;
        last_ms = t.elapsed().as_secs_f64() * 1000.0;
        total_ms += last_ms;
        if !cleaned_so_far.is_empty() {
            cleaned_so_far.push(' ');
        }
        cleaned_so_far.push_str(piece.trim());
        pieces.push(piece.trim().to_string());
    }
    Ok(Outcome { text: pieces.join(" "), total_ms, residual_ms: last_ms })
}

#[cfg(feature = "cleaner")]
struct Agg {
    tier: String,
    strategy: String,
    wer_gold: f64,
    wer_whole: f64,
    names_found: usize,
    names_total: usize,
    total_ms: f64,
    residual_ms: f64,
}

/// Segment a raw string into ~n-word windows (realistic streaming trigger absent VAD).
#[cfg(feature = "cleaner")]
fn word_windows(raw: &str, n: usize) -> Vec<String> {
    let words: Vec<&str> = raw.split_whitespace().collect();
    if words.is_empty() {
        return vec![];
    }
    words.chunks(n).map(|c| c.join(" ")).collect()
}

/// How many of the expected target spellings appear verbatim in the output.
#[cfg(feature = "cleaner")]
fn name_recall(text: &str, names: &[String]) -> (usize, usize) {
    let found = names.iter().filter(|n| text.contains(n.as_str())).count();
    (found, names.len())
}

#[cfg(feature = "cleaner")]
fn normalize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|w| w.to_string())
        .collect()
}

#[cfg(feature = "cleaner")]
fn wer(reference: &[String], hyp: &[String]) -> f64 {
    let n = reference.len();
    let m = hyp.len();
    if n == 0 {
        return if m == 0 { 0.0 } else { 1.0 };
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if reference[i - 1] == hyp[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m] as f64 / n as f64
}

#[cfg(feature = "cleaner")]
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
