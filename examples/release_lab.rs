//! Release-to-text lab: the closest testable proxy for "end of speech to text
//! inserted". Times what runs after the key is released, on a fixed WAV:
//! whole-buffer Cohere MLX ASR, then the S1-mini cleanup pass. Both models are
//! loaded and warmed once; each rep is hot, like a dictation in a running daemon.
//! Injection is left out because a lab must not type into another app.
//!
//! ```bash
//! cargo run --release --features full --example release_lab -- [WAV] [REPS]
//! ```
//!
//! No microphone, clipboard, Accessibility or history is touched. It prints
//! numbers only, never the transcript.

#[cfg(not(feature = "full"))]
fn main() {
    eprintln!("release_lab needs the `full` feature: cargo run --release --features full --example release_lab");
    std::process::exit(1);
}

#[cfg(feature = "full")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    use fast_dictate_backend::audio::SAMPLE_RATE;
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::prompts::DEFAULT_CLEANUP;
    use fast_dictate_backend::settings::Settings;
    use fast_dictate_backend::transcriber::{load_wav_mono16k, LocalInferenceWorker};
    use std::time::Instant;

    let mut args = std::env::args().skip(1);
    let wav = args
        .next()
        .unwrap_or_else(|| "testdata/dictation_sample.wav".to_string());
    let reps: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let audio = load_wav_mono16k(&wav)?;

    let settings = Settings::load();
    let mut asr = LocalInferenceWorker::initialize(&settings.resolve_asr())?;
    let llm_path =
        settings.resolve_llm_model(&fast_dictate_backend::app_paths::llm_default_path());
    let cleaner = TextCleanupEngine::initialize(&llm_path)?;

    let warm = asr.transcribe_pcm(&audio)?;
    cleaner.eval_cleanup(DEFAULT_CLEANUP, warm.trim(), false).await?;
    let _ = asr.transcribe_pcm(&vec![0.0_f32; SAMPLE_RATE as usize]);

    let (mut asr_ms, mut clean_ms, mut total_ms) = (vec![], vec![], vec![]);
    for _ in 0..reps {
        let t = Instant::now();
        let raw = asr.transcribe_pcm(&audio)?;
        let a = t.elapsed().as_secs_f64() * 1e3;
        cleaner.eval_cleanup(DEFAULT_CLEANUP, raw.trim(), false).await?;
        let total = t.elapsed().as_secs_f64() * 1e3;
        asr_ms.push(a);
        clean_ms.push(total - a);
        total_ms.push(total);
    }

    fn pct(v: &mut [f64], p: f64) -> f64 {
        v.sort_by(|a, b| a.total_cmp(b));
        v[((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1]
    }
    println!(
        "audio={:.2}s reps={reps}",
        audio.len() as f64 / SAMPLE_RATE as f64
    );
    for (name, v) in [
        ("asr_ms", &mut asr_ms),
        ("cleanup_ms", &mut clean_ms),
        ("release_to_text_ms", &mut total_ms),
    ] {
        println!(
            "{name} p50={:.0} p90={:.0} min={:.0}",
            pct(v, 0.5),
            pct(v, 0.9),
            pct(v, 0.0)
        );
    }
    Ok(())
}
