use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub const SAMPLE_RATE: u32 = 16_000;
/// 5 min of headroom (16 000 samples/s · 300). The ring buffer is only drained
/// after the key is released, so anything spoken past this cap is silently
/// dropped — keep it generously larger than the longest realistic dictation.
/// At f32 that's ~18 MB, which is fine.
pub const BUFFER_CAPACITY: usize = 16_000 * 300;

pub type HeapAudioConsumer = HeapCons<f32>;
pub type HeapAudioProducer = HeapProd<f32>;

/// Owns the cpal input Stream (held alive for the recording's duration) plus
/// the recording flag. Drop the engine to stop capture and release the mic.
///
/// Lifecycle is three-state so a warm-window can reuse the stream across
/// back-to-back dictations instead of paying cpal's stream-open cost each time:
///   * **active**  — `start_microphone`/`resume` have set `is_recording` true;
///     the callback pushes every resampled chunk into the producer.
///   * **parked**  — `park` set `is_recording` false but left `stream` alive; the
///     callback now *drops* chunks (no producer push) but the mic stays open, so
///     the next `resume` costs nothing. The mic indicator stays lit.
///   * **dropped** — `stop_capture`/`Drop` released the stream entirely.
pub struct AudioCaptureEngine {
    is_recording: Arc<AtomicBool>,
    producer: Option<HeapAudioProducer>,
    stream: Option<Stream>,
    // Native rate of the chosen input device, captured when the stream starts.
    // Useful for diagnostics / future smarter resampling.
    pub native_sample_rate: u32,
    pub native_channels: u16,
    // Name of the input device this stream was opened on (empty until `start`).
    // Needed so a warm-window reuse can detect the default input changing since
    // the stream was built, and rebuild instead of silently reusing a stale one.
    device_name: String,
    // Set by cpal's error callback. A parked stream that has failed must be
    // rebuilt on the next press, not resumed into silence.
    stream_failed: Arc<AtomicBool>,
    // Wall clock at stream open, paired with the monotonic clock at the same
    // instant. `Instant` pauses while the Mac sleeps and `SystemTime` does
    // not, so the gap between the two is how much of this stream's life was
    // spent asleep. See `slept_for`.
    opened_at: std::time::SystemTime,
    opened_at_mono: std::time::Instant,
}

// cpal::Stream is not Send (CoreAudio invariant); we keep AudioCaptureEngine
// on a single thread. Mark explicitly so the type system reflects this if we
// ever try to move it.
unsafe impl Sync for AudioCaptureEngine {}

impl AudioCaptureEngine {
    pub fn new(capacity: usize) -> (Self, HeapAudioConsumer) {
        let rb = HeapRb::<f32>::new(capacity);
        let (producer, consumer) = rb.split();
        (
            Self {
                is_recording: Arc::new(AtomicBool::new(false)),
                producer: Some(producer),
                stream: None,
                native_sample_rate: 0,
                native_channels: 0,
                device_name: String::new(),
                stream_failed: Arc::new(AtomicBool::new(false)),
                opened_at: std::time::SystemTime::UNIX_EPOCH,
                opened_at_mono: std::time::Instant::now(),
            },
            consumer,
        )
    }

    /// Mock mode: synthetic sine pumped from a background thread (kept for tests).
    pub fn start_mock_loop(&mut self) -> Arc<AtomicBool> {
        self.is_recording.store(true, Ordering::SeqCst);
        let is_recording = self.is_recording.clone();
        let mut producer = self
            .producer
            .take()
            .expect("capture already started on this engine");

        let thread_flag = is_recording.clone();
        std::thread::spawn(move || {
            let mut sample_count: u64 = 0;
            while thread_flag.load(Ordering::SeqCst) {
                for _ in 0..160 {
                    let sample = (sample_count as f32 * 0.05).sin() * 0.1;
                    let _ = producer.try_push(sample);
                    sample_count += 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        is_recording
    }

    /// Real microphone capture. Opens the default input device, reads native
    /// samples on cpal's high-priority audio callback thread, downmixes to
    /// mono and resamples to `SAMPLE_RATE` (16 kHz) via simple linear
    /// interpolation, and pushes f32 samples into the SPSC ring buffer.
    ///
    /// The returned `Arc<AtomicBool>` is the recording flag; flip it to false
    /// (or call `stop_capture`) to halt and release the mic.
    pub fn start_microphone(&mut self) -> eyre::Result<Arc<AtomicBool>> {
        self.is_recording.store(true, Ordering::SeqCst);

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| eyre::eyre!("no default input device available"))?;
        let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());

        let supported = device
            .default_input_config()
            .map_err(|e| eyre::eyre!("default_input_config failed: {e}"))?;
        let native_rate = supported.sample_rate().0;
        let native_channels = supported.channels();
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        self.native_sample_rate = native_rate;
        self.native_channels = native_channels;
        self.device_name = device_name.clone();
        self.stream_failed.store(false, Ordering::SeqCst);
        self.opened_at = std::time::SystemTime::now();
        self.opened_at_mono = std::time::Instant::now();
        crate::health::clear(crate::health::Fault::AudioLost);
        eprintln!(
            "[audio] capturing on `{device_name}` @ {native_rate} Hz × {native_channels} ch ({:?}); resampling to {SAMPLE_RATE} Hz mono",
            sample_format
        );

        let mut producer = self
            .producer
            .take()
            .expect("capture already started on this engine");
        let stride = native_rate as f64 / SAMPLE_RATE as f64;
        let channels = native_channels as usize;
        // Carry: fractional sample position across callback boundaries.
        let mut pos: f64 = 0.0;
        let mut last: f32 = 0.0;
        // Grip the recording flag so the callback can gate pushes: while parked
        // (warm-window idle) it must drop chunks, not fill the ring with silence.
        let recording = self.is_recording.clone();
        let mut picker = ChannelPicker::new(channels, native_rate);
        // A cpal error used to be a log line and nothing else, so an unplugged
        // mic, a default-device change or a sleep/wake left a dead stream that
        // the warm-window path happily reused: the user got silence and a
        // "no speech" line. The flag reaches the worker, which rebuilds.
        let failed = self.stream_failed.clone();
        let err_fn = move |e| {
            eprintln!("[audio] cpal stream error: {e}");
            failed.store(true, Ordering::SeqCst);
            crate::health::raise(crate::health::Fault::AudioLost, format!("{e}"));
        };

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    push_resampled(
                        data, channels, stride, &mut pos, &mut last, &mut producer, &recording,
                        &mut picker,
                    );
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32_768.0).collect();
                    push_resampled(&f, channels, stride, &mut pos, &mut last, &mut producer, &recording, &mut picker);
                },
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &config,
                move |data: &[u16], _| {
                    let f: Vec<f32> = data
                        .iter()
                        .map(|s| (*s as f32 - 32_768.0) / 32_768.0)
                        .collect();
                    push_resampled(&f, channels, stride, &mut pos, &mut last, &mut producer, &recording, &mut picker);
                },
                err_fn,
                None,
            ),
            other => return Err(eyre::eyre!("unsupported sample format: {other:?}")),
        }
        .map_err(|e| eyre::eyre!("build_input_stream failed: {e}"))?;

        stream
            .play()
            .map_err(|e| eyre::eyre!("stream.play failed: {e}"))?;
        self.stream = Some(stream);

        Ok(self.is_recording.clone())
    }

    pub fn stop_capture(&mut self) {
        self.is_recording.store(false, Ordering::SeqCst);
        // Dropping the stream pauses + releases the input device, ending the
        // CoreAudio callback loop.
        self.stream.take();
    }

    /// Park an idle engine: keep the stream (and thus the input device) open so a
    /// follow-up dictation within the warm window can [`resume`](Self::resume)
    /// without re-opening it, but drop incoming chunks so the ring doesn't fill
    /// with silence. The recording flag goes false; the mic indicator stays lit.
    pub fn park(&mut self) {
        self.is_recording.store(false, Ordering::SeqCst);
    }

    /// Reuse a parked engine: flip the recording flag back true so the callback
    /// pushes live audio into the (still-consumable) ring. Returns the flag. No
    /// stream re-open, so this costs ~0 ms versus `start_microphone`'s ~120 ms.
    pub fn resume(&mut self) -> Arc<AtomicBool> {
        self.is_recording.store(true, Ordering::SeqCst);
        self.is_recording.clone()
    }

    /// Name of the input device this stream is bound to (empty before `start`).
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// True if the system default input device has changed since this stream was
    /// opened. A warm-window reuse must rebuild rather than keep capturing the old
    /// device (e.g. AirPods connected mid-session). Conservative: if the current
    /// default can't be read, returns false rather than churning the stream.
    pub fn device_changed(&self) -> bool {
        match default_input_name() {
            Some(name) => name != self.device_name,
            None => false,
        }
    }

    /// Has cpal reported an error on this stream? The object stays alive after
    /// a device unplug or a sample-rate switch, but the stream is dead, so a
    /// warm-window resume would capture nothing.
    pub fn stream_failed(&self) -> bool {
        self.stream_failed.load(Ordering::SeqCst)
    }

    /// How long this stream has existed by the wall clock, and by the
    /// monotonic clock. The difference is sleep — see [`slept_for`].
    pub fn age(&self) -> (std::time::Duration, std::time::Duration) {
        (
            self.opened_at
                .elapsed()
                .unwrap_or(std::time::Duration::ZERO),
            self.opened_at_mono.elapsed(),
        )
    }

    /// Must the parked stream be thrown away rather than resumed?
    pub fn should_rebuild(&self) -> Option<&'static str> {
        if self.stream_failed() {
            return Some("the stream reported an error");
        }
        if self.device_changed() {
            return Some("the default input changed");
        }
        let (wall, mono) = self.age();
        if slept_for(wall, mono) > SLEEP_SLACK {
            return Some("the Mac slept while the stream was parked");
        }
        None
    }
}

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
    probe_frames: usize,
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

/// Downmix interleaved native samples to mono, then linearly resample at
/// `stride = native_rate / 16000` and push to the SPSC ring buffer. `pos` and
/// `last` carry fractional resample state across cpal callback invocations.
fn push_resampled(
    interleaved: &[f32],
    channels: usize,
    stride: f64,
    pos: &mut f64,
    last: &mut f32,
    producer: &mut HeapAudioProducer,
    recording: &AtomicBool,
    picker: &mut ChannelPicker,
) {
    let frames = interleaved.len() / channels.max(1);
    if frames == 0 {
        return;
    }

    let mix = picker.observe(interleaved);
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
    // Always advance `pos`/`last` so a warm-window [AudioCaptureEngine::resume]
    // continues from a consistent position, but only push to the producer while
    // recording — a parked engine keeps the mic open but must not fill the ring
    // with room tone.
    while *pos < mono.len() as f64 {
        let idx = pos.floor() as usize;
        let frac = (*pos - idx as f64) as f32;
        let a = if idx == 0 { *last } else { mono[idx - 1] };
        let b = mono[idx];
        let sample = a + (b - a) * frac;
        if recording.load(Ordering::SeqCst) {
            let _ = producer.try_push(sample);
        }
        *pos += stride;
    }
    // Subtract the consumed frames so `pos` stays a small fractional offset
    // into the next chunk.
    *pos -= mono.len() as f64;
    *last = *mono.last().unwrap();
}

#[cfg(test)]
mod tests {
    use super::{fold_frame, ChannelPicker, Downmix};

    /// Regression: the parked-stream check compared the stream's WALL-CLOCK
    /// age against the warm window, so a stream that had simply been open
    /// longer than the window — a two-minute dictation, say — was rebuilt and
    /// the log claimed the Mac had slept. Sleep is the wall clock running on
    /// while the monotonic clock does not, and nothing else.
    #[test]
    fn only_a_real_sleep_reads_as_a_sleep() {
        use super::{slept_for, SLEEP_SLACK};
        use std::time::Duration;

        // A long-lived stream, both clocks in step: no sleep, however old.
        for age in [1u64, 60, 3_600, 86_400] {
            let d = Duration::from_secs(age);
            assert_eq!(slept_for(d, d), Duration::ZERO, "{age}s awake");
            assert!(slept_for(d, d) <= SLEEP_SLACK);
        }

        // A lid closed overnight: eight hours of wall clock over two minutes
        // of uptime.
        let slept = slept_for(Duration::from_secs(8 * 3_600), Duration::from_secs(120));
        assert!(slept > SLEEP_SLACK, "{slept:?}");

        // Small skew (an NTP correction, scheduler jitter) is not a sleep.
        assert!(
            slept_for(Duration::from_secs(65), Duration::from_secs(60)) <= SLEEP_SLACK,
            "a few seconds of skew must not throw the stream away"
        );
        // And a monotonic clock ahead of the wall clock saturates at zero
        // rather than underflowing.
        assert_eq!(
            slept_for(Duration::from_secs(1), Duration::from_secs(9)),
            Duration::ZERO
        );
    }

    #[test]
    fn a_one_and_two_channel_microphone_keeps_the_mean() {
        // Both capsules of a stereo mic carry the same voice, so averaging is
        // right and must not change.
        for channels in [1usize, 2] {
            let mut picker = ChannelPicker::new(channels, 48_000);
            for _ in 0..100 {
                assert_eq!(picker.observe(&vec![0.5; channels * 64]), Downmix::Mean);
            }
        }
        assert_eq!(fold_frame(&[1.0, 0.0], Downmix::Mean), 0.5);
        assert_eq!(fold_frame(&[], Downmix::Mean), 0.0);
    }

    #[test]
    fn an_interface_listens_to_the_loudest_input_instead_of_averaging_it_away() {
        // Regression: a mic on input 3 of an 8-in interface was averaged with
        // seven silent channels, an 18 dB loss, and every utterance came back
        // as "no speech above noise floor".
        let channels = 8;
        let rate = 48_000;
        let mut picker = ChannelPicker::new(channels, rate);
        let frames = rate as usize * super::PROBE_MS / 1000;
        let mut buffer = vec![0.0f32; channels * frames];
        for frame in buffer.chunks_exact_mut(channels) {
            frame[3] = 0.4;
        }
        // While it listens, the mean: it carries the voice, quietly, rather
        // than betting on a channel it has not heard yet.
        assert_eq!(picker.observe(&buffer[..channels * 8]), Downmix::Mean);
        assert_eq!(picker.observe(&buffer), Downmix::Channel(3));
        // And the choice sticks: it must not move channel mid-word.
        let mut louder = vec![0.0f32; channels * frames];
        for frame in louder.chunks_exact_mut(channels) {
            frame[6] = 0.9;
        }
        assert_eq!(picker.observe(&louder), Downmix::Channel(3));

        // The picked channel arrives at full amplitude.
        let frame = [0.0, 0.0, 0.0, 0.4, 0.0, 0.0, 0.0, 0.0];
        assert_eq!(fold_frame(&frame, Downmix::Channel(3)), 0.4);
        // The old behaviour, for the record: an eighth of the level.
        assert!((fold_frame(&frame, Downmix::Mean) - 0.05).abs() < 1e-6);
        // A channel index past the end is silence, never a panic.
        assert_eq!(fold_frame(&frame, Downmix::Channel(99)), 0.0);
    }

    /// Regression: the picker used to commit to the loudest channel of the
    /// FIRST 200 ms whatever was in it. A stream opens before anyone speaks
    /// (the always-on stream opens minutes before), so that window is silence,
    /// the tie-break picked channel 0, and the choice stuck for the life of
    /// the stream — on an interface with the microphone on input 3 that is a
    /// silent recording, strictly worse than the averaging it replaced.
    #[test]
    fn a_silent_probe_window_decides_nothing_and_the_mean_carries_on() {
        use super::{pick_channel, PICK_RMS_FLOOR};
        let channels = 8;
        let rate = 16_000u32;
        let frames = rate as usize * super::PROBE_MS / 1000;
        let mut picker = ChannelPicker::new(channels, rate);

        // Several windows of room tone: no decision, and never channel 0.
        let quiet = vec![0.000_1f32; channels * frames];
        for _ in 0..5 {
            assert_eq!(picker.observe(&quiet), Downmix::Mean);
        }

        // Then the user speaks, on input 3. Now it decides, and sticks.
        let mut speech = vec![0.0f32; channels * frames];
        for frame in speech.chunks_exact_mut(channels) {
            frame[3] = 0.4;
        }
        assert_eq!(picker.observe(&speech), Downmix::Channel(3));
        assert_eq!(picker.observe(&quiet), Downmix::Channel(3));

        // The floor itself: exactly at it decides, just under it does not.
        let at_floor = vec![PICK_RMS_FLOOR * PICK_RMS_FLOOR; 2];
        assert_eq!(pick_channel(&at_floor, 1), Some(0));
        let under = vec![0.0, (PICK_RMS_FLOOR * 0.9) * (PICK_RMS_FLOOR * 0.9)];
        assert_eq!(pick_channel(&under, 1), None);
        // And the degenerate inputs are answers, not panics.
        assert_eq!(pick_channel(&[], 4), None);
        assert_eq!(pick_channel(&[1.0], 0), None);
    }

    #[test]
    fn cancelled_capture_drops_later_microphone_samples_while_worker_is_busy() {
        use super::*;
        let (mut producer, mut consumer) = HeapRb::<f32>::new(128).split();
        let recording = AtomicBool::new(true);
        let mut pos = 0.0;
        let mut last = 0.0;
        let mut picker = ChannelPicker::new(1, 16_000);
        push_resampled(&[0.2; 8], 1, 1.0, &mut pos, &mut last, &mut producer, &recording, &mut picker);
        let before = consumer.occupied_len();
        recording.store(false, Ordering::SeqCst); // accepted out-of-band Cancel
        push_resampled(&[0.9; 32], 1, 1.0, &mut pos, &mut last, &mut producer, &recording, &mut picker);
        assert_eq!(consumer.occupied_len(), before, "waiting for ASR must not capture post-cancel speech");
        let samples = drain_session(&mut consumer, &recording);
        assert_eq!(samples.len(), before);
        assert!(samples.iter().all(|sample| *sample <= 0.2));
    }

    use super::*;

    #[test]
    fn preroll_samples_from_ms() {
        assert_eq!(preroll_samples(0), 0);
        assert_eq!(preroll_samples(1000), 16_000);
        assert_eq!(preroll_samples(400), 6_400);
    }

    #[test]
    fn preroll_ring_retains_only_the_last_cap_samples() {
        let mut r = PrerollRing::new(4);
        r.push(&[1.0, 2.0, 3.0]);
        assert_eq!(r.snapshot(), vec![1.0, 2.0, 3.0]);
        // Pushing past cap evicts the oldest, keeping the newest `cap`.
        r.push(&[4.0, 5.0]);
        assert_eq!(r.snapshot(), vec![2.0, 3.0, 4.0, 5.0]);
        assert_eq!(r.len(), 4);
    }

    #[test]
    fn preroll_ring_handles_oversized_push() {
        let mut r = PrerollRing::new(3);
        // A single push larger than cap keeps only its last `cap` samples.
        r.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(r.snapshot(), vec![4.0, 5.0, 6.0]);
    }

    #[test]
    fn preroll_ring_zero_cap_is_a_noop() {
        let mut r = PrerollRing::new(0);
        r.push(&[1.0, 2.0, 3.0]);
        assert!(r.is_empty());
        assert_eq!(r.snapshot(), Vec::<f32>::new());
    }

    #[test]
    fn bluetooth_device_detection() {
        assert!(is_bluetooth_device("Sam's AirPods Pro"));
        assert!(is_bluetooth_device("Beats Studio Buds"));
        assert!(is_bluetooth_device("Sony Bluetooth Headphones"));
        // Built-in / wired inputs must NOT trip the warning.
        assert!(!is_bluetooth_device("MacBook Pro Microphone"));
        assert!(!is_bluetooth_device("External Microphone"));
        assert!(!is_bluetooth_device("USB Audio Device"));
    }

    #[test]
    fn normalize_boosts_quiet_speech_toward_target() {
        // A "whisper": low-amplitude tone well under the target RMS.
        let mut buf: Vec<f32> = (0..16_000)
            .map(|n| if n % 2 == 0 { 0.01 } else { -0.01 })
            .collect();
        let gain = normalize_for_asr(&mut buf);
        assert!(gain > 5.0, "quiet input should be boosted hard, got {gain}");
        let rms = (buf.iter().map(|x| x * x).sum::<f32>() / buf.len() as f32).sqrt();
        // Boosted close to the target (capped by ASR_MAX_GAIN here: 0.01*16 = 0.16).
        assert!(rms > 0.10, "boosted RMS too low: {rms}");
        assert!(
            buf.iter().all(|x| x.abs() <= ASR_PEAK_CEILING),
            "must not clip"
        );
    }

    #[test]
    fn normalize_never_clips_loud_speech() {
        // A loud, near-full-scale signal must come DOWN, not blow past ceiling.
        let mut buf: Vec<f32> = (0..16_000)
            .map(|n| if n % 2 == 0 { 0.8 } else { -0.8 })
            .collect();
        let gain = normalize_for_asr(&mut buf);
        assert!(gain < 1.0, "loud input should be attenuated, got {gain}");
        assert!(
            buf.iter().all(|x| x.abs() <= ASR_PEAK_CEILING),
            "must not clip"
        );
    }

    #[test]
    fn normalize_leaves_silence_untouched() {
        let mut buf = vec![0.0f32; 16_000];
        assert_eq!(normalize_for_asr(&mut buf), 1.0);
        assert!(buf.iter().all(|&x| x == 0.0), "silence must stay silent");
        // A tiny click below the silence floor is also left alone.
        let mut nearly = vec![0.0f32; 16_000];
        nearly[0] = 1e-5;
        assert_eq!(normalize_for_asr(&mut nearly), 1.0);
    }

    #[test]
    fn resample_passthrough_at_matching_rate() {
        // stride 1.0 (16k→16k), mono: output mirrors input (first sample uses
        // the `last` carry of 0.0).
        let mut pos = 0.0;
        let mut last = 0.0;
        let out = resample_to_vec(
            &[0.5, 0.5, 0.5, 0.5],
            1,
            1.0,
            &mut pos,
            &mut last,
            &mut ChannelPicker::new(1, 16_000),
        );
        assert_eq!(out.len(), 4);
        assert_eq!(*out.last().unwrap(), 0.5);
    }

    #[test]
    fn frame_rms_chunks_into_frames() {
        let data = vec![0.5f32; 4];
        assert_eq!(frame_rms(&data, 2), vec![0.5, 0.5]);
        assert_eq!(frame_rms(&data, 4), vec![0.5]);
        // A non-divisible tail is its own (shorter) frame.
        assert_eq!(frame_rms(&[0.0, 0.0, 0.0, 0.0, 0.0], 4).len(), 2);
        // Zero/empty input.
        assert!(frame_rms(&[], 4).is_empty());
        assert!(frame_rms(&data, 0).is_empty());
    }

    #[test]
    fn noise_floor_tracks_low_percentile() {
        // Mostly quiet room tone with a burst of speech near the top.
        let frames = vec![0.004, 0.005, 0.006, 0.004, 0.005, 0.2, 0.3, 0.15, 0.006, 0.005];
        let floor = noise_floor(&frames, 0.10);
        assert!(floor < 0.01, "low percentile must stay near room tone, got {floor}");
        assert!(floor > 0.0);
        // Empty input → 0.
        assert_eq!(noise_floor(&[], 0.10), 0.0);
        // Higher percentile sits above the floor.
        assert!(noise_floor(&frames, 0.5) > floor);
    }

    #[test]
    fn speech_threshold_scales_floor_and_has_absolute_floor() {
        // Loud room: floor*3.
        assert_eq!(speech_threshold(0.10), 0.30);
        // Hushed room: absolute floor kicks in so a whisper still reads as speech.
        assert_eq!(speech_threshold(0.0), TAIL_ABS_FLOOR);
        assert_eq!(speech_threshold(0.0001), TAIL_ABS_FLOOR);
        // A quiet-but-not-dead floor that triples under the absolute floor.
        assert_eq!(speech_threshold(0.002), 0.01); // 0.002*3=0.006 < 0.01
    }

    #[test]
    fn whisper_threshold_is_strictly_lower_than_normal() {
        for floor in [0.0_f32, 0.001, 0.005, 0.02, 0.05, 0.1] {
            let normal = speech_threshold_with_mode(floor, false);
            let whisper = speech_threshold_with_mode(floor, true);
            assert_eq!(normal, speech_threshold(floor), "normal path must match legacy fn");
            assert!(whisper < normal, "floor {floor}: whisper {whisper} !< normal {normal}");
            assert!(whisper > 0.002, "whisper floor {whisper} must stay above a tiny positive floor");
        }
        // Hushed room: whisper's lower absolute floor kicks in.
        assert_eq!(speech_threshold_with_mode(0.0, true), WHISPER_ABS_FLOOR);
        assert!(WHISPER_ABS_FLOOR < TAIL_ABS_FLOOR);
    }

    #[test]
    fn frames_quiet_checks_only_the_trailing_window() {
        let frames = vec![0.5, 0.5, 0.002, 0.003, 0.002];
        let thresh = 0.01;
        // Whole tail is quiet.
        assert!(frames_quiet(&frames, thresh, 3));
        // Including the loud prefix would fail.
        assert!(!frames_quiet(&frames, thresh, 5));
        // More frames than available: only the available (all-quiet) ones matter.
        assert!(frames_quiet(&[0.002, 0.003], thresh, 10));
        // Not quiet.
        let loud = vec![0.5, 0.5, 0.5];
        assert!(!frames_quiet(&loud, thresh, 3));
        // n == 0 is trivially quiet.
        assert!(frames_quiet(&frames, thresh, 0));
    }

    #[test]
    fn quiet_frame_count_rounds_up_to_cover_the_window() {
        assert_eq!(quiet_frame_count(250, 30), 9);  // 250/30 = 8.33 → 9
        assert_eq!(quiet_frame_count(300, 30), 10); // 300/30 = 10
        assert_eq!(quiet_frame_count(0, 30), 0);
        assert_eq!(quiet_frame_count(250, 0), 0);
    }

    #[test]
    fn quiet_run_requires_continuous_silence_for_the_quiet_duration() {
        let thresh = 0.01;
        // dt=40ms, quiet_ms=250 → need 7 consecutive quiet samples (240 < 250,
        // so 7×40=280 ≥ 250).
        let seven_quiet = vec![0.5, 0.5, 0.002, 0.002, 0.002, 0.002, 0.002, 0.002, 0.002, 0.002];
        assert!(quiet_run(&seven_quiet, 40, thresh, 250));
        // Only 6 quiet trailing (6×40=240 < 250) → not yet.
        let six_quiet = vec![0.002, 0.002, 0.002, 0.002, 0.002, 0.002, 0.5, 0.5];
        assert!(!quiet_run(&six_quiet, 40, thresh, 250));
        // A loud sample within the last 7 breaks the run (only 4 quiet trail).
        let interrupted = vec![0.002, 0.002, 0.002, 0.9, 0.002, 0.002, 0.002, 0.002];
        assert!(!quiet_run(&interrupted, 40, thresh, 250));
        // Empty series is never quiet.
        assert!(!quiet_run(&[], 40, thresh, 250));
        // quiet_ms of 0 is trivially satisfied.
        assert!(quiet_run(&[0.5], 40, thresh, 0));
    }

    #[test]
    fn noise_floor_adaptive_threshold_engages_on_speech_tail() {
        // A recording that ends mid-speech: the low-percentile floor stays low
        // (most of the clip is room tone), so the threshold is small and the
        // last (loud) frames are decidedly NOT quiet → grace should engage.
        let rec: Vec<f32> = vec![0.004, 0.005, 0.004, 0.2, 0.3, 0.25];
        let floor = noise_floor(&rec, 0.10);
        let thresh = speech_threshold(floor);
        assert!(!frames_quiet(&rec, thresh, quiet_frame_count(300, 30)));
        // A release during a proper (>300 ms) pause: the trailing 10 frames are
        // all quiet below the (adaptive, low) threshold → skip grace.
        let mut rec2 = vec![0.004, 0.005, 0.2];
        rec2.extend(std::iter::repeat(0.0).take(10));
        let thresh2 = speech_threshold(noise_floor(&rec2, 0.10));
        assert!(frames_quiet(&rec2, thresh2, quiet_frame_count(300, 30)));
    }

    #[test]
    fn trim_trailing_silence_cuts_quiet_tail_keeps_hangover() {
        // 30 ms frames at 16 kHz: frame_len = 480. 10 loud frames + 20 quiet.
        let frame_len = 480;
        let frame_ms = 30;
        let mut samples = Vec::new();
        samples.extend(std::iter::repeat(0.3_f32).take(10 * frame_len)); // 300 ms speech
        samples.extend(std::iter::repeat(0.001_f32).take(20 * frame_len)); // 600 ms tail
        let threshold = speech_threshold(0.001); // max(0.003, 0.01) = 0.01
        let keep = trim_trailing_silence(&samples, frame_len, threshold, frame_ms);
        // Expect: 10 speech frames + ceil(120/30)=4 hangover frames = 14 frames.
        assert_eq!(keep, 14 * frame_len);
    }

    #[test]
    fn trim_trailing_silence_all_quiet_returns_zero() {
        let samples = vec![0.001_f32; 480 * 10];
        let threshold = speech_threshold(0.001);
        assert_eq!(trim_trailing_silence(&samples, 480, threshold, 30), 0);
    }

    #[test]
    fn transcribe_keep_len_skips_near_silence() {
        // A 0.5 s misfire tap: well under MIN_SPEECH_MS of speech → skip.
        let misfire = vec![0.002_f32; 480 * 17];
        let threshold = speech_threshold(0.002);
        assert_eq!(transcribe_keep_len(&misfire, 480, threshold, 30), None);
        // 300 ms of speech + quiet tail → keep the speech + hangover.
        let mut real = Vec::new();
        real.extend(std::iter::repeat(0.3_f32).take(10 * 480));
        real.extend(std::iter::repeat(0.001_f32).take(20 * 480));
        let keep = transcribe_keep_len(&real, 480, speech_threshold(0.001), 30).unwrap();
        assert_eq!(keep, 14 * 480);
    }

    #[test]
    fn noise_floor_estimator_tracks_recent_low_percentile_incrementally() {
        // Feed room tone first, then speech; the estimator (fed incrementally)
        // must land on the same low percentile as a one-shot sort over the same
        // input, i.e. the incremental path is not just cheaper but equivalent.
        let mut est = NoiseFloorEstimator::new(TAIL_FLOOR_RING_FRAMES);
        let room_tone = vec![0.004_f32, 0.005, 0.006, 0.004, 0.005];
        let speech = vec![0.2_f32, 0.3, 0.15];
        let mut all = Vec::new();
        for r in room_tone.iter().chain(speech.iter()) {
            est.update(*r);
            all.push(*r);
        }
        let expect = noise_floor(&all, 0.10);
        assert!((est.floor(0.10) - expect).abs() < 1e-4);
        // Empty estimator → 0.
        assert_eq!(NoiseFloorEstimator::new(8).floor(0.10), 0.0);
    }

    #[test]
    fn noise_floor_estimator_windows_to_capacity() {
        // Once full, the oldest frames fall off so the floor reflects the recent
        // window, not the whole history — this is what bounds the estimate.
        let mut est = NoiseFloorEstimator::new(4);
        for r in [0.001_f32, 0.002, 0.003, 0.004] {
            est.update(r);
        }
        assert_eq!(est.len(), 4);
        // Push two more; the two oldest (0.001, 0.002) are dropped.
        est.update(0.005);
        est.update(0.006);
        assert_eq!(est.len(), 4);
        assert_eq!(est.floor(1.0), 0.006); // newest stays
        // clear() empties the window.
        est.clear();
        assert!(est.is_empty());
        assert_eq!(est.floor(0.10), 0.0);
    }

    #[test]
    fn tail_capture_engage_only_scans_the_trailing_window() {
        // A release mid-speech: the bounded trailing window ends loud, so the
        // adaptive (low) threshold says NOT quiet → grace engages.
        let tail_loud = vec![0.004_f32, 0.2, 0.3]; // <300 ms, all short
        let (_, engage) = tail_capture_engage(&tail_loud, 1, 0.005, 300, 30);
        assert!(engage, "loud tail should engage the grace");

        // A release in a proper (≥300 ms) pause: the trailing 10 frames are all
        // quiet below the adaptive threshold → skip grace for zero added latency.
        let mut tail_quiet = vec![0.004_f32, 0.2];
        tail_quiet.extend(std::iter::repeat(0.0).take(10));
        let (thresh, engage) =
            tail_capture_engage(&tail_quiet, 1, noise_floor(&[0.004, 0.005], 0.10), 300, 30);
        assert!(
            thresh >= TAIL_SOFT_ABS_FLOOR,
            "threshold keeps the (soft) absolute floor"
        );
        assert!(!engage, "900-ms quiet tail should skip the grace");

        // A SOFT trailing word: level between the soft threshold and the old ×3
        // threshold. Under the old math this read as "released in a pause" and
        // the word clipped; the soft tail threshold must engage the grace.
        let floor = 0.004_f32;
        let soft = tail_speech_threshold(floor, false) + 0.0005; // just above soft
        assert!(
            soft < speech_threshold(floor),
            "test level must sit under the plain speech threshold"
        );
        let tail_soft = vec![floor, floor, soft, soft];
        let (_, engage) = tail_capture_engage(&tail_soft, 1, floor, 300, 30);
        assert!(engage, "a soft trailing word must engage the grace");

        // Plain-speech threshold math is unchanged: floor*3, clamped to the
        // absolute floor. The tail path is more sensitive; whisper mode wins.
        assert_eq!(speech_threshold(0.10), 0.30);
        assert_eq!(speech_threshold(0.0), TAIL_ABS_FLOOR);
        assert_eq!(tail_speech_threshold(0.10, false), 0.20);
        assert_eq!(tail_speech_threshold(0.0, false), TAIL_SOFT_ABS_FLOOR);
        assert_eq!(
            tail_speech_threshold(0.002, true),
            speech_threshold_with_mode(0.002, true),
            "whisper mode keeps its own (lower) thresholds"
        );
    }

    #[test]
    fn recording_tail_only_returns_the_trailing_window() {
        use ringbuf::traits::{Producer, Split};
        // 1 ms at 16 kHz = 16 samples.
        let win = (1.0 / 1000.0 * SAMPLE_RATE as f64) as usize;
        assert_eq!(win, 16);

        // stream_audio holds the OLDER drained samples; the ring the NEWER ones.
        // ring_occupied=5 < 16 → from_stream = 11 → 11 newest stream_audio + 5 ring.
        let (mut prod, cons) = HeapRb::<f32>::new(64).split();
        let stream_audio = vec![1.0_f32; 40];
        for &v in &[10.0_f32, 20.0, 30.0, 40.0, 50.0] {
            let _ = prod.try_push(v);
        }
        let tail = recording_tail(&cons, &stream_audio, 1);
        assert_eq!(tail.len(), 16);
        assert_eq!(&tail[..11], &[1.0_f32; 11]);
        assert_eq!(&tail[11..], &[10.0, 20.0, 30.0, 40.0, 50.0]);

        // Ring alone covers the whole window → stream_audio contributes nothing.
        let (mut prod2, cons2) = HeapRb::<f32>::new(64).split();
        for &v in &[
            1.0_f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0,
            16.0,
        ] {
            let _ = prod2.try_push(v);
        }
        let tail2 = recording_tail(&cons2, &[99.0_f32; 100], 1);
        assert_eq!(tail2.len(), 16);
        assert_eq!(tail2, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0]);
    }
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
const ASR_SILENCE_PEAK: f32 = 1.0e-4;

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

/// Always-on microphone capture with a rolling pre-roll buffer.
///
/// Unlike [`AudioCaptureEngine`] (which opens the device on key-press and so
/// both pays CoreAudio's stream-start latency *and* misses the audio before the
/// callback spins up), this keeps the input stream warm for the daemon's whole
/// life. Every callback feeds a rolling [`PrerollRing`]; while idle, nothing
/// reaches the SPSC consumer. When [`begin_session`](Self::begin_session) flips
/// the recording flag, the next callback flushes the pre-roll lookback into the
/// consumer *first*, then streams live audio — so the transcript starts a few
/// hundred ms before the key press and the first words are never clipped.
///
/// One stream, one consumer, reused across every utterance for the daemon's
/// lifetime (the consumer stays empty between sessions because idle callbacks
/// don't push to it).
pub struct AlwaysOnCapture {
    stream: Option<Stream>,
    producer: Option<HeapAudioProducer>,
    recording: Arc<AtomicBool>,
    preroll_samples: usize,
    pub native_sample_rate: u32,
    pub native_channels: u16,
    // Name of the input device this stream was opened on. The warm stream stays
    // bound to it for life, so if the system default input later changes
    // (AirPods connect, a call app switches the device) we'd silently keep
    // capturing the old device — `device_changed` detects that so the daemon can
    // rebuild. Empty until `start`.
    device_name: String,
    // As on `AudioCaptureEngine`: cpal's error callback sets this so a dead
    // always-on stream is rebuilt instead of reused into silence.
    stream_failed: Arc<AtomicBool>,
    // Wall clock and monotonic clock at stream open, for the same sleep/wake
    // reason. See `slept_for`.
    opened_at: std::time::SystemTime,
    opened_at_mono: std::time::Instant,
}

unsafe impl Sync for AlwaysOnCapture {}

impl AlwaysOnCapture {
    /// Create the engine + its SPSC consumer. `preroll_samples` is the rolling
    /// lookback retained before each session; `capacity` is the recording ring
    /// (use [`BUFFER_CAPACITY`]). The stream is not opened until [`start`](Self::start).
    pub fn new(preroll_samples: usize, capacity: usize) -> (Self, HeapAudioConsumer) {
        let rb = HeapRb::<f32>::new(capacity);
        let (producer, consumer) = rb.split();
        (
            Self {
                stream: None,
                producer: Some(producer),
                recording: Arc::new(AtomicBool::new(false)),
                preroll_samples,
                native_sample_rate: 0,
                native_channels: 0,
                device_name: String::new(),
                stream_failed: Arc::new(AtomicBool::new(false)),
                opened_at: std::time::SystemTime::UNIX_EPOCH,
                opened_at_mono: std::time::Instant::now(),
            },
            consumer,
        )
    }

    pub fn recording_flag(&self) -> Arc<AtomicBool> {
        self.recording.clone()
    }

    /// Begin an utterance: subsequent callbacks push the pre-roll lookback then
    /// live audio into the consumer. Idempotent.
    pub fn begin_session(&self) {
        self.recording.store(true, Ordering::SeqCst);
    }

    /// End an utterance: callbacks stop pushing to the consumer (pre-roll keeps
    /// rolling). The worker drains whatever is queued, then transcribes.
    pub fn end_session(&self) {
        self.recording.store(false, Ordering::SeqCst);
    }

    /// Open the input device and start the always-on stream. The consumer
    /// returned by [`new`](Self::new) receives pre-roll + live audio only while
    /// a session is active.
    pub fn start(&mut self) -> eyre::Result<()> {
        let mut producer = self
            .producer
            .take()
            .ok_or_else(|| eyre::eyre!("always-on capture already started"))?;
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| eyre::eyre!("no default input device available"))?;
        let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
        let supported = device
            .default_input_config()
            .map_err(|e| eyre::eyre!("default_input_config failed: {e}"))?;
        let native_rate = supported.sample_rate().0;
        let native_channels = supported.channels();
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        self.native_sample_rate = native_rate;
        self.native_channels = native_channels;
        self.device_name = device_name.clone();
        self.stream_failed.store(false, Ordering::SeqCst);
        self.opened_at = std::time::SystemTime::now();
        self.opened_at_mono = std::time::Instant::now();
        crate::health::clear(crate::health::Fault::AudioLost);
        eprintln!(
            "[audio] always-on on `{device_name}` @ {native_rate} Hz × {native_channels} ch ({:?}); \
             pre-roll {} ms",
            sample_format,
            self.preroll_samples * 1000 / SAMPLE_RATE as usize
        );
        // Holding an input stream open on a Bluetooth headset forces macOS into
        // the low-quality HFP "call" profile for as long as the stream lives —
        // and with always-on that's the daemon's whole lifetime, so the device's
        // *output* (music, video) is degraded even when you're not dictating.
        // Warn so the user can turn pre-roll off for Bluetooth headsets.
        if is_bluetooth_device(&device_name) {
            eprintln!(
                "[warn] always-on mic is on a Bluetooth device (`{device_name}`) — macOS keeps it \
                 in low-quality call mode while the stream is open, degrading its audio output. \
                 Consider disabling pre-roll (preroll_ms = 0) for Bluetooth headsets."
            );
        }

        let stride = native_rate as f64 / SAMPLE_RATE as f64;
        let channels = native_channels as usize;
        let mut pos: f64 = 0.0;
        let mut last: f32 = 0.0;
        let mut preroll = PrerollRing::new(self.preroll_samples);
        let mut prev_recording = false;
        let mut picker = ChannelPicker::new(channels, native_rate);
        let recording = self.recording.clone();
        let failed = self.stream_failed.clone();
        let err_fn = move |e| {
            eprintln!("[audio] cpal stream error: {e}");
            failed.store(true, Ordering::SeqCst);
            crate::health::raise(crate::health::Fault::AudioLost, format!("{e}"));
        };

        // One callback body, generic over the sample type, shared by all formats.
        macro_rules! make_cb {
            ($t:ty, $to_f32:expr) => {
                move |data: &[$t], _: &_| {
                    let f: Vec<f32> = data.iter().map($to_f32).collect();
                    let chunk = resample_to_vec(&f, channels, stride, &mut pos, &mut last, &mut picker);
                    if chunk.is_empty() {
                        return;
                    }
                    let rec = recording.load(Ordering::SeqCst);
                    // Rising edge: flush the lookback captured *before* the press.
                    if rec && !prev_recording {
                        for s in preroll.snapshot() {
                            if !recording.load(Ordering::SeqCst) { break; }
                            let _ = producer.try_push(s);
                        }
                    }
                    preroll.push(&chunk);
                    if rec {
                        for &s in &chunk {
                            if !recording.load(Ordering::SeqCst) { break; }
                            let _ = producer.try_push(s);
                        }
                    }
                    prev_recording = recording.load(Ordering::SeqCst);
                }
            };
        }

        let stream = match sample_format {
            SampleFormat::F32 => {
                device.build_input_stream(&config, make_cb!(f32, |s: &f32| *s), err_fn, None)
            }
            SampleFormat::I16 => device.build_input_stream(
                &config,
                make_cb!(i16, |s: &i16| *s as f32 / 32_768.0),
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &config,
                make_cb!(u16, |s: &u16| (*s as f32 - 32_768.0) / 32_768.0),
                err_fn,
                None,
            ),
            other => return Err(eyre::eyre!("unsupported sample format: {other:?}")),
        }
        .map_err(|e| eyre::eyre!("build_input_stream failed: {e}"))?;

        stream
            .play()
            .map_err(|e| eyre::eyre!("stream.play failed: {e}"))?;
        self.stream = Some(stream);
        Ok(())
    }

    /// Name of the input device this stream is bound to (empty before `start`).
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// True if the system default input device has changed since this stream was
    /// opened. The warm stream stays bound to the *original* device for life, so
    /// after the user switches inputs (connects AirPods, joins a call that grabs
    /// the mic) it would otherwise keep capturing the wrong device. The daemon
    /// calls this on key-press and rebuilds the stream when it returns true.
    /// Conservative: if the current default can't be read, returns false (don't
    /// churn the stream on a transient query failure).
    pub fn device_changed(&self) -> bool {
        match default_input_name() {
            Some(name) => name != self.device_name,
            None => false,
        }
    }

    /// Has cpal reported an error on this stream? The object stays alive after
    /// a device unplug or a sample-rate switch, but the stream is dead, so a
    /// warm-window resume would capture nothing.
    pub fn stream_failed(&self) -> bool {
        self.stream_failed.load(Ordering::SeqCst)
    }

    /// How long this stream has existed by the wall clock, and by the
    /// monotonic clock. The difference is sleep — see [`slept_for`].
    pub fn age(&self) -> (std::time::Duration, std::time::Duration) {
        (
            self.opened_at
                .elapsed()
                .unwrap_or(std::time::Duration::ZERO),
            self.opened_at_mono.elapsed(),
        )
    }

    /// Must the parked stream be thrown away rather than resumed?
    pub fn should_rebuild(&self) -> Option<&'static str> {
        if self.stream_failed() {
            return Some("the stream reported an error");
        }
        if self.device_changed() {
            return Some("the default input changed");
        }
        let (wall, mono) = self.age();
        if slept_for(wall, mono) > SLEEP_SLACK {
            return Some("the Mac slept while the stream was parked");
        }
        None
    }
}

/// Name of the current system default input device, or `None` if there isn't
/// one (or its name can't be read). Cheap host query; used to detect a
/// default-input change against a warm always-on stream.
pub fn default_input_name() -> Option<String> {
    cpal::default_host()
        .default_input_device()
        .and_then(|d| d.name().ok())
}

/// Heuristic: does this input-device name look like a Bluetooth headset? Used
/// only to warn that always-on capture pins it into low-quality call mode.
/// Pure + name-based so it's unit-testable without an audio device.
fn is_bluetooth_device(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("airpods")
        || n.contains("bluetooth")
        || n.contains("beats")
        || n.contains("buds")
        || n.contains("headphones")
}

/// Like [`push_resampled`] but returns the resampled mono 16 kHz chunk instead
/// of pushing it into a producer, so the always-on callback can route it to
/// both the pre-roll ring and (when recording) the consumer. Also feeds the
/// waveform level ring, matching the PTT path.
fn resample_to_vec(
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

/// Drain a *borrowed* SPSC consumer until `is_recording` is false and the queue
/// is empty, returning the accumulated PCM. The borrowed form (vs
/// [`drain_until_stopped`], which consumes the consumer) lets the always-on
/// engine reuse one consumer across every session. Synchronous — the daemon
/// worker thread owns the loop and can block here.
pub fn drain_session(consumer: &mut HeapAudioConsumer, is_recording: &AtomicBool) -> Vec<f32> {
    let mut audio_buffer: Vec<f32> = Vec::with_capacity(SAMPLE_RATE as usize * 5);
    while is_recording.load(Ordering::SeqCst) || !consumer.is_empty() {
        let pending = consumer.occupied_len();
        if pending > 0 {
            let mut chunk = vec![0.0_f32; pending];
            let got = consumer.pop_slice(&mut chunk);
            audio_buffer.extend_from_slice(&chunk[..got]);
        } else {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }
    audio_buffer
}

/// Drain the SPSC ring buffer into a single contiguous PCM buffer, returning
/// once `is_recording` is false **and** the buffer has been fully consumed.
///
/// This is the system's "when is an utterance done" rule: capture has stopped
/// *and* every queued sample has been pulled. It lives here, decoupled from
/// the transcription model, so the termination invariant has exactly one home
/// — the transcriber composes it with `transcribe_pcm`, and the integration
/// test exercises this very function instead of a hand-rolled copy.
pub async fn drain_until_stopped(
    mut consumer: HeapAudioConsumer,
    is_recording: Arc<AtomicBool>,
) -> Vec<f32> {
    let mut audio_buffer: Vec<f32> = Vec::with_capacity(SAMPLE_RATE as usize * 5);
    while is_recording.load(Ordering::SeqCst) || !consumer.is_empty() {
        let pending = consumer.occupied_len();
        if pending > 0 {
            let mut chunk = vec![0.0_f32; pending];
            let got = consumer.pop_slice(&mut chunk);
            audio_buffer.extend_from_slice(&chunk[..got]);
        } else {
            tokio::time::sleep(tokio::time::Duration::from_millis(15)).await;
        }
    }
    audio_buffer
}

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
const ASR_SPLIT_SEARCH_FRAC: f32 = 0.40;

/// Frame length used by the split search, in milliseconds.
const ASR_SPLIT_FRAME_MS: f32 = 20.0;

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

#[cfg(test)]
mod asr_split_tests {
    use super::*;

    #[test]
    fn short_audio_is_one_whole_range() {
        let audio = vec![0.5_f32; SAMPLE_RATE as usize * 5];
        assert_eq!(split_for_asr(&audio, SAMPLE_RATE, 30.0), vec![(0, audio.len())]);
    }

    #[test]
    fn empty_audio_yields_one_empty_range() {
        assert_eq!(split_for_asr(&[], SAMPLE_RATE, 30.0), vec![(0, 0)]);
    }

    #[test]
    fn long_audio_is_split_under_the_cap_with_full_coverage() {
        let sr = SAMPLE_RATE;
        let audio = vec![0.5_f32; sr as usize * 100];
        let segs = split_for_asr(&audio, sr, 30.0);
        assert!(segs.len() >= 4, "100 s at a 30 s cap needs ≥4 chunks: {segs:?}");
        let max_len = (30.0 * sr as f32) as usize;
        for &(s, e) in &segs {
            assert!(e > s, "empty range in {segs:?}");
            assert!(e - s <= max_len, "chunk over the cap in {segs:?}");
        }
        // Gapless, non-overlapping, complete coverage.
        assert_eq!(segs[0].0, 0);
        assert_eq!(segs.last().unwrap().1, audio.len());
        for w in segs.windows(2) {
            assert_eq!(w[0].1, w[1].0, "gap or overlap in {segs:?}");
        }
    }

    #[test]
    fn cut_lands_in_the_quiet_gap() {
        let sr = SAMPLE_RATE;
        let mut audio = vec![0.5_f32; sr as usize * 40];
        // A 400 ms silence at 25 s, inside the 18–30 s search window.
        let gap_start = sr as usize * 25;
        let gap_end = gap_start + sr as usize * 4 / 10;
        for s in &mut audio[gap_start..gap_end] {
            *s = 0.0;
        }
        let segs = split_for_asr(&audio, sr, 30.0);
        let cut = segs[0].1;
        assert!(
            cut >= gap_start && cut <= gap_end,
            "cut {cut} outside the silence {gap_start}..{gap_end}"
        );
    }
}
