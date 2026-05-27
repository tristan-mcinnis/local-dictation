//! Cleanup-model bake-off — run one model (set via GEMMA_MODEL_PATH) over a
//! curated case set with a "full-rules" cleanup prompt (base cleanup + numbers +
//! auto-quotation), printing per-case output, a simple pass flag, and latency.
//! Run once per candidate model and compare; answers "does a bigger model do the
//! hard contextual features (quotation, self-correction, no fragment-completion)
//! WITHOUT regressing casing — and at what latency cost?".
//!
//! ```bash
//! for m in gemma-3-1b-it/gemma-3-1b-it-Q4_K_M qwen-2.5-1.5b-it/qwen2.5-1.5b-instruct-q4_k_m \
//!          qwen-2.5-3b-it/qwen2.5-3b-instruct-q4_k_m llama-3.2-3b-it/Llama-3.2-3B-Instruct-Q4_K_M \
//!          gemma-3-4b-it/gemma-3-4b-it-Q4_K_M; do
//!   GEMMA_MODEL_PATH=models/llm/$m.gguf cargo run --release --features cleaner --example model_bakeoff
//! done
//! ```

#[cfg(not(feature = "cleaner"))]
fn main() {
    eprintln!("model_bakeoff needs --features cleaner");
    std::process::exit(1);
}

#[cfg(feature = "cleaner")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    use fast_dictate_backend::app_paths;
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::prompts::DEFAULT_CLEANUP;
    use fast_dictate_backend::settings::Settings;
    use std::time::Instant;

    // base cleanup + numbers rule + auto-quotation rule (the "everything" prompt).
    let prompt = format!(
        "{DEFAULT_CLEANUP}\n\
         - Write spoken numbers as numerals (3, 23, 3.11), but keep 'one' as a word when it is a pronoun.\n\
         - When the speaker clearly reports speech (after 'said', 'asked', 'told', 'replied'), wrap the quoted words in double quotation marks: 'he said hey Sally how are you' -> 'He said, \"Hey Sally, how are you?\"'. Only for actual quoted speech."
    );

    const REPS: usize = 5;
    // Differentiator cases (id, input, must_contain[], must_not_contain[], must_keep_casesensitive[]).
    // Quote checks accept straight OR curly quotes (some models emit “ ”).
    let cases: &[(&str, &str, &[&str], &[&str], &[&str])] = &[
        ("quote-said", "harry met sally and he said hey sally how are you",
            &["hey sally", "how are you"], &[], &[]),
        ("quote-asked", "she asked me are you free tomorrow",
            &["are you free"], &[], &[]),
        ("quote-told", "then john said we should leave before it gets dark",
            &["we should leave"], &[], &[]),
        ("quote-none", "i think we should ship the build today",
            &[], &["\"", "\u{201c}", "\u{201d}"], &[]),
        ("casing-a", "we run it on macos with onnx and the parakeet model from github",
            &[], &[], &["macOS", "ONNX", "GitHub", "Parakeet"]),
        ("casing-b", "i pushed the typescript fix to github and tested on ios",
            &[], &[], &["TypeScript", "GitHub", "iOS"]),
        ("disfluency", "so um I think we should uh ship the the thing tomorrow you know",
            &[], &["um", " uh ", "the the"], &[]),
        ("selfcorrect-a", "let's put it in the blue folder wait no the red folder",
            &["red"], &["blue"], &[]),
        ("selfcorrect-b", "the meeting is on tuesday i mean wednesday at noon",
            &["wednesday"], &["tuesday"], &[]),
        ("fragment", "the user's name was", &[], &["John", "Sarah", "Sally"], &[]),
        ("number-decimal", "the latest version is three point one one",
            &["3.11"], &["point one one"], &[]),
    ];

    let gemma = Settings::load().resolve_gemma(&app_paths::gemma_default_path());
    let name = std::path::Path::new(&gemma)
        .file_name().and_then(|s| s.to_str()).unwrap_or(&gemma);
    eprintln!("[bakeoff] model: {name}");
    let t = Instant::now();
    let engine = TextCleanupEngine::initialize(&gemma)?;
    eprintln!("[bakeoff] loaded in {:?}\n", t.elapsed());

    // warm
    let _ = engine.eval_cleanup(&prompt, "warm up the model", false).await?;

    println!("=================== MODEL: {name} ({REPS} reps/case) ===================");
    let mut all_ms: Vec<f64> = Vec::new();
    let mut total_pass = 0usize;
    let mut total_runs = 0usize;
    for (id, input, must, mustnt, keep) in cases {
        let needs_quote = id.starts_with("quote-") && *id != "quote-none";
        let mut case_pass = 0usize;
        let mut last = String::new();
        for _ in 0..REPS {
            let t = Instant::now();
            let out = engine.eval_cleanup(&prompt, input, false).await?;
            all_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            let lo = out.to_lowercase();
            let mut ok = true;
            for m in *must { if !lo.contains(&m.to_lowercase()) { ok = false; } }
            for m in *mustnt { if lo.contains(&m.to_lowercase()) { ok = false; } }
            for k in *keep { if !out.contains(*k) { ok = false; } }
            // quotation cases must actually wrap in quote marks (straight or curly)
            if needs_quote && !out.chars().any(|c| matches!(c, '"' | '\u{201c}' | '\u{201d}')) {
                ok = false;
            }
            if ok { case_pass += 1; }
            last = out;
        }
        total_pass += case_pass;
        total_runs += REPS;
        let mark = if case_pass == REPS { "✅" } else if case_pass == 0 { "❌" } else { "⚠️ " };
        println!("{mark} {id:<14} {case_pass}/{REPS}  e.g. {last:?}");
    }
    all_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = all_ms.iter().sum::<f64>() / all_ms.len() as f64;
    let p90 = all_ms[(all_ms.len() as f64 * 0.9) as usize];
    println!("\n→ {name}: {total_pass}/{total_runs} runs passed · latency mean {mean:.0}ms · p90 {p90:.0}ms\n");
    Ok(())
}
