//! Prompt-lab harness — evaluate Gemma cleanup / transform / format prompts
//! against a fixed corpus, with the model loaded ONCE.
//!
//! Run (needs the cleaner stack + a Gemma GGUF on disk):
//!
//! ```bash
//! cargo run --release --features cleaner --example prompt_lab
//! # or point at a different model to compare:
//! GEMMA_MODEL_PATH=models/llm/gemma-3-4b-it/gemma-3-4b-it-Q4_K_M.gguf \
//!   cargo run --release --features cleaner --example prompt_lab
//! ```
//!
//! Everything it runs is data-driven from `prompts-lab/lab.json`, so candidate
//! prompts and corpus cases can be tuned WITHOUT recompiling. Each case is run
//! `reps` times (default 2) to surface any platform nondeterminism — note that
//! production samples with a *fixed* seed at temp 0.2, so per-input behaviour is
//! near-deterministic; the real robustness probe is corpus breadth, not reruns.
//!
//! Output: a scored Markdown report + a raw JSONL of every generation under
//! `prompts-lab/results/`, plus a stdout summary. The point is to make the
//! qualitative ("does this read right?") cheap to eyeball and the quantitative
//! ("did the proper noun survive?") automatic.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Deserialize;

#[cfg(not(feature = "cleaner"))]
fn main() {
    eprintln!("prompt_lab needs the `cleaner` feature: cargo run --release --features cleaner --example prompt_lab");
    std::process::exit(1);
}

// ---- corpus / lab spec (prompts-lab/lab.json) ------------------------------

#[derive(Debug, Deserialize)]
struct Lab {
    /// Reps per case. Default 2.
    #[serde(default)]
    reps: Option<usize>,
    /// Named candidate cleanup system prompts. The key "default" / "baseline"
    /// is conventional; whichever the cases reference by name is used.
    #[serde(default)]
    cleanup_candidates: BTreeMap<String, String>,
    #[serde(default)]
    transform_candidates: BTreeMap<String, String>,
    /// Named format-preset system prompts (numbered / bullets / email / ...).
    #[serde(default)]
    format_candidates: BTreeMap<String, String>,
    #[serde(default)]
    cleanup_cases: Vec<CleanupCase>,
    #[serde(default)]
    transform_cases: Vec<TransformCase>,
    #[serde(default)]
    format_cases: Vec<FormatCase>,
}

#[derive(Debug, Deserialize, Clone)]
struct Checks {
    /// Output must contain each (case-insensitive substring).
    #[serde(default)]
    must_contain: Vec<String>,
    /// Output must NOT contain any (case-insensitive substring).
    #[serde(default)]
    must_not_contain: Vec<String>,
    /// Output must contain each, CASE-SENSITIVE (proper-noun / casing checks).
    #[serde(default)]
    must_keep: Vec<String>,
    /// [from, to] pairs: `to` must be present and `from` absent (expansions).
    #[serde(default)]
    expand: Vec<[String; 2]>,
    /// Output must not start with a chatty preamble.
    #[serde(default)]
    no_preamble: bool,
    /// Output must be a single line (cleanup default).
    #[serde(default)]
    single_line: bool,
    /// At least this many non-empty lines (list formats).
    #[serde(default)]
    min_lines: Option<usize>,
    /// Every non-empty line must match this kind: "numbered" => `^\d+[.)]`,
    /// "bullet" => `^[-*•]`.
    #[serde(default)]
    line_kind: Option<String>,
    /// Output length / input length must stay below this (anti-ballooning).
    #[serde(default)]
    max_len_ratio: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct CleanupCase {
    id: String,
    #[serde(default)]
    category: String,
    /// Which cleanup_candidates to run (defaults to all of them).
    #[serde(default)]
    candidates: Vec<String>,
    input: String,
    #[serde(default)]
    checks: Option<Checks>,
}

#[derive(Debug, Deserialize)]
struct TransformCase {
    id: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    candidates: Vec<String>,
    instruction: String,
    input: String,
    #[serde(default)]
    checks: Option<Checks>,
}

#[derive(Debug, Deserialize)]
struct FormatCase {
    id: String,
    /// Which format_candidate (by key) to apply.
    format: String,
    input: String,
    #[serde(default)]
    checks: Option<Checks>,
}

// ---- check engine ----------------------------------------------------------

// True meta-preambles only. We deliberately exclude things like "I have" /
// "the text", which are legitimate sentence openers in transform outputs.
// "okay"/"ok" stay: for cleanup they catch a real failure (leading filler
// "okay so ..." that should have been stripped).
const PREAMBLE_TELLS: &[&str] = &[
    "here's", "here is", "sure,", "sure!", "okay,", "okay ", "ok,",
    "certainly", "of course", "below is", "the cleaned text", "the cleaned version",
];

fn line_count(s: &str) -> usize {
    s.lines().filter(|l| !l.trim().is_empty()).count()
}

/// Returns (failures). Empty vec == all checks passed.
fn evaluate(out: &str, input: &str, c: &Checks) -> Vec<String> {
    let mut fail = Vec::new();
    let lo = out.to_lowercase();

    for needle in &c.must_contain {
        if !lo.contains(&needle.to_lowercase()) {
            fail.push(format!("missing {needle:?}"));
        }
    }
    for needle in &c.must_not_contain {
        if lo.contains(&needle.to_lowercase()) {
            fail.push(format!("contains forbidden {needle:?}"));
        }
    }
    for needle in &c.must_keep {
        if !out.contains(needle.as_str()) {
            fail.push(format!("dropped/altered {needle:?} (case-sensitive)"));
        }
    }
    for [from, to] in &c.expand {
        if !lo.contains(&to.to_lowercase()) {
            fail.push(format!("did not expand to {to:?}"));
        }
        // crude word-ish check for the colloquial source surviving
        if lo.contains(&from.to_lowercase()) {
            fail.push(format!("colloquial {from:?} survived"));
        }
    }
    if c.no_preamble {
        let head: String = lo.chars().take(16).collect();
        if let Some(t) = PREAMBLE_TELLS.iter().find(|t| head.starts_with(*t)) {
            fail.push(format!("preamble tell {t:?}"));
        }
    }
    if c.single_line && out.contains('\n') {
        fail.push("expected single line, got newlines".into());
    }
    if let Some(min) = c.min_lines {
        let n = line_count(out);
        if n < min {
            fail.push(format!("only {n} lines, want >= {min}"));
        }
    }
    if let Some(kind) = &c.line_kind {
        for l in out.lines().filter(|l| !l.trim().is_empty()) {
            let t = l.trim_start();
            let ok = match kind.as_str() {
                "numbered" => {
                    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
                    !digits.is_empty()
                        && t[digits.len()..].starts_with(['.', ')'])
                }
                "bullet" => t.starts_with(['-', '*', '•']),
                _ => true,
            };
            if !ok {
                fail.push(format!("line not {kind}: {l:?}"));
                break;
            }
        }
    }
    if let Some(ratio) = c.max_len_ratio {
        let r = out.chars().count() as f32 / input.chars().count().max(1) as f32;
        if r > ratio {
            fail.push(format!("len ratio {r:.2} > {ratio:.2} (ballooned)"));
        }
    }
    fail
}

#[cfg(feature = "cleaner")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::settings::Settings;

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let lab_path = root.join("prompts-lab/lab.json");
    let body = std::fs::read_to_string(&lab_path)
        .map_err(|e| eyre::eyre!("read {}: {e}", lab_path.display()))?;
    let lab: Lab = serde_json::from_str(&body)
        .map_err(|e| eyre::eyre!("parse lab.json: {e}"))?;
    let reps = lab.reps.unwrap_or(2).max(1);

    let gemma = Settings::load()
        .resolve_gemma(&fast_dictate_backend::app_paths::gemma_default_path());
    eprintln!("[lab] model: {gemma}");
    let t = std::time::Instant::now();
    let engine = TextCleanupEngine::initialize(&gemma)?;
    eprintln!("[lab] loaded in {:?}; reps={reps}", t.elapsed());

    let mut md = String::new();
    let mut jsonl = String::new();
    let model_name = PathBuf::from(&gemma)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = writeln!(md, "# Prompt-lab results — `{model_name}`\n");
    let _ = writeln!(md, "Reps per case: {reps} (fixed seed → near-deterministic; reps surface platform jitter).\n");

    let mut total = 0usize;
    let mut clean_passes = 0usize;

    // ---- cleanup cases ----
    let _ = writeln!(md, "## Cleanup\n");
    for case in &lab.cleanup_cases {
        let names: Vec<String> = if case.candidates.is_empty() {
            lab.cleanup_candidates.keys().cloned().collect()
        } else {
            case.candidates.clone()
        };
        for name in names {
            let Some(prompt) = lab.cleanup_candidates.get(&name) else {
                eprintln!("[lab] cleanup case {} references unknown candidate {name:?}", case.id);
                continue;
            };
            let preserve = case
                .checks
                .as_ref()
                .map(|c| !c.single_line && (c.min_lines.is_some() || c.line_kind.is_some()))
                .unwrap_or(false);
            let mut outs = Vec::new();
            for _ in 0..reps {
                outs.push(engine.eval_cleanup(prompt, &case.input, preserve).await?);
            }
            total += 1;
            let pass = report_case(
                &mut md, &mut jsonl, "cleanup", &case.id, &case.category, &name,
                Some(&case.input), None, &case.input, &outs, case.checks.as_ref(),
            );
            if pass { clean_passes += 1; }
        }
    }

    // ---- transform cases ----
    let _ = writeln!(md, "\n## Transform\n");
    for case in &lab.transform_cases {
        let names: Vec<String> = if case.candidates.is_empty() {
            lab.transform_candidates.keys().cloned().collect()
        } else {
            case.candidates.clone()
        };
        for name in names {
            let Some(prompt) = lab.transform_candidates.get(&name) else {
                eprintln!("[lab] transform case {} references unknown candidate {name:?}", case.id);
                continue;
            };
            let mut outs = Vec::new();
            for _ in 0..reps {
                outs.push(engine.eval_transform(prompt, &case.instruction, &case.input).await?);
            }
            total += 1;
            let pass = report_case(
                &mut md, &mut jsonl, "transform", &case.id, &case.category, &name,
                Some(&case.input), Some(&case.instruction), &case.input, &outs, case.checks.as_ref(),
            );
            if pass { clean_passes += 1; }
        }
    }

    // ---- format cases (cleanup path, newlines preserved) ----
    let _ = writeln!(md, "\n## Formats\n");
    for case in &lab.format_cases {
        let Some(prompt) = lab.format_candidates.get(&case.format) else {
            eprintln!("[lab] format case {} references unknown format {:?}", case.id, case.format);
            continue;
        };
        let mut outs = Vec::new();
        for _ in 0..reps {
            outs.push(engine.eval_cleanup(prompt, &case.input, true).await?);
        }
        total += 1;
        let pass = report_case(
            &mut md, &mut jsonl, "format", &case.id, &case.format, &case.format,
            Some(&case.input), None, &case.input, &outs, case.checks.as_ref(),
        );
        if pass { clean_passes += 1; }
    }

    let _ = writeln!(md, "\n---\n**Total: {clean_passes}/{total} configurations passed all checks.**");
    eprintln!("[lab] {clean_passes}/{total} configurations passed all checks");

    let results = root.join("prompts-lab/results");
    std::fs::create_dir_all(&results)?;
    let stamp = chrono_stamp();
    let md_path = results.join(format!("{stamp}-{model_name}.md"));
    let jsonl_path = results.join(format!("{stamp}-{model_name}.jsonl"));
    std::fs::write(&md_path, &md)?;
    std::fs::write(&jsonl_path, &jsonl)?;
    eprintln!("[lab] wrote {}", md_path.display());
    eprintln!("[lab] wrote {}", jsonl_path.display());
    Ok(())
}

/// Append a case to the markdown + jsonl. Returns whether ALL reps passed.
#[allow(clippy::too_many_arguments)]
fn report_case(
    md: &mut String,
    jsonl: &mut String,
    kind: &str,
    id: &str,
    category: &str,
    candidate: &str,
    _input_dbg: Option<&str>,
    instruction: Option<&str>,
    input: &str,
    outs: &[String],
    checks: Option<&Checks>,
) -> bool {
    let mut all_pass = true;
    let stable = outs.iter().all(|o| o == &outs[0]);
    let _ = writeln!(md, "### `{kind}` · {id} · _{category}_ · candidate `{candidate}`");
    if let Some(instr) = instruction {
        let _ = writeln!(md, "- **instruction:** {instr}");
    }
    let _ = writeln!(md, "- **input:** `{}`", one_line(input));
    for (i, out) in outs.iter().enumerate() {
        let fails = checks.map(|c| evaluate(out, input, c)).unwrap_or_default();
        if !fails.is_empty() { all_pass = false; }
        let verdict = if fails.is_empty() { "✅" } else { "❌" };
        let _ = writeln!(md, "- **out[{i}]** {verdict}:\n```\n{out}\n```");
        if !fails.is_empty() {
            let _ = writeln!(md, "  - fails: {}", fails.join("; "));
        }
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
        let _ = writeln!(
            jsonl,
            "{{\"kind\":\"{kind}\",\"id\":\"{id}\",\"candidate\":\"{candidate}\",\"rep\":{i},\"pass\":{},\"out\":\"{}\"}}",
            fails.is_empty(), esc(out)
        );
    }
    if !stable {
        let _ = writeln!(md, "- ⚠️ outputs varied across reps");
    }
    let _ = writeln!(md);
    all_pass
}

fn one_line(s: &str) -> String {
    s.replace('\n', " ⏎ ")
}

/// Cheap timestamp without pulling a date crate: seconds since epoch.
fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}
