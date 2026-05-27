//! Energy-based pause segmenter — splits a mono 16 kHz f32 buffer into speech
//! segments at silence gaps, so the streaming-cleanup path can clean one sentence
//! at a time (each segment ≈ one utterance bounded by a pause).
//!
//! This is the cheapest primitive that works (per the project's design
//! principle): pure DSP, no model, no deps. It was validated in
//! `examples/vad_stream_lab.rs`, where pause-aligned segmentation tracked the
//! oracle sentence boundaries and a learned VAD (Silero) was not worth its cost
//! on clean audio. The one tuned constant is `min_pause_ms` (400): long enough to
//! ignore intra-sentence breaths (300 ms over-split sentences in testing) while
//! still catching real sentence gaps.
//!
//! NOTE: validated on synthetic speech with clean pauses. Real mic audio has
//! noise floor and softer pauses, so `peak_frac`/`min_pause_ms` may need
//! re-tuning against live recordings before this drives the default path.

/// Tuning for [`segment_speech`]. Defaults are the values validated in the lab.
#[derive(Debug, Clone, Copy)]
pub struct VadConfig {
    /// Analysis frame length in milliseconds (RMS computed per frame).
    pub frame_ms: f32,
    /// A silence run at least this long closes the current segment.
    pub min_pause_ms: f32,
    /// Speech threshold as a fraction of the clip's peak frame RMS.
    pub peak_frac: f32,
    /// Context padding added to each segment edge, in milliseconds.
    pub pad_ms: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self { frame_ms: 20.0, min_pause_ms: 400.0, peak_frac: 0.04, pad_ms: 60.0 }
    }
}

/// Split `audio` (mono f32 at `sr` Hz) into speech segments as inclusive-start /
/// exclusive-end sample ranges. Silence-only or empty input yields a single
/// whole-buffer segment, so callers can always treat the result as "the pieces to
/// process" without special-casing.
pub fn segment_speech(audio: &[f32], sr: u32, cfg: &VadConfig) -> Vec<(usize, usize)> {
    let frame = (cfg.frame_ms / 1000.0 * sr as f32) as usize;
    let min_pause_frames = (cfg.min_pause_ms / cfg.frame_ms).round() as usize;
    let pad = (cfg.pad_ms / 1000.0 * sr as f32) as usize;
    if frame == 0 || audio.is_empty() {
        return vec![(0, audio.len())];
    }

    let rms: Vec<f32> = audio
        .chunks(frame)
        .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let peak = rms.iter().cloned().fold(0.0f32, f32::max).max(1e-9);
    let thresh = cfg.peak_frac * peak;
    let speech: Vec<bool> = rms.iter().map(|&r| r > thresh).collect();

    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < speech.len() {
        if !speech[i] {
            i += 1;
            continue;
        }
        // Start of a speech run; extend until a long-enough silence gap.
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
        let s = (start * frame).saturating_sub(pad);
        let e = ((last_speech + 1) * frame + pad).min(audio.len());
        segs.push((s, e.max(s + 1)));
        i = j;
    }
    if segs.is_empty() {
        segs.push((0, audio.len()));
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 16_000;

    /// Append `secs` of a loud-ish tone (constant amplitude) to `buf`.
    fn tone(buf: &mut Vec<f32>, secs: f32, amp: f32) {
        for n in 0..(secs * SR as f32) as usize {
            // simple alternating signal so RMS is `amp` regardless of frame phase
            buf.push(if n % 2 == 0 { amp } else { -amp });
        }
    }
    fn silence(buf: &mut Vec<f32>, secs: f32) {
        buf.extend(std::iter::repeat(0.0).take((secs * SR as f32) as usize));
    }

    #[test]
    fn empty_and_silence_yield_one_segment() {
        let cfg = VadConfig::default();
        assert_eq!(segment_speech(&[], SR, &cfg), vec![(0, 0)]);
        let sil = vec![0.0f32; SR as usize];
        assert_eq!(segment_speech(&sil, SR, &cfg), vec![(0, sil.len())]);
    }

    #[test]
    fn two_sentences_split_at_a_long_pause() {
        // speech | 0.5s pause | speech  → 2 segments (pause > 400ms)
        let mut a = Vec::new();
        silence(&mut a, 0.2);
        tone(&mut a, 1.0, 0.5);
        silence(&mut a, 0.5);
        tone(&mut a, 1.0, 0.5);
        silence(&mut a, 0.2);
        let segs = segment_speech(&a, SR, &VadConfig::default());
        assert_eq!(segs.len(), 2, "got {segs:?}");
        // first segment ends before the second begins
        assert!(segs[0].1 <= segs[1].0);
    }

    #[test]
    fn short_internal_gap_does_not_split() {
        // a 200ms gap (< 400ms min_pause) must NOT split a single utterance
        let mut a = Vec::new();
        tone(&mut a, 0.8, 0.5);
        silence(&mut a, 0.2);
        tone(&mut a, 0.8, 0.5);
        let segs = segment_speech(&a, SR, &VadConfig::default());
        assert_eq!(segs.len(), 1, "200ms gap should not split: {segs:?}");
    }

    #[test]
    fn segments_are_ordered_and_in_bounds() {
        let mut a = Vec::new();
        for _ in 0..4 {
            tone(&mut a, 0.6, 0.4);
            silence(&mut a, 0.5);
        }
        let segs = segment_speech(&a, SR, &VadConfig::default());
        assert_eq!(segs.len(), 4);
        for (s, e) in &segs {
            assert!(s < e && *e <= a.len());
        }
        for w in segs.windows(2) {
            assert!(w[0].1 <= w[1].0, "segments overlap/disordered: {segs:?}");
        }
    }
}
