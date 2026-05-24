use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
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
