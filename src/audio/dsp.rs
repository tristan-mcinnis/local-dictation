//! Sample processing: downmix and channel pick, resample, loudness
//! normalisation for the ASR, and the pre-roll ring.
use super::*;

/// How a multi-channel callback buffer becomes one mono sample per frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Downmix {
    /// Average every channel. Right for a mono or stereo microphone, where
    /// both channels carry the same voice.
    Mean,
    /// Take one channel. Right for an interface or aggregate device, where the
    /// microphone is on one input and the rest are silence — averaging those
    /// in costs 20·log10(N) dB and makes normal speech read as "no speech".
    Channel(usize),
}

/// Picks one input channel per stream and then sticks to it.
///
/// Switching channels mid-word would click, so the choice is made once: for a
/// device with more than two inputs, the loudest channel of the first
/// [`PROBE_MS`] window that actually carries sound. One and two channel
/// devices keep the mean.
///
/// **The window must carry sound.** A stream opens before the user speaks —
/// the always-on and warm-window streams open long before — so the first
/// 200 ms is almost always silence, and locking onto the loudest channel of
/// silence picks channel 0 by tie-break and keeps it for the life of the
/// stream. On the very interface this exists to fix (the mic on input 3 of
/// eight) that is a *silent* capture: worse than the averaging it replaced,
/// which at least carried the voice at an eighth of its level. So a window
/// below [`PICK_RMS_FLOOR`] is discarded and the next one is listened to,
/// and the mean is used in the meantime.
pub struct ChannelPicker {
    channels: usize,
    energy: Vec<f64>,
    frames_seen: usize,
    pub(super) probe_frames: usize,
    chosen: Option<Downmix>,
}

/// How much of a stream is listened to before its channel is fixed.
pub const PROBE_MS: usize = 200;

/// Quietest window that is allowed to decide the channel, as RMS. Well above
/// digital silence and converter dither, well below any speech — a whisper at
/// arm's length is an order of magnitude louder.
pub const PICK_RMS_FLOOR: f64 = 0.004;

/// The channel to listen to, given one probe window's summed squares per
/// channel and how many frames went into it.
///
/// `None` when the window was too quiet to decide anything — the caller
/// listens to another window rather than committing to a guess.
pub fn pick_channel(energy: &[f64], frames: usize) -> Option<usize> {
    if energy.is_empty() || frames == 0 {
        return None;
    }
    // Strictly greater, so a tie — a duplicated signal across inputs — keeps
    // the lowest channel rather than drifting to the last.
    let mut best = 0usize;
    for (channel, value) in energy.iter().enumerate() {
        if *value > energy[best] {
            best = channel;
        }
    }
    let rms = (energy[best] / frames as f64).sqrt();
    (rms >= PICK_RMS_FLOOR).then_some(best)
}

impl ChannelPicker {
    pub fn new(channels: usize, native_rate: u32) -> Self {
        let probe_frames = (native_rate as usize * PROBE_MS / 1000).max(1);
        Self {
            channels,
            energy: vec![0.0; channels],
            frames_seen: 0,
            probe_frames,
            chosen: (channels <= 2).then_some(Downmix::Mean),
        }
    }

    /// Feed one interleaved callback buffer and get the downmix to use for it.
    pub fn observe(&mut self, interleaved: &[f32]) -> Downmix {
        if let Some(chosen) = self.chosen {
            return chosen;
        }
        for frame in interleaved.chunks_exact(self.channels) {
            for (channel, sample) in frame.iter().enumerate() {
                self.energy[channel] += (*sample as f64) * (*sample as f64);
            }
        }
        self.frames_seen += interleaved.len() / self.channels.max(1);
        if self.frames_seen < self.probe_frames {
            // The mean while we listen: never silence, whatever the wiring.
            return Downmix::Mean;
        }
        let decided = pick_channel(&self.energy, self.frames_seen);
        self.energy.iter_mut().for_each(|e| *e = 0.0);
        self.frames_seen = 0;
        let Some(best) = decided else {
            // Nothing but room tone in that window. Keep listening.
            return Downmix::Mean;
        };
        eprintln!("[audio] listening on input channel {best} of {}", self.channels);
        let chosen = Downmix::Channel(best);
        self.chosen = Some(chosen);
        chosen
    }

    /// A new utterance is starting on this stream. Drop any partial probe
    /// window — its accumulated energy belongs to the previous utterance — but
    /// keep a channel that was already chosen: a device change forces a rebuild,
    /// so the wiring has not changed, and re-probing would attenuate the first
    /// 200 ms of every reused utterance on a multi-input interface.
    pub fn reset_for_new_utterance(&mut self) {
        if self.chosen.is_none() {
            self.energy.iter_mut().for_each(|e| *e = 0.0);
            self.frames_seen = 0;
        }
    }
}

/// How much of `wall` a stream spent asleep, from the wall-clock and monotonic
/// ages of the same stream.
///
/// `Instant` does not advance while the Mac is asleep and `SystemTime` does,
/// so the difference is the sleep. Comparing the wall-clock age against the
/// warm window instead — which is what this replaced — called every stream
/// that had simply been *open* longer than the window a sleep, including one
/// that had just carried a long dictation, and logged a lie about it.
pub fn slept_for(wall: std::time::Duration, mono: std::time::Duration) -> std::time::Duration {
    wall.saturating_sub(mono)
}

/// How much clock skew is tolerated before a gap is read as a sleep. Comfortably
/// over any NTP correction or scheduling jitter, far under a lid closed for a
/// coffee.
pub const SLEEP_SLACK: std::time::Duration = std::time::Duration::from_secs(20);

/// Fold one interleaved frame to a single sample under `mix`.
pub fn fold_frame(frame: &[f32], mix: Downmix) -> f32 {
    match mix {
        Downmix::Mean => {
            if frame.is_empty() {
                return 0.0;
            }
            frame.iter().sum::<f32>() / frame.len() as f32
        }
        Downmix::Channel(channel) => frame.get(channel).copied().unwrap_or(0.0),
    }
}

/// The per-stream state a capture callback carries across invocations: the
/// fractional resample position, the previous mono sample (interpolated across
/// a chunk boundary), the channel picker, and the epoch this state was last
/// reset at. Held as one value so the "new utterance" reset is a single,
/// testable method instead of three pieces of loose state in the callback.
pub(super) struct ResampleState {
    pos: f64,
    last: f32,
    picker: ChannelPicker,
    epoch: u64,
}

impl ResampleState {
    pub(super) fn new(channels: usize, native_rate: u32, epoch: u64) -> Self {
        Self {
            pos: 0.0,
            last: 0.0,
            picker: ChannelPicker::new(channels, native_rate),
            epoch,
        }
    }

    /// Called at the top of every recording callback with the stream's current
    /// epoch. A changed epoch means [`resume`](AudioCaptureEngine::resume)
    /// restarted a parked stream: this is a new utterance, so drop the previous
    /// one's fractional carry and re-arm the channel probe. Without this the
    /// first sample of the new utterance is interpolated against the last
    /// sample of the old one — a click at the seam.
    pub(super) fn begin_callback(&mut self, stream_epoch: u64) {
        if stream_epoch != self.epoch {
            self.epoch = stream_epoch;
            self.pos = 0.0;
            self.last = 0.0;
            self.picker.reset_for_new_utterance();
        }
    }
}

/// Downmix interleaved native samples to mono, then linearly resample at
/// `stride = native_rate / 16000` and push to the SPSC ring buffer.
///
/// `state` carries the fractional resample position, the last mono sample and
/// the channel picker across cpal callback invocations. `epoch` is the engine's
/// resume generation: it is read **after** the recording flag, so a callback
/// that sees `recording == true` is guaranteed to read the epoch `resume`
/// bumped and reset the carry before it emits a sample.
pub(super) fn push_resampled(
    interleaved: &[f32],
    channels: usize,
    stride: f64,
    state: &mut ResampleState,
    epoch: &AtomicU64,
    producer: &mut HeapAudioProducer,
    recording: &AtomicBool,
) {
    let frames = interleaved.len() / channels.max(1);
    if frames == 0 {
        return;
    }

    // Recording first, epoch second — see the function docs. The reset only
    // matters while recording; a parked stream delivers no callbacks at all.
    if recording.load(Ordering::SeqCst) {
        state.begin_callback(epoch.load(Ordering::SeqCst));
    }

    let mix = state.picker.observe(interleaved);
    // Reusable mono buffer.
    let mut mono = Vec::with_capacity(frames);
    if channels <= 1 {
        mono.extend_from_slice(interleaved);
    } else {
        for frame in interleaved.chunks_exact(channels) {
            mono.push(fold_frame(frame, mix));
        }
    }

    // Push RMS of this chunk to the waveform ring for the UI pill. Computed
    // pre-resample on native mono so it reflects real microphone activity.
    let sum_sq: f32 = mono.iter().map(|x| x * x).sum();
    let rms = (sum_sq / mono.len() as f32).sqrt();
    crate::ui_channel::push_level(rms);

    // Linear-interpolate at `stride`. `pos` indexes into the mono buffer.
    // When pos crosses a frame boundary the carry advances by `stride` again.
    // Always advance `pos`/`last` so the carry stays a consistent position, but
    // only push to the producer while recording.
    while state.pos < mono.len() as f64 {
        let idx = state.pos.floor() as usize;
        let frac = (state.pos - idx as f64) as f32;
        let a = if idx == 0 { state.last } else { mono[idx - 1] };
        let b = mono[idx];
        let sample = a + (b - a) * frac;
        if recording.load(Ordering::SeqCst) {
            let _ = producer.try_push(sample);
        }
        state.pos += stride;
    }
    // Subtract the consumed frames so `pos` stays a small fractional offset
    // into the next chunk.
    state.pos -= mono.len() as f64;
    state.last = *mono.last().unwrap();
}

/// Target RMS the ASR front-end normalizes each utterance toward (~ -18 dBFS).
/// Quiet / whispered speech is boosted up to this level and a raised voice is
/// gently brought down, so the Cohere MLX engine sees a consistent loudness no
/// matter how close or loud you speak — this is what lets you dictate at a
/// whisper instead of having to project (cloud tools like Wispr Flow do the same).
pub const ASR_TARGET_RMS: f32 = 0.12;

/// Hard ceiling on any sample after gain — keeps a boost from ever clipping.
pub const ASR_PEAK_CEILING: f32 = 0.97;

/// Cap on the boost factor, so near-silence / room tone can't be amplified into
/// a roar when there's no real speech to normalize.
pub const ASR_MAX_GAIN: f32 = 16.0;

/// Peak below which the buffer is treated as silence and left untouched (warm-up
/// passes, empty captures) — avoids dividing by ~0 and amplifying nothing.
pub(super) const ASR_SILENCE_PEAK: f32 = 1.0e-4;

/// Automatic input gain for the ASR front-end, applied in place. Normalizes the
/// utterance to a consistent loudness ([`ASR_TARGET_RMS`]) so a whisper
/// transcribes as well as a raised voice, while clamping the gain so it can
/// never clip ([`ASR_PEAK_CEILING`]) or blow up room tone ([`ASR_MAX_GAIN`]).
/// One O(n) pass, run at release (off the critical path). Returns the gain
/// applied (1.0 when the buffer was silent or already at level), for logging.
pub fn normalize_for_asr(audio: &mut [f32]) -> f32 {
    if audio.is_empty() {
        return 1.0;
    }
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &x in audio.iter() {
        let a = x.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += (x as f64) * (x as f64);
    }
    // Silence / warm-up: nothing to normalize.
    if peak < ASR_SILENCE_PEAK {
        return 1.0;
    }
    let rms = (sum_sq / audio.len() as f64).sqrt() as f32;
    if rms <= 0.0 {
        return 1.0;
    }
    // Move toward the target RMS, but never clip and never exceed the cap.
    let gain = (ASR_TARGET_RMS / rms)
        .min(ASR_PEAK_CEILING / peak)
        .min(ASR_MAX_GAIN);
    // Skip a no-op pass when the buffer is already essentially at level.
    if (gain - 1.0).abs() < 0.01 {
        return 1.0;
    }
    for x in audio.iter_mut() {
        *x *= gain;
    }
    gain
}

/// Default pre-roll lookback (ms) when always-on capture is enabled. ~400 ms is
/// enough to recover the syllable or two people start before the modifier
/// fully registers, without dragging in seconds of room tone. Measured against
/// `examples/preroll_lab`. 0 disables pre-roll (current press-to-open behaviour).
pub const DEFAULT_PREROLL_MS: u32 = 400;

/// Convert `preroll_ms` of 16 kHz audio to a sample count.
pub fn preroll_samples(preroll_ms: u32) -> usize {
    (SAMPLE_RATE as u64 * preroll_ms as u64 / 1000) as usize
}

/// A fixed-capacity rolling buffer of the most recent samples. Pushing past
/// capacity drops the *oldest* samples (unlike the SPSC ring, which drops the
/// newest when full) — exactly the semantics pre-roll needs: always retain the
/// last N ms of audio so the moment before the key press is still available.
///
/// Pure + allocation-stable (a `VecDeque` of fixed cap), so the rolling
/// behaviour is unit-tested without any audio device.
#[derive(Debug)]
pub struct PrerollRing {
    cap: usize,
    buf: VecDeque<f32>,
}

impl PrerollRing {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: VecDeque::with_capacity(cap + 1),
        }
    }

    /// Append samples, evicting the oldest so length never exceeds `cap`.
    pub fn push(&mut self, samples: &[f32]) {
        if self.cap == 0 {
            return;
        }
        // Only the last `cap` of the incoming run can ever survive.
        let tail = if samples.len() > self.cap {
            &samples[samples.len() - self.cap..]
        } else {
            samples
        };
        self.buf.extend(tail.iter().copied());
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
    }

    /// Copy the retained lookback into a contiguous buffer (oldest → newest).
    pub fn snapshot(&self) -> Vec<f32> {
        self.buf.iter().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Like [`push_resampled`] but returns the resampled mono 16 kHz chunk instead
/// of pushing it into a producer, so the always-on callback can route it to
/// both the pre-roll ring and (when recording) the consumer. Also feeds the
/// waveform level ring, matching the PTT path.
pub(super) fn resample_to_vec(
    interleaved: &[f32],
    channels: usize,
    stride: f64,
    pos: &mut f64,
    last: &mut f32,
    picker: &mut ChannelPicker,
) -> Vec<f32> {
    let frames = interleaved.len() / channels.max(1);
    if frames == 0 {
        return Vec::new();
    }
    let mix = picker.observe(interleaved);
    let mut mono = Vec::with_capacity(frames);
    if channels <= 1 {
        mono.extend_from_slice(interleaved);
    } else {
        for frame in interleaved.chunks_exact(channels) {
            mono.push(fold_frame(frame, mix));
        }
    }
    let sum_sq: f32 = mono.iter().map(|x| x * x).sum();
    let rms = (sum_sq / mono.len() as f32).sqrt();
    crate::ui_channel::push_level(rms);

    let mut out = Vec::with_capacity((mono.len() as f64 / stride).ceil() as usize + 1);
    while *pos < mono.len() as f64 {
        let idx = pos.floor() as usize;
        let frac = (*pos - idx as f64) as f32;
        let a = if idx == 0 { *last } else { mono[idx - 1] };
        let b = mono[idx];
        out.push(a + (b - a) * frac);
        *pos += stride;
    }
    *pos -= mono.len() as f64;
    *last = *mono.last().unwrap();
    out
}
