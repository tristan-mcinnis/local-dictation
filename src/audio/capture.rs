//! Microphone capture: the push-to-talk engine, the always-on pre-roll
//! engine, park and resume, and draining a session from the ring.
use super::*;

pub type HeapAudioConsumer = HeapCons<f32>;

pub type HeapAudioProducer = HeapProd<f32>;

/// The two operations the retained-stream lifecycle needs: stop delivering
/// callbacks, and start again. Implemented for `cpal::Stream`; the deterministic
/// lifecycle tests implement it with a fake, so the park/resume ordering — clear
/// the flag before pausing, only claim recording after `play` succeeds, mark the
/// stream failed on either error — is proven without a microphone.
pub(crate) trait AudioStreamControl {
    fn start_io(&self) -> Result<(), String>;
    fn stop_io(&self) -> Result<(), String>;
}

impl AudioStreamControl for Stream {
    fn start_io(&self) -> Result<(), String> {
        StreamTrait::play(self).map_err(|e| e.to_string())
    }
    fn stop_io(&self) -> Result<(), String> {
        StreamTrait::pause(self).map_err(|e| e.to_string())
    }
}

/// Owns the cpal input Stream (held alive for the recording's duration) plus
/// the recording flag. Drop the engine to stop capture and release the mic.
///
/// Lifecycle is three-state so a warm-window can reuse the stream across
/// back-to-back dictations instead of paying cpal's stream-open cost each time:
///   * **active**  — `start_microphone`/`resume` set `is_recording` true *after*
///     the stream is really running; the callback pushes every resampled chunk
///     into the producer.
///   * **parked**  — `park` cleared `is_recording` and *paused* the retained
///     stream, so cpal stops delivering callbacks: nothing is captured and
///     nothing is dropped. The initialized stream object, and with it the
///     input device, is retained, so the next `resume` restarts the same stream
///     instead of paying a cold open. A refused `pause`/`play` marks the stream
///     [`stream_failed`](Self::stream_failed) for rebuild and raises an
///     actionable audio fault rather than pretending to record.
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
    // Set by cpal's error callback, or by a refused pause/play. A parked stream
    // that has failed must be rebuilt on the next press, not resumed into
    // silence.
    stream_failed: Arc<AtomicBool>,
    // Bumped by `resume` before the recording flag goes true. The callback reads
    // the flag first and this epoch second, so a callback that observes
    // `recording == true` is guaranteed to read the new epoch and drop the
    // previous utterance's fractional resample carry before it emits a sample.
    epoch: Arc<AtomicU64>,
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
                epoch: Arc::new(AtomicU64::new(0)),
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
        // Per-stream callback state: the fractional resample carry, the channel
        // picker, and the epoch it was built at. Held in one value so the
        // park/resume reset seam is a single method (`ResampleState`).
        let epoch = self.epoch.clone();
        let mut state = ResampleState::new(channels, native_rate, epoch.load(Ordering::SeqCst));
        // Grip the recording flag so the callback can gate pushes: while parked
        // (warm-window idle) it must not fill the ring with captured room tone.
        let recording = self.is_recording.clone();
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
                    push_resampled(data, channels, stride, &mut state, &epoch, &mut producer, &recording);
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32_768.0).collect();
                    push_resampled(&f, channels, stride, &mut state, &epoch, &mut producer, &recording);
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
                    push_resampled(&f, channels, stride, &mut state, &epoch, &mut producer, &recording);
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

    /// Park an idle engine: keep the initialized stream (and thus the input
    /// device) retained so a follow-up dictation within the warm window can
    /// [`resume`](Self::resume) without re-opening it, but **pause** its IO so
    /// cpal stops delivering callbacks — nothing is captured and nothing is
    /// dropped. The recording flag is cleared first, so even a backend that
    /// refuses to pause cannot leave samples flowing into the ring.
    ///
    /// Returns `Err` if the stream could not be paused. The stream is then
    /// marked [`stream_failed`](Self::stream_failed) and an actionable audio
    /// fault is raised, so the caller rebuilds instead of trusting a stream
    /// whose run state is unknown. Parking an engine that never opened a stream
    /// is a successful no-op.
    pub fn park(&mut self) -> eyre::Result<()> {
        self.is_recording.store(false, Ordering::SeqCst);
        match self.stream.as_ref() {
            Some(stream) => pause_retained(stream, &self.is_recording, &self.stream_failed),
            // Never started, or already released: nothing to pause.
            None => Ok(()),
        }
    }

    /// Reuse a parked engine: restart the **retained** stream and, only once it
    /// is really running, set the recording flag. Returns that flag so the
    /// worker can hand it to the drain loop. No stream re-open, so this costs a
    /// `StreamTrait::play` call rather than `start_microphone`'s cold open.
    ///
    /// Returns `Err` if there is no retained stream (the caller must cold-open)
    /// or if `play` failed. On a `play` failure the recording flag is **not**
    /// set: the stream is marked [`stream_failed`](Self::stream_failed) and an
    /// actionable fault is raised, so the worker rebuilds instead of recording
    /// silence from a stream that never restarted.
    pub fn resume(&mut self) -> eyre::Result<Arc<AtomicBool>> {
        let Some(stream) = self.stream.as_ref() else {
            return Err(eyre::eyre!(
                "no retained microphone stream to resume; a cold open is required"
            ));
        };
        resume_retained(stream, &self.is_recording, &self.stream_failed, &self.epoch)?;
        Ok(self.is_recording.clone())
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

/// Stop a retained stream's IO. The recording flag is cleared *before* the
/// pause, so a refused pause still cannot capture. On failure the stream is
/// marked for rebuild and an actionable fault is raised: the caller must not
/// keep a stream whose run state is unknown.
pub(super) fn pause_retained(
    stream: &dyn AudioStreamControl,
    recording: &AtomicBool,
    failed: &AtomicBool,
) -> eyre::Result<()> {
    recording.store(false, Ordering::SeqCst);
    if let Err(e) = stream.stop_io() {
        failed.store(true, Ordering::SeqCst);
        crate::health::raise(
            crate::health::Fault::AudioLost,
            format!("microphone stream could not pause: {e}"),
        );
        return Err(eyre::eyre!("stream.pause failed: {e}"));
    }
    Ok(())
}

/// Restart a retained stream's IO for a new utterance. The epoch is bumped
/// first and the recording flag set *last*, so the flag is only true after the
/// backend actually resumed and the callback is guaranteed to reset its
/// resample carry (see [`ResampleState::begin_callback`]). On failure the flag
/// stays false: the app never claims to record on a stream that did not start.
pub(super) fn resume_retained(
    stream: &dyn AudioStreamControl,
    recording: &AtomicBool,
    failed: &AtomicBool,
    epoch: &AtomicU64,
) -> eyre::Result<()> {
    // New utterance: signal the callback to drop the previous one's carry before
    // the flag can say recording.
    epoch.fetch_add(1, Ordering::SeqCst);
    if let Err(e) = stream.start_io() {
        failed.store(true, Ordering::SeqCst);
        crate::health::raise(
            crate::health::Fault::AudioLost,
            format!("microphone stream could not resume: {e}"),
        );
        return Err(eyre::eyre!("stream.play failed: {e}"));
    }
    recording.store(true, Ordering::SeqCst);
    Ok(())
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
pub(super) fn is_bluetooth_device(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("airpods")
        || n.contains("bluetooth")
        || n.contains("beats")
        || n.contains("buds")
        || n.contains("headphones")
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
