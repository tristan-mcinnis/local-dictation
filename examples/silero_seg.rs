//! Minimal Silero VAD segmenter (round-3b) — ⚠️ PARKED / WIP.
//!
//! The `ort` wiring works end-to-end: the model loads, runs over 512-sample
//! (32ms) windows, and the hidden state propagates correctly (stateN L1 ≈ 494,
//! evolving each window). BUT the speech probabilities come back flat (~0, max
//! ~0.2 on loud speech), so no segment crosses the 0.5 threshold. Verified NOT
//! caused by: sr tensor shape (scalar vs [1]), state propagation, or window size
//! (1536 errors — v5 wants 512). This is a model-version / invocation-convention
//! detail in this specific silero_vad.onnx export (the silero team has changed
//! the ONNX I/O several times). Parked rather than rabbit-holed — the energy VAD
//! in vad_stream_lab already answers the segmentation question well enough, and
//! its only weakness (mild over-segmentation) was addressed there directly.
//! To revive: cross-check against the `voice_activity_detector` crate's exact
//! call sequence, or pin a known-good model export.
//!
//! ```bash
//! ./scripts/gen-vad-corpus.sh
//! cargo run --release --features parakeet --example silero_seg
//! ```
//! (only needs `parakeet` for the WAV loader; ort is a dev-dependency.)

#[cfg(not(feature = "parakeet"))]
fn main() {
    eprintln!("silero_seg needs --features parakeet (for the wav loader)");
    std::process::exit(1);
}

#[cfg(feature = "parakeet")]
fn main() -> eyre::Result<()> {
    use fast_dictate_backend::transcriber::load_wav_mono16k;
    use ort::session::Session;
    use ort::value::Tensor;

    const SR: i64 = 16_000;
    const WIN: usize = 512; // Silero v5 window @ 16 kHz = 32 ms

    let model = "testdata/vad-model/silero_vad.onnx";
    if !std::path::Path::new(model).exists() {
        eyre::bail!("missing {model} — download silero_vad.onnx first");
    }
    let mut sess = Session::builder()?.commit_from_file(model)?;

    let dir = std::path::Path::new("testdata/vad");
    let mut clips: Vec<(String, usize)> = Vec::new(); // (wav, true sentence count)
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) == Some("json") {
            let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
            let wav = v["wav"].as_str().unwrap_or_default().to_string();
            let n = v["sentences"].as_array().map(|a| a.len()).unwrap_or(0);
            if std::path::Path::new(&wav).exists() {
                clips.push((wav, n));
            }
        }
    }
    clips.sort();

    println!("{:<20} {:>6} {:>10} {:>12} {:>11}", "clip", "true", "silero_seg", "silero_speech%", "energy_seg");
    for (wav, true_n) in &clips {
        let audio = load_wav_mono16k(wav)?;

        // ---- Silero: per-window speech probability with carried state ----
        let mut state = vec![0.0f32; 2 * 128]; // [2,1,128]
        let mut probs: Vec<f32> = Vec::new();
        let mut buf = [0.0f32; WIN];
        let mut i = 0;
        while i < audio.len() {
            let n = (audio.len() - i).min(WIN);
            buf[..n].copy_from_slice(&audio[i..i + n]);
            if n < WIN {
                buf[n..].fill(0.0);
            }
            let input = Tensor::from_array(([1_i64, WIN as i64], buf.to_vec()))?;
            let state_t = Tensor::from_array(([2_i64, 1, 128], state.clone()))?;
            let sr_t = Tensor::from_array((vec![] as Vec<i64>, vec![SR]))?;
            let outputs = sess.run(ort::inputs![
                "input" => input,
                "state" => state_t,
                "sr" => sr_t,
            ])?;
            let (_, prob) = outputs["output"].try_extract_tensor::<f32>()?;
            probs.push(prob[0]);
            let (_, new_state) = outputs["stateN"].try_extract_tensor::<f32>()?;
            state.copy_from_slice(new_state);
            i += WIN;
        }

        let speech: Vec<bool> = probs.iter().map(|&p| p > 0.5).collect();
        let win_s = WIN as f32 / SR as f32;
        let sil_count = speech.iter().filter(|&&s| !s).count();
        let speech_pct = 100.0 * (1.0 - sil_count as f32 / speech.len().max(1) as f32);
        let silero_segs = count_segments(&speech, (0.3 / win_s as f64) as usize);
        let energy_segs = energy_segments(&audio, 16_000);

        println!("{:<20} {:>6} {:>10} {:>11.0}% {:>11}",
            stem(wav), true_n, silero_segs, speech_pct, energy_segs);
    }
    Ok(())
}

/// Count speech segments: runs of speech split by silence ≥ min_sil frames.
#[cfg(feature = "parakeet")]
fn count_segments(speech: &[bool], min_sil: usize) -> usize {
    let mut segs = 0;
    let mut i = 0;
    while i < speech.len() {
        if !speech[i] {
            i += 1;
            continue;
        }
        segs += 1;
        let mut last = i;
        let mut j = i + 1;
        while j < speech.len() {
            if speech[j] {
                last = j;
            } else if j - last >= min_sil {
                break;
            }
            j += 1;
        }
        i = j;
    }
    segs.max(1)
}

/// Mirror of the energy VAD in vad_stream_lab, for side-by-side comparison.
#[cfg(feature = "parakeet")]
fn energy_segments(audio: &[f32], sr: u32) -> usize {
    let frame = (0.020 * sr as f32) as usize;
    if frame == 0 || audio.is_empty() {
        return 1;
    }
    let rms: Vec<f32> = audio
        .chunks(frame)
        .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let peak = rms.iter().cloned().fold(0.0f32, f32::max).max(1e-9);
    let speech: Vec<bool> = rms.iter().map(|&r| r > 0.04 * peak).collect();
    count_segments(&speech, (0.300 / 0.020) as usize)
}

#[cfg(feature = "parakeet")]
fn stem(wav: &str) -> String {
    std::path::Path::new(wav).file_stem().and_then(|s| s.to_str()).unwrap_or(wav).to_string()
}
