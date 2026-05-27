//! Streaming lab — does chunked/"streaming" transcription make a long dictation
//! feel as snappy as a short one, and what does it cost in accuracy + cleanup?
//!
//! This is an EXPERIMENT. It touches nothing in the daemon. It loads Parakeet +
//! Gemma once and drives them from the synthetic corpus produced by
//! `scripts/gen-streaming-corpus.sh` (each .wav has a matching .txt ground truth).
//!
//! ```bash
//! ./scripts/gen-streaming-corpus.sh
//! cargo run --release --features full --example streaming_lab
//! ```
//!
//! Background facts that shape this test (see CLAUDE.md / parakeet-rs 0.3.5):
//!   * Parakeet has NO streaming API — only whole-buffer `transcribe_samples`,
//!     full-context encoder. So "streaming" is SIMULATED by chunking the buffer
//!     into overlapping windows, transcribing each, and stitching at the overlap
//!     midpoint using word timestamps. True streaming needs a different export.
//!   * Today the post-release tail = ASR(whole) + cleanup(whole). BOTH grow with
//!     length. The first thing we measure is how that tail splits, because if
//!     cleanup dominates at 3 min, hiding ASR during the hold barely helps.
//!
//! Phases:
//!   1. TAIL DECOMPOSITION   — ASR ms vs cleanup ms vs clip length (the decider).
//!   2. CHUNKED ASR          — accuracy (WER vs ground truth & vs whole-buffer,
//!                             seam errors) + "residual at release" latency model.
//!   3. CLEANUP INTERACTION  — whole-transcript cleanup vs per-chunk cleanup
//!                             (quality delta + total Gemma cost).
//!   4. VAD TEASER           — silence ratio per clip (round-2 input only).

#[cfg(not(all(feature = "parakeet", feature = "cleaner")))]
fn main() {
    eprintln!("streaming_lab needs both `parakeet` and `cleaner` — run with --features full");
    std::process::exit(1);
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    use fast_dictate_backend::app_paths;
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::settings::Settings;
    use fast_dictate_backend::transcriber::load_wav_mono16k;
    use parakeet_rs::{ParakeetTDT, TimestampMode, Transcriber};
    use std::time::Instant;

    const SR: u32 = 16_000;
    // Chunking knobs for the simulated stream. CHUNK_S = window length the
    // model sees; OVERLAP_S = how much consecutive windows share (so a word
    // straddling a boundary is fully seen by at least one window and we can
    // dedup at the overlap midpoint).
    const CHUNK_S: f32 = 8.0;
    const OVERLAP_S: f32 = 1.5;

    // ---- corpus ----
    let mut clips: Vec<(String, String)> = Vec::new(); // (wav_path, ground_truth)
    for name in ["tiny", "short", "medium", "long"] {
        let wav = format!("testdata/streaming/{name}.wav");
        let txt = format!("testdata/streaming/{name}.txt");
        if std::path::Path::new(&wav).exists() {
            let gt = std::fs::read_to_string(&txt).unwrap_or_default();
            clips.push((wav, gt));
        }
    }
    // The real (human) 8 s sample — no ground truth, so WER-vs-GT is skipped,
    // but it still exercises real acoustics for the whole-vs-chunked deltas.
    if std::path::Path::new("testdata/dictation_sample.wav").exists() {
        clips.push(("testdata/dictation_sample.wav".into(), String::new()));
    }
    if clips.is_empty() {
        eyre::bail!("no corpus — run ./scripts/gen-streaming-corpus.sh first");
    }

    // ---- load models once ----
    let parakeet_dir = std::env::var("PARAKEET_MODEL_DIR")
        .unwrap_or_else(|_| app_paths::parakeet_default_dir());
    eprintln!("[lab] parakeet: {parakeet_dir}");
    let mut asr = ParakeetTDT::from_pretrained(&parakeet_dir, None)
        .map_err(|e| eyre::eyre!("parakeet load: {e:?}"))?;
    let gemma = Settings::load().resolve_gemma(&app_paths::gemma_default_path());
    eprintln!("[lab] gemma:    {gemma}");
    let t = Instant::now();
    let cleaner = TextCleanupEngine::initialize(&gemma)?;
    eprintln!("[lab] models loaded in {:?}\n", t.elapsed());

    // Helper: whole-buffer transcribe (Words mode for stitching), return (text, tokens).
    let transcribe = |asr: &mut ParakeetTDT, audio: &[f32]| -> eyre::Result<TranscriptionOut> {
        let r = asr
            .transcribe_samples(audio.to_vec(), SR, 1, Some(TimestampMode::Words))
            .map_err(|e| eyre::eyre!("transcribe: {e:?}"))?;
        Ok(TranscriptionOut {
            text: r.text,
            words: r
                .tokens
                .iter()
                .map(|t| (t.text.clone(), t.start, t.end))
                .collect(),
        })
    };

    // Warm both models (first call pays one-time graph/Metal init).
    {
        let (_w, gt) = &clips[0];
        let a = load_wav_mono16k(&clips[0].0)?;
        let _ = transcribe(&mut asr, &a)?;
        let _ = cleaner.process_transcript("warm up the cleaner once").await?;
        let _ = gt;
    }

    // =====================================================================
    // PHASE 1 — TAIL DECOMPOSITION
    // =====================================================================
    println!("\n========== PHASE 1: post-release tail = ASR + cleanup ==========");
    println!(
        "{:<8} {:>7} {:>10} {:>10} {:>10} {:>9}",
        "clip", "dur_s", "asr_ms", "clean_ms", "total_ms", "asr_xRT"
    );
    let mut whole_cache: Vec<WholeRun> = Vec::new();
    for (wav, gt) in &clips {
        let audio = load_wav_mono16k(wav)?;
        let dur = audio.len() as f32 / SR as f32;

        let t = Instant::now();
        let asr_out = transcribe(&mut asr, &audio)?;
        let asr_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let cleaned = cleaner.process_transcript(&asr_out.text).await?;
        let clean_ms = t.elapsed().as_secs_f64() * 1000.0;

        let name = clip_name(wav);
        println!(
            "{:<8} {:>7.1} {:>10.1} {:>10.1} {:>10.1} {:>8.1}x",
            name,
            dur,
            asr_ms,
            clean_ms,
            asr_ms + clean_ms,
            dur as f64 * 1000.0 / asr_ms
        );
        whole_cache.push(WholeRun {
            name: name.to_string(),
            gt: gt.clone(),
            dur,
            audio,
            asr_text: asr_out.text,
            asr_ms,
            cleaned,
            clean_ms,
        });
    }
    println!(
        "→ READ THIS FIRST: if clean_ms dwarfs asr_ms on `long`, streaming ASR\n  alone won't make long clips snappy — cleanup would have to be chunked too."
    );

    // =====================================================================
    // PHASE 2 — CHUNKED ASR: accuracy + residual-at-release latency model
    // =====================================================================
    println!("\n========== PHASE 2: chunked ASR ({CHUNK_S}s window / {OVERLAP_S}s overlap) ==========");
    println!(
        "{:<8} {:>6} {:>10} {:>11} {:>11} {:>12} {:>11} {:>11}",
        "clip", "chunks", "sumASR_ms", "lastASR_ms", "WERvsGT%", "WERvsWhole%", "base_ms", "resid_ms"
    );
    for w in &whole_cache {
        let plan = chunk_plan(w.audio.len(), SR, CHUNK_S, OVERLAP_S);
        let mut chunk_words: Vec<Vec<(String, f32, f32)>> = Vec::new();
        let mut chunk_ms: Vec<f64> = Vec::new();
        for c in &plan {
            let seg = &w.audio[c.start_sample..c.end_sample];
            let t = Instant::now();
            let out = transcribe(&mut asr, seg)?;
            chunk_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            // shift token times into global timeline
            let off = c.start_s;
            chunk_words.push(
                out.words
                    .into_iter()
                    .map(|(w, s, e)| (w, s + off, e + off))
                    .collect(),
            );
        }
        let stitched = stitch(&plan, &chunk_words);

        let sum_asr: f64 = chunk_ms.iter().sum();
        let last_asr = *chunk_ms.last().unwrap_or(&0.0);
        // residual at release: only the LAST chunk's ASR is unavoidable post-release
        // (earlier chunks finished during the hold), then whole-transcript cleanup.
        let resid = last_asr + w.clean_ms;
        let base = w.asr_ms + w.clean_ms;

        let wer_gt = if w.gt.trim().is_empty() {
            f64::NAN
        } else {
            wer(&normalize(&w.gt), &normalize(&stitched)) * 100.0
        };
        let wer_whole = wer(&normalize(&w.asr_text), &normalize(&stitched)) * 100.0;

        println!(
            "{:<8} {:>6} {:>10.1} {:>11.1} {:>11} {:>12.2} {:>11.1} {:>11.1}",
            w.name,
            plan.len(),
            sum_asr,
            last_asr,
            if wer_gt.is_nan() { "  n/a".into() } else { format!("{wer_gt:.2}") },
            wer_whole,
            base,
            resid,
        );
    }
    println!(
        "→ resid_ms vs base_ms = the felt latency win if ASR streams during the hold.\n  WERvsWhole% = damage from chunk seams (0 = stitched output identical to today's)."
    );

    // =====================================================================
    // PHASE 3 — CLEANUP INTERACTION: whole vs per-chunk cleanup
    // =====================================================================
    println!("\n========== PHASE 3: cleanup whole vs per-chunk ==========");
    println!(
        "{:<8} {:>11} {:>13} {:>12} {:>14}",
        "clip", "whole_ms", "perchunk_ms", "n_calls", "WER(whole↔pc)%"
    );
    for w in &whole_cache {
        // Re-chunk the AUDIO, transcribe, then clean each chunk's text separately
        // and concatenate — the "chunked cleanup" world.
        let plan = chunk_plan(w.audio.len(), SR, CHUNK_S, OVERLAP_S);
        let mut chunk_words: Vec<Vec<(String, f32, f32)>> = Vec::new();
        for c in &plan {
            let seg = &w.audio[c.start_sample..c.end_sample];
            let out = transcribe(&mut asr, seg)?;
            let off = c.start_s;
            chunk_words.push(
                out.words.into_iter().map(|(w, s, e)| (w, s + off, e + off)).collect(),
            );
        }
        // Stitch into per-keep-window text segments, clean each separately.
        let segments = stitch_segments(&plan, &chunk_words);
        let t = Instant::now();
        let mut pieces = Vec::new();
        for seg_text in &segments {
            if seg_text.trim().is_empty() {
                continue;
            }
            pieces.push(cleaner.process_transcript(seg_text).await?);
        }
        let perchunk_ms = t.elapsed().as_secs_f64() * 1000.0;
        let perchunk_joined = pieces.join(" ");

        let wer_pc = wer(&normalize(&w.cleaned), &normalize(&perchunk_joined)) * 100.0;
        println!(
            "{:<8} {:>11.1} {:>13.1} {:>12} {:>14.2}",
            w.name,
            w.clean_ms,
            perchunk_ms,
            segments.iter().filter(|s| !s.trim().is_empty()).count(),
            wer_pc,
        );
        if w.name == "long" {
            println!("    whole-cleanup    : {:?}", truncate(&w.cleaned, 240));
            println!("    perchunk-cleanup : {:?}", truncate(&perchunk_joined, 240));
        }
    }
    println!(
        "→ per-chunk cleanup is what hides cleanup latency, but WER(whole↔pc) and the\n  sample text show the quality cost (lost cross-sentence context, seam joins)."
    );

    // =====================================================================
    // PHASE 4 — VAD TEASER: how much of each clip is silence?
    // =====================================================================
    println!("\n========== PHASE 4: silence ratio (VAD round-2 teaser) ==========");
    println!("{:<8} {:>7} {:>12} {:>14}", "clip", "dur_s", "silence_%", "speech_dur_s");
    for w in &whole_cache {
        let (sil, speech_s) = silence_ratio(&w.audio, SR);
        println!("{:<8} {:>7.1} {:>11.1} {:>14.1}", w.name, w.dur, sil * 100.0, speech_s);
    }
    println!(
        "→ silence_% ≈ ASR compute a VAD could skip; round 2 tests Silero-segmented\n  chunking (boundaries at pauses never cut a word)."
    );

    Ok(())
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
struct TranscriptionOut {
    text: String,
    words: Vec<(String, f32, f32)>, // (word, start_s, end_s)
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
struct WholeRun {
    name: String,
    gt: String,
    dur: f32,
    audio: Vec<f32>,
    asr_text: String,
    asr_ms: f64,
    cleaned: String,
    clean_ms: f64,
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
struct Chunk {
    start_sample: usize,
    end_sample: usize,
    start_s: f32,
    end_s: f32,
}

/// Split [0, n) samples into overlapping windows. stride = chunk - overlap.
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn chunk_plan(n: usize, sr: u32, chunk_s: f32, overlap_s: f32) -> Vec<Chunk> {
    let chunk = (chunk_s * sr as f32) as usize;
    let stride = ((chunk_s - overlap_s) * sr as f32) as usize;
    let mut out = Vec::new();
    if n <= chunk {
        out.push(Chunk { start_sample: 0, end_sample: n, start_s: 0.0, end_s: n as f32 / sr as f32 });
        return out;
    }
    let mut start = 0usize;
    while start < n {
        let end = (start + chunk).min(n);
        out.push(Chunk {
            start_sample: start,
            end_sample: end,
            start_s: start as f32 / sr as f32,
            end_s: end as f32 / sr as f32,
        });
        if end == n {
            break;
        }
        start += stride;
    }
    out
}

/// Per-chunk keep window [keep_start, keep_end): the overlap-midpoint boundary
/// between consecutive chunks decides which chunk "owns" the shared region.
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn keep_windows(plan: &[Chunk]) -> Vec<(f32, f32)> {
    let mut wins = Vec::with_capacity(plan.len());
    for i in 0..plan.len() {
        let ks = if i == 0 {
            0.0
        } else {
            (plan[i].start_s + plan[i - 1].end_s) / 2.0
        };
        let ke = if i == plan.len() - 1 {
            f32::INFINITY
        } else {
            (plan[i + 1].start_s + plan[i].end_s) / 2.0
        };
        wins.push((ks, ke));
    }
    wins
}

/// Stitch chunk word-tokens into one transcript using the keep windows.
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn stitch(plan: &[Chunk], chunk_words: &[Vec<(String, f32, f32)>]) -> String {
    stitch_segments(plan, chunk_words).join(" ")
}

/// Like `stitch` but returns one text segment per chunk keep-window (for the
/// per-chunk cleanup experiment).
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn stitch_segments(plan: &[Chunk], chunk_words: &[Vec<(String, f32, f32)>]) -> Vec<String> {
    let wins = keep_windows(plan);
    let mut segs = Vec::with_capacity(plan.len());
    for (i, words) in chunk_words.iter().enumerate() {
        let (ks, ke) = wins[i];
        let kept: Vec<&str> = words
            .iter()
            .filter(|(_, s, e)| {
                let center = (s + e) / 2.0;
                center >= ks && center < ke
            })
            .map(|(w, _, _)| w.as_str())
            .collect();
        segs.push(kept.join(" "));
    }
    segs
}

/// Normalize for WER: lowercase, keep [a-z0-9'] runs, split on whitespace.
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn normalize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|w| w.to_string())
        .collect()
}

/// Word-level WER = Levenshtein(ref, hyp) / len(ref).
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
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

/// Crude energy VAD just to estimate silence fraction (NOT used for decoding).
/// 25 ms frames; a frame is "speech" if its RMS exceeds 6% of the clip's peak RMS.
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn silence_ratio(audio: &[f32], sr: u32) -> (f32, f32) {
    let frame = (0.025 * sr as f32) as usize;
    if frame == 0 || audio.is_empty() {
        return (0.0, 0.0);
    }
    let rms: Vec<f32> = audio
        .chunks(frame)
        .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let peak = rms.iter().cloned().fold(0.0f32, f32::max).max(1e-9);
    let thresh = 0.06 * peak;
    let speech_frames = rms.iter().filter(|&&r| r > thresh).count();
    let total = rms.len();
    let speech_s = speech_frames as f32 * frame as f32 / sr as f32;
    (1.0 - speech_frames as f32 / total as f32, speech_s)
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn clip_name(wav: &str) -> &str {
    std::path::Path::new(wav)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(wav)
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
