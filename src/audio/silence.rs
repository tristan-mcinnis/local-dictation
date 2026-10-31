//! Silence: frame energy, the noise floor, speech thresholds, tail capture,
//! trailing-silence trim, and the split of long audio for the ASR.
use super::*;

// ─── Tail capture (release grace) ───────────────────────────────────────
//
// On key release the daemon used to drop the input stream immediately, which
// clipped the last syllable of a word that was still being spoken. Tail capture
// keeps the mic open for a brief grace loop after release, draining newly
// arrived audio into the same path the streaming ASR uses, and stops once the
// level has stayed below an *adaptive* speech threshold for a short quiet run
// (or a hard cap). The decision logic below is pure + tested on synthetic RMS
// series so the tuning is provable without a microphone.

/// Tail-capture grace: how long the level must stay below the speech threshold
/// before we declare the utterance over. 250 ms is a comfortable inter-word
/// pause; any shorter risks stopping mid-word, any longer just adds latency.
pub const DEFAULT_TAIL_QUIET_MS: u64 = 250;

/// Tail-capture grace: how often the grace loop polls + re-samples the level.
pub const DEFAULT_TAIL_POLL_MS: u64 = 40;

/// Tail-capture grace: hard ceiling the grace loop will never exceed, so a
/// user who keeps talking indefinitely can't stall the release pipeline.
pub const DEFAULT_TAIL_CAP_MS: u64 = 1500;

/// Tail-capture analysis frame length (ms) used to compute per-frame RMS for
/// the noise-floor estimate and the release-tail quietness check. 30 ms is long
/// enough to average out a noisy click while staying finer than a 250 ms pause.
pub const DEFAULT_TAIL_FRAME_MS: u32 = 30;

/// Tail-capture grace: how quiet (below threshold) the last `DEFAULT_TAIL_ENGAGE_MS`
/// before release must already be for us to skip the grace entirely. If the user
/// let go during a natural pause, there's nothing to recover — zero added latency.
pub const DEFAULT_TAIL_ENGAGE_MS: u32 = 300;

/// How much the adaptive speech threshold rises above the measured noise floor
/// (floor × factor). A factor of 3 keeps real speech well above ambient tone
/// without being blind to soft speech in a noisy room.
pub const TAIL_THRESHOLD_FLOOR_FACTOR: f32 = 3.0;

/// Whisper-mode multiplier on the noise floor: lower than the normal ×3 so
/// quiet speech stays above the threshold. See [`speech_threshold_with_mode`].
pub const WHISPER_THRESHOLD_FLOOR_FACTOR: f32 = 1.5;

/// Whisper-mode absolute floor: lower than [`TAIL_ABS_FLOOR`] so a hushed
/// whisper in a near-silent room still registers as speech.
pub const WHISPER_ABS_FLOOR: f32 = 0.005;

/// Absolute floor on the speech threshold, so an ultra-quiet room can never push
/// the threshold to zero (and thus never treat pure silence as "speech").
pub const TAIL_ABS_FLOOR: f32 = 0.01;

/// Tail-capture sensitivity: the release-tail engage decision and the grace
/// loop's quiet check use a LOWER threshold than plain speech detection.
/// Voices naturally fall in level at the end of a sentence, so a trailing word
/// spoken softly sat just under the ×3 threshold, read as "released in a
/// pause", skipped the grace, and got clipped — the one drop path tail-capture
/// left open. ×2 with a lower absolute floor keeps that word visible while a
/// genuinely quiet pause (ambient ≈ floor) still skips for zero added latency.
pub const TAIL_SOFT_FLOOR_FACTOR: f32 = 2.0;

/// Absolute floor paired with [`TAIL_SOFT_FLOOR_FACTOR`] (see there).
pub const TAIL_SOFT_ABS_FLOOR: f32 = 0.006;

/// Low percentile of the per-frame RMS distribution used as the noise-floor proxy.
pub const TAIL_NOISE_PERCENTILE: f32 = 0.10;

/// Warm-window (ms): how long a press-to-open recording keeps the mic stream
/// open after it stops, so a back-to-back dictation reuses the live stream and
/// skips cpal's ~120 ms stream-open cost. 0 disables it (every utterance opens
/// fresh; today's behaviour). Measured against the always-on path, which already
/// never closes the mic.
pub const DEFAULT_WARM_WINDOW_MS: u32 = 60_000;

/// Per-frame RMS of a mono buffer: chunk `samples` into `frame_len`-sample
/// frames and return the RMS of each. Empty/zero-frame input yields an empty
/// Vec. This is the primitive the tail-capture decision logic operates on.
pub fn frame_rms(samples: &[f32], frame_len: usize) -> Vec<f32> {
    if frame_len == 0 || samples.is_empty() {
        return Vec::new();
    }
    samples
        .chunks(frame_len)
        .map(|c| {
            let sum: f32 = c.iter().map(|x| x * x).sum();
            (sum / c.len() as f32).sqrt()
        })
        .collect()
}

/// Estimate the room's noise floor as the low `percentile` of the frame-RMS
/// distribution observed during the recording. Speech pushes the upper tail up;
/// the bottom decile stays representative of ambient tone, so returning it as the
/// floor makes the speech threshold adaptive to the room (quiet study vs. a fan
/// running). Returns 0.0 for empty input.
pub fn noise_floor(rms_frames: &[f32], percentile: f32) -> f32 {
    if rms_frames.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f32> = rms_frames.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() - 1) as f32 * percentile.clamp(0.0, 1.0)).round() as usize;
    sorted[idx]
}

/// How many recent per-frame RMS values the [`NoiseFloorEstimator`] keeps to
/// estimate the room's noise floor (~96 frames at 30 ms ≈ 2.9 s). A small,
/// bounded window is enough: the noise floor is a stable property of the room,
/// not of how long you talk, and keeping it bounded keeps the release-path
/// floor computation O(ring-size) instead of O(utterance).
pub const TAIL_FLOOR_RING_FRAMES: usize = 96;

/// Bounded running estimate of the room's noise floor, fed incrementally
/// DURING capture (a per-frame RMS value pushed as each frame is captured)
/// rather than re-sorted from the whole utterance at release. Keeps a small
/// ring of recent per-frame RMS values; [`floor`](Self::floor) returns the low
/// percentile over that recent window. This is what removes the O(utterance)
/// noise-floor sort from the release→inject critical path.
pub struct NoiseFloorEstimator {
    frames: std::collections::VecDeque<f32>,
    capacity: usize,
}

impl NoiseFloorEstimator {
    pub fn new(capacity: usize) -> Self {
        Self {
            frames: std::collections::VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }
    }

    /// Push one per-frame RMS value, dropping the oldest once the ring is full.
    pub fn update(&mut self, rms: f32) {
        if self.frames.len() == self.capacity {
            self.frames.pop_front();
        }
        self.frames.push_back(rms);
    }

    /// Low `percentile` of the recent per-frame RMS window (0.0 for empty).
    pub fn floor(&self, percentile: f32) -> f32 {
        if self.frames.is_empty() {
            return 0.0;
        }
        let vals: Vec<f32> = self.frames.iter().copied().collect();
        noise_floor(&vals, percentile)
    }

    /// Drop the whole window (called when a fresh utterance begins).
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }
}

/// Estimate the room's noise floor directly from a bounded audio slice (the
/// ~tail-engage-ms trailing window), for the whole-buffer path where the
/// estimator wasn't fed during capture. Bounded: never scans the whole
/// utterance, so the release path stays O(window).
pub fn noise_floor_of_audio(audio: &[f32], frame_len: usize, percentile: f32) -> f32 {
    let frames = frame_rms(audio, frame_len);
    noise_floor(&frames, percentile)
}

/// Adaptive speech threshold from a measured noise floor: `max(floor × factor,
/// abs_floor)`. The absolute floor guards against a hyper-quiet room where the
/// floor is ~0, which would otherwise make even a whisper look like quiet.
pub fn speech_threshold(floor: f32) -> f32 {
    speech_threshold_with_mode(floor, false)
}

/// Like [`speech_threshold`] but with a whisper-mode option. Whisper mode uses
/// a lower floor multiplier and a lower absolute floor so quiet speech is
/// detected as speech rather than silence. The normal path is bit-identical to
/// [`speech_threshold`].
pub fn speech_threshold_with_mode(floor: f32, whisper: bool) -> f32 {
    let (factor, abs_floor) = if whisper {
        (WHISPER_THRESHOLD_FLOOR_FACTOR, WHISPER_ABS_FLOOR)
    } else {
        (TAIL_THRESHOLD_FLOOR_FACTOR, TAIL_ABS_FLOOR)
    };
    (floor * factor).max(abs_floor)
}

/// Speech threshold for the TAIL-CAPTURE path only (engage decision + grace
/// quiet check): more sensitive than [`speech_threshold_with_mode`] so a soft
/// trailing word still reads as speech — see [`TAIL_SOFT_FLOOR_FACTOR`].
/// Whisper mode is lower still and unchanged.
pub fn tail_speech_threshold(floor: f32, whisper: bool) -> f32 {
    if whisper {
        speech_threshold_with_mode(floor, true)
    } else {
        (floor * TAIL_SOFT_FLOOR_FACTOR).max(TAIL_SOFT_ABS_FLOOR)
    }
}

/// True if the last `n` frames (`rms_frames`) are all below `threshold` — i.e.
/// the release happens during a pause. Used both for the grace-engagement check
/// (last ~300 ms) and as a building block. Fewer than `n` frames returns true
/// only if every available frame is below threshold (nothing to lose).
pub fn frames_quiet(rms_frames: &[f32], threshold: f32, n: usize) -> bool {
    let start = rms_frames.len().saturating_sub(n);
    let tail = &rms_frames[start..];
    tail.iter().all(|&r| r < threshold)
}

/// Number of consecutive frames needed to represent `ms` of audio at `frame_ms`
/// per frame (ceiling, so a partial frame still counts as covering the window).
pub fn quiet_frame_count(ms: u32, frame_ms: u32) -> usize {
    if frame_ms == 0 {
        return 0;
    }
    (ms as f32 / frame_ms as f32).ceil() as usize
}

/// Pure quiet-run detector for the grace loop. `rms_series` is the per-poll RMS
/// sequence (newest last), sampled every `dt_ms`. Returns true once the trailing
/// run of below-`threshold` samples has persisted for at least `quiet_ms` of
/// wall-clock time — i.e. the speaker has been silent long enough to declare the
/// utterance finished. Kept free of `Instant` so it's unit-testable on synthetic
/// series: `dt_ms` is the caller's poll interval, not a real clock.
pub fn quiet_run(rms_series: &[f32], dt_ms: u64, threshold: f32, quiet_ms: u64) -> bool {
    let mut count: u64 = 0;
    for &r in rms_series.iter().rev() {
        if r < threshold {
            count += 1;
        } else {
            break;
        }
    }
    count * dt_ms >= quiet_ms
}

/// Tail-capture engage decision, bounded to a trailing window. Given ONLY a
/// bounded slice of the last ~`engage_ms` before release (never the whole
/// utterance) plus the already-decided adaptive floor, decide whether release
/// happened mid-speech. `floor` comes from the incremental
/// [`NoiseFloorEstimator`] (streaming) or a bounded [`noise_floor_of_audio`]
/// pass over the same trailing window (whole-buffer). Scans only `tail` —
/// O(window), never O(utterance). Returns the (threshold, engage) pair so the
/// caller reuses the same threshold in the grace loop's quiet check.
pub fn tail_capture_engage(
    tail: &[f32],
    frame_len: usize,
    floor: f32,
    engage_ms: u32,
    frame_ms: u32,
) -> (f32, bool) {
    tail_capture_engage_with_mode(tail, frame_len, floor, engage_ms, frame_ms, false)
}

/// Like [`tail_capture_engage`] but with a whisper-mode option so the daemon can
/// thread the user's whisper preference through the tail-capture engage decision.
pub fn tail_capture_engage_with_mode(
    tail: &[f32],
    frame_len: usize,
    floor: f32,
    engage_ms: u32,
    frame_ms: u32,
    whisper: bool,
) -> (f32, bool) {
    let threshold = tail_speech_threshold(floor, whisper);
    let frames = frame_rms(tail, frame_len);
    let engage_frames = quiet_frame_count(engage_ms, frame_ms);
    let engage = !frames_quiet(&frames, threshold, engage_frames);
    (threshold, engage)
}

/// Minimum above-threshold audio (ms) a chunk must hold to be worth
/// transcribing at all. Below this the ASR only hallucinates — room tone in,
/// "Thank you." out. A misfire tap (~0.5 s of near-silence) and a release-time
/// tail both fall under this.
pub const MIN_SPEECH_MS: f32 = 200.0;

/// Trailing audio kept after the last above-threshold frame, so decaying word
/// tails (fricatives, breath) aren't clipped by the trim.
pub const TRIM_HANGOVER_MS: u32 = 120;

/// Total above-`threshold` audio in `samples`, in milliseconds.
pub fn speech_ms(samples: &[f32], frame_len: usize, threshold: f32, frame_ms: u32) -> f32 {
    frame_rms(samples, frame_len)
        .iter()
        .filter(|&&r| r >= threshold)
        .count() as f32
        * frame_ms as f32
}

/// Keep-length after trimming trailing sub-threshold audio (+[`TRIM_HANGOVER_MS`]
/// hangover). Returns 0 when nothing in `samples` crosses `threshold`.
pub fn trim_trailing_silence(
    samples: &[f32],
    frame_len: usize,
    threshold: f32,
    frame_ms: u32,
) -> usize {
    let frames = frame_rms(samples, frame_len);
    match frames.iter().rposition(|&r| r >= threshold) {
        None => 0,
        Some(last) => {
            let keep_frames = last + 1 + quiet_frame_count(TRIM_HANGOVER_MS, frame_ms);
            (keep_frames * frame_len).min(samples.len())
        }
    }
}

/// Pre-transcribe hallucination guard: trim trailing room tone and decide
/// whether a chunk holds enough speech to transcribe. Returns `Some(keep_len)`
/// — transcribe `&samples[..keep_len]` — or `None` to skip transcription
/// entirely (the chunk is silence; ASR would only invent text).
pub fn transcribe_keep_len(
    samples: &[f32],
    frame_len: usize,
    threshold: f32,
    frame_ms: u32,
) -> Option<usize> {
    if speech_ms(samples, frame_len, threshold, frame_ms) < MIN_SPEECH_MS {
        return None;
    }
    Some(trim_trailing_silence(samples, frame_len, threshold, frame_ms).max(1))
}

/// Build a bounded chronological snapshot of ONLY the trailing `window_ms` of the
/// current utterance's captured audio, WITHOUT draining the live ring and WITHOUT
/// copying the whole buffer. In the streaming path the drained samples live in
/// `stream_audio` (older) and the sub-tick tail is still in the ring (newer); in
/// the whole-buffer path the ring holds everything. Returns the trailing
/// `window_ms` across that split, so the tail-capture engage check stays O(window)
/// and never O(utterance).
pub fn recording_tail(
    consumer: &HeapAudioConsumer,
    stream_audio: &[f32],
    window_ms: u32,
) -> Vec<f32> {
    use ringbuf::traits::{Consumer, Observer};
    let window = (window_ms as f64 / 1000.0 * SAMPLE_RATE as f64) as usize;
    let ring_keep = consumer.occupied_len().min(window);
    let from_stream = window.saturating_sub(ring_keep);
    let mut out = Vec::with_capacity(window);
    // Older drained samples first (the part of `stream_audio` still inside window).
    let stream_start = stream_audio.len().saturating_sub(from_stream);
    out.extend_from_slice(&stream_audio[stream_start..]);
    // Then the newest ring samples, oldest→newest across the wrap: [tail of left][tail of right].
    if ring_keep > 0 {
        let (left, right) = consumer.as_slices();
        let right_take = right.len().min(ring_keep);
        let left_needed = ring_keep.saturating_sub(right_take);
        if left_needed > 0 {
            let left_start = left.len().saturating_sub(left_needed);
            out.extend_from_slice(&left[left_start..]);
        }
        let right_start = right.len().saturating_sub(right_take);
        out.extend_from_slice(&right[right_start..]);
    }
    out
}

// ---------------------------------------------------------------------------
// ASR request length cap
// ---------------------------------------------------------------------------

/// The most audio Cohere Transcribe Q4 decodes faithfully in one request.
///
/// Measured 2026-09-09 against the shipped helper with a 177 s script of forty
/// numbered markers (`docs/truncation-diagnosis-20260909.md`): every marker
/// survives up to 40 s; at 50 s the model starts silently dropping whole
/// sentences from the middle; by 100 s only nine of twenty markers remain and
/// the word count saturates near 140 no matter how long the audio runs. There
/// is no error and no truncation marker — the transcript simply comes back
/// short and plausible, which is the worst possible failure for dictation.
///
/// `max_tokens` is not the limit (256, 1024 and 4096 return identical text),
/// and mlx-audio's own chunking is disabled by `chunkDuration: 1200` in the
/// Swift bridge. So the cap has to be enforced here, on the Rust side, as a
/// floor that holds even when the VAD fails to segment a long hold.
///
/// 30 s keeps a safety margin under the measured 40 s cliff. Chunking is also
/// *faster* than one long request (3.9 s vs 4.7 s over the same 177 s).
pub const ASR_MAX_CHUNK_SECS: f32 = 30.0;

/// Fraction of a chunk that is eligible as a cut point. The split search scans
/// the last 40% of each chunk (18 s–30 s at the default cap) for the quietest
/// frame, so a cut lands in a pause rather than mid-word whenever the speaker
/// draws breath at all.
pub(super) const ASR_SPLIT_SEARCH_FRAC: f32 = 0.40;

/// Frame length used by the split search, in milliseconds.
pub(super) const ASR_SPLIT_FRAME_MS: f32 = 20.0;

/// Split `audio` into chunks no longer than `max_secs`, cutting at the quietest
/// frame near the end of each chunk so the boundary falls in a pause.
///
/// Audio at or under the cap comes back as a single whole-buffer range, so the
/// common short dictation is untouched (same bytes, same ASR request, no cost).
/// Ranges are inclusive-start / exclusive-end and always cover the whole buffer
/// with no gaps and no overlap: every sample is transcribed exactly once.
pub fn split_for_asr(audio: &[f32], sr: u32, max_secs: f32) -> Vec<(usize, usize)> {
    let max_len = (max_secs.max(1.0) * sr as f32) as usize;
    if audio.is_empty() {
        return vec![(0, 0)];
    }
    if audio.len() <= max_len {
        return vec![(0, audio.len())];
    }
    let frame_len = ((ASR_SPLIT_FRAME_MS / 1000.0) * sr as f32).max(1.0) as usize;
    let search_len = ((max_len as f32) * ASR_SPLIT_SEARCH_FRAC) as usize;

    let mut out = Vec::new();
    let mut start = 0usize;
    while audio.len() - start > max_len {
        let hard_end = start + max_len;
        let search_start = hard_end.saturating_sub(search_len).max(start + frame_len);
        // Quietest frame in the eligible window becomes the cut point. Ties take
        // the earliest frame, which keeps chunks a touch shorter rather than
        // risking the hard cap.
        let quietest = frame_rms(&audio[search_start..hard_end], frame_len)
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        // Cut at the middle of the quietest frame, never before `start`.
        let cut = (search_start + quietest * frame_len + frame_len / 2).min(hard_end);
        let cut = cut.max(start + frame_len).min(hard_end);
        out.push((start, cut));
        start = cut;
    }
    if start < audio.len() {
        out.push((start, audio.len()));
    }
    out
}
