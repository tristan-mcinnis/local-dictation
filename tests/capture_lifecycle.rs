//! Public lifecycle contract for the warm-window capture engine.
//!
//! These exercise the production `AudioCaptureEngine` without a microphone:
//! they pin the *fallible* park/resume contract the worker relies on, so a
//! stream that is missing or failed is reported rather than silently recording
//! nothing. The pause/play ordering against a backend is covered by the
//! deterministic unit tests in `src/audio.rs`; the real hardware callbacks are
//! covered by `examples/capture_lifecycle_lab.rs`.

use fast_dictate_backend::audio::{AudioCaptureEngine, BUFFER_CAPACITY};

/// Parking an engine that never opened a stream is a no-op, not an error: the
/// warm-window path parks defensively.
#[test]
fn parking_an_engine_with_no_stream_is_a_noop() {
    let (mut engine, _consumer) = AudioCaptureEngine::new(BUFFER_CAPACITY);
    engine
        .park()
        .expect("park with no retained stream must succeed as a no-op");
}

/// Resuming an engine that has no retained stream must fail explicitly so the
/// worker cold-opens, instead of reporting success and recording silence.
#[test]
fn resuming_with_no_retained_stream_fails_explicitly() {
    let (mut engine, _consumer) = AudioCaptureEngine::new(BUFFER_CAPACITY);
    let err = engine
        .resume()
        .expect_err("resume without a retained stream must be an error");
    assert!(
        err.to_string().contains("retained microphone stream"),
        "unhelpful resume error: {err}"
    );
}
