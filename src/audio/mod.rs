use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

mod capture;
mod dsp;
mod silence;
pub use capture::{
    AlwaysOnCapture,
    AudioCaptureEngine,
    HeapAudioConsumer,
    HeapAudioProducer,
    default_input_name,
    drain_session,
    drain_until_stopped,
};
#[cfg(test)]
use capture::*;
pub use dsp::{
    ASR_MAX_GAIN,
    ASR_PEAK_CEILING,
    ASR_TARGET_RMS,
    ChannelPicker,
    DEFAULT_PREROLL_MS,
    Downmix,
    PICK_RMS_FLOOR,
    PROBE_MS,
    PrerollRing,
    SLEEP_SLACK,
    fold_frame,
    normalize_for_asr,
    pick_channel,
    preroll_samples,
    slept_for,
};
use dsp::*;
pub use silence::{
    ASR_MAX_CHUNK_SECS,
    DEFAULT_TAIL_CAP_MS,
    DEFAULT_TAIL_ENGAGE_MS,
    DEFAULT_TAIL_FRAME_MS,
    DEFAULT_TAIL_POLL_MS,
    DEFAULT_TAIL_QUIET_MS,
    DEFAULT_WARM_WINDOW_MS,
    MIN_SPEECH_MS,
    NoiseFloorEstimator,
    TAIL_ABS_FLOOR,
    TAIL_FLOOR_RING_FRAMES,
    TAIL_NOISE_PERCENTILE,
    TAIL_SOFT_ABS_FLOOR,
    TAIL_SOFT_FLOOR_FACTOR,
    TAIL_THRESHOLD_FLOOR_FACTOR,
    TRIM_HANGOVER_MS,
    WHISPER_ABS_FLOOR,
    WHISPER_THRESHOLD_FLOOR_FACTOR,
    frame_rms,
    frames_quiet,
    noise_floor,
    noise_floor_of_audio,
    quiet_frame_count,
    quiet_run,
    recording_tail,
    speech_ms,
    speech_threshold,
    speech_threshold_with_mode,
    split_for_asr,
    tail_capture_engage,
    tail_capture_engage_with_mode,
    tail_speech_threshold,
    transcribe_keep_len,
    trim_trailing_silence,
};

pub const SAMPLE_RATE: u32 = 16_000;
/// 5 min of headroom (16 000 samples/s · 300). The streaming path drains this
/// ring every ~120 ms while the key is held (the worker's `recv_timeout`); the
/// whole-buffer path drains it on release. The cap is headroom for a stalled
/// drain, not the utterance length — the SPSC ring drops the *newest* sample
/// once it is full, so past this point audio is silently lost. At f32 that's
/// ~18 MB, which is fine.
pub const BUFFER_CAPACITY: usize = 16_000 * 300;

#[cfg(test)]
mod tests;

/// Deterministic proofs of the park/resume lifecycle against a fake backend,
/// plus the resample-carry reset at the utterance seam. No microphone and no
/// cpal stream: these exercise the exact `pause_retained`/`resume_retained`
/// functions `AudioCaptureEngine::park`/`resume` call.
#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod asr_split_tests;
