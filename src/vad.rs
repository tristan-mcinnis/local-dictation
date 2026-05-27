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

/// Incremental front-end to [`segment_speech`] for the streaming-cleanup path.
/// Audio is pushed in as it's captured; [`Self::take_complete`] returns the
/// sample ranges of sentence-segments that are *confirmed finished* (a pause of
/// at least `min_pause_ms` follows them), so the daemon can transcribe + clean
/// them mid-hold. A segment is never emitted while it might still be growing, so
/// callers never clean a half-spoken sentence. At key release,
/// [`Self::take_final`] flushes whatever remains regardless of trailing silence.
///
/// Ranges are absolute offsets into the full pushed stream ([`Self::buf`]), and
/// each is emitted exactly once. No audio padding/overlap is added — segment ASR
/// of a pause-bounded clip already matches whole-buffer ASR, and overlap was
/// measured to *hurt* (see examples/vad_stream_lab.rs).
pub struct SegmentStream {
    sr: u32,
    cfg: VadConfig,
    buf: Vec<f32>,
    committed: usize,
}

impl SegmentStream {
    pub fn new(sr: u32, cfg: VadConfig) -> Self {
        // No audio overlap in the streaming path (measured to hurt).
        Self { sr, cfg: VadConfig { pad_ms: 0.0, ..cfg }, buf: Vec::new(), committed: 0 }
    }

    /// Append newly captured samples.
    pub fn push(&mut self, samples: &[f32]) {
        self.buf.extend_from_slice(samples);
    }

    /// The full audio captured so far (ranges from `take_*` index into this).
    pub fn buf(&self) -> &[f32] {
        &self.buf
    }

    fn min_pause_samples(&self) -> usize {
        (self.cfg.min_pause_ms / 1000.0 * self.sr as f32) as usize
    }

    /// Sample ranges of segments that are confirmed finished (a long-enough pause
    /// follows). Advances the internal cursor past them so each emits once.
    pub fn take_complete(&mut self) -> Vec<(usize, usize)> {
        if self.committed >= self.buf.len() {
            return Vec::new();
        }
        // A silence-only remainder comes back as one whole-range "segment" with no
        // trailing pause, which maybe_emit correctly declines to emit.
        let rel = segment_speech(&self.buf[self.committed..], self.sr, &self.cfg);
        self.maybe_emit(rel)
    }

    /// Decide which of `rel` (relative ranges) are confirmed-finished, emit them
    /// as absolute ranges, and advance the cursor.
    fn maybe_emit(&mut self, rel: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
        let buf_rel_len = self.buf.len() - self.committed;
        let min_pause = self.min_pause_samples();
        let n = rel.len();
        let mut out = Vec::new();
        for (idx, &(s, e)) in rel.iter().enumerate() {
            let is_last = idx == n - 1;
            // earlier segments were closed by a real pause; the last is finished
            // only if enough silence trails it within the buffer.
            let finished = !is_last || buf_rel_len.saturating_sub(e) >= min_pause;
            if finished {
                out.push((self.committed + s, self.committed + e));
            } else {
                break;
            }
        }
        // Advance to the start of the first not-yet-finished segment, or to the
        // end of the buffer if everything was emitted.
        if out.len() == n {
            self.committed = self.buf.len();
        } else {
            self.committed += rel[out.len()].0;
        }
        out
    }

    /// Drop the already-emitted (committed) prefix so a long-lived stream (e.g.
    /// the always-on wake-word listener) doesn't grow without bound. Only the
    /// not-yet-finished tail is retained; ranges from earlier `take_*` calls are
    /// no longer valid after this. Safe to call after draining `take_complete`.
    pub fn compact(&mut self) {
        if self.committed == 0 {
            return;
        }
        self.buf.drain(0..self.committed);
        self.committed = 0;
    }

    /// Flush all remaining audio as final segments (called at key release).
    pub fn take_final(&mut self) -> Vec<(usize, usize)> {
        if self.committed >= self.buf.len() {
            return Vec::new();
        }
        let rel = segment_speech(&self.buf[self.committed..], self.sr, &self.cfg);
        let base = self.committed;
        self.committed = self.buf.len();
        rel.into_iter()
            .map(|(s, e)| (base + s, base + e))
            .filter(|(s, e)| e > s)
            .collect()
    }
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
    fn stream_emits_finished_sentences_and_holds_the_open_one() {
        let cfg = VadConfig::default();
        let mut st = SegmentStream::new(SR, cfg);

        // push: speech (sentence 1) — no trailing pause yet → nothing finished
        let mut a = Vec::new();
        tone(&mut a, 1.0, 0.5);
        st.push(&a);
        assert!(st.take_complete().is_empty(), "open sentence must not emit");

        // push a long pause, then start sentence 2 → sentence 1 now confirmed
        let mut b = Vec::new();
        silence(&mut b, 0.5);
        tone(&mut b, 0.5, 0.5);
        st.push(&b);
        let done = st.take_complete();
        assert_eq!(done.len(), 1, "sentence 1 should be confirmed: {done:?}");

        // sentence 2 is still open → nothing more until its pause or release
        assert!(st.take_complete().is_empty());

        // release: flush the tail
        let fin = st.take_final();
        assert_eq!(fin.len(), 1, "tail sentence at release: {fin:?}");
    }

    #[test]
    fn stream_each_segment_emitted_once_and_covers_speech() {
        let cfg = VadConfig::default();
        let mut st = SegmentStream::new(SR, cfg);
        // three sentences separated by long pauses, pushed in one go
        let mut a = Vec::new();
        for _ in 0..3 {
            tone(&mut a, 0.6, 0.5);
            silence(&mut a, 0.5);
        }
        st.push(&a);
        let mut all = st.take_complete();
        all.extend(st.take_final());
        assert_eq!(all.len(), 3, "all three sentences exactly once: {all:?}");
        for w in all.windows(2) {
            assert!(w[0].1 <= w[1].0, "ordered, non-overlapping: {all:?}");
        }
    }

    #[test]
    fn compact_drops_emitted_prefix_but_keeps_open_tail() {
        let cfg = VadConfig::default();
        let mut st = SegmentStream::new(SR, cfg);
        // sentence 1 + long pause + start of sentence 2
        let mut a = Vec::new();
        tone(&mut a, 0.6, 0.5);
        silence(&mut a, 0.5);
        tone(&mut a, 0.4, 0.5);
        st.push(&a);
        let done = st.take_complete();
        assert_eq!(done.len(), 1, "sentence 1 confirmed: {done:?}");
        let before = st.buf().len();
        st.compact();
        // The open tail (sentence 2, still growing) must survive.
        assert!(st.buf().len() < before, "prefix dropped");
        assert!(!st.buf().is_empty(), "open tail retained");
        // After compaction the tail still flushes exactly once at the end.
        let fin = st.take_final();
        assert_eq!(fin.len(), 1, "tail flushes after compact: {fin:?}");
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
