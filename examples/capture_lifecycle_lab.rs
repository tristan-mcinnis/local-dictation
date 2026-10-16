//! Retained-capture lifecycle proof for the warm window.
//!
//! Drives the PRODUCTION `AudioCaptureEngine` (real cpal stream, real ring
//! buffer) and only counts + discards the local samples it captures. No ASR, no
//! audio is written to disk, nothing is injected into another app, and no user
//! log or history is read.
//!
//! What it measures and asserts:
//!   * cold open — `start_microphone` cost, the open the warm window skips;
//!   * parked    — after `park` (which pauses the retained stream) the ring is
//!                 empty AND the in-process waveform ring stops changing: no
//!                 callbacks run at all, not merely a gated callback dropping
//!                 samples;
//!   * resume    — `resume` restarts the SAME stream and frames arrive again.
//!
//! The waveform ring (`ui_channel::push_level`) is fed by every capture
//! callback regardless of the recording flag, so "no new levels while parked"
//! is evidence that the IO actually stopped, not just that delivery was gated.
//!
//! Run once no other process is using the microphone (needs mic permission):
//!   cargo run --release --example capture_lifecycle_lab
//!
//! Exit status is 0 only if the paused/resumed assertions hold.

use fast_dictate_backend::audio::{
    drain_session, AudioCaptureEngine, BUFFER_CAPACITY, SAMPLE_RATE,
};
use fast_dictate_backend::ui_channel;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Drain whatever the ring currently holds and return its length. An
/// always-false flag keeps `drain_session` from waiting for a recording to end,
/// so a paused (empty) ring returns immediately with zero.
fn take_queued(consumer: &mut fast_dictate_backend::audio::HeapAudioConsumer) -> usize {
    drain_session(consumer, &AtomicBool::new(false)).len()
}

fn main() -> eyre::Result<()> {
    let (mut engine, mut consumer) = AudioCaptureEngine::new(BUFFER_CAPACITY);

    let t0 = Instant::now();
    engine.start_microphone()?;
    let cold_open = t0.elapsed();
    let device = engine.device_name().to_string();

    // Let the fresh stream prove it delivers, then clear both rings.
    std::thread::sleep(Duration::from_millis(250));
    let delivered_open = take_queued(&mut consumer);

    // Park: the retained stream's IO stops, so no callbacks run. Settle briefly
    // so any callback already in flight finishes, then clear the level ring and
    // watch it stay empty for the whole idle window.
    let t_park = Instant::now();
    engine.park()?;
    let park_ms = t_park.elapsed();
    std::thread::sleep(Duration::from_millis(50));
    // Discard samples accepted between the initial drain and park. Only new
    // arrivals during the paused observation window count against the claim.
    let _ = take_queued(&mut consumer);
    ui_channel::reset_levels();
    let idle_ms = 400u64;
    std::thread::sleep(Duration::from_millis(idle_ms));
    let levels_while_paused = ui_channel::recent_levels().len();
    let delivered_paused = take_queued(&mut consumer);

    // Resume: the SAME stream restarts; callbacks and frames arrive again.
    let t_resume = Instant::now();
    let _flag = engine.resume()?;
    let resume_ms = t_resume.elapsed();
    std::thread::sleep(Duration::from_millis(250));
    let levels_after_resume = ui_channel::recent_levels().len();
    let delivered_resume = take_queued(&mut consumer);

    engine.stop_capture();

    let cold_ms = ms(cold_open).max(0.1);
    println!("capture lifecycle  (device `{device}`, target {SAMPLE_RATE} Hz)");
    println!("  cold open           {:>7.1} ms", ms(cold_open));
    println!("  park (pause IO)     {:>7.1} ms", ms(park_ms));
    println!(
        "  resume (play)       {:>7.1} ms  (resume is {:.0}% of a cold open)",
        ms(resume_ms),
        100.0 * ms(resume_ms) / cold_ms
    );
    println!("  delivered on open   {delivered_open:>7} samples");
    println!("  callbacks paused    {levels_while_paused:>7} levels over {idle_ms} ms idle");
    println!("  delivered paused    {delivered_paused:>7} samples");
    println!("  callbacks resumed   {levels_after_resume:>7} levels");
    println!("  delivered resumed   {delivered_resume:>7} samples");

    assert!(delivered_open > 0, "cold stream delivered no samples");
    assert_eq!(
        levels_while_paused, 0,
        "capture callbacks ran while the stream was parked"
    );
    assert_eq!(
        delivered_paused, 0,
        "paused stream delivered {delivered_paused} samples"
    );
    assert!(levels_after_resume > 0, "no capture callbacks after resume");
    assert!(delivered_resume > 0, "resumed stream delivered no samples");
    println!(
        "  ok: pause stops the callbacks and the ring; resume restarts them on the same stream."
    );
    Ok(())
}
