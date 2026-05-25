//! Latency lab — measure the cleanup leg's cost under three proposed changes,
//! with the Gemma model loaded ONCE. No mic / Accessibility needed (the cleaner
//! is pure Gemma+Metal), so this runs without the live daemon.
//!
//! ```bash
//! cargo run --release --features cleaner --example latency_lab
//! ```
//!
//! Answers three questions empirically before we ship anything:
//!   1. Baseline: what does a normal cleanup cost today (HOT)?
//!   2. Vocab cost: how much does appending +20 / +64 known-vocab terms add?
//!      (the price of screen-context vocabulary + Contacts names)
//!   3. Prefix-cache ceiling: full ~400-token instruction prompt vs a minimal
//!      prompt on the SAME transcript. The delta ≈ the prefill cost of the
//!      constant instructions — i.e. the most a prompt-prefix cache could save.

#[cfg(not(feature = "cleaner"))]
fn main() {
    eprintln!("latency_lab needs the `cleaner` feature: cargo run --release --features cleaner --example latency_lab");
    std::process::exit(1);
}

#[cfg(feature = "cleaner")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::prompts::{vocabulary_suffix, DEFAULT_CLEANUP};
    use fast_dictate_backend::settings::Settings;
    use std::time::Instant;

    // Representative push-to-talk transcripts (short → mid). These are the kind
    // of raw Parakeet output the cleaner sees in production.
    let transcripts = [
        ("short", "um so i think we should just ship the the thing today"),
        (
            "mid",
            "okay so basically what i wanna do is refactor the cleaner module \
             and then you know move the prompt stuff out and uh test it against \
             the corpus before we like merge it into main",
        ),
        (
            "names",
            "i talked to lingzi and tristan about the parakeet model on macos and \
             we're gonna push it to github after the uh standup",
        ),
    ];

    // Synthetic known-vocab terms (plausible proper nouns / domain casing), to
    // measure the token cost of the vocabulary suffix at two cap levels.
    let pool: Vec<String> = [
        "Lingzi", "Tristan McInnis", "Parakeet", "macOS", "ONNX", "GitHub", "CoreML",
        "Anthropic", "Cupertino", "Soniox", "Browserbase", "Neon", "Postgres", "Gemma",
        "llama.cpp", "Metal", "CGEventTap", "Accessibility", "Tencent", "Seoul",
        "Kubernetes", "Terraform", "Datadog", "Grafana", "Prometheus", "Redis", "Kafka",
        "Snowflake", "Databricks", "Cloudflare", "Vercel", "Netlify", "Supabase",
        "Firebase", "DynamoDB", "ElastiCache", "Lambda", "Fargate", "CloudFront",
        "Route53", "Wasabi", "Backblaze", "Tailscale", "WireGuard", "Caddy", "Traefik",
        "Nginx", "Envoy", "Istio", "Linkerd", "Consul", "Vault", "Nomad", "Packer",
        "Ansible", "Pulumi", "ArgoCD", "Tekton", "Buildkite", "CircleCI", "Drone",
        "Gitea", "Forgejo", "Codeberg", "SourceHut",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let vocab20 = vocabulary_suffix(&pool[..20].to_vec()).unwrap();
    let vocab64 = vocabulary_suffix(&pool[..64.min(pool.len())].to_vec()).unwrap();

    let prompt_baseline = DEFAULT_CLEANUP.to_string();
    let prompt_vocab20 = format!("{DEFAULT_CLEANUP}{vocab20}");
    let prompt_vocab64 = format!("{DEFAULT_CLEANUP}{vocab64}");
    // Minimal prompt: strips the ~400 tokens of constant instructions. The HOT
    // delta vs baseline on the same transcript ≈ the instruction prefill cost,
    // which is the ceiling a prompt-prefix cache could remove.
    let prompt_minimal = "Clean up this dictation. Output only the cleaned text.".to_string();

    let variants: [(&str, &str); 4] = [
        ("baseline (full instructions)", &prompt_baseline),
        ("baseline + 20 vocab terms", &prompt_vocab20),
        ("baseline + 64 vocab terms", &prompt_vocab64),
        ("minimal prompt (prefix-cache floor)", &prompt_minimal),
    ];

    println!("prompt char lengths (≈ tokens/4): baseline={}, +20={}, +64={}, minimal={}",
        prompt_baseline.len(), prompt_vocab20.len(), prompt_vocab64.len(), prompt_minimal.len());

    let gemma = Settings::load()
        .resolve_gemma(&fast_dictate_backend::app_paths::gemma_default_path());
    eprintln!("[lat] model: {gemma}");
    let t = Instant::now();
    let engine = TextCleanupEngine::initialize(&gemma)?;
    eprintln!("[lat] loaded in {:?}\n", t.elapsed());

    const REPS: usize = 5;

    // ---- PARITY: cached KV-reuse output must equal a from-scratch decode ----
    // If Gemma's sliding-window attention broke prefix reuse, this is where it
    // shows up. We interleave runs so the cached run actually reuses a resident
    // prefix (the prior run), which is the real production condition.
    println!("\n=== PARITY: cached vs uncached (must match) ===");
    let mut parity_ok = true;
    for (tname, raw) in &transcripts {
        // Prime residency with a different transcript so `cached` exercises a
        // real partial-reuse, then compare.
        let _ = engine.eval_cleanup(&prompt_baseline, "warm the cache with something else", false).await?;
        let cached = engine.eval_cleanup(&prompt_baseline, raw, false).await?;
        let uncached = engine.eval_cleanup_uncached(&prompt_baseline, raw, false).await?;
        let ok = cached == uncached;
        parity_ok &= ok;
        println!("  {tname:<8} {}", if ok { "✅ identical".to_string() } else { format!("❌ DIFFER\n    cached:   {cached:?}\n    uncached: {uncached:?}") });
    }
    println!("  → parity: {}", if parity_ok { "ALL MATCH" } else { "MISMATCH — prefix reuse is UNSAFE" });

    // ---- LATENCY: realized cached win + vocab cost ----
    for (tname, raw) in &transcripts {
        println!("\n=== transcript: {tname} ({} chars) ===", raw.len());

        // Cached vs uncached on the SAME baseline prompt = the realized win.
        for (label, no_reuse) in [("baseline CACHED", false), ("baseline UNCACHED", true)] {
            let _ = run_timed(&engine, &prompt_baseline, raw, no_reuse, 1).await?; // warm
            let (mean, min, max, last) = run_timed(&engine, &prompt_baseline, raw, no_reuse, REPS).await?;
            println!("  {label:<38} mean {mean:7.1} ms  (min {min:6.1}, max {max:6.1})");
            if no_reuse { println!("      → {last:?}"); }
        }
        // Vocab cost (cached path, as it'd run in production).
        for (vname, prompt) in [("+ 20 vocab terms (cached)", &prompt_vocab20), ("+ 64 vocab terms (cached)", &prompt_vocab64)] {
            let _ = run_timed(&engine, prompt, raw, false, 1).await?;
            let (mean, min, max, _l) = run_timed(&engine, prompt, raw, false, REPS).await?;
            println!("  {vname:<38} mean {mean:7.1} ms  (min {min:6.1}, max {max:6.1})");
        }
    }

    println!("\nCACHED vs UNCACHED on baseline = the realized prefix-cache win.");
    println!("+N vocab (cached) vs baseline CACHED = per-utterance vocab cost.");

    // ---- END-TO-END: screen-context vocabulary fixes proper nouns ----
    // Simulate the on-screen text the daemon would harvest (the doc you're
    // editing), extract terms from it exactly as the daemon does, then run the
    // real production method with those terms and confirm the names get fixed.
    use fast_dictate_backend::screen_context::extract_terms;
    println!("\n=== END-TO-END: screen-context vocabulary ===");
    // Names that are NOT in corrections.json, so the only thing that can fix
    // them is the harvested screen text (isolates the screen-context effect).
    let screen = "Meeting notes — Saoirse Kavanagh, project Nimbari. Reviewing the \
                  Vakthorn proposal with Eilish.";
    let terms = extract_terms(screen, 16);
    println!("  harvested screen text: {screen:?}");
    println!("  extracted terms: {}", terms.join(", "));
    let raw = "i talked to saoirse kavanagh about the nimbari project and the vakthorn proposal with eilish";
    let without = engine.process_transcript_with_vocab(raw, &[]).await?;
    let with = engine.process_transcript_with_vocab(raw, &terms).await?;
    println!("  raw:            {raw:?}");
    println!("  WITHOUT vocab:  {without:?}");
    println!("  WITH vocab:     {with:?}");
    Ok(())
}

#[cfg(feature = "cleaner")]
async fn run_timed(
    engine: &fast_dictate_backend::cleaner::TextCleanupEngine,
    prompt: &str,
    raw: &str,
    no_reuse: bool,
    reps: usize,
) -> eyre::Result<(f64, f64, f64, String)> {
    use std::time::Instant;
    let mut times = Vec::with_capacity(reps);
    let mut last = String::new();
    for _ in 0..reps {
        let t = Instant::now();
        last = if no_reuse {
            engine.eval_cleanup_uncached(prompt, raw, false).await?
        } else {
            engine.eval_cleanup(prompt, raw, false).await?
        };
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    Ok((mean, times[0], times[times.len() - 1], last))
}
