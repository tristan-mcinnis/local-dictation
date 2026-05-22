use fast_dictate_backend::audio::AudioCaptureEngine;
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
    let rb = HeapRb::<f32>::new(1000);
    let (mut prod, mut cons) = rb.split();

    let is_recording = Arc::new(AtomicBool::new(true));

    let pushed = prod.push_slice(&[0.0_f32; 500]);
    assert_eq!(pushed, 500, "ring buffer should accept all 500 samples");

    let flag = is_recording.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        flag.store(false, Ordering::SeqCst);
    });

    let mut audio_buffer = Vec::new();
    while is_recording.load(Ordering::SeqCst) || !cons.is_empty() {
        let pending = cons.occupied_len();
        if pending > 0 {
            let mut chunk = vec![0.0_f32; pending];
            let got = cons.pop_slice(&mut chunk);
            audio_buffer.extend_from_slice(&chunk[..got]);
        } else {
            tokio::time::sleep(tokio::time::Duration::from_millis(2)).await;
        }
    }

    assert_eq!(
        audio_buffer.len(),
        500,
        "drain loop should consume every queued sample after capture stops"
    );
}
