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
    let epoch = AtomicU64::new(0);
    let mut state = ResampleState::new(1, 16_000, 0);
    push_resampled(&[0.2; 8], 1, 1.0, &mut state, &epoch, &mut producer, &recording);
    let before = consumer.occupied_len();
    recording.store(false, Ordering::SeqCst); // accepted out-of-band Cancel
    push_resampled(&[0.9; 32], 1, 1.0, &mut state, &epoch, &mut producer, &recording);
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
    rec2.extend(std::iter::repeat_n(0.0, 10));
    let thresh2 = speech_threshold(noise_floor(&rec2, 0.10));
    assert!(frames_quiet(&rec2, thresh2, quiet_frame_count(300, 30)));
}

#[test]
fn trim_trailing_silence_cuts_quiet_tail_keeps_hangover() {
    // 30 ms frames at 16 kHz: frame_len = 480. 10 loud frames + 20 quiet.
    let frame_len = 480;
    let frame_ms = 30;
    let mut samples = Vec::new();
    samples.extend(std::iter::repeat_n(0.3_f32, 10 * frame_len)); // 300 ms speech
    samples.extend(std::iter::repeat_n(0.001_f32, 20 * frame_len)); // 600 ms tail
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
    real.extend(std::iter::repeat_n(0.3_f32, 10 * 480));
    real.extend(std::iter::repeat_n(0.001_f32, 20 * 480));
    let keep = transcribe_keep_len(&real, 480, speech_threshold(0.001), 30).unwrap();
    assert_eq!(keep, 14 * 480);
}

#[test]
fn noise_floor_estimator_tracks_recent_low_percentile_incrementally() {
    // Feed room tone first, then speech; the estimator (fed incrementally)
    // must land on the same low percentile as a one-shot sort over the same
    // input, i.e. the incremental path is not just cheaper but equivalent.
    let mut est = NoiseFloorEstimator::new(TAIL_FLOOR_RING_FRAMES);
    let room_tone = [0.004_f32, 0.005, 0.006, 0.004, 0.005];
    let speech = [0.2_f32, 0.3, 0.15];
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
    tail_quiet.extend(std::iter::repeat_n(0.0, 10));
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
