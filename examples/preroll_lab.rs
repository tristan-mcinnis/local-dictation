//! Pre-roll capture proof — quantifies the leading audio that open-on-press
//! loses and that the always-on pre-roll ring recovers.
//!
//! No models, no mic: we simulate the exact callback logic of both capture
//! strategies on a synthetic "utterance" whose speech begins *before* the key
//! press (the real-world case the user reported — first words clipped). It
//! prints how many ms of leading speech each strategy keeps, so the win is a
//! number, not a claim.
//!
//! Run:  cargo run --release --example preroll_lab
//!
//! The model: a person starts speaking at t=0; their finger reaches the
//! modifier key `press_ms` later; the OS then takes `open_latency_ms` to spin
//! up a freshly-opened input stream. Open-on-press captures nothing until the
//! stream is live (press + open latency). Always-on is already warm and, on the
//! press, flushes its rolling lookback — so it recovers everything back to
//! `now - preroll_ms`.

use fast_dictate_backend::audio::{preroll_samples, PrerollRing, SAMPLE_RATE};

/// Mark a sample as "speech" (1.0) once the speaker has started (t >= 0 here).
fn synth_utterance(total_ms: u32) -> Vec<f32> {
    let n = (SAMPLE_RATE as u64 * total_ms as u64 / 1000) as usize;
    // A simple tone the whole time — every sample is "speech" so any dropped
    // leading sample is unambiguously lost content.
    (0..n).map(|i| if i % 2 == 0 { 0.3 } else { -0.3 }).collect()
}

fn ms_of(samples: usize) -> f64 {
    samples as f64 * 1000.0 / SAMPLE_RATE as f64
}

fn main() {
    let press_ms = 180u32; // finger travel: speech that precedes the key press
    let open_latency_ms = 120u32; // CoreAudio cold stream-start before callbacks flow
    let speech_total_ms = 1500u32;
    let preroll_ms = 400u32;

    let speech = synth_utterance(speech_total_ms);
    let press_at = (SAMPLE_RATE as u64 * press_ms as u64 / 1000) as usize;
    let open_done_at = press_at + (SAMPLE_RATE as u64 * open_latency_ms as u64 / 1000) as usize;

    // ── Open-on-press: nothing is captured until the stream is actually live.
    let on_press_captured = speech.len().saturating_sub(open_done_at);
    let on_press_lost = open_done_at.min(speech.len());

    // ── Always-on pre-roll: the stream was warm the whole time, feeding a
    // rolling ring. We replay the stream up to the press to fill the ring, then
    // on the press flush the lookback + continue live (no open latency).
    let mut ring = PrerollRing::new(preroll_samples(preroll_ms));
    // Feed in realistic ~10 ms callback chunks up to the press moment.
    let chunk = (SAMPLE_RATE / 100) as usize; // 10 ms
    let mut i = 0;
    while i < press_at {
        let end = (i + chunk).min(press_at);
        ring.push(&speech[i..end]);
        i = end;
    }
    let lookback = ring.snapshot(); // flushed into the consumer on press
    let live_after_press = speech.len() - press_at; // no open latency: warm stream
    let always_on_captured = lookback.len() + live_after_press;
    let always_on_lost = speech.len().saturating_sub(always_on_captured);

    println!("pre-roll capture proof  (SR = {SAMPLE_RATE} Hz)");
    println!("  scenario: speech starts at t=0, key pressed at +{press_ms} ms,");
    println!("            cold stream-open latency {open_latency_ms} ms, pre-roll {preroll_ms} ms\n");

    println!("  open-on-press (current default):");
    println!("    captured {:>7.1} ms   lost {:>6.1} ms of leading speech",
        ms_of(on_press_captured), ms_of(on_press_lost));

    println!("  always-on pre-roll:");
    println!("    captured {:>7.1} ms   lost {:>6.1} ms of leading speech",
        ms_of(always_on_captured.min(speech.len())), ms_of(always_on_lost));

    let recovered = ms_of(on_press_lost) - ms_of(always_on_lost);
    println!("\n  → pre-roll recovers {recovered:.1} ms of leading speech that open-on-press drops.");
    println!("    (lookback flushed on press: {:.1} ms; plus zero stream-open latency.)",
        ms_of(lookback.len()));

    // Sanity: with preroll >= press_ms, no leading speech should be lost at all.
    assert!(always_on_lost == 0, "pre-roll {preroll_ms}ms should cover {press_ms}ms press");
    println!("\n  assertion ok: with pre-roll ≥ finger-travel, leading words are never clipped.");
}
