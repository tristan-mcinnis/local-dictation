//! Deterministic software-of-one evaluation for the installed S1-mini GGUF.
//!
//! The corpus is intentionally small and human-readable. It tests the things
//! that matter for one-person dictation: preserving every clause, removing
//! speech filler, keeping a personal vocabulary, adding punctuation, and
//! refusing to answer or complete an unfinished thought. It uses the same
//! `TextCleanupEngine::eval_cleanup` API as the prompt lab, so the model is
//! loaded once and the persistent KV-cache is exercised between cases.
//!
//! ```text
//! cargo run --release --features cleaner --example s1_mini_eval
//! DICTATE_LLM_MODEL_PATH=/path/to/s1-mini.gguf cargo run --release --features cleaner --example s1_mini_eval -- --reps 3
//! cargo run --release --features cleaner --example s1_mini_eval -- --prompt-file /tmp/candidate.txt
//! ```
//!
//! The runner does not modify user settings or dictionary files. Vocabulary
//! terms in a case are supplied as an ephemeral prompt hint, which makes the
//! result reproducible on a machine with a different personal dictionary.

#[cfg(not(feature = "cleaner"))]
fn main() {
    eprintln!("s1_mini_eval needs the `cleaner` feature");
    std::process::exit(1);
}

#[cfg(feature = "cleaner")]
mod enabled {
    use fast_dictate_backend::app_paths::llm_default_path;
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::prompts::{vocabulary_suffix, DEFAULT_CLEANUP};
    use serde::Deserialize;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Instant;

    const COMPACT_PROMPT: &str = "You are a real-time dictation cleaner. Output only the cleaned text. Preserve every word and clause the speaker said; never answer, summarize, complete, or invent. Remove speech fillers and repeated words, expand wanna/gonna/gotta, fix capitalization and punctuation, and preserve proper nouns and technical terms exactly.";

    #[derive(Debug, Deserialize)]
    struct EvalCase {
        id: String,
        category: String,
        input: String,
        #[serde(default)]
        must_contain: Vec<String>,
        #[serde(default)]
        must_not_contain: Vec<String>,
        #[serde(default)]
        preserve_exact: Vec<String>,
        #[serde(default)]
        known_vocabulary: Vec<String>,
        #[serde(default)]
        require_terminal_punctuation: bool,
        #[serde(default)]
        min_char_retention_ratio: f64,
        #[serde(default = "default_expansion")]
        max_expansion_ratio: f64,
    }

    fn default_expansion() -> f64 {
        4.0
    }

    #[derive(Debug, Default, Clone)]
    struct Score {
        passed: usize,
        total: usize,
        failures: Vec<String>,
    }

    impl Score {
        fn check(&mut self, ok: bool, label: impl Into<String>) {
            self.total += 1;
            if ok {
                self.passed += 1;
            } else {
                self.failures.push(label.into());
            }
        }
    }

    fn score(case: &EvalCase, output: &str) -> Score {
        let mut s = Score::default();
        let lower = output.to_lowercase();
        let input_chars = case.input.chars().count().max(1) as f64;
        let output_trimmed = output.trim();

        for expected in &case.must_contain {
            s.check(
                lower.contains(&expected.to_lowercase()),
                format!("missing {expected:?}"),
            );
        }
        for forbidden in &case.must_not_contain {
            s.check(
                !lower.contains(&forbidden.to_lowercase()),
                format!("contains forbidden {forbidden:?}"),
            );
        }
        for term in &case.preserve_exact {
            s.check(
                output.contains(term),
                format!("lost exact spelling {term:?}"),
            );
        }

        s.check(!output_trimmed.is_empty(), "empty output");
        let marker = ["<|", "<start_of_turn>", "<end_of_turn>", "</s>", "<s>"]
            .iter()
            .any(|m| output.contains(m));
        s.check(!marker, "leaked chat-template marker");
        let output_chars = output_trimmed.chars().count() as f64;
        if case.min_char_retention_ratio > 0.0 {
            s.check(
                output_chars >= input_chars * case.min_char_retention_ratio,
                format!(
                    "output character count is below {:.0}% of input",
                    case.min_char_retention_ratio * 100.0
                ),
            );
        }
        s.check(
            output_chars <= input_chars * case.max_expansion_ratio + 32.0,
            format!("output expanded beyond {:.1}x", case.max_expansion_ratio),
        );
        if case.require_terminal_punctuation {
            s.check(
                output_trimmed
                    .chars()
                    .last()
                    .is_some_and(|c| matches!(c, '.' | '!' | '?' | ':')),
                "missing terminal punctuation",
            );
        }
        s
    }

    fn model_path(args: &[String]) -> PathBuf {
        if let Some(i) = args.iter().position(|a| a == "--model") {
            if let Some(path) = args.get(i + 1) {
                return PathBuf::from(path);
            }
        }
        fast_dictate_backend::settings::llm_model_env()
            .map(|(_, v)| PathBuf::from(v))
            .unwrap_or_else(|| PathBuf::from(llm_default_path()))
    }

    fn reps(args: &[String]) -> usize {
        args.iter()
            .position(|a| a == "--reps")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .filter(|n: &usize| *n > 0)
            .unwrap_or(1)
    }

    fn compact(args: &[String]) -> bool {
        args.iter().any(|a| a == "--compact")
    }

    fn prompt_file(args: &[String]) -> Option<PathBuf> {
        args.iter()
            .position(|a| a == "--prompt-file")
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
    }

    fn json_output(args: &[String]) -> bool {
        args.iter().any(|a| a == "--json")
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let index = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[index]
    }

    pub async fn run() -> eyre::Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let path = model_path(&args);
        if !path.exists() {
            return Err(eyre::eyre!(
                "S1-mini model not found at {} (set DICTATE_LLM_MODEL_PATH or pass --model PATH)",
                path.display()
            ));
        }
        let reps = reps(&args);
        let prompt_override = match prompt_file(&args) {
            Some(path) => Some((
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("prompt-file")
                    .to_string(),
                std::fs::read_to_string(&path)
                    .map_err(|e| eyre::eyre!("read prompt file {}: {e}", path.display()))?,
            )),
            None => None,
        };
        let (prompt_name, base_prompt): (&str, &str) = match prompt_override.as_ref() {
            Some((name, prompt)) => (name, prompt),
            None if compact(&args) => ("compact", COMPACT_PROMPT),
            None => ("production", DEFAULT_CLEANUP),
        };
        let cases: Vec<EvalCase> =
            serde_json::from_str(include_str!("../evals/s1_mini_corpus.json"))
                .map_err(|e| eyre::eyre!("invalid S1 corpus: {e}"))?;

        eprintln!("[s1-eval] model: {}", path.display());
        eprintln!(
            "[s1-eval] prompt: {prompt_name} · {} cases · {reps} rep(s)",
            cases.len()
        );
        let load_started = Instant::now();
        let engine = TextCleanupEngine::initialize(&path)?;
        eprintln!(
            "[s1-eval] loaded in {:.0} ms",
            load_started.elapsed().as_secs_f64() * 1000.0
        );

        // Prime the worker/context but exclude model warm-up from case latency.
        let _ = engine.eval_cleanup(base_prompt, "warm up", false).await?;
        let mut latencies = Vec::with_capacity(cases.len() * reps);
        let mut category: BTreeMap<String, (usize, usize, usize, usize)> = BTreeMap::new();
        let mut rows = Vec::new();

        for case in &cases {
            let mut case_score = Score::default();
            let mut last_output = String::new();
            let mut case_times = Vec::with_capacity(reps);
            for _ in 0..reps {
                let mut prompt = base_prompt.to_string();
                if let Some(suffix) = vocabulary_suffix(&case.known_vocabulary) {
                    prompt.push_str(&suffix);
                }
                let started = Instant::now();
                let output = engine.eval_cleanup(&prompt, &case.input, false).await?;
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                latencies.push(elapsed);
                case_times.push(elapsed);
                let run_score = score(case, &output);
                case_score.passed += run_score.passed;
                case_score.total += run_score.total;
                case_score.failures.extend(run_score.failures);
                last_output = output;
            }
            let case_ok = case_score.passed == case_score.total;
            let (cp, ct, runs, passed_runs) = category.entry(case.category.clone()).or_default();
            *cp += case_score.passed;
            *ct += case_score.total;
            *runs += reps;
            if case_ok {
                *passed_runs += reps;
            }
            rows.push((case, case_score, last_output, case_times));
        }

        latencies.sort_by(|a, b| a.total_cmp(b));
        let mean = latencies.iter().sum::<f64>() / latencies.len().max(1) as f64;
        let all_passed = rows
            .iter()
            .filter(|(_, s, _, _)| s.passed == s.total)
            .count();
        if json_output(&args) {
            let json_rows: Vec<serde_json::Value> = rows
                .iter()
                .map(|(case, s, out, times)| {
                    serde_json::json!({
                        "id": case.id, "category": case.category, "input": case.input,
                        "output": out, "passed": s.passed, "checks": s.total,
                        "case_pass": s.passed == s.total, "failures": s.failures,
                        "latency_ms": times,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "model": path, "prompt": prompt_name, "reps": reps,
                    "cases": json_rows, "passed_cases": all_passed,
                    "total_cases": cases.len(), "latency_ms": {
                        "mean": mean, "p50": percentile(&latencies, 0.5), "p90": percentile(&latencies, 0.9),
                    }
                })
            );
            return Ok(());
        }

        println!("\nS1-mini cleanup evaluation ({prompt_name})");
        println!("────────────────────────────────────────────────────────────────────────────");
        for (case, s, out, times) in &rows {
            let mark = if s.passed == s.total { "PASS" } else { "FAIL" };
            let why = if s.failures.is_empty() {
                String::new()
            } else {
                format!(" — {}", s.failures.join(", "))
            };
            let ms = times.iter().sum::<f64>() / times.len().max(1) as f64;
            println!(
                "{mark:<4} {:<24} {}/{} checks · {:>6.0} ms{}",
                case.id, s.passed, s.total, ms, why
            );
            println!("     {}", out.trim());
        }
        println!("\nBy category (check score; case pass rate):");
        for (name, (passed, total, runs, passed_runs)) in category {
            println!(
                "  {name:<20} {passed}/{total} ({:.0}%) · {passed_runs}/{runs} runs",
                100.0 * passed as f64 / total.max(1) as f64
            );
        }
        println!("\nSummary: {all_passed}/{} cases passed · latency mean {:.0} ms · p50 {:.0} ms · p90 {:.0} ms", cases.len(), mean, percentile(&latencies, 0.5), percentile(&latencies, 0.9));
        Ok(())
    }
}

#[cfg(feature = "cleaner")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    enabled::run().await
}
