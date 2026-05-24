use fast_dictate_backend::audio::{drain_until_stopped, AudioCaptureEngine};
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapRb,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[test]
fn test_lock_free_ring_buffer_concurrency() {
    let (mut engine, mut consumer) = AudioCaptureEngine::new(1000);
    let _is_recording = engine.start_mock_loop();

    // Let the high-priority audio thread fill some frames.
    std::thread::sleep(std::time::Duration::from_millis(40));

    let pending = consumer.occupied_len();
    assert!(
        pending > 0,
        "audio thread should push data without mutex locks (got {pending})"
    );

    let mut local_drain = vec![0.0_f32; pending];
    let elements_read = consumer.pop_slice(&mut local_drain);
    assert_eq!(elements_read, local_drain.len());

    engine.stop_capture();
}

#[tokio::test]
async fn test_inference_termination_logic() {
    // Exercise the PRODUCTION drain function (`audio::drain_until_stopped`),
    // not a hand-rolled copy — so this test actually guards the real
    // "capture stopped AND buffer empty" termination rule.
    let rb = HeapRb::<f32>::new(1000);
    let (mut prod, cons) = rb.split();

    let is_recording = Arc::new(AtomicBool::new(true));

    let pushed = prod.push_slice(&[0.0_f32; 500]);
    assert_eq!(pushed, 500, "ring buffer should accept all 500 samples");

    let flag = is_recording.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        flag.store(false, Ordering::SeqCst);
    });

    let audio_buffer = drain_until_stopped(cons, is_recording).await;

    assert_eq!(
        audio_buffer.len(),
        500,
        "drain loop should consume every queued sample after capture stops"
    );
}
