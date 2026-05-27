//! VAD-stream lab (ROUND 3) — does the round-2 win survive REAL (non-oracle)
//! segmentation, end to end on audio?
//!
//! Round 2 proved (in text space, with ORACLE sentence boundaries) that cleaning
//! per-sentence during the hold collapses the post-release tail to ~one sentence
//! while holding quality — IF segmentation is good. This lab closes the loop on
//! real audio: a VAD finds pause boundaries, each segment is transcribed
//! (Parakeet) AND cleaned (Gemma) as it closes during the hold, and we measure
//! the felt latency + quality + dictionary against today's whole-buffer pipeline.
//!
//! ```bash
//! ./scripts/gen-vad-corpus.sh
//! cargo run --release --features full --example vad_stream_lab
//! ```
//!
//! Segmenter is an ENERGY VAD (cheapest primitive first, per the project's design
//! principle). The corpus has clean injected 550ms pauses, so this measures the
//! BEST case for pause segmentation; Silero is the round-3b upgrade if energy
//! VAD's boundary quality (not the streaming idea) turns out to be the limiter.
//!
//! Per entry, compares:
//!   * WHOLE      — transcribe whole buffer, clean whole (today's pipeline)
//!   * VAD-STREAM — VAD segments → per-segment transcribe + incremental cleanup
//! Metrics: detected segments vs true sentence count; end-to-end residual-at-
//! release (last segment ASR+cleanup) vs whole total; WER(stream vs whole) +
//! WER(both vs gold); dictionary name-recall. All run production's corrections pass.

#[cfg(not(all(feature = "parakeet", feature = "cleaner")))]
fn main() {
    eprintln!("vad_stream_lab needs --features full");
    std::process::exit(1);
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
use serde::Deserialize;

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
#[derive(Deserialize)]
struct Truth {
    wav: String,
    pause_ms: u32,
    sentences: Vec<String>,
    dur_s: f32,
    tier: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    vocab: Vec<String>,
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    use fast_dictate_backend::app_paths;
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::corrections::Corrections;
    use fast_dictate_backend::settings::Settings;
    use fast_dictate_backend::transcriber::load_wav_mono16k;
    use parakeet_rs::{ParakeetTDT, TimestampMode, Transcriber};
    use std::time::Instant;

    const SR: u32 = 16_000;

    // load truth manifests
    let dir = std::path::Path::new("testdata/vad");
    if !dir.exists() {
        eyre::bail!("run ./scripts/gen-vad-corpus.sh first");
    }
    let mut truths: Vec<Truth> = Vec::new();
    let order = [
        "speaker-id", "slide-review", "realtime-transcript", "readme-agent",
        "hands-free", "dictation-apps", "evidence-julien", "logs-readme",
        "commit-github", "lingzi-note", "anthropic-onnx", "self-correction",
    ];
    for name in order {
        let p = dir.join(format!("{name}.truth.json"));
        if p.exists() {
            truths.push(serde_json::from_str(&std::fs::read_to_string(p)?)?);
        }
    }

    // models
    let parakeet_dir = std::env::var("PARAKEET_MODEL_DIR")
        .unwrap_or_else(|_| app_paths::parakeet_default_dir());
    let mut asr = ParakeetTDT::from_pretrained(&parakeet_dir, None)
        .map_err(|e| eyre::eyre!("parakeet: {e:?}"))?;
    let gemma = Settings::load().resolve_gemma(&app_paths::gemma_default_path());
    let t = Instant::now();
    let engine = TextCleanupEngine::initialize(&gemma)?;
    let corrections =
        Corrections::load_default().unwrap_or_else(|_| Corrections::from_map(Default::default()));
    eprintln!("[lab] models loaded in {:?}\n", t.elapsed());

    let transcribe = |asr: &mut ParakeetTDT, audio: &[f32]| -> eyre::Result<String> {
        if audio.is_empty() {
            return Ok(String::new());
        }
        Ok(asr
            .transcribe_samples(audio.to_vec(), SR, 1, Some(TimestampMode::Words))
            .map_err(|e| eyre::eyre!("transcribe: {e:?}"))?
            .text)
    };

    // warm
    {
        let a = load_wav_mono16k(&truths[0].wav)?;
        let _ = transcribe(&mut asr, &a[..a.len().min(SR as usize)])?;
        let _ = engine.process_transcript("warm up").await?;
    }

    println!("{:<20} {:>5} {:>4} {:>5} {:>9} {:>10} {:>8} {:>8} {:>9} {:>7}",
        "clip", "dur", "sent", "segs", "whole_ms", "strm_resid", "asrNoOv", "asrOv", "WERvWhl", "names");

    // tier, whole_ms, resid, wer_gold_stream, wer_stream_whole, asr_noov, asr_ov
    let mut agg: Vec<(String, f64, f64, f64, f64, f64, f64)> = Vec::new();

    for tr in &truths {
        let audio = load_wav_mono16k(&tr.wav)?;
        let gold = tr.sentences.join(" ");

        // ---- WHOLE (today): transcribe all + clean all ----
        let t = Instant::now();
        let whole_raw = transcribe(&mut asr, &audio)?;
        let whole_clean = corrections.apply(&engine.process_transcript_with_vocab(&whole_raw, &tr.vocab).await?);
        let whole_ms = t.elapsed().as_secs_f64() * 1000.0;

        // ---- VAD segmentation ----
        let segs = energy_vad_segments(&audio, SR);
        // ~0.5s of prior audio prepended to each segment so Parakeet keeps the
        // acoustic context it would have in the whole buffer (fixes isolated-
        // segment mis-hears like brief→brave); the pre-roll words are dropped.
        let pad = (0.5 * SR as f32) as usize;

        // ---- ASR-only A/B: segment ASR with vs without overlap, vs whole ----
        let mut raw_noov: Vec<String> = Vec::new();
        let mut raw_ov: Vec<String> = Vec::new();
        for (s, e) in &segs {
            let n = transcribe(&mut asr, &audio[*s..*e])?;
            if !n.trim().is_empty() {
                raw_noov.push(n);
            }
            let o = transcribe_overlap(&mut asr, &audio, *s, *e, pad, SR)?;
            if !o.trim().is_empty() {
                raw_ov.push(o);
            }
        }
        let asr_noov = wer(&normalize(&whole_raw), &normalize(&raw_noov.join(" "))) * 100.0;
        let asr_ov = wer(&normalize(&whole_raw), &normalize(&raw_ov.join(" "))) * 100.0;

        // ---- VAD-STREAM: per segment transcribe (w/ overlap) + incremental clean ----
        let mut cleaned_so_far = String::new();
        let mut pieces: Vec<String> = Vec::new();
        let mut last_ms = 0.0;
        // Stream uses NO overlap: the A/B below shows pre-roll overlap only adds
        // boundary word-drop errors on pause-bounded segments (asrOv > asrNoOv),
        // while no-overlap segment ASR already matches the whole buffer (~0.6%).
        for (s, e) in &segs {
            let t = Instant::now();
            let raw = transcribe(&mut asr, &audio[*s..*e])?;
            if raw.trim().is_empty() {
                continue;
            }
            let prompt = ctx_prompt(&tr.vocab, &cleaned_so_far);
            let piece = engine.eval_cleanup(&prompt, &raw, false).await?;
            last_ms = t.elapsed().as_secs_f64() * 1000.0;
            if !cleaned_so_far.is_empty() {
                cleaned_so_far.push(' ');
            }
            cleaned_so_far.push_str(piece.trim());
            pieces.push(piece.trim().to_string());
        }
        let stream_clean = corrections.apply(&pieces.join(" "));

        // ---- metrics ----
        let gnorm = normalize(&gold);
        let wer_gold_stream = wer(&gnorm, &normalize(&stream_clean)) * 100.0;
        let wer_gold_whole = wer(&gnorm, &normalize(&whole_clean)) * 100.0;
        let wer_stream_whole = wer(&normalize(&whole_clean), &normalize(&stream_clean)) * 100.0;
        let (nf_s, nt) = name_recall(&stream_clean, &tr.names);
        let (nf_w, _) = name_recall(&whole_clean, &tr.names);

        println!("{:<20} {:>4.0}s {:>4} {:>5} {:>9.1} {:>10.1} {:>7.1}% {:>7.1}% {:>8.1}% {}/{} v{}/{}",
            trunc(&tr.wav), tr.dur_s, tr.sentences.len(), segs.len(),
            whole_ms, last_ms, asr_noov, asr_ov, wer_stream_whole,
            nf_s, nt, nf_w, nt);

        agg.push((tr.tier.clone(), whole_ms, last_ms, wer_gold_stream, wer_stream_whole, asr_noov, asr_ov));

        if tr.tier == "long" {
            println!("    gold  : {:?}", trunc2(&gold, 180));
            println!("    whole : {:?}", trunc2(&whole_clean, 180));
            println!("    stream: {:?}", trunc2(&stream_clean, 180));
            // segmentation accuracy: detected segments vs true sentence count
            println!("    segs={} vs true sentences={} (pause {}ms)",
                segs.len(), tr.sentences.len(), tr.pause_ms);
        }
        let _ = wer_gold_whole;
    }

    // summary
    println!("\n══════════ SUMMARY by tier ══════════");
    println!("{:<8} {:>9} {:>11} {:>9} {:>9} {:>9} {:>9}",
        "tier", "whole_ms", "strm_resid", "felt_cut", "asrNoOv", "asrOv", "WERvWhol");
    for tier in ["long", "medium", "short"] {
        let rows: Vec<&(String, f64, f64, f64, f64, f64, f64)> = agg.iter().filter(|a| a.0 == tier).collect();
        if rows.is_empty() { continue; }
        let n = rows.len() as f64;
        let m = |f: &dyn Fn(&(String, f64, f64, f64, f64, f64, f64)) -> f64| rows.iter().map(|a| f(a)).sum::<f64>() / n;
        let whole = m(&|a| a.1);
        let resid = m(&|a| a.2);
        println!("{:<8} {:>9.1} {:>11.1} {:>8.0}% {:>8.1}% {:>8.1}% {:>8.1}%",
            tier, whole, resid, (1.0 - resid / whole) * 100.0, m(&|a| a.5), m(&|a| a.6), m(&|a| a.4));
    }
    println!("\nfelt_cut = today's post-release wait that disappears (resid = last segment only).\n  asrNoOv/asrOv = segment-ASR WER vs whole-buffer ASR, WITHOUT vs WITH 0.5s\n  pre-roll overlap (lower = overlap recovers acoustic context). WERvWhol = final\n  cleaned-output drift from today.");

    Ok(())
}

/// Transcribe segment [s,e) with `pad` samples of pre-roll context prepended,
/// then drop the words that fall in the pre-roll (their start < pad seconds) so
/// the result is just the segment — but decoded with the prior acoustic context.
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn transcribe_overlap(
    asr: &mut parakeet_rs::ParakeetTDT,
    audio: &[f32],
    s: usize,
    e: usize,
    pad: usize,
    sr: u32,
) -> eyre::Result<String> {
    use parakeet_rs::{TimestampMode, Transcriber};
    let start = s.saturating_sub(pad);
    let pre_s = (s - start) as f32 / sr as f32; // seconds of pre-roll actually present
    let clip = &audio[start..e];
    if clip.is_empty() {
        return Ok(String::new());
    }
    let r = asr
        .transcribe_samples(clip.to_vec(), sr, 1, Some(TimestampMode::Words))
        .map_err(|err| eyre::eyre!("transcribe: {err:?}"))?;
    if pre_s <= 0.0 {
        return Ok(r.text);
    }
    // keep words whose midpoint is after the pre-roll
    let kept: Vec<&str> = r
        .tokens
        .iter()
        .filter(|t| (t.start + t.end) / 2.0 >= pre_s)
        .map(|t| t.text.as_str())
        .collect();
    Ok(kept.join(" "))
}

/// Energy-based pause segmenter. 20ms frames; speech where RMS exceeds a floor
/// relative to the clip peak; a silence run ≥ MIN_PAUSE closes a segment.
/// Returns inclusive-start/exclusive-end sample ranges of speech segments.
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn energy_vad_segments(audio: &[f32], sr: u32) -> Vec<(usize, usize)> {
    let frame = (0.020 * sr as f32) as usize; // 20 ms
    let min_pause_frames = (0.300 / 0.020) as usize; // 300 ms silence ends a segment
    let pad = frame * 3; // include a little context around each segment edge
    if frame == 0 || audio.is_empty() {
        return vec![(0, audio.len())];
    }
    let rms: Vec<f32> = audio
        .chunks(frame)
        .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let peak = rms.iter().cloned().fold(0.0f32, f32::max).max(1e-9);
    let thresh = 0.04 * peak; // 4% of peak ≈ silence vs speech on clean TTS
    let speech: Vec<bool> = rms.iter().map(|&r| r > thresh).collect();

    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < speech.len() {
        if !speech[i] {
            i += 1;
            continue;
        }
        // start of a speech run; extend until a long-enough silence gap
        let start = i;
        let mut last_speech = i;
        let mut j = i + 1;
        while j < speech.len() {
            if speech[j] {
                last_speech = j;
            } else if j - last_speech >= min_pause_frames {
                break;
            }
            j += 1;
        }
        let s = start.saturating_mul(frame).saturating_sub(pad);
        let e = ((last_speech + 1) * frame + pad).min(audio.len());
        segs.push((s, e.max(s + 1)));
        i = j;
    }
    if segs.is_empty() {
        segs.push((0, audio.len()));
    }
    segs
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn ctx_prompt(vocab: &[String], ctx: &str) -> String {
    use fast_dictate_backend::prompts::{vocabulary_suffix, DEFAULT_CLEANUP};
    let mut p = DEFAULT_CLEANUP.to_string();
    if let Some(suffix) = vocabulary_suffix(vocab) {
        p.push_str(&suffix);
    }
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

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn name_recall(text: &str, names: &[String]) -> (usize, usize) {
    (names.iter().filter(|n| text.contains(n.as_str())).count(), names.len())
}

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

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn wer(reference: &[String], hyp: &[String]) -> f64 {
    let (n, m) = (reference.len(), hyp.len());
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

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn trunc(wav: &str) -> String {
    std::path::Path::new(wav).file_stem().and_then(|s| s.to_str()).unwrap_or(wav).to_string()
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn trunc2(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}
