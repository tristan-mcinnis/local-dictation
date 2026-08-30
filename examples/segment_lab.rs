//! Segmented-ASR agreement lab — measure whether incremental (VAD-segmented)
//! transcription during the hold matches a single whole-buffer transcribe.
//!
//! ```bash
//! cargo run --release --features full --example segment_lab
//! cargo run --release --features full --example segment_lab -- path/to.wav
//! ```
//!
//! Loads `testdata/dictation_sample.wav` by default (mono 16 kHz), or the WAV
//! given as the first CLI arg, and runs the same ASR
//! worker two ways:
//!   1. **whole-buffer** — `transcribe_pcm` on the entire clip (the proven,
//!      non-streaming release path).
//!   2. **simulated streaming** — feed the clip in ~100 ms chunks through
//!      `SegmentStream`, transcribe each VAD-completed segment (plus the final
//!      open segment at the end), then join the pieces — the exact shape the
//!      daemon's streaming-ASR path produces during the hold.
//!
//! Prints both transcripts plus a word-level agreement metric (WER-style:
//! Levenshtein over word tokens divided by the reference count). The closer to
//! 1.0, the less accuracy segmented ASR costs vs whole-buffer. No mic, no
//! Accessibility, no cleanup model — only the fixed Cohere MLX ASR worker.

#[cfg(not(feature = "cohere-mlx"))]
fn main() {
    eprintln!(
        "segment_lab needs the `cohere-mlx` feature: \
         cargo run --release --features full --example segment_lab"
    );
    std::process::exit(1);
}

#[cfg(feature = "cohere-mlx")]
fn main() -> eyre::Result<()> {
    use fast_dictate_backend::audio::SAMPLE_RATE;
    use fast_dictate_backend::settings::Settings;
    use fast_dictate_backend::transcriber::{load_wav_mono16k, LocalInferenceWorker};
    use fast_dictate_backend::vad::{SegmentStream, VadConfig};
    use std::time::Instant;

    let wav_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "testdata/dictation_sample.wav".to_string());
    let audio = load_wav_mono16k(&wav_path)?;
    eprintln!(
        "[seg] loaded {wav_path} · {} samples · {:.2}s @ {} Hz",
        audio.len(),
        audio.len() as f64 / SAMPLE_RATE as f64,
        SAMPLE_RATE
    );

    let cfg = Settings::load().resolve_asr();
    eprintln!("[seg] asr: {}", cfg.describe());
    let mut worker = LocalInferenceWorker::initialize(&cfg)?;

    // ---- Warm the model so the measured runs are hot ----
    let _ = worker.transcribe_pcm(&vec![0.0_f32; SAMPLE_RATE as usize]);

    // ---- (a) whole-buffer transcribe (baseline / reference) ----
    let t = Instant::now();
    let whole = worker.transcribe_pcm(&audio)?;
    let whole_ms = t.elapsed().as_millis();
    let whole = whole.trim().to_string();
    println!("\n=== whole-buffer (reference) · {whole_ms} ms ===\n  {whole:?}");

    // ---- (b) simulated streaming: ~100 ms chunks through SegmentStream ----
    let chunk = SAMPLE_RATE as usize / 10; // 100 ms of 16 kHz samples
    let mut st = SegmentStream::new(SAMPLE_RATE, VadConfig::default());
    let mut pieces: Vec<String> = Vec::new();
    let mut seg_count = 0usize;
    let mut transcribe_ms = 0u128;

    for (i, c) in audio.chunks(chunk).enumerate() {
        st.push(c);
        for (s, e) in st.take_complete() {
            let seg = st.buf()[s..e].to_vec();
            let t = Instant::now();
            let raw = worker.transcribe_pcm(&seg)?.trim().to_string();
            transcribe_ms += t.elapsed().as_millis();
            seg_count += 1;
            if !raw.is_empty() {
                pieces.push(raw);
            }
        }
        let _ = i;
    }
    // Final open segment(s), flushed at release.
    for (s, e) in st.take_final() {
        let seg = st.buf()[s..e].to_vec();
        let t = Instant::now();
        let raw = worker.transcribe_pcm(&seg)?.trim().to_string();
        transcribe_ms += t.elapsed().as_millis();
        seg_count += 1;
        if !raw.is_empty() {
            pieces.push(raw);
        }
    }
    let segmented = pieces.join(" ").trim().to_string();
    println!(
        "\n=== segmented streaming · {seg_count} segment(s) · {transcribe_ms} ms total transcribe ==="
    );
    for p in &pieces {
        println!("    · {p:?}");
    }
    println!("  joined: {segmented:?}");

    // ---- word-level agreement (WER over word tokens) ----
    let ref_tokens: Vec<&str> = whole.split_whitespace().collect();
    let hyp_tokens: Vec<&str> = segmented.split_whitespace().collect();
    let dist = levenshtein(&ref_tokens, &hyp_tokens);
    let wer = if ref_tokens.is_empty() {
        if hyp_tokens.is_empty() {
            0.0
        } else {
            1.0
        }
    } else {
        dist as f64 / ref_tokens.len() as f64
    };
    let agreement = 1.0 - wer;
    println!("\n=== agreement ===");
    println!(
        "  ref words: {} · hyp words: {} · edits: {} · WER: {:.3} · agreement: {:.3}",
        ref_tokens.len(),
        hyp_tokens.len(),
        dist,
        wer,
        agreement
    );
    println!("  (1.000 = identical; lower = more segmented-ASR drift)");

    Ok(())
}

/// Word-token Levenshtein distance (insertions + deletions + substitutions).
#[cfg(feature = "cohere-mlx")]
fn levenshtein(a: &[&str], b: &[&str]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![0usize; b.len() + 1];
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1)
                .min(cur[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}
