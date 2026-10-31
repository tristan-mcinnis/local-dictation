//! Local-only, invented-text smoke checks for the production model adapters.
//! Run the built binary with a temporary HOME to keep user configuration out.
//! Usage: model_contract_lab CLEANUP.gguf [INSTRUCTION.gguf]
//! No microphone, clipboard, network or user history is accessed.
#[cfg(feature = "cleaner")]
fn anchor_matches(output: &str, anchor: &str) -> bool {
    if anchor.chars().all(|c| c.is_ascii_digit()) {
        // Formatting 1842 as $1,842 preserves the amount, not a loss of digits.
        output.replace([',', '，'], "").contains(anchor)
    } else {
        output.to_lowercase().contains(&anchor.to_lowercase())
    }
}

#[cfg(feature = "cleaner")]
fn main() -> eyre::Result<()> {
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use std::path::Path;
    use std::time::Instant;

    let args: Vec<String> = std::env::args().collect();
    let cleanup = args
        .get(1)
        .ok_or_else(|| eyre::eyre!("expected cleanup GGUF path"))?;
    let instruction = args.get(2).map(Path::new);
    let boot = Instant::now();
    let engine = TextCleanupEngine::initialize_with_instruction_model(cleanup, instruction)?;
    let boot_ms = boot.elapsed().as_millis();
    let rt = tokio::runtime::Runtime::new()?;
    let mut reports = Vec::new();
    // Anchors test preservation, not one model's preferred prose punctuation.
    let mut fixtures: Vec<(&str, &str, &[&str])> = vec![
        ("filler", "um i need to send the Q4 deck to Avery tomorrow at 3 pm", &["q4", "avery", "tomorrow", "3"]),
        ("numbers", "the invoice is 1842 dollars and the reference is ZQ-718", &["1842", "zq-718"]),
        ("technical", "the build failed in parseUserID with error E403 at line 128", &["parseuserid", "e403", "128"]),
        ("negation", "do not delete the old files because we still need them for the audit", &["not", "old", "files", "audit"]),
        ("longer", "i checked the first report and the second report but the third report is still missing please ask Avery to upload it before the meeting tomorrow", &["first", "second", "third", "missing", "avery", "tomorrow"]),
    ];
    if std::env::var("DICTATE_COHERE_LANG").ok().as_deref() == Some("zh") {
        fixtures.push((
            "chinese",
            "明天把编号 ZQ-718 的发票发给小林，金额是 1842 元",
            &["明天", "zq-718", "1842", "元"],
        ));
    }
    let mut failures = 0;
    for (id, raw, anchors) in fixtures {
        let start = Instant::now();
        let result = rt.block_on(engine.process_transcript(raw));
        let elapsed_ms = start.elapsed().as_millis();
        match result {
            Ok(output) => {
                let lower = output.to_lowercase();
                let missing: Vec<_> = anchors
                    .iter()
                    .filter(|anchor| !anchor_matches(&output, anchor))
                    .collect();
                let passed = !output.trim().is_empty()
                    && missing.is_empty()
                    && !lower.contains("<think>")
                    && !lower.contains("styling:");
                if !passed {
                    failures += 1;
                }
                reports.push(serde_json::json!({"id":id,"kind":"cleanup","input":raw,"output":output,"elapsed_ms":elapsed_ms,"anchors_missing":missing,"passed":passed}));
            }
            Err(error) => {
                failures += 1;
                reports.push(serde_json::json!({"id":id,"kind":"cleanup","elapsed_ms":elapsed_ms,"error":format!("{error:#}"),"passed":false}));
            }
        }
    }
    if instruction.is_some() {
        if !engine.supports_instructions() {
            failures += 1;
        }
        for (id, directive, text, anchors) in [
            ("translate", "Translate into Spanish. Return only the translation.", "Hello, how are you?", vec!["hola"]),
            ("rewrite", "Make this concise. Keep the identifier and amount exactly. Return only the rewrite.", "Please make sure that the reference ZQ-718 and the amount 1842 dollars are included.", vec!["zq-718", "1842"]),
        ] {
            let start = Instant::now();
            let result = rt.block_on(engine.transform(directive, text));
            let elapsed_ms = start.elapsed().as_millis();
            match result {
                Ok(output) => {
                    let lower = output.to_lowercase();
                    let passed = !output.trim().is_empty() && anchors.iter().all(|anchor| lower.contains(anchor)) && !lower.contains("<think>");
                    if !passed { failures += 1; }
                    reports.push(serde_json::json!({"id":id,"kind":"instruction","input":text,"instruction":directive,"output":output,"elapsed_ms":elapsed_ms,"passed":passed}));
                }
                Err(error) => {
                    failures += 1;
                    reports.push(serde_json::json!({"id":id,"kind":"instruction","elapsed_ms":elapsed_ms,"error":format!("{error:#}"),"passed":false}));
                }
            }
        }
    } else {
        let refused = rt
            .block_on(engine.transform("Translate to Spanish", "Hello"))
            .is_err();
        if !refused {
            failures += 1;
        }
        reports.push(serde_json::json!({"id":"missing_instruction_is_explicit","passed":refused}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"boot_ms":boot_ms,"instruction_available":engine.supports_instructions(),"instruction_problem":engine.instruction_problem(),"failures":failures,"fixtures":reports})
        )?
    );
    eyre::ensure!(
        failures == 0,
        "{failures} local model contract checks failed"
    );
    Ok(())
}

#[cfg(all(test, feature = "cleaner"))]
mod tests {
    #[test]
    fn number_grouping_preserves_amount_but_not_an_altered_digit() {
        assert!(super::anchor_matches("Invoice: $1,842", "1842"));
        assert!(!super::anchor_matches("Invoice: $1,843", "1842"));
        assert!(!super::anchor_matches("References ZQ-719", "zq-718"));
    }
}

#[cfg(not(feature = "cleaner"))]
fn main() {
    eprintln!("Build this lab with --features cleaner");
}
