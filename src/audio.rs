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
pub struct AudioCaptureEngine {
    is_recording: Arc<AtomicBool>,
    producer: Option<HeapAudioProducer>,
    stream: Option<Stream>,
    // Native rate of the chosen input device, captured when the stream starts.
    // Useful for diagnostics / future smarter resampling.
    pub native_sample_rate: u32,
    pub native_channels: u16,
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
        let err_fn = |e| eprintln!("[audio] cpal stream error: {e}");

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    push_resampled(data, channels, stride, &mut pos, &mut last, &mut producer);
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32_768.0).collect();
                    push_resampled(&f, channels, stride, &mut pos, &mut last, &mut producer);
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
                    push_resampled(&f, channels, stride, &mut pos, &mut last, &mut producer);
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
) {
    let frames = interleaved.len() / channels.max(1);
    if frames == 0 {
        return;
    }

    // Reusable mono buffer.
    let mut mono = Vec::with_capacity(frames);
    if channels <= 1 {
        mono.extend_from_slice(interleaved);
    } else {
        for frame in interleaved.chunks_exact(channels) {
            let sum: f32 = frame.iter().sum();
            mono.push(sum / channels as f32);
        }
    }

    // Push RMS of this chunk to the waveform ring for the UI pill. Computed
    // pre-resample on native mono so it reflects real microphone activity.
    let sum_sq: f32 = mono.iter().map(|x| x * x).sum();
    let rms = (sum_sq / mono.len() as f32).sqrt();
    crate::ui_channel::push_level(rms);

    // Linear-interpolate at `stride`. `pos` indexes into the mono buffer.
    // When pos crosses a frame boundary the carry advances by `stride` again.
    while *pos < mono.len() as f64 {
        let idx = pos.floor() as usize;
        let frac = (*pos - idx as f64) as f32;
        let a = if idx == 0 { *last } else { mono[idx - 1] };
        let b = mono[idx];
        let sample = a + (b - a) * frac;
        let _ = producer.try_push(sample);
        *pos += stride;
    }
    // Subtract the consumed frames so `pos` stays a small fractional offset
    // into the next chunk.
    *pos -= mono.len() as f64;
    *last = *mono.last().unwrap();
}

#[cfg(test)]
mod tests {
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
        assert!(is_bluetooth_device("Tristan's AirPods Pro"));
        assert!(is_bluetooth_device("Beats Studio Buds"));
        assert!(is_bluetooth_device("Sony Bluetooth Headphones"));
        // Built-in / wired inputs must NOT trip the warning.
        assert!(!is_bluetooth_device("MacBook Pro Microphone"));
        assert!(!is_bluetooth_device("External Microphone"));
        assert!(!is_bluetooth_device("USB Audio Device"));
    }

    #[test]
    fn resample_passthrough_at_matching_rate() {
        // stride 1.0 (16k→16k), mono: output mirrors input (first sample uses
        // the `last` carry of 0.0).
        let mut pos = 0.0;
        let mut last = 0.0;
        let out = resample_to_vec(&[0.5, 0.5, 0.5, 0.5], 1, 1.0, &mut pos, &mut last);
        assert_eq!(out.len(), 4);
        assert_eq!(*out.last().unwrap(), 0.5);
    }
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
        Self { cap, buf: VecDeque::with_capacity(cap + 1) }
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
        let recording = self.recording.clone();
        let err_fn = |e| eprintln!("[audio] cpal stream error: {e}");

        // One callback body, generic over the sample type, shared by all formats.
        macro_rules! make_cb {
            ($t:ty, $to_f32:expr) => {
                move |data: &[$t], _: &_| {
                    let f: Vec<f32> = data.iter().map($to_f32).collect();
                    let chunk = resample_to_vec(&f, channels, stride, &mut pos, &mut last);
                    if chunk.is_empty() {
                        return;
                    }
                    let rec = recording.load(Ordering::SeqCst);
                    // Rising edge: flush the lookback captured *before* the press.
                    if rec && !prev_recording {
                        for s in preroll.snapshot() {
                            let _ = producer.try_push(s);
                        }
                    }
                    preroll.push(&chunk);
                    if rec {
                        for &s in &chunk {
                            let _ = producer.try_push(s);
                        }
                    }
                    prev_recording = rec;
                }
            };
        }

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &config,
                make_cb!(f32, |s: &f32| *s),
                err_fn,
                None,
            ),
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

        stream.play().map_err(|e| eyre::eyre!("stream.play failed: {e}"))?;
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
) -> Vec<f32> {
    let frames = interleaved.len() / channels.max(1);
    if frames == 0 {
        return Vec::new();
    }
    let mut mono = Vec::with_capacity(frames);
    if channels <= 1 {
        mono.extend_from_slice(interleaved);
    } else {
        for frame in interleaved.chunks_exact(channels) {
            let sum: f32 = frame.iter().sum();
            mono.push(sum / channels as f32);
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
